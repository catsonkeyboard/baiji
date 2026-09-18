//! TUI 应用状态与主循环

use anyhow::Result;
use baiji_agent::{AgentEvent, ConfirmationDecision, SteeringQueue};
use baiji_harness::{AgentHarness, SessionMeta};
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::confirm::ConfirmDialog;
use crate::settings::{self, AutoContinueConfig, RuntimeSettings};
use crate::theme::Theme;

/// 聊天区的一行（逻辑行）
#[derive(Debug, Clone)]
pub enum ChatLine {
    User(String),
    Assistant(String),
    /// 工具活动行：启动/结束摘要
    Tool(String),
    System(String),
}

impl ChatLine {
    pub fn user(content: impl Into<String>) -> Self {
        Self::User(content.into())
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant(content.into())
    }
}

/// 字节数人性化展示
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

/// UI 事件：键盘输入 / 粘贴 / Agent 事件
/// （模型发现结果走专用通道，见事件循环 models_rx）
enum UiEvent {
    Key(KeyEvent),
    Paste(String),
    Agent(AgentEvent),
}

// ===== /config 配置向导 =====

/// 向导步骤（选厂商 → 选端点 → 输 Key → 选模型 → 生效）
#[derive(Debug, Clone, PartialEq)]
pub enum WizardStep {
    Vendor,
    Endpoint,
    Key,
    Model,
    ModelManual,
}

impl WizardStep {
    fn is_list(&self) -> bool {
        matches!(self, Self::Vendor | Self::Endpoint | Self::Model)
    }

    fn is_input(&self) -> bool {
        matches!(self, Self::Key | Self::ModelManual)
    }
}

pub struct ConfigWizard {
    pub step: WizardStep,
    pub vendor: String,
    pub endpoint: Option<String>,
    /// 向导输入的 Key（空 = 沿用现有配置）
    pub api_key: String,
    pub model: Option<String>,
    pub models: Vec<baiji_ai::ModelInfo>,
    pub models_error: Option<String>,
    pub fetching: bool,
    pub selected: usize,
}

impl ConfigWizard {
    fn new(vendor: &str, endpoint: Option<&str>) -> Self {
        Self {
            step: WizardStep::Vendor,
            vendor: vendor.to_string(),
            endpoint: endpoint.map(String::from),
            api_key: String::new(),
            model: None,
            models: Vec::new(),
            models_error: None,
            fetching: false,
            selected: 0,
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn move_down(&mut self, len: usize) {
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }
}

/// 向导输入步骤的按键路由：Enter/Esc 由向导拦截，其余（字符/退格/粘贴）
/// 落入正常输入处理进输入框。
fn wizard_input_step_intercept(code: KeyCode) -> bool {
    matches!(code, KeyCode::Enter | KeyCode::Esc)
}

/// 粘贴内容净化：去掉首尾空白，内部换行折叠为单个空格（API Key/模型名/单行消息语义）
pub fn sanitize_paste(text: &str) -> String {
    text.trim()
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// 从 `dir`（含父目录，≤8 层）读取 git 分支名。
/// worktree 的 `.git` 是文件，`join(".git/HEAD")` 读取失败按"此层无仓库"继续向上。
fn read_git_branch(dir: &std::path::Path) -> Option<String> {
    let mut current = Some(dir);
    for _ in 0..8 {
        let dir = current?;
        current = dir.parent();
        let Ok(head) = std::fs::read_to_string(dir.join(".git/HEAD")) else {
            continue;
        };
        let head = head.trim();
        if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
            return Some(branch.to_string());
        }
        if head.starts_with("gitdir:") {
            return None; // worktree：不追 gitdir 文件
        }
        return head
            .split_whitespace()
            .next()
            .map(|sha| sha.chars().take(7).collect::<String>()); // detached HEAD
    }
    None
}

/// 斜杠命令注册表：(名称, 用法说明)
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    (
        "config",
        "打开配置向导：选厂商 → 端点 → API Key → 模型（热生效）",
    ),
    ("model", "切换模型：/model <名称>，或不带参数打开模型选择器"),
    ("status", "查看当前 vendor / endpoint / model / session"),
    (
        "fork",
        "分叉会话：/fork 继承全部历史；/fork <n> 回到 n 轮之前重来（原会话保留）",
    ),
    ("help", "显示命令帮助"),
];

/// 输入以 `/` 开头时的命令提示（按前缀过滤）。
/// 返回 None = 非斜杠输入；Some(vec) = 匹配的命令（可能为空 = 无匹配）。
pub fn slash_hints(input: &str) -> Option<Vec<(&'static str, &'static str)>> {
    let rest = input.strip_prefix('/')?.trim_start();
    // 空格后进入参数区：不再列命令，但保留精确命令的用法提示
    if rest.contains(' ') {
        let name = rest.split(' ').next().unwrap_or("");
        return Some(
            SLASH_COMMANDS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(n, u)| vec![(*n, *u)])
                .unwrap_or_default(),
        );
    }
    let lower = rest.to_ascii_lowercase();
    Some(
        SLASH_COMMANDS
            .iter()
            .filter(|(n, _)| n.starts_with(lower.as_str()))
            .copied()
            .collect(),
    )
}

/// 斜杠命令解析：("/cmd", "rest args") 或 None
pub fn split_slash(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix('/')?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let (cmd, args) = match rest.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (rest, ""),
    };
    Some((cmd, args))
}

/// 会话选择器状态
pub struct SessionPicker {
    /// 全部会话（过滤的源头）
    all: Vec<SessionMeta>,
    /// 项目过滤键（None = 不过滤显示全部）
    filter: Option<String>,
    /// 树形顺序：根会话最新在前，分叉出的子会话紧跟其父
    pub items: Vec<SessionMeta>,
    /// 与 items 一一对应的缩进深度
    pub depths: Vec<usize>,
    pub selected: usize,
    /// 当前会话 id（过滤后重定位选中用）
    current_id: String,
}

impl SessionPicker {
    /// 按分叉关系排成树，默认选中当前会话。
    /// `project` 有值时初始只显示该项目的会话（当前会话无项目则显示全部）
    pub fn from_metas(metas: Vec<SessionMeta>, current_id: &str) -> Self {
        let current_project = metas
            .iter()
            .find(|m| m.id == current_id)
            .and_then(|m| m.project.clone());
        // 当前会话有项目时初始只显示该项目（无项目 = 旧会话，显示全部）
        let filter = current_project.clone().filter(|key| {
            metas
                .iter()
                .any(|m| m.project.as_deref() == Some(key.as_str()))
        });
        let mut picker = Self {
            all: metas,
            filter,
            items: Vec::new(),
            depths: Vec::new(),
            selected: 0,
            current_id: current_id.to_string(),
        };
        picker.rebuild();
        picker
    }

    /// 用当前过滤条件重建可见列表
    fn rebuild(&mut self) {
        let visible: Vec<SessionMeta> = self
            .all
            .iter()
            .filter(|m| {
                self.filter
                    .as_ref()
                    .is_none_or(|key| m.project.as_deref() == Some(key.as_str()))
            })
            .cloned()
            .collect();
        let tree = baiji_harness::SessionTree::from_metas(visible);
        let (depths, items): (Vec<usize>, Vec<SessionMeta>) = tree
            .flattened()
            .into_iter()
            .map(|(depth, meta)| (depth, meta.clone()))
            .unzip();
        self.selected = items
            .iter()
            .position(|m| m.id == self.current_id)
            .unwrap_or(0);
        self.items = items;
        self.depths = depths;
    }

    /// a 键：本项目 ↔ 全部切换
    pub fn toggle_project_filter(&mut self) {
        if self.filter.is_some() {
            self.filter = None;
        } else {
            // 取当前会话（或首个有项目的会话）的项目作为过滤键
            self.filter = self
                .all
                .iter()
                .find(|m| m.id == self.current_id)
                .and_then(|m| m.project.clone())
                .or_else(|| self.all.iter().find_map(|m| m.project.clone()));
        }
        self.rebuild();
    }

    /// 是否处于项目过滤态（选择器测试断言用）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn filtering_by_project(&self) -> bool {
        self.filter.is_some()
    }

    pub fn selected_meta(&self) -> Option<&SessionMeta> {
        self.items.get(self.selected)
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    /// 选择器展示行：树形缩进 + id · 标题（当前会话打标；
    /// 全部视图下其它项目的会话附项目标注）
    pub fn display_rows(&self, current_id: &str) -> Vec<String> {
        self.items
            .iter()
            .zip(&self.depths)
            .map(|(meta, depth)| {
                let mark = if meta.id == current_id { "▸ " } else { "  " };
                let title = meta.title.as_deref().unwrap_or("(无标题)");
                let branch = if *depth > 0 {
                    format!("{}└ ", "  ".repeat(depth - 1))
                } else {
                    String::new()
                };
                // 项目过滤关闭时，为非当前项目的会话附标注
                let project_tag = if self.filter.is_none() {
                    match &meta.project {
                        Some(project) => format!(" · ⌂{project}"),
                        None => " · ⌂?".to_string(),
                    }
                } else {
                    String::new()
                };
                format!("{mark}{branch}{} · {}{project_tag}", meta.id, title)
            })
            .collect()
    }
}

/// 待用户裁决的确认
pub struct PendingConfirm {
    pub tool_name: String,
    pub args: String,
    pub reply: oneshot::Sender<ConfirmationDecision>,
}

impl PendingConfirm {
    fn from_dialog(dialog: ConfirmDialog) -> Self {
        Self {
            // 完整展示：用户必须能看到自己批准的全部内容（UI 负责折行与溢出提示）
            args: match dialog.request.args.get("command").and_then(|c| c.as_str()) {
                // bash：直接展示命令原文（保留换行）
                Some(command) if dialog.request.tool_name == "bash" => command.to_string(),
                _ => serde_json::to_string_pretty(&dialog.request.args)
                    .unwrap_or_else(|_| dialog.request.args.to_string()),
            },
            tool_name: dialog.request.tool_name,
            reply: dialog.reply,
        }
    }
}

pub struct App {
    harness: Arc<tokio::sync::Mutex<AgentHarness>>,
    /// 最近一次 LLM 调用的真实上下文占用（厂商上报；0 = 尚无数据）
    context_tokens: u64,
    /// 用户 prompt 模板：(调用名, 描述)
    templates: Vec<(String, String)>,
    status_hint: String,
    theme: Theme,
    lines: Vec<ChatLine>,
    input: String,
    /// 滚动偏移；usize::MAX 为「贴底」哨兵，渲染时钳制
    scroll: usize,
    agent_running: bool,
    current_turn: u32,
    tool_calls: u32,
    /// 当前流式回答的累计文本（落定后并入 lines）
    streaming: String,
    /// 进行中的思考内容（只实时展示，不进入聊天记录）
    thinking: String,
    session_id: String,
    /// 当前运行的取消令牌
    cancel: CancellationToken,
    /// 当前运行的 steering 队列（运行中输入进入此处）
    steering: Arc<SteeringQueue>,
    /// 本次会话累计节省的上下文字节（工具输出压缩台账）
    bytes_saved: u64,
    /// 本次会话累计节省的 token 估算（与字节台账同源）
    tokens_saved: u64,
    /// 配置文件路径（/config 向导写回）
    config_path: std::path::PathBuf,
    /// 运行时设置摘要（来自 AppConfig，/status 展示）
    settings_summary: String,
    /// 自动接力配置（T4）
    auto: AutoContinueConfig,
    /// 本次接力链已用轮次（用户手动提交时清零；TurnStarted 累计）
    auto_turns: u32,
    /// 本次 run 是否被中断（中断后不再接力）
    run_interrupted: bool,
    /// 当前生效设置镜像（vendor/endpoint/model/key）
    settings: RuntimeSettings,
    /// 配置向导打开时为 Some
    wizard: Option<ConfigWizard>,
    /// 模型发现结果回送通道（事件循环注入）
    models_tx: Option<UnboundedSender<Result<Vec<baiji_ai::ModelInfo>, String>>>,
    /// 斜杠命令提示的选中项（Tab 补全 / ↑↓ 移动）
    hint_selected: usize,
    /// 会话选择器打开时为 Some
    picker: Option<SessionPicker>,
    /// 待裁决的 HITL 确认
    pending: Option<PendingConfirm>,
    /// 确认请求到达通道（来自 InteractiveApprover）
    confirm_rx: Option<UnboundedReceiver<ConfirmDialog>>,
    /// 当前项目名（项目分组；头部展示）
    project: Option<String>,
    /// 启动时的工作目录（头部展示，~ 缩写）
    workdir: std::path::PathBuf,
    /// 启动时的 git 分支（向上查找 .git/HEAD）
    git_branch: Option<String>,
    /// 渲染帧计数（驱动运行中指示器的旋转动画）
    frame: u64,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        harness: Arc<tokio::sync::Mutex<AgentHarness>>,
        theme: Theme,
        confirm_rx: Option<UnboundedReceiver<ConfirmDialog>>,
        config_path: std::path::PathBuf,
        settings: RuntimeSettings,
        settings_summary: String,
        auto: AutoContinueConfig,
    ) -> Self {
        let session_id = harness
            .try_lock()
            .map(|h| h.session().meta.id.clone())
            .unwrap_or_default();
        let project = harness.try_lock().ok().and_then(|h| h.current_project());
        let workdir = std::env::current_dir().unwrap_or_default();
        let git_branch = read_git_branch(&workdir);
        let status_hint = settings::status_hint(&settings);
        let templates = harness
            .try_lock()
            .map(|h| h.template_list())
            .unwrap_or_default();
        Self {
            harness,
            context_tokens: 0,
            templates,
            status_hint,
            theme,
            settings,
            config_path,
            settings_summary,
            auto,
            auto_turns: 0,
            run_interrupted: false,
            wizard: None,
            models_tx: None,
            hint_selected: 0,
            // 空聊天区由欢迎屏兜底（快捷键提示在那里展示）
            lines: Vec::new(),
            input: String::new(),
            scroll: usize::MAX,
            agent_running: false,
            current_turn: 0,
            tool_calls: 0,
            streaming: String::new(),
            thinking: String::new(),
            session_id,
            cancel: CancellationToken::new(),
            steering: Arc::new(SteeringQueue::new()),
            bytes_saved: 0,
            tokens_saved: 0,
            picker: None,
            pending: None,
            confirm_rx,
            project,
            workdir,
            git_branch,
            frame: 0,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut terminal = ratatui::init();
        // 启用终端 bracketed paste：Cmd+V / Ctrl+Shift+V 粘贴以事件送达
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste).ok();
        let result = self.event_loop(&mut terminal).await;
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste).ok();
        ratatui::restore();
        result
    }

    async fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let (ui_tx, mut ui_rx): (UnboundedSender<UiEvent>, UnboundedReceiver<UiEvent>) =
            mpsc::unbounded_channel();

        // 键盘监听线程：crossterm read 是阻塞 IO，放到独立线程
        let key_tx = ui_tx.clone();
        std::thread::spawn(move || {
            while let Ok(event) = crossterm::event::read() {
                let forwarded = match event {
                    CrosstermEvent::Key(key) => Some(UiEvent::Key(key)),
                    CrosstermEvent::Paste(text) => Some(UiEvent::Paste(text)),
                    _ => None,
                };
                if let Some(event) = forwarded {
                    if key_tx.send(event).is_err() {
                        break;
                    }
                }
            }
        });

        /// select! 各分支产出（统一在 select 之后处理，避免借用冲突）
        enum LoopEvent {
            Tick,
            Key(KeyEvent),
            Paste(String),
            Agent(AgentEvent),
            Dialog(ConfirmDialog),
            Models(Result<Vec<baiji_ai::ModelInfo>, String>),
        }

        // 确认通道移到局部：None 时该分支恒 pending
        let mut confirm_rx = self.confirm_rx.take();
        // 模型发现结果通道（向导拉取任务 → 事件循环）
        let (models_tx, mut models_rx) =
            mpsc::unbounded_channel::<Result<Vec<baiji_ai::ModelInfo>, String>>();
        self.models_tx = Some(models_tx);

        loop {
            // 250ms tick：驱动流式文本的周期性重绘
            let tick = tokio::time::sleep(Duration::from_millis(250));
            let next_dialog = async {
                match confirm_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            };
            let next_models = async { models_rx.recv().await };

            let event = tokio::select! {
                _ = tick => LoopEvent::Tick,
                event = ui_rx.recv() => match event {
                    Some(UiEvent::Key(key)) => LoopEvent::Key(key),
                    Some(UiEvent::Paste(text)) => LoopEvent::Paste(text),
                    Some(UiEvent::Agent(agent_event)) => LoopEvent::Agent(agent_event),
                    None => return Ok(()),
                },
                dialog = next_dialog => match dialog {
                    Some(dialog) => LoopEvent::Dialog(dialog),
                    // 审批人已退出（通道关闭）：不再关注
                    None => LoopEvent::Tick,
                },
                models = next_models => match models {
                    Some(result) => LoopEvent::Models(result),
                    None => LoopEvent::Tick,
                },
            };

            match event {
                LoopEvent::Tick => {
                    if terminal.draw(|frame| crate::ui::draw(frame, self)).is_err() {
                        return Ok(());
                    }
                }
                LoopEvent::Key(key) => {
                    if self.handle_key(key, &ui_tx).await {
                        return Ok(());
                    }
                    if terminal.draw(|frame| crate::ui::draw(frame, self)).is_err() {
                        return Ok(());
                    }
                }
                LoopEvent::Paste(text) => {
                    self.handle_paste(text);
                    if terminal.draw(|frame| crate::ui::draw(frame, self)).is_err() {
                        return Ok(());
                    }
                }
                LoopEvent::Agent(agent_event) => {
                    self.handle_agent_event(agent_event, &ui_tx);
                    if terminal.draw(|frame| crate::ui::draw(frame, self)).is_err() {
                        return Ok(());
                    }
                }
                LoopEvent::Dialog(dialog) => {
                    // 新请求顶掉未裁决的旧请求（按 Deny 处理）
                    self.dismiss_pending(ConfirmationDecision::Deny(
                        "superseded by another request".to_string(),
                    ));
                    self.pending = Some(PendingConfirm::from_dialog(dialog));
                    terminal.draw(|frame| crate::ui::draw(frame, self)).ok();
                }
                LoopEvent::Models(result) => {
                    self.handle_models(result);
                    if terminal.draw(|frame| crate::ui::draw(frame, self)).is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// 处理按键。返回 true 表示退出主循环。
    async fn handle_key(&mut self, key: KeyEvent, ui_tx: &UnboundedSender<UiEvent>) -> bool {
        // Ctrl+C 全局退出
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.dismiss_pending(ConfirmationDecision::Deny("app exit".to_string()));
            return true;
        }

        // 会话选择器模式
        if self.picker.is_some() {
            return self.handle_picker_key(key).await;
        }

        // 配置向导模式：列表步骤全权拦截；输入步骤只拦 Enter/Esc，
        // 字符/退格/粘贴落入下方正常输入处理（进输入框）
        if self.wizard.is_some() {
            let is_input_step = self.wizard.as_ref().is_some_and(|w| w.step.is_input());
            if !is_input_step {
                return self.handle_wizard_key(key, ui_tx).await;
            }
            if wizard_input_step_intercept(key.code) {
                match key.code {
                    KeyCode::Enter => self.wizard_enter(ui_tx).await,
                    _ => self.wizard = None, // Esc
                }
                return false;
            }
            // 其余按键继续走正常输入处理
        }

        // 确认对话框优先拦截
        if self.pending.is_some() {
            self.handle_confirm_key(key);
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                if self.agent_running {
                    self.cancel.cancel();
                    self.lines.push(ChatLine::System("已请求取消…".to_string()));
                } else {
                    return true;
                }
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return false;
                }
                // 斜杠命令（/model /config /status /help）。
                // 用户 prompt 模板（/name 参数）不在此处理：当作普通消息交给 Harness 展开
                let is_template = split_slash(&text)
                    .is_some_and(|(cmd, _)| self.templates.iter().any(|(name, _)| name == cmd));
                if let Some((cmd, args)) = split_slash(&text).filter(|_| !is_template) {
                    self.input.clear();
                    self.handle_slash(cmd, args, ui_tx).await;
                    self.scroll_to_bottom();
                    return false;
                }
                self.input.clear();
                if self.agent_running {
                    // 运行中：作为 steering 注入
                    self.steering.push(&text);
                    self.lines
                        .push(ChatLine::System(format!("（steering）{text}")));
                } else {
                    self.lines.push(ChatLine::user(&text));
                    self.auto_turns = 0; // 用户手动输入 = 新的接力链
                    self.spawn_run(text, ui_tx.clone());
                }
                self.scroll_to_bottom();
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.hint_selected = 0;
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_picker().await;
            }
            KeyCode::Tab => {
                // 斜杠提示激活时 Tab 补全选中的命令
                let hints = slash_hints(&self.input).unwrap_or_default();
                if let Some((name, _)) =
                    hints.get(self.hint_selected.min(hints.len().saturating_sub(1)))
                {
                    self.input = format!("/{name} ");
                }
            }
            KeyCode::Up => {
                // 斜杠提示激活时移动选中项（↑ 也用于滚动，二者按是否在命令输入态区分）
                if slash_hints(&self.input).is_some_and(|h| !h.is_empty()) {
                    self.hint_selected = self.hint_selected.saturating_sub(1);
                } else if self.scroll == usize::MAX {
                    self.scroll = 0;
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(hints) = slash_hints(&self.input) {
                    if self.hint_selected + 1 < hints.len() {
                        self.hint_selected += 1;
                        return false;
                    }
                }
                if self.scroll != usize::MAX {
                    self.scroll += 1;
                }
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.hint_selected = 0;
            }
            KeyCode::PageUp => {
                if self.scroll == usize::MAX {
                    self.scroll = 0;
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            KeyCode::PageDown => {
                if self.scroll != usize::MAX {
                    self.scroll += 1;
                }
            }
            _ => {}
        }
        false
    }

    /// 确认对话框按键：y 允许 / a 全部允许 / n 或 Esc 拒绝
    fn handle_confirm_key(&mut self, key: KeyEvent) {
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ConfirmationDecision::Allow),
            KeyCode::Char('a') | KeyCode::Char('A') => Some(ConfirmationDecision::AllowAll),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                Some(ConfirmationDecision::Deny("user declined".to_string()))
            }
            _ => None,
        };
        if let Some(decision) = decision {
            let name = self
                .pending
                .as_ref()
                .map(|p| p.tool_name.clone())
                .unwrap_or_default();
            self.dismiss_pending(decision.clone());
            let note = match decision {
                ConfirmationDecision::Allow => "已允许",
                ConfirmationDecision::AllowAll => "已允许（本次运行内不再询问）",
                ConfirmationDecision::Deny(_) => "已拒绝",
            };
            self.lines
                .push(ChatLine::System(format!("⚠ {name}: {note}")));
            self.scroll_to_bottom();
        }
    }

    fn dismiss_pending(&mut self, decision: ConfirmationDecision) {
        if let Some(pending) = self.pending.take() {
            let _ = pending.reply.send(decision);
        }
    }

    // ---- 配置向导 ----

    async fn handle_wizard_key(&mut self, key: KeyEvent, ui_tx: &UnboundedSender<UiEvent>) -> bool {
        let is_list = match &self.wizard {
            Some(w) => w.step.is_list(),
            None => return false,
        };

        match key.code {
            KeyCode::Esc => {
                // 任意步骤直接关闭向导（重开成本低，避免多级回退）
                self.wizard = None;
            }
            KeyCode::Up | KeyCode::Char('k') if is_list => {
                if let Some(w) = &mut self.wizard {
                    w.move_up();
                }
            }
            KeyCode::Down | KeyCode::Char('j') if is_list => {
                let len = self.wizard_rows().map(|r| r.len()).unwrap_or(0);
                if let Some(w) = &mut self.wizard {
                    w.move_down(len);
                }
            }
            KeyCode::Enter => self.wizard_enter(ui_tx).await,
            _ => {}
        }
        false
    }

    /// 向导内 Enter：按步骤推进或生效
    async fn wizard_enter(&mut self, ui_tx: &UnboundedSender<UiEvent>) {
        let _ = ui_tx;
        // 先取只读快照，避免与下方可变借用冲突
        let Some(wizard) = self.wizard.as_ref() else {
            return;
        };
        let step = wizard.step.clone();
        let selected = wizard.selected;

        match step {
            WizardStep::Vendor => {
                let Some(vendors) = baiji_ai::all_vendors().get(selected) else {
                    return;
                };
                let id = vendors.id.to_string();
                let wizard = self.wizard.as_mut().unwrap();
                wizard.vendor = id;
                wizard.endpoint = None;
                wizard.step = WizardStep::Endpoint;
                wizard.selected = 0;
            }
            WizardStep::Endpoint => {
                let endpoint = self
                    .wizard
                    .as_ref()
                    .and_then(|w| baiji_ai::find_vendor(&w.vendor))
                    .and_then(|preset| {
                        preset.endpoint_names().get(selected).map(|n| {
                            if *n == "api" {
                                None
                            } else {
                                Some(n.to_string())
                            }
                        })
                    });
                let Some(endpoint) = endpoint else {
                    return;
                };
                let wizard = self.wizard.as_mut().unwrap();
                wizard.endpoint = endpoint;
                wizard.step = WizardStep::Key;
                wizard.selected = 0;
            }
            WizardStep::Key => {
                // 输入框内容为 Key；空 = 沿用现有
                let typed = self.input.trim().to_string();
                self.input.clear();
                {
                    let wizard = self.wizard.as_mut().unwrap();
                    if !typed.is_empty() {
                        wizard.api_key = typed;
                    }
                    wizard.step = WizardStep::Model;
                    wizard.selected = 0;
                    wizard.fetching = true;
                    wizard.models.clear();
                    wizard.models_error = None;
                }
                self.spawn_model_discovery();
            }
            WizardStep::Model => {
                // 最后一项固定为「手动输入」
                let rows_len = self.wizard_rows().map(|r| r.len()).unwrap_or(0);
                let manual = selected + 1 >= rows_len;
                if manual {
                    let wizard = self.wizard.as_mut().unwrap();
                    wizard.step = WizardStep::ModelManual;
                    wizard.selected = 0;
                    return;
                }
                let model = self
                    .wizard
                    .as_ref()
                    .and_then(|w| w.models.get(selected))
                    .map(|m| m.id.clone());
                if let Some(model) = model {
                    self.wizard.as_mut().unwrap().model = Some(model);
                    self.apply_wizard().await;
                }
            }
            WizardStep::ModelManual => {
                let typed = self.input.trim().to_string();
                self.input.clear();
                if typed.is_empty() {
                    return;
                }
                if let Some(w) = &mut self.wizard {
                    w.model = Some(typed);
                }
                self.apply_wizard().await;
            }
        }
    }

    /// 向导当前应使用的 Key：新输入优先；同厂商沿用现有；
    /// 切换了厂商则只取新厂商的环境变量——旧厂商的 Key 绝不外发给新厂商
    fn wizard_key(&self, wizard: &ConfigWizard) -> String {
        if !wizard.api_key.is_empty() {
            wizard.api_key.clone()
        } else if wizard.vendor == self.settings.vendor {
            self.settings.api_key.clone()
        } else {
            settings::key_for_vendor(&wizard.vendor)
        }
    }

    /// 派发模型发现任务（结果经通道回送事件循环）
    fn spawn_model_discovery(&mut self) {
        let Some(wizard) = &self.wizard else { return };
        let settings = RuntimeSettings {
            vendor: wizard.vendor.clone(),
            endpoint: wizard.endpoint.clone(),
            model: None,
            api_key: self.wizard_key(wizard),
        };
        let Some(tx) = &self.models_tx else { return };
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = settings::discover_models_async(&settings)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    /// 模型发现结果回填
    fn handle_models(&mut self, result: Result<Vec<baiji_ai::ModelInfo>, String>) {
        let Some(wizard) = &mut self.wizard else {
            return;
        };
        if wizard.step != WizardStep::Model {
            return;
        }
        wizard.fetching = false;
        match result {
            Ok(models) => {
                wizard.models = models;
                wizard.selected = 0;
            }
            Err(e) => wizard.models_error = Some(e),
        }
    }

    /// 保存 + 重建 Provider + 热切换 + 状态栏更新
    async fn apply_wizard(&mut self) {
        let Some(wizard) = self.wizard.take() else {
            return;
        };
        let api_key = self.wizard_key(&wizard);
        // 新模型的上下文窗口（发现值优先，否则内置兜底）→ 压缩阈值
        let limits = wizard
            .model
            .as_deref()
            .map(|id| baiji_ai::model_limits(id, wizard.models.iter().find(|m| m.id == id)));
        // 只有用户显式输入的 Key 才落盘；展开后的明文（来自 $ENV）绝不回写
        let typed_key = wizard.api_key.clone();
        let key_update = if !typed_key.is_empty() {
            settings::KeyUpdate::Set(&typed_key)
        } else if wizard.vendor != self.settings.vendor {
            settings::KeyUpdate::Remove
        } else {
            settings::KeyUpdate::Keep
        };
        let new_settings = RuntimeSettings {
            vendor: wizard.vendor,
            endpoint: wizard.endpoint.filter(|e| e != "api"),
            model: wizard.model,
            api_key,
        };

        if let Err(e) = settings::save(&self.config_path, &new_settings, key_update) {
            self.lines
                .push(ChatLine::System(format!("✗ 配置保存失败: {e}")));
            self.scroll_to_bottom();
            return;
        }
        match settings::build_provider(&new_settings) {
            Ok(provider) => {
                let hint = settings::status_hint(&new_settings);
                {
                    let mut harness = self.harness.lock().await;
                    harness.swap_provider(provider);
                    if let Some(limits) = limits {
                        harness.set_context_window(limits.context_length);
                    }
                }
                self.status_hint = hint;
                let endpoint = new_settings.endpoint.as_deref().unwrap_or("api");
                self.lines.push(ChatLine::System(format!(
                    "✓ 配置已生效：{} · endpoint={} · model={}（已写入配置文件）",
                    new_settings.vendor,
                    endpoint,
                    new_settings.model.as_deref().unwrap_or("?")
                )));
                self.settings = new_settings;
            }
            Err(e) => {
                self.lines.push(ChatLine::System(format!(
                    "✗ 切换失败（配置已保存，重启后生效）: {e}"
                )));
                self.settings = new_settings;
            }
        }
        self.scroll_to_bottom();
    }

    // ---- 斜杠命令 ----

    async fn handle_slash(&mut self, cmd: &str, args: &str, ui_tx: &UnboundedSender<UiEvent>) {
        let _ = ui_tx;
        match cmd {
            "help" => {
                self.lines.push(ChatLine::System(
                    "命令：/config 打开配置向导（厂商/端点/Key/模型）· /model [名称] 切换模型（不带参数打开选择）· /fork [n] 分叉会话（n = 回退轮数）· /status 查看当前配置 · /help 本帮助"
                        .to_string(),
                ));
                if !self.templates.is_empty() {
                    let list: Vec<String> = self
                        .templates
                        .iter()
                        .map(|(name, desc)| {
                            if desc.is_empty() {
                                format!("/{name}")
                            } else {
                                format!("/{name}（{desc}）")
                            }
                        })
                        .collect();
                    self.lines.push(ChatLine::System(format!(
                        "prompt 模板：{}",
                        list.join(" · ")
                    )));
                }
            }
            "fork" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        "运行中无法分叉，先按 Esc 取消".to_string(),
                    ));
                } else {
                    match args.trim() {
                        "" => self.fork_session(0).await,
                        n => match n.parse::<usize>() {
                            Ok(turns) => self.fork_session(turns).await,
                            Err(_) => self.lines.push(ChatLine::System(
                                "用法：/fork 或 /fork <回退轮数>".to_string(),
                            )),
                        },
                    }
                }
            }
            "status" => {
                let s = &self.settings;
                self.lines.push(ChatLine::System(format!(
                    "vendor: {} · endpoint: {} · model: {} · session: {}\n{}",
                    s.vendor,
                    s.endpoint.as_deref().unwrap_or("api"),
                    s.model.as_deref().unwrap_or("自动发现"),
                    self.session_id,
                    self.settings_summary
                )));
            }
            "config" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        "运行中不可修改配置，请先等待或 Esc 取消".to_string(),
                    ));
                    return;
                }
                let s = &self.settings;
                self.wizard = Some(ConfigWizard::new(&s.vendor, s.endpoint.as_deref()));
            }
            "model" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        "运行中不可修改配置，请先等待或 Esc 取消".to_string(),
                    ));
                    return;
                }
                if args.is_empty() {
                    // 直接进入模型选择（沿用当前厂商与 Key）
                    let s = &self.settings;
                    let mut wizard = ConfigWizard::new(&s.vendor, s.endpoint.as_deref());
                    wizard.step = WizardStep::Model;
                    wizard.fetching = true;
                    self.wizard = Some(wizard);
                    self.spawn_model_discovery();
                } else {
                    let mut new_settings = self.settings.clone();
                    new_settings.model = Some(args.to_string());
                    // 复用向导生效路径
                    self.wizard = Some(ConfigWizard {
                        step: WizardStep::ModelManual,
                        vendor: new_settings.vendor.clone(),
                        endpoint: new_settings.endpoint.clone(),
                        api_key: String::new(),
                        model: new_settings.model.clone(),
                        models: Vec::new(),
                        models_error: None,
                        fetching: false,
                        selected: 0,
                    });
                    // 直接应用
                    self.apply_wizard().await;
                }
            }
            other => {
                self.lines.push(ChatLine::System(format!(
                    "未知命令 /{other}（可用: /config /model /fork /status /help）"
                )));
            }
        }
    }

    // ---- 向导列表行（供键处理与渲染共用）----

    // ---- 会话选择器 ----

    async fn open_picker(&mut self) {
        let sessions = {
            match self.harness.try_lock() {
                Ok(harness) => harness.list_sessions().unwrap_or_default(),
                Err(_) => return, // harness 忙（运行中）——不打开
            }
        };
        if sessions.is_empty() {
            self.lines
                .push(ChatLine::System("（暂无历史会话）".to_string()));
            return;
        }
        self.picker = Some(SessionPicker::from_metas(sessions, &self.session_id));
    }

    async fn handle_picker_key(&mut self, key: KeyEvent) -> bool {
        let Some(picker) = &mut self.picker else {
            return false;
        };
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
            }
            KeyCode::Char('a') => picker.toggle_project_filter(),
            KeyCode::Up | KeyCode::Char('k') => picker.move_up(),
            KeyCode::Down | KeyCode::Char('j') => picker.move_down(),
            KeyCode::Enter => {
                let selected = picker.selected_meta().cloned();
                self.picker = None;
                if let Some(meta) = selected {
                    self.switch_session(&meta.id).await;
                }
            }
            KeyCode::Char('b') => {
                self.picker = None;
                self.branch_session().await;
            }
            _ => {}
        }
        false
    }

    async fn switch_session(&mut self, session_id: &str) {
        let result = {
            let mut harness = self.harness.lock().await;
            harness
                .switch_session(session_id)
                .map(|_| harness.session().clone())
        };
        match result {
            Ok(session) => {
                self.rebuild_lines(&session.messages);
                self.session_id = session.meta.id.clone();
                let title = session.meta.title.as_deref().unwrap_or("");
                self.lines.push(ChatLine::System(format!(
                    "已切换会话 {session_id} · {title}"
                )));
            }
            Err(e) => {
                self.lines
                    .push(ChatLine::System(format!("✗ 切换会话失败: {e}")));
            }
        }
        self.scroll_to_bottom();
    }

    /// 分叉并回退 `turns_back` 轮；被丢弃的那条提问回填到输入框，便于修改后重发
    async fn fork_session(&mut self, turns_back: usize) {
        let result = {
            let mut harness = self.harness.lock().await;
            harness.branch_rewind(turns_back).map(|dropped| {
                (
                    harness.session().meta.id.clone(),
                    harness.session().messages.clone(),
                    dropped,
                )
            })
        };
        match result {
            Ok((new_id, messages, dropped)) => {
                self.session_id = new_id.clone();
                self.rebuild_lines(&messages);
                self.context_tokens = 0;
                self.lines.push(ChatLine::System(if turns_back == 0 {
                    format!("已从当前会话分叉 → {new_id}（历史已继承）")
                } else {
                    format!(
                        "已分叉 → {new_id}（回退 {turns_back} 轮；原会话保留，可用会话选择器切回）"
                    )
                }));
                if let Some(input) = dropped {
                    self.input = sanitize_paste(&input);
                }
            }
            Err(e) => self
                .lines
                .push(ChatLine::System(format!("✗ 分叉失败: {e}"))),
        }
        self.scroll_to_bottom();
    }

    async fn branch_session(&mut self) {
        let result = {
            let mut harness = self.harness.lock().await;
            harness.branch().map(|_| harness.session().meta.id.clone())
        };
        match result {
            Ok(new_id) => {
                self.session_id = new_id.clone();
                self.lines.push(ChatLine::System(format!(
                    "已从当前会话分叉 → {new_id}（历史已继承）"
                )));
            }
            Err(e) => {
                self.lines
                    .push(ChatLine::System(format!("✗ 分叉失败: {e}")));
            }
        }
        self.scroll_to_bottom();
    }

    fn rebuild_lines(&mut self, messages: &[baiji_ai::Message]) {
        use baiji_ai::Role;
        self.lines.clear();
        for msg in messages {
            match msg.role {
                Role::User => self.lines.push(ChatLine::user(&msg.content)),
                Role::Assistant if !msg.content.is_empty() => {
                    self.lines.push(ChatLine::assistant(&msg.content))
                }
                Role::Assistant => {
                    let names: Vec<&str> = msg
                        .tool_calls
                        .as_ref()
                        .map(|cs| cs.iter().map(|c| c.name.as_str()).collect())
                        .unwrap_or_default();
                    self.lines
                        .push(ChatLine::Tool(format!("● {}", names.join(", "))));
                }
                Role::Tool => {
                    for result in msg.tool_results.iter().flatten() {
                        let brief: String = result.content.chars().take(120).collect();
                        self.lines.push(ChatLine::Tool(format!("⎿ {brief}")));
                    }
                }
                Role::System => {
                    self.lines
                        .push(ChatLine::System("[历史摘要已注入]".to_string()));
                }
            }
        }
        self.streaming.clear();
    }

    /// 处理 Agent 事件（`ui_tx` 供自动接力发起下一次 run）
    fn handle_agent_event(&mut self, event: AgentEvent, ui_tx: &UnboundedSender<UiEvent>) {
        match event {
            AgentEvent::TurnStarted { turn } => {
                self.auto_turns += 1; // 接力链预算（用户手动提交时清零）
                self.current_turn = turn;
            }
            // 持久化层的内部事件（Harness 不转发），UI 无需处理
            AgentEvent::MessageCommitted { .. } => {}
            AgentEvent::StreamRestarted => {
                self.streaming.clear();
                self.thinking.clear();
            }
            AgentEvent::UsageReported {
                input_tokens,
                output_tokens,
            } => {
                self.context_tokens = input_tokens as u64 + output_tokens as u64;
            }
            AgentEvent::TextDelta { text } => {
                // 答案开始输出：思考过程让位
                self.thinking.clear();
                self.streaming.push_str(&text);
                self.scroll_to_bottom();
            }
            AgentEvent::ReasoningDelta { text } => {
                self.thinking.push_str(&text);
                self.scroll_to_bottom();
            }
            AgentEvent::ToolStarted { name, args, .. } => {
                self.thinking.clear();
                let brief: String = args.to_string().chars().take(80).collect();
                self.lines
                    .push(ChatLine::Tool(format!("● {name}({brief})")));
                self.scroll_to_bottom();
            }
            AgentEvent::ToolFinished {
                output,
                is_error,
                original_bytes,
                original_tokens,
                ..
            } => {
                self.tool_calls += 1;
                if let Some(original) = original_bytes {
                    self.bytes_saved = self
                        .bytes_saved
                        .saturating_add(original.saturating_sub(output.len() as u64));
                }
                if let Some(original) = original_tokens {
                    let delivered = baiji_agent::estimate_text_tokens(&output) as u64;
                    self.tokens_saved = self
                        .tokens_saved
                        .saturating_add(original.saturating_sub(delivered));
                }
                let brief: String = output.chars().take(120).collect();
                // 工具名已在 ● 行出现过；⎿ 行只给结果摘要，出错时带 ✗ 前缀
                let mark = if is_error { "✗ " } else { "" };
                self.lines.push(ChatLine::Tool(format!("⎿ {mark}{brief}")));
                self.scroll_to_bottom();
            }
            AgentEvent::TurnFinished { .. } => self.thinking.clear(),
            AgentEvent::RunCompleted { answer } => {
                if !self.streaming.is_empty() {
                    self.lines.push(ChatLine::assistant(self.streaming.clone()));
                    self.streaming.clear();
                } else if !answer.is_empty() {
                    self.lines.push(ChatLine::assistant(&answer));
                }
                self.finish_run();
                self.maybe_auto_continue(ui_tx.clone());
            }
            AgentEvent::RunFailed { error } => {
                self.streaming.clear();
                self.thinking.clear();
                self.lines
                    .push(ChatLine::System(format!("✗ 出错: {error}")));
                self.finish_run();
            }
            AgentEvent::Interrupted => {
                self.streaming.clear();
                self.thinking.clear();
                self.lines.push(ChatLine::System("已取消".to_string()));
                self.run_interrupted = true; // 中断后本次接力链终止
                self.finish_run();
            }
        }
    }

    fn finish_run(&mut self) {
        self.agent_running = false;
        self.current_turn = 0;
        self.tool_calls = 0;
        self.scroll_to_bottom();
    }

    /// 自动接力（T4）：run 正常结束且 todo 仍有未完成项、轮次未超上限时，
    /// 以固定输入继续（用户可见、进入历史；Esc 可随时终止链）
    fn maybe_auto_continue(&mut self, ui_tx: UnboundedSender<UiEvent>) {
        if !self.auto.enabled || self.agent_running || self.run_interrupted {
            return;
        }
        let has_open = self
            .harness
            .try_lock()
            .map(|h| h.has_open_todos())
            .unwrap_or(false);
        if !has_open {
            return;
        }
        if self.auto_turns >= self.auto.max_turns {
            self.lines.push(ChatLine::System(format!(
                "⏹ 自动接力停止：达到轮次上限 {}（todo 仍有未完成项，可手动继续）",
                self.auto.max_turns
            )));
            return;
        }
        self.lines.push(ChatLine::System(format!(
            "⏩ 自动接力（轮次 {}/{}）：todo 未完成，继续任务（Esc 可停）",
            self.auto_turns, self.auto.max_turns
        )));
        self.scroll_to_bottom();
        self.spawn_run(baiji_harness::AUTO_CONTINUE_PROMPT.to_string(), ui_tx);
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll = usize::MAX; // 哨兵值，渲染时钳制
    }

    /// 启动一次 agent 运行（后台任务），事件转发回 UI 通道
    fn spawn_run(&mut self, text: String, ui_tx: UnboundedSender<UiEvent>) {
        self.agent_running = true;
        self.run_interrupted = false;

        let steering = Arc::new(SteeringQueue::new());
        self.steering = Arc::clone(&steering);
        let cancel = CancellationToken::new();
        self.cancel = cancel.clone();

        let harness = Arc::clone(&self.harness);
        tokio::spawn(async move {
            let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
            // 转发任务事件到 UI
            let forward = tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if ui_tx.send(UiEvent::Agent(event)).is_err() {
                        break;
                    }
                }
            });

            let result = {
                let mut harness = harness.lock().await;
                harness.run(text, &tx, &cancel, &steering).await
            };
            drop(tx);
            let _ = forward.await;
            if let Err(e) = result {
                tracing::error!("agent run failed: {e}");
            }
        });
    }

    /// 状态栏左半：运行状态（旋转指示器由 ui 按帧计数拼上）
    pub(crate) fn status_left(&self) -> String {
        if self.agent_running {
            format!("Turn {} · {} tools", self.current_turn, self.tool_calls)
        } else {
            "就绪".to_string()
        }
    }

    /// 状态栏右半：会话与运行台账（各项在无数据时省略）
    pub(crate) fn status_right(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.context_tokens > 0 {
            parts.push(format!("ctx {:.1}k", self.context_tokens as f64 / 1000.0));
        }
        if self.bytes_saved > 0 {
            parts.push(format!(
                "省 {} (~{} tok)",
                format_bytes(self.bytes_saved),
                self.tokens_saved
            ));
        }
        if self.auto.enabled && self.auto_turns > 0 {
            parts.push(format!("自动 {}/{}", self.auto_turns, self.auto.max_turns));
        }
        parts.push(format!("session {}", self.session_id));
        parts.join(" · ")
    }

    // 访问器供 ui 模块渲染
    pub(crate) fn lines(&self) -> &[ChatLine] {
        &self.lines
    }

    pub(crate) fn streaming(&self) -> &str {
        &self.streaming
    }

    /// 思考内容的尾部（最多 `max_chars` 字符）：思考可能很长，只展示最新进展
    pub(crate) fn thinking_tail(&self, max_chars: usize) -> Option<String> {
        if self.thinking.trim().is_empty() {
            return None;
        }
        let total = self.thinking.chars().count();
        let tail: String = self
            .thinking
            .chars()
            .skip(total.saturating_sub(max_chars))
            .collect();
        Some(if total > max_chars {
            format!("…{tail}")
        } else {
            tail
        })
    }

    pub(crate) fn input(&self) -> &str {
        &self.input
    }

    pub(crate) fn scroll(&self) -> usize {
        self.scroll
    }

    pub(crate) fn set_scroll(&mut self, value: usize) {
        self.scroll = value;
    }

    pub(crate) fn agent_running(&self) -> bool {
        self.agent_running
    }

    pub(crate) fn theme(&self) -> Theme {
        self.theme
    }

    /// 头部/输入框右下角的设置提示（"智谱 GLM · glm-4.7"）
    pub(crate) fn hint(&self) -> &str {
        &self.status_hint
    }

    /// 当前项目名（项目分组；无项目时 None）
    pub(crate) fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    /// 启动时的工作目录（~ 缩写后的展示串）
    pub(crate) fn workdir_display(&self) -> String {
        let path = self.workdir.display().to_string();
        std::env::var("HOME")
            .ok()
            .filter(|home| !home.is_empty() && path.starts_with(home.as_str()))
            .map(|home| format!("~{}", &path[home.len()..]))
            .unwrap_or(path)
    }

    /// 启动时的 git 分支（非仓库时 None）
    pub(crate) fn git_branch(&self) -> Option<&str> {
        self.git_branch.as_deref()
    }

    /// 渲染帧计数 +1（旋转指示器动画）
    pub(crate) fn bump_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    pub(crate) fn frame(&self) -> u64 {
        self.frame
    }

    /// 当前任务清单快照（右上角悬浮面板；锁忙时跳过本帧）
    pub(crate) fn todo_items(&self) -> Vec<baiji_harness::TodoItem> {
        self.harness
            .try_lock()
            .map(|h| h.todos_snapshot())
            .unwrap_or_default()
    }

    /// 会话是否尚无内容（欢迎屏兜底展示）
    pub(crate) fn is_fresh(&self) -> bool {
        self.lines.is_empty() && self.streaming.is_empty()
    }

    pub(crate) fn picker(&self) -> Option<&SessionPicker> {
        self.picker.as_ref()
    }

    pub(crate) fn picker_rows(&self) -> Option<Vec<String>> {
        self.picker
            .as_ref()
            .map(|p| p.display_rows(&self.session_id))
    }

    pub(crate) fn pending_confirm(&self) -> Option<&PendingConfirm> {
        self.pending.as_ref()
    }

    /// 粘贴并入输入框（换行折叠为空格，保持单行输入语义）
    pub(crate) fn handle_paste(&mut self, text: String) {
        self.input.push_str(&sanitize_paste(&text));
    }

    pub(crate) fn wizard_active(&self) -> bool {
        self.wizard.is_some()
    }

    /// 斜杠命令提示（供渲染）：(选中下标, 匹配命令)
    pub(crate) fn slash_hints_view(&self) -> Option<(usize, Vec<(&'static str, &'static str)>)> {
        let hints = slash_hints(&self.input)?;
        Some((self.hint_selected.min(hints.len().saturating_sub(1)), hints))
    }

    /// 向导标题（按步骤）
    pub(crate) fn wizard_title(&self) -> Option<String> {
        let wizard = self.wizard.as_ref()?;
        Some(match wizard.step {
            WizardStep::Vendor => " 选择厂商 (Enter=选择 Esc=取消) ".to_string(),
            WizardStep::Endpoint => format!(" 选择端点 · {} (Enter=选择 Esc=返回) ", wizard.vendor),
            WizardStep::Key => format!(
                " 输入 API Key（{} 提示；留空沿用现有）· 输入框回车确认 ",
                baiji_ai::find_vendor(&wizard.vendor)
                    .map(|v| v.api_key_env)
                    .unwrap_or("?")
            ),
            WizardStep::Model => " 选择模型 (Enter=使用 末项=手动输入 Esc=返回) ".to_string(),
            WizardStep::ModelManual => " 手动输入模型名（输入框回车确认） ".to_string(),
        })
    }

    /// 向导列表行（输入步骤返回 None，改由对话框渲染）
    pub(crate) fn wizard_rows(&self) -> Option<Vec<String>> {
        let wizard = self.wizard.as_ref()?;
        match wizard.step {
            WizardStep::Vendor => Some(
                baiji_ai::all_vendors()
                    .iter()
                    .map(|v| format!("{} — {}（key: {}）", v.id, v.display_name, v.api_key_env))
                    .collect(),
            ),
            WizardStep::Endpoint => {
                let preset = baiji_ai::find_vendor(&wizard.vendor)?;
                Some(
                    preset
                        .endpoint_names()
                        .iter()
                        .map(|name| {
                            if *name == "api" {
                                format!("api — 默认端点（按量付费）· {}", preset.base_url)
                            } else if let Some(v) = preset.variants.iter().find(|v| v.name == *name)
                            {
                                format!("{} — {} · {}", v.name, v.note, v.base_url)
                            } else {
                                name.to_string()
                            }
                        })
                        .collect(),
                )
            }
            WizardStep::Model => {
                let mut rows: Vec<String> = Vec::new();
                if wizard.fetching {
                    rows.push("（模型列表获取中…）".to_string());
                } else if let Some(err) = &wizard.models_error {
                    rows.push(format!("模型列表获取失败：{err}"));
                    rows.push(
                        "→ 请选末项「手动输入模型名…」直接指定（Coding Plan 端点常无列表接口）"
                            .to_string(),
                    );
                } else {
                    rows.extend(wizard.models.iter().map(|m| match &m.display_name {
                        Some(d) if d != &m.id => format!("{} — {}", m.id, d),
                        _ => m.id.clone(),
                    }));
                }
                rows.push("✏ 手动输入模型名…".to_string());
                Some(rows)
            }
            WizardStep::Key | WizardStep::ModelManual => None,
        }
    }

    pub(crate) fn wizard_selected(&self) -> usize {
        self.wizard.as_ref().map(|w| w.selected).unwrap_or(0)
    }

    /// 输入型向导步骤的提示文案（Key/ModelManual 对话框）
    pub(crate) fn wizard_input_prompt(&self) -> Option<String> {
        let wizard = self.wizard.as_ref()?;
        match wizard.step {
            WizardStep::Key => Some(format!(
                "API Key（推荐环境变量 {}；直接粘贴回车，留空沿用现有）",
                baiji_ai::find_vendor(&wizard.vendor)
                    .map(|v| v.api_key_env)
                    .unwrap_or("?")
            )),
            WizardStep::ModelManual => Some("模型名称（如 glm-4.7）".to_string()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2.0KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0MB");
    }

    #[test]
    fn test_sanitize_paste() {
        // Key 粘贴：常见尾随换行被去掉
        assert_eq!(sanitize_paste("sk-abc123\n"), "sk-abc123");
        assert_eq!(sanitize_paste("  sk-abc123  \r\n"), "sk-abc123");
        // 多行折叠为单行
        assert_eq!(sanitize_paste("line1\nline2\n"), "line1 line2");
        assert_eq!(sanitize_paste(""), "");
    }

    /// 全布局渲染冒烟（TestBackend）：修复过 chunks 越界 panic 的回归测试；
    /// 含向导输入步骤的按键/粘贴路径（回归：字符曾被吞掉）
    #[tokio::test]
    async fn test_render_all_layouts_no_panic() {
        use async_trait::async_trait;
        use baiji_agent::{AgentRuntime, ToolRegistry};
        use baiji_ai::{ChatRequest, ChatResponse, Protocol, Provider, StreamChunk};
        use futures::StreamExt as _;
        use futures::stream::BoxStream;

        struct Echo;
        #[async_trait]
        impl Provider for Echo {
            async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
                unreachable!()
            }
            async fn chat_stream(
                &self,
                _: ChatRequest,
            ) -> anyhow::Result<BoxStream<'static, anyhow::Result<StreamChunk>>> {
                Ok(futures::stream::iter(vec![Ok(StreamChunk::Done)]).boxed())
            }
            fn protocol(&self) -> Protocol {
                Protocol::OpenAIChat
            }
            fn model(&self) -> &str {
                "mock"
            }
            fn provider_name(&self) -> &str {
                "mock"
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(AgentRuntime::new(Arc::new(Echo)).with_tools(ToolRegistry::new()));
        let mut harness = AgentHarness::new(runtime, dir.path().join("sessions")).unwrap();
        let todo_store = Arc::new(baiji_harness::TodoStore::new());
        harness.set_todos(todo_store.clone());
        let mut app = App::new(
            Arc::new(tokio::sync::Mutex::new(harness)),
            Theme::dark(),
            None,
            dir.path().join("config.json"),
            RuntimeSettings {
                vendor: "glm".to_string(),
                endpoint: None,
                model: Some("glm-4.7".to_string()),
                api_key: "k".to_string(),
            },
            "max_turns: 24 · compaction: on (auto)".to_string(),
            AutoContinueConfig::default(),
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let screen = |t: &ratatui::Terminal<ratatui::backend::TestBackend>| -> Vec<String> {
            let buffer = t.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect()
        };

        // 普通三段布局（空会话：欢迎屏兜底）
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let rows = screen(&terminal);
        assert!(
            rows.iter().any(|r| r.contains("██████╗")),
            "welcome screen shows the logo"
        );
        assert!(
            rows.iter().any(|r| r.contains("/help")),
            "welcome screen shows key hints"
        );

        // 多行回答保留换行（回归：曾被压成一行）；长中文回复贴底时末行可见
        // （回归：按字符数而非显示宽度估行，滚不到底）
        app.lines
            .push(ChatLine::Assistant("fn main() {\n    hi();\n}".to_string()));
        app.lines.push(ChatLine::Assistant(format!(
            "{}终点标记",
            "中文".repeat(400)
        )));
        app.scroll_to_bottom();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let rows = screen(&terminal);
        // 宽字符后跟一个空占位格，去掉空格再比对
        assert!(
            rows.iter().any(|r| r.replace(' ', "").contains("终点标记")),
            "bottom of a long CJK reply must be reachable"
        );
        app.scroll = 0;
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let rows = screen(&terminal);
        let code_row = rows.iter().position(|r| r.contains("fn main() {")).unwrap();
        assert!(
            rows[code_row + 1].contains("    hi();"),
            "newlines preserved"
        );
        assert!(rows[code_row + 2].contains('}'));
        app.lines.clear();

        // 用户消息（❯ 前缀 + 底色条）与工具活动行：● Name(args) / ⎿ 结果（错误带 ✗）
        app.lines.push(ChatLine::user("帮我看看这个项目"));
        app.lines
            .push(ChatLine::Tool(r#"● read({"path":"lib.rs"})"#.to_string()));
        app.lines.push(ChatLine::Tool("⎿ 200 行已读取".to_string()));
        app.lines
            .push(ChatLine::Tool("⎿ ✗ bash: exit 1".to_string()));
        app.scroll_to_bottom();
        // TestBackend 的增量刷新对宽字符覆盖有残留：断言前强制全量重绘
        terminal.clear().unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let rows = screen(&terminal);
        assert!(rows.iter().any(|r| r.contains("❯")), "user prompt mark");
        assert!(
            rows.iter()
                .any(|r| r.replace(' ', "").contains("帮我看看这个项目"))
        );
        assert!(rows.iter().any(|r| r.contains("● read")));
        assert!(
            rows.iter()
                .any(|r| r.replace(' ', "").contains("⎿200行已读取"))
        );

        // 右上角悬浮 todo 面板：有清单才出现
        todo_store.replace(vec![
            baiji_harness::TodoItem {
                id: 1,
                content: "分析依赖".to_string(),
                status: baiji_harness::TodoStatus::Done,
                note: None,
            },
            baiji_harness::TodoItem {
                id: 2,
                content: "实现悬浮面板".to_string(),
                status: baiji_harness::TodoStatus::InProgress,
                note: None,
            },
            baiji_harness::TodoItem {
                id: 3,
                content: "补测试".to_string(),
                status: baiji_harness::TodoStatus::Pending,
                note: None,
            },
        ]);
        terminal.clear().unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let rows = screen(&terminal);
        // 宽字符的跳过格在 TestBackend 里呈现为空格：比对前去掉
        let plain = |s: &str| s.replace(' ', "");
        assert!(rows.iter().any(|r| r.contains("Todo")), "todo panel title");
        assert!(
            rows.iter()
                .any(|r| plain(r).contains(&plain("实现悬浮面板")))
        );
        // 清空后面板消失
        todo_store.replace(Vec::new());
        terminal.clear().unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert!(
            !screen(&terminal).iter().any(|r| r.contains("Todo")),
            "panel hidden when list empty"
        );

        // 运行态：顶栏/状态栏运行指示 + 输入框顶边 steering 提示
        app.agent_running = true;
        app.current_turn = 2;
        app.tool_calls = 3;
        terminal.clear().unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let rows = screen(&terminal);
        assert!(rows.iter().any(|r| r.contains("Turn 2 · 3 tools")));
        assert!(rows.iter().any(|r| {
            r.replace(' ', "")
                .contains(&"Esc 取消 · 输入即 steering".replace(' ', ""))
        }));
        app.agent_running = false;
        app.current_turn = 0;
        app.tool_calls = 0;

        // 斜杠提示条（四段布局）："/" 全量、"/m" 过滤、"/model x" 参数用法
        for input in ["/", "/m", "/model glm-4.7", "/zzz"] {
            app.input = input.to_string();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        }

        // 向导打开（overlay 渲染路径）
        app.input.clear();
        app.wizard = Some(ConfigWizard::new("glm", None));
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        // 端点列表 + 模型列表（含错误态）
        if let Some(w) = app.wizard.as_mut() {
            w.step = WizardStep::Endpoint;
        }
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        if let Some(w) = app.wizard.as_mut() {
            w.step = WizardStep::Model;
            w.fetching = false;
            w.models_error = Some("401".to_string());
        }
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        // Key 输入步骤（输入型对话框）
        if let Some(w) = app.wizard.as_mut() {
            w.step = WizardStep::Key;
        }
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

        // 输入步骤按键路径：字符/退格/粘贴必须进输入框（回归：曾被吞掉）
        use crossterm::event::{KeyCode as K, KeyModifiers as M};
        let ui_tx = tokio::sync::mpsc::unbounded_channel().0;
        app.handle_key(KeyEvent::new(K::Char('s'), M::NONE), &ui_tx)
            .await;
        assert_eq!(app.input(), "s", "after 's'");
        app.handle_key(KeyEvent::new(K::Char('k'), M::NONE), &ui_tx)
            .await;
        assert_eq!(app.input(), "sk", "after 'k'");
        app.handle_key(KeyEvent::new(K::Backspace, M::NONE), &ui_tx)
            .await;
        assert_eq!(app.input(), "s", "after backspace");
        app.handle_key(KeyEvent::new(K::Char('-'), M::NONE), &ui_tx)
            .await;
        assert_eq!(app.input(), "s-", "after '-'");
        // 粘贴同样进输入框（含尾随换行被净化）
        app.handle_paste("live-key\n".to_string());
        assert_eq!(app.input(), "s-live-key");
        // Esc 关闭向导
        app.handle_key(KeyEvent::new(K::Esc, M::NONE), &ui_tx).await;
        assert!(!app.wizard_active());
    }

    #[test]
    fn test_wizard_input_step_routing() {
        // 向导输入步骤只拦截 Enter/Esc；字符、退格、粘贴必须放行到输入框
        assert!(wizard_input_step_intercept(KeyCode::Enter));
        assert!(wizard_input_step_intercept(KeyCode::Esc));
        assert!(!wizard_input_step_intercept(KeyCode::Char('g')));
        assert!(!wizard_input_step_intercept(KeyCode::Backspace));
        assert!(!wizard_input_step_intercept(KeyCode::Tab));
    }

    #[test]
    fn test_slash_hints_filtering() {
        // "/" → 全部命令
        let all = slash_hints("/").unwrap();
        assert_eq!(all.len(), SLASH_COMMANDS.len());
        // 前缀过滤
        let hints = slash_hints("/m").unwrap();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].0, "model");
        let hints = slash_hints("/MO").unwrap(); // 大小写不敏感
        assert_eq!(hints[0].0, "model");
        // 完整命令仍显示（用于用法提示）
        let hints = slash_hints("/model").unwrap();
        assert_eq!(hints.len(), 1);
        // 参数区：只保留精确命令的用法
        let hints = slash_hints("/model glm-4.7").unwrap();
        assert_eq!(hints.len(), 1);
        assert!(hints[0].1.contains("切换模型"));
        // 无匹配 / 非斜杠
        assert!(slash_hints("/xyz").unwrap().is_empty());
        assert!(slash_hints("普通消息").is_none());
    }

    #[test]
    fn test_split_slash() {
        assert_eq!(split_slash("/model glm-4.7"), Some(("model", "glm-4.7")));
        assert_eq!(split_slash("/status"), Some(("status", "")));
        assert_eq!(split_slash("/config  "), Some(("config", "")));
        assert_eq!(split_slash("普通消息"), None);
        assert_eq!(split_slash("/"), None);
    }

    #[test]
    fn test_wizard_step_traits() {
        assert!(WizardStep::Vendor.is_list());
        assert!(!WizardStep::Vendor.is_input());
        assert!(WizardStep::Key.is_input());
        assert!(WizardStep::Model.is_list());
        assert!(WizardStep::ModelManual.is_input());

        let mut wizard = ConfigWizard::new("glm", None);
        assert_eq!(wizard.step, WizardStep::Vendor);
        wizard.move_down(3);
        wizard.move_down(3); // 到底不再前进
        assert_eq!(wizard.selected, 2);
        wizard.move_up();
        assert_eq!(wizard.selected, 1);
    }

    #[test]
    fn test_picker_project_filter_toggle_and_tags() {
        let mk = |id: &str, project: Option<&str>| SessionMeta {
            id: id.to_string(),
            parent_id: None,
            created_at: format!("2026-09-17T00:00:0{id}:00Z"),
            title: Some(format!("t-{id}")),
            project: project.map(str::to_string),
        };
        // 三个项目:alpha(当前)、beta、旧会话(无项目)
        let metas = vec![
            mk("cur", Some("alpha-1111")),
            mk("other", Some("beta-2222")),
            mk("legacy", None),
        ];

        // 当前会话有项目 → 初始只显示本项目
        let picker = SessionPicker::from_metas(metas.clone(), "cur");
        let ids: Vec<&str> = picker.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["cur"], "project view shows only current project");
        assert!(picker.filtering_by_project());
        assert_eq!(picker.selected, 0);

        // a 切换到全部:跨项目附 ⌂ 标注
        let mut picker = SessionPicker::from_metas(metas.clone(), "cur");
        picker.toggle_project_filter();
        let ids: Vec<&str> = picker.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "all view");
        assert!(!picker.filtering_by_project());
        let rows = picker.display_rows("cur");
        assert!(rows.iter().any(|r| r.contains("⌂beta-2222")), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("⌂?")),
            "legacy tagged: {rows:?}"
        );
        assert!(picker.selected < picker.items.len());

        // 当前会话为旧会话(无项目) → 初始显示全部
        let picker = SessionPicker::from_metas(metas, "legacy");
        assert!(!picker.filtering_by_project());
        assert_eq!(picker.items.len(), 3);
    }

    #[test]
    fn test_picker_navigation_and_rows() {
        let mk = |id: &str, parent: Option<&str>| SessionMeta {
            id: id.to_string(),
            parent_id: parent.map(String::from),
            created_at: format!("2026-09-17T00:0{id}:00Z"),
            title: Some(format!("title-{id}")),
            project: None,
        };
        let mut picker =
            SessionPicker::from_metas(vec![mk("3", None), mk("2", Some("1")), mk("1", None)], "2");
        // 树形顺序：根最新在前，分叉紧跟其父
        let ids: Vec<&str> = picker.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["3", "1", "2"]);
        assert_eq!(picker.selected, 2, "current session preselected");
        let rows = picker.display_rows("2");
        assert!(rows[2].starts_with("▸")); // 当前会话标记
        assert!(rows[2].contains("└ 2 · title-2")); // 分叉缩进
        picker.selected = 0;

        picker.move_down();
        assert_eq!(picker.selected, 1);
        picker.move_down();
        picker.move_down(); // 到底不再前进
        assert_eq!(picker.selected, 2);
        picker.move_up();
        assert_eq!(picker.selected, 1);
        assert_eq!(picker.selected_meta().unwrap().id, "1");
    }
}
