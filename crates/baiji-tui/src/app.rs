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

/// 斜杠命令注册表：(名称, 用法说明)。字典序——ghost 补全取首个前缀匹配
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("btw", "旁路提问：一次性问答，不写入当前会话历史（规划中）"),
    ("compact", "手动压缩当前会话上下文（旧结果 stub 化 + 摘要）"),
    (
        "config",
        "打开配置向导：选厂商 → 端点 → API Key → 模型（热生效）",
    ),
    (
        "fork",
        "分叉会话：/fork 继承全部历史；/fork <n> 回到 n 轮之前重来（原会话保留）",
    ),
    ("help", "显示命令帮助"),
    ("kill", "停止后台任务：/kill <id>（列表见 /tasks）"),
    ("model", "切换模型：/model <名称>，或不带参数打开模型选择器"),
    ("new", "开新会话（当前会话完整保留在磁盘）"),
    (
        "plan",
        "计划模式：/plan 切换只读规划态；/plan <目标> 开始规划；计划给出后 Enter 批准执行",
    ),
    ("quit", "退出 baiji"),
    ("resume", "恢复其它会话（打开会话选择器）"),
    ("session", "显示当前会话信息与统计"),
    ("status", "查看当前 vendor / endpoint / model / session"),
    (
        "subagents",
        "管理子代理角色：查看/热重载（agents/*.md 定义的角色）",
    ),
    ("tasks", "查看后台任务列表"),
    (
        "thinking",
        "设置思考级别：/thinking <minimal|low|medium|high|off>，热生效",
    ),
    ("todos", "显示当前任务清单"),
    ("usage", "显示用量统计：上下文 / 压缩节省 / 工具调用"),
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
    let (cmd, a) = match rest.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (rest, ""),
    };
    Some((cmd, a))
}

/// 输入框灰色补全（fish-style ghost）：命令输入态（`/` 开头、未进参数区）时
/// 取首个前缀匹配。Tab 键接受补全
pub fn ghost_completion(input: &str) -> Option<(&'static str, &'static str)> {
    let rest = input.strip_prefix('/')?;
    if rest.is_empty() || rest.contains(' ') {
        return None;
    }
    slash_hints(input)?.first().copied()
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

/// 子代理角色面板状态（数据每帧从 harness 快照，热重载后即时刷新）
pub struct SubagentsPanel {
    pub selected: usize,
}

impl SubagentsPanel {
    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn move_down(&mut self, len: usize) {
        if self.selected + 1 < len {
            self.selected += 1;
        }
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
    /// 会话选择器打开时为 Some
    picker: Option<SessionPicker>,
    /// 子代理角色面板打开时为 Some
    subagents_panel: Option<SubagentsPanel>,
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
    /// 后台任务注册表（/tasks、/kill；headless 装配为 None）
    jobs: Option<Arc<baiji_tools::tools::jobs::JobRegistry>>,
    /// 本次会话累计工具调用次数（/usage；跨 run 累计）
    tools_total: u64,
    /// 计划模式展示镜像（真值在 runtime；每帧渲染免锁）
    plan_mode: bool,
    /// 计划已给出、等待用户 Enter 批准执行（Enter=退出计划模式并执行）
    awaiting_plan: bool,
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
        jobs: Option<Arc<baiji_tools::tools::jobs::JobRegistry>>,
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
        // 初始门控状态跟随 runtime（headless `--plan` 同样进 TUI 时镜像为真）
        let plan_mode = harness.try_lock().is_ok_and(|h| h.plan_mode());
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
            // 空聊天区由欢迎屏兜底（快捷键提示在那里展示）
            lines: Vec::new(),
            input: String::new(),
            scroll: usize::MAX,
            agent_running: false,
            current_turn: 0,
            tool_calls: 0,
            tools_total: 0,
            streaming: String::new(),
            thinking: String::new(),
            session_id,
            cancel: CancellationToken::new(),
            steering: Arc::new(SteeringQueue::new()),
            bytes_saved: 0,
            tokens_saved: 0,
            picker: None,
            subagents_panel: None,
            pending: None,
            confirm_rx,
            project,
            workdir,
            git_branch,
            frame: 0,
            jobs,
            plan_mode,
            awaiting_plan: false,
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

        // 子代理角色面板模式
        if self.subagents_panel.is_some() {
            return self.handle_subagents_key(key).await;
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
                } else if self.awaiting_plan {
                    self.awaiting_plan = false;
                    self.lines.push(ChatLine::System(
                        "已取消执行确认（仍处于计划模式）".to_string(),
                    ));
                } else {
                    return true;
                }
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    // 计划等待态：空回车 = 批准执行（退出计划模式并发起执行 run）
                    if self.awaiting_plan {
                        self.approve_plan(ui_tx.clone()).await;
                        self.scroll_to_bottom();
                    }
                    return false;
                }
                self.awaiting_plan = false; // 任何输入都视为继续对话，批准作废
                // 斜杠命令（/model /config /status /help …）。
                // 用户 prompt 模板（/name 参数）不在此处理：当作普通消息交给 Harness 展开
                let is_template = split_slash(&text)
                    .is_some_and(|(cmd, _)| self.templates.iter().any(|(name, _)| name == cmd));
                if let Some((cmd, args)) = split_slash(&text).filter(|_| !is_template) {
                    self.input.clear();
                    let quit = self.handle_slash(cmd, args, ui_tx).await;
                    self.scroll_to_bottom();
                    return quit; // /quit 请求退出
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
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_picker().await;
            }
            KeyCode::Tab => {
                // Tab 接受输入框里的灰色补全（ghost）——补全为完整命令 + 尾随空格
                if let Some((name, _)) = self.slash_ghost() {
                    self.input = format!("/{name} ");
                }
            }
            KeyCode::Up => {
                if self.scroll == usize::MAX {
                    self.scroll = 0;
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if self.scroll != usize::MAX {
                    self.scroll += 1;
                }
            }
            KeyCode::Char(c) => {
                self.input.push(c);
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
            thinking: self.settings.thinking,
            external_agents: self.settings.external_agents.clone(),
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
            // 向导不动思考级别与外部 agent：沿用当前值（save 会一并落盘）
            thinking: self.settings.thinking,
            external_agents: self.settings.external_agents.clone(),
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

    /// 斜杠命令分发。返回 true = 请求退出（/quit）
    async fn handle_slash(
        &mut self,
        cmd: &str,
        args: &str,
        ui_tx: &UnboundedSender<UiEvent>,
    ) -> bool {
        match cmd {
            "quit" => return true,
            "help" => {
                let list: Vec<String> = SLASH_COMMANDS
                    .iter()
                    .map(|(name, _)| format!("/{name}"))
                    .collect();
                self.lines.push(ChatLine::System(format!(
                    "命令（输入 / 后输入框灰色提示补全，Tab 接受）：{}",
                    list.join(" · ")
                )));
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
                let external = if s.external_agents.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n外部 agent: {}",
                        s.external_agents
                            .iter()
                            .map(|a| format!("/{}", a.name))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                };
                self.lines.push(ChatLine::System(format!(
                    "vendor: {} · endpoint: {} · model: {} · thinking: {} · session: {}\n计划模式: {} · {}{external}",
                    s.vendor,
                    s.endpoint.as_deref().unwrap_or("api"),
                    s.model.as_deref().unwrap_or("自动发现"),
                    s.thinking.map(|l| l.effort()).unwrap_or("off"),
                    self.session_id,
                    if self.plan_mode { "开启（只读）" } else { "关闭" },
                    self.settings_summary
                )));
            }
            "config" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        "运行中不可修改配置，请先等待或 Esc 取消".to_string(),
                    ));
                    return false;
                }
                let s = &self.settings;
                self.wizard = Some(ConfigWizard::new(&s.vendor, s.endpoint.as_deref()));
            }
            "model" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        "运行中不可修改配置，请先等待或 Esc 取消".to_string(),
                    ));
                    return false;
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
            "resume" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        "运行中无法切换会话，先按 Esc 取消".to_string(),
                    ));
                } else {
                    self.open_picker().await;
                }
            }
            "new" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        "运行中无法开新会话，先按 Esc 取消".to_string(),
                    ));
                } else {
                    let result = self.harness.lock().await.start_new_session();
                    match result {
                        Ok(id) => {
                            self.session_id = id.clone();
                            self.rebuild_lines(&[]);
                            // 会话级台账归零
                            self.context_tokens = 0;
                            self.bytes_saved = 0;
                            self.tokens_saved = 0;
                            self.tools_total = 0;
                            self.lines.push(ChatLine::System(format!(
                                "已开新会话 {id}（原会话保留，Ctrl+O 可切回）"
                            )));
                        }
                        Err(e) => {
                            self.lines
                                .push(ChatLine::System(format!("✗ 开新会话失败: {e}")));
                        }
                    }
                }
            }
            "session" => {
                let info = self.harness.try_lock().ok().map(|h| {
                    let meta = &h.session().meta;
                    (
                        meta.id.clone(),
                        meta.parent_id.clone(),
                        meta.created_at.clone(),
                        meta.title.clone(),
                        meta.project.clone(),
                        h.session().messages.len(),
                        h.todos_snapshot().len(),
                    )
                });
                match info {
                    Some((id, parent, created, title, project, msgs, todos)) => {
                        self.lines.push(ChatLine::System(format!(
                            "session {id}\n创建: {created}\n标题: {}\n项目: {}\n分叉自: {}\n消息: {msgs} 条 · todo: {todos} 条",
                            title.as_deref().unwrap_or("（无）"),
                            project.as_deref().unwrap_or("（无）"),
                            parent.as_deref().unwrap_or("（根会话）"),
                        )));
                    }
                    None => self
                        .lines
                        .push(ChatLine::System("会话忙（运行中），稍后再试".to_string())),
                }
            }
            "compact" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        "运行中无法压缩，先按 Esc 取消".to_string(),
                    ));
                } else {
                    let mut harness = self.harness.lock().await;
                    let (stubbed, summary) = harness.compact_now().await;
                    let messages = harness.session().messages.clone();
                    drop(harness);
                    self.rebuild_lines(&messages);
                    match summary {
                        Some(s) => self.lines.push(ChatLine::System(format!(
                            "压缩完成：摘要 {} 字符 · stub 化 {stubbed} 条旧工具结果（可用 expand 取回）",
                            s.chars().count()
                        ))),
                        None if stubbed > 0 => self.lines.push(ChatLine::System(format!(
                            "压缩完成：stub 化 {stubbed} 条旧工具结果（无需摘要）"
                        ))),
                        None => self.lines.push(ChatLine::System(
                            "历史太短，没有可压缩的内容".to_string(),
                        )),
                    }
                }
            }
            "todos" => {
                let items = self.todo_items();
                if items.is_empty() {
                    self.lines.push(ChatLine::System(
                        "当前没有任务清单（模型可用 todo 工具创建）".to_string(),
                    ));
                } else {
                    let rows: Vec<String> = items
                        .iter()
                        .map(|t| format!("{} {}", t.status.marker(), t.content))
                        .collect();
                    self.lines
                        .push(ChatLine::System(format!("任务清单：\n{}", rows.join("\n"))));
                }
            }
            "usage" => {
                let messages = self
                    .harness
                    .try_lock()
                    .map(|h| h.session().messages.len())
                    .unwrap_or(0);
                let saved = if self.bytes_saved > 0 {
                    format!(
                        "{}（约 {} tok）",
                        format_bytes(self.bytes_saved),
                        self.tokens_saved
                    )
                } else {
                    "0B".to_string()
                };
                self.lines.push(ChatLine::System(format!(
                    "上下文: {} tokens · 工具调用: {} 次\n压缩节省: {saved} · 自动接力: {}/{}\n历史消息: {messages} 条 · 模型: {}",
                    if self.context_tokens > 0 {
                        self.context_tokens.to_string()
                    } else {
                        "尚无数据".to_string()
                    },
                    self.tools_total,
                    self.auto_turns,
                    self.auto.max_turns,
                    self.settings.model.as_deref().unwrap_or("自动发现"),
                )));
            }
            "tasks" => match self.jobs.as_ref() {
                None => self.lines.push(ChatLine::System(
                    "后台任务未启用（未接入 JobRegistry）".to_string(),
                )),
                Some(registry) => {
                    let snapshot = registry.snapshot();
                    if snapshot.is_empty() {
                        self.lines
                            .push(ChatLine::System("没有后台任务".to_string()));
                    } else {
                        let rows: Vec<String> = snapshot
                            .iter()
                            .map(|(id, command, log, state)| {
                                format!(" #{id} [{}] {command}（{log}）", state.label())
                            })
                            .collect();
                        self.lines
                            .push(ChatLine::System(format!("后台任务：\n{}", rows.join("\n"))));
                    }
                }
            },
            "kill" => {
                let Some(registry) = self.jobs.as_ref() else {
                    self.lines.push(ChatLine::System(
                        "后台任务未启用（未接入 JobRegistry）".to_string(),
                    ));
                    return false;
                };
                match args.trim().parse::<u32>() {
                    Ok(id) => {
                        if registry.stop(id) {
                            self.lines
                                .push(ChatLine::System(format!("已停止后台任务 #{id}")));
                        } else {
                            self.lines.push(ChatLine::System(format!(
                                "未找到运行中的任务 #{id}（/tasks 查看列表）"
                            )));
                        }
                    }
                    Err(_) => self.lines.push(ChatLine::System(
                        "用法：/kill <id>（id 见 /tasks）".to_string(),
                    )),
                }
            }
            // 思考级别：/thinking <minimal|low|medium|high|off>，热生效（下一次请求）
            // 并落盘；不带参数显示当前级别
            "thinking" => {
                let arg = args.trim().to_ascii_lowercase();
                if arg.is_empty() {
                    let current = self
                        .settings
                        .thinking
                        .map(|l| l.effort().to_string())
                        .unwrap_or_else(|| "off".to_string());
                    self.lines.push(ChatLine::System(format!(
                        "当前思考级别: {current}\n用法：/thinking <minimal|low|medium|high>，或 /thinking off 关闭\nAnthropic 端点映射 thinking.budget_tokens，OpenAI 系映射 reasoning_effort / reasoning.effort"
                    )));
                } else {
                    let level = if arg == "off" || arg == "none" {
                        None
                    } else {
                        match baiji_ai::ThinkingLevel::parse(&arg) {
                            Some(level) => Some(level),
                            None => {
                                self.lines.push(ChatLine::System(
                                    "用法：/thinking <minimal|low|medium|high|off>".to_string(),
                                ));
                                return false;
                            }
                        }
                    };
                    self.settings.thinking = level;
                    if let Err(e) =
                        settings::save(&self.config_path, &self.settings, settings::KeyUpdate::Keep)
                    {
                        self.lines
                            .push(ChatLine::System(format!("✗ 配置保存失败: {e}")));
                    } else {
                        let shown = level
                            .map(|l| l.effort().to_string())
                            .unwrap_or_else(|| "off".to_string());
                        self.harness.lock().await.set_thinking(level);
                        self.lines.push(ChatLine::System(format!(
                            "思考级别已设为 {shown}（下一次请求生效，已写入配置）"
                        )));
                    }
                }
            }
            // 计划模式：/plan 开关（热切换，运行中亦可）；/plan <目标> = 开启并开始规划。
            // 开关真值在 runtime（工具门控 + 系统提示段随之生效），此处镜像仅驱动渲染
            "plan" => {
                let arg = args.trim();
                match arg.to_ascii_lowercase().as_str() {
                    "" => self.set_plan_mode(!self.plan_mode).await,
                    "on" => self.set_plan_mode(true).await,
                    "off" => self.set_plan_mode(false).await,
                    goal => {
                        if !self.plan_mode {
                            self.set_plan_mode(true).await;
                        }
                        if self.agent_running {
                            // 运行中：目标作为 steering 注入当前 run
                            // （写工具立即被计划模式门控拒绝，本轮即可转入规划）
                            self.steering.push(goal);
                            self.lines
                                .push(ChatLine::System(format!("（steering·规划）{goal}")));
                        } else {
                            self.lines.push(ChatLine::user(goal));
                            self.auto_turns = 0;
                            self.spawn_run(goal.to_string(), ui_tx.clone());
                        }
                    }
                }
            }
            // 子代理角色管理面板：查看角色（agent 文件定义）+ r 热重载
            "subagents" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        "运行中无法打开子代理面板，先按 Esc 取消".to_string(),
                    ));
                } else {
                    self.subagents_panel = Some(SubagentsPanel { selected: 0 });
                }
            }
            "btw" => self.lines.push(ChatLine::System(
                "旁路提问尚未实现（规划中：不写入当前会话历史的一次性问答）".to_string(),
            )),
            other => {
                // 外部 coding agent 动态命令：/codex <任务> /claude <任务> …
                if let Some(spec) = self
                    .settings
                    .external_agents
                    .iter()
                    .find(|a| a.name == other)
                    .cloned()
                {
                    let prompt = args.trim().to_string();
                    if prompt.is_empty() {
                        self.lines.push(ChatLine::System(format!(
                            "用法：/{} <任务描述>（委托给外部 {} agent CLI）",
                            spec.name, spec.name
                        )));
                    } else {
                        self.lines
                            .push(ChatLine::user(format!("/{} {prompt}", spec.name)));
                        self.spawn_external_agent(spec, prompt, ui_tx.clone());
                    }
                    return false;
                }
                let mut list: Vec<String> = SLASH_COMMANDS
                    .iter()
                    .map(|(name, _)| format!("/{name}"))
                    .collect();
                for agent in &self.settings.external_agents {
                    list.push(format!("/{}", agent.name));
                }
                self.lines.push(ChatLine::System(format!(
                    "未知命令 /{other}（可用: {} · Tab 可补全）",
                    list.join(" ")
                )));
            }
        }
        false
    }

    // ---- 计划模式 ----

    /// 切换计划模式：runtime 门控（工具白名单 + 系统提示段）+ 本地镜像 + 提示行
    async fn set_plan_mode(&mut self, on: bool) {
        self.plan_mode = on;
        self.awaiting_plan = false;
        self.harness.lock().await.set_plan_mode(on);
        if on {
            self.lines.push(ChatLine::System(
                "⏸ 计划模式已开启（只读）— 写/编辑/bash 被禁用；/plan off 退出".to_string(),
            ));
        } else {
            self.lines.push(ChatLine::System(
                "▶ 计划模式已关闭，恢复完整工具".to_string(),
            ));
        }
    }

    /// Enter 批准执行计划：退出计划模式并以固定指令发起执行 run
    async fn approve_plan(&mut self, ui_tx: UnboundedSender<UiEvent>) {
        self.awaiting_plan = false;
        self.set_plan_mode(false).await;
        self.lines.push(ChatLine::System(format!(
            "⏩ 执行计划：{plan}",
            plan = baiji_harness::PLAN_EXECUTE_PROMPT
        )));
        self.auto_turns = 0;
        self.spawn_run(baiji_harness::PLAN_EXECUTE_PROMPT.to_string(), ui_tx);
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

    /// 子代理面板按键：↑↓ 选择 · r 热重载 · Esc/Enter/q 关闭
    async fn handle_subagents_key(&mut self, key: KeyEvent) -> bool {
        let roles_len = self.subagent_roles_len();
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.subagents_panel = None;
            }
            KeyCode::Char('q') => {
                self.subagents_panel = None;
            }
            KeyCode::Char('r') => {
                // 热重载：编辑 agents/*.md 后免重启生效（task 分发与系统提示段共用注册表）
                match self.harness.try_lock() {
                    Ok(harness) => {
                        let count = harness.subagents_reload();
                        self.lines.push(ChatLine::System(format!(
                            "已重载子代理角色：{count} 个（下一次 task 调用生效）"
                        )));
                    }
                    Err(_) => self.lines.push(ChatLine::System(
                        "harness 忙，稍后再试（运行中不可重载）".to_string(),
                    )),
                }
            }
            KeyCode::Up => {
                if let Some(panel) = &mut self.subagents_panel {
                    panel.move_up();
                }
            }
            KeyCode::Down => {
                if let Some(panel) = &mut self.subagents_panel {
                    panel.move_down(roles_len);
                }
            }
            _ => {}
        }
        false
    }

    fn subagent_roles_len(&self) -> usize {
        self.harness
            .try_lock()
            .map(|h| h.subagent_roles().len())
            .unwrap_or(0)
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
                self.tools_total += 1; // /usage 台账（跨 run 累计）
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
                // 计划模式跑完一轮：非空回答视为待批准的计划
                if self.plan_mode && !answer.is_empty() {
                    self.awaiting_plan = true;
                    self.lines.push(ChatLine::System(
                        "⏸ 计划已给出 — 空回车退出计划模式并开始执行 · 或直接输入继续修改计划 · /plan off 仅退出".to_string(),
                    ));
                }
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
        // 计划模式下不接力：规划结果等待用户批准，而不是自动开跑
        if !self.auto.enabled || self.plan_mode || self.agent_running || self.run_interrupted {
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

    /// 外部 coding agent 运行（后台任务）：复用 Agent 事件流渲染进度——
    /// ToolStarted/ToolFinished 画 ●/⎿ 行，RunCompleted 复位运行态；
    /// Esc 经共享 cancel 令牌击杀进程组。输出不进会话历史（本地展示）
    fn spawn_external_agent(
        &mut self,
        spec: baiji_tools::tools::external_agent::ExternalAgentSpec,
        prompt: String,
        ui_tx: UnboundedSender<UiEvent>,
    ) {
        self.agent_running = true;
        self.run_interrupted = false;
        let cancel = CancellationToken::new();
        self.cancel = cancel.clone();
        let workdir = self.workdir.clone();

        tokio::spawn(async move {
            let _ = ui_tx.send(UiEvent::Agent(AgentEvent::ToolStarted {
                id: spec.name.clone(),
                name: spec.name.clone(),
                args: serde_json::json!({"prompt": prompt}),
            }));
            let (output, is_error) = tokio::select! {
                _ = cancel.cancelled() => (
                    "[已取消] 外部 agent 被用户中断".to_string(),
                    true,
                ),
                result = baiji_tools::tools::external_agent::run_external(
                    &spec, &prompt, &workdir,
                ) => match result {
                    Ok(text) => {
                        let is_error = text.starts_with("[external agent exited with")
                            || text.starts_with("[Timeout]");
                        (text, is_error)
                    }
                    Err(text) => (text, true),
                },
            };
            let _ = ui_tx.send(UiEvent::Agent(AgentEvent::ToolFinished {
                id: spec.name.clone(),
                name: spec.name.clone(),
                output,
                is_error,
                duration_ms: 0,
                original_bytes: None,
                original_tokens: None,
            }));
            // 复位运行态（空答案不产生额外聊天行）
            let _ = ui_tx.send(UiEvent::Agent(AgentEvent::RunCompleted {
                answer: String::new(),
            }));
        });
    }

    /// 启动一次 agent 运行（后台任务），事件转发回 UI 通道
    fn spawn_run(&mut self, text: String, ui_tx: UnboundedSender<UiEvent>) {
        self.agent_running = true;
        self.run_interrupted = false;
        self.awaiting_plan = false; // 新一轮开始，旧的批准待办作废

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
        if self.plan_mode {
            parts.push(if self.awaiting_plan {
                "计划·待执行".to_string()
            } else {
                "计划·只读".to_string()
            });
        }
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

    /// 计划模式（展示镜像；门控真相在 AgentRuntime）
    pub(crate) fn plan_mode(&self) -> bool {
        self.plan_mode
    }

    /// 计划已给出、等待空回车批准执行
    pub(crate) fn awaiting_plan(&self) -> bool {
        self.awaiting_plan
    }

    pub(crate) fn streaming(&self) -> &str {
        &self.streaming
    }

    /// 进行中的思考全文：与回答正文同一套贴底滚动渲染。
    /// （旧实现只展示 400 字符尾部——前沿不断消失的"收缩"观感即源于此）
    pub(crate) fn thinking(&self) -> &str {
        &self.thinking
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

    /// 子代理面板展示行（手风琴式：选中角色附描述与提示词预览）
    pub(crate) fn subagents_rows(&self) -> Option<Vec<String>> {
        let panel = self.subagents_panel.as_ref()?;
        let roles = self
            .harness
            .try_lock()
            .map(|h| h.subagent_roles())
            .unwrap_or_default();
        let mut rows: Vec<String> = Vec::new();
        for (i, role) in roles.iter().enumerate() {
            let mark = if i == panel.selected { "▸ " } else { "  " };
            rows.push(format!(
                "{mark}{:<16} model:{:<10} think:{:<7} turns:{:<4} tools:{}",
                role.name,
                role.model.as_deref().unwrap_or("继承"),
                role.thinking.map(|t| t.effort()).unwrap_or("-"),
                role.max_turns
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "默认".into()),
                role.tools
                    .as_ref()
                    .map(|t| t.len().to_string())
                    .unwrap_or_else(|| "全部".into()),
            ));
            if i == panel.selected {
                if !role.description.is_empty() {
                    rows.push(format!("    {}", role.description));
                }
                if !role.system_prompt.is_empty() {
                    let preview: String = role.system_prompt.chars().take(160).collect();
                    rows.push(format!("    提示词: {}…", preview.trim_end()));
                }
            }
        }
        if roles.is_empty() {
            rows.push("（尚无角色）在下列目录创建 <name>.md：frontmatter 声明".into());
            rows.push(
                "name/description/tools/model/thinking/max_turns，正文为该角色的系统提示".into(),
            );
        }
        Some(rows)
    }

    /// 子代理角色目录提示行（面板脚注）
    pub(crate) fn subagents_dirs_hint(&self) -> Option<String> {
        self.subagents_panel.as_ref()?;
        let dirs = self
            .harness
            .try_lock()
            .map(|h| h.subagent_dirs())
            .unwrap_or_default();
        let joined = if dirs.is_empty() {
            "（未启用）".to_string()
        } else {
            dirs.iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(" · ")
        };
        Some(format!("角色目录: {joined}（编辑后按 r 热重载）"))
    }

    /// 子代理面板选中项（渲染高亮用）
    pub(crate) fn subagents_selected(&self) -> Option<usize> {
        self.subagents_panel.as_ref().map(|p| p.selected)
    }

    /// 粘贴并入输入框（换行折叠为空格，保持单行输入语义）
    pub(crate) fn handle_paste(&mut self, text: String) {
        self.input.push_str(&sanitize_paste(&text));
    }

    pub(crate) fn wizard_active(&self) -> bool {
        self.wizard.is_some()
    }

    /// 全部命令（静态注册表 + 外部 agent 动态命令）：裸 `/` 的清单展示用
    pub(crate) fn command_names(&self) -> Vec<String> {
        let mut names: Vec<String> = SLASH_COMMANDS.iter().map(|(n, _)| n.to_string()).collect();
        names.extend(self.settings.external_agents.iter().map(|a| a.name.clone()));
        names
    }

    /// 输入框灰色补全（供渲染与 Tab 接受）：(命令名, 用法)。
    /// 裸 `/` 也给首个命令（Tab 锚点 + 清单展示后的补全目标）；
    /// 动态命令（外部 coding agent）优先匹配，静态注册表兜底；
    /// 参数区（含空格）与非斜杠输入不出补全；向导打开时抑制
    pub(crate) fn slash_ghost(&self) -> Option<(String, String)> {
        if self.wizard.is_some() {
            return None;
        }
        let input = self.input.as_str();
        let rest = input.strip_prefix('/')?;
        if rest.contains(' ') {
            return None;
        }
        if rest.is_empty() {
            // 裸 "/"：首个命令作为 Tab 目标（完整清单由输入框 ghost 灰字展示）
            let (name, usage) = SLASH_COMMANDS.first()?;
            return Some((name.to_string(), usage.to_string()));
        }
        let lower = rest.to_ascii_lowercase();
        if let Some(agent) = self
            .settings
            .external_agents
            .iter()
            .find(|a| a.name.to_lowercase().starts_with(&lower))
        {
            return Some((
                agent.name.clone(),
                format!(
                    "委托任务给外部 {} agent：/{} <任务描述>",
                    agent.name, agent.name
                ),
            ));
        }
        ghost_completion(input).map(|(n, u)| (n.to_string(), u.to_string()))
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
                thinking: None,
                // 假外部 agent：验证动态命令 ghost 与分发（printf 立即返回）
                external_agents: vec![baiji_tools::tools::external_agent::ExternalAgentSpec {
                    name: "fakeagent".to_string(),
                    command: "printf 'ans:%s' {prompt}".to_string(),
                    description: String::new(),
                    timeout_secs: 10,
                }],
            },
            "max_turns: 24 · compaction: on (auto)".to_string(),
            AutoContinueConfig::default(),
            None,
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
        // 思考流同样贴底滚动（回归：旧 400 字符尾部窗口呈"前沿收缩"而非滚动）
        // 保留一行历史：空会话画的是欢迎屏而非聊天区
        app.lines = vec![ChatLine::user("go")];
        app.thinking = format!("{}思考尾部标记", "推理".repeat(400));
        app.scroll_to_bottom();
        terminal.clear().unwrap(); // TestBackend 宽字符差分残留：断言前全量重绘
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert!(
            screen(&terminal)
                .iter()
                .any(|r| r.replace(' ', "").contains("思考尾部标记")),
            "long thinking must follow the bottom like the answer stream"
        );
        app.thinking.clear();
        // 恢复后续断言用的代码块消息
        app.lines = vec![
            ChatLine::Assistant("fn main() {\n    hi();\n}".to_string()),
            ChatLine::Assistant(format!("{}终点标记", "中文".repeat(400))),
        ];
        app.scroll = 0;
        terminal.clear().unwrap();
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

        // /thinking <level>：热切换 + 落盘（save 读文件，先写入基线配置）
        let ui_tx = tokio::sync::mpsc::unbounded_channel().0;
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"vendor":"glm","api_key":"k"}"#,
        )
        .unwrap();
        app.handle_slash("thinking", "high", &ui_tx).await;
        assert_eq!(app.settings.thinking, Some(baiji_ai::ThinkingLevel::High));
        assert!(
            std::fs::read_to_string(dir.path().join("config.json"))
                .unwrap()
                .contains("\"thinking\": \"high\""),
            "persisted to the config file"
        );
        assert_eq!(
            app.harness.try_lock().unwrap().thinking_level(),
            Some(baiji_ai::ThinkingLevel::High),
            "runtime hot-swapped"
        );
        // off 关闭
        app.handle_slash("thinking", "off", &ui_tx).await;
        assert_eq!(app.settings.thinking, None);
        // 非法值提示用法
        app.handle_slash("thinking", "bogus", &ui_tx).await;
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, ChatLine::System(s) if s.contains("用法：/thinking")))
        );

        // /plan 计划模式：镜像切换 + runtime 门控生效 + 状态栏与输入框指示
        app.handle_slash("plan", "", &ui_tx).await;
        assert!(app.plan_mode(), "slash toggles the display mirror");
        assert!(
            app.harness.try_lock().unwrap().plan_mode(),
            "runtime gate hot-swapped"
        );
        assert!(app.status_right().contains("计划·只读"));
        terminal.clear().unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert!(
            screen(&terminal)
                .iter()
                .any(|r| r.replace(' ', "").contains("计划模式（只读）")),
            "input box top border shows the plan-mode hint"
        );
        app.handle_slash("plan", "off", &ui_tx).await;
        assert!(!app.plan_mode());
        assert!(!app.harness.try_lock().unwrap().plan_mode());

        // 批准流：开启 → 模拟计划回答完成 → awaiting → 空回车批准执行
        app.handle_slash("plan", "on", &ui_tx).await;
        app.handle_agent_event(
            AgentEvent::RunCompleted {
                answer: "计划：三步实施".to_string(),
            },
            &ui_tx,
        );
        assert!(app.awaiting_plan(), "plan answer enters approval state");
        assert!(app.status_right().contains("计划·待执行"));
        app.input.clear();
        app.handle_key(KeyEvent::new(K::Enter, M::NONE), &ui_tx)
            .await;
        assert!(!app.plan_mode(), "approval exits plan mode");
        assert!(!app.awaiting_plan());
        assert!(app.agent_running, "approval spawns the execute run");
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, ChatLine::System(s) if s.contains("执行计划"))),
            "execute prompt visible in history"
        );
        app.agent_running = false; // 后台 run 与后续断言解耦

        // /subagents 面板：打开 → 渲染（角色行 + 目录脚注）→ r 重载 → Esc 关闭
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: finds issues\n---\nYou review code.",
        )
        .unwrap();
        let registry = Arc::new(baiji_agent::SubagentRegistry::new(vec![agents_dir]));
        registry.load();
        app.harness.lock().await.set_subagents(registry);
        app.handle_slash("subagents", "", &ui_tx).await;
        assert!(app.subagents_rows().is_some(), "panel opens");
        terminal.clear().unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let rows = screen(&terminal);
        assert!(rows.iter().any(|r| r.contains("Subagents")));
        assert!(rows.iter().any(|r| r.contains("reviewer")));
        assert!(
            rows.iter().any(|r| r.replace(' ', "").contains("角色目录")),
            "dirs footer shown"
        );
        // r 热重载（面板键路由）
        app.handle_key(KeyEvent::new(K::Char('r'), M::NONE), &ui_tx)
            .await;
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, ChatLine::System(s) if s.contains("已重载子代理角色：1"))),
            "reload reports the count"
        );
        // Esc 关闭
        app.handle_key(KeyEvent::new(K::Esc, M::NONE), &ui_tx).await;
        assert!(app.subagents_rows().is_none(), "panel closes");

        // 外部 coding agent 动态命令：ghost 提示 → Tab 补全 → 分发执行
        app.input = "/fak".to_string();
        let ghost = app.slash_ghost().expect("dynamic agent ghost");
        assert_eq!(ghost.0, "fakeagent");
        assert!(ghost.1.contains("外部"), "{}", ghost.1);
        app.handle_key(KeyEvent::new(K::Tab, M::NONE), &ui_tx).await;
        assert_eq!(
            app.input(),
            "/fakeagent ",
            "Tab completes the dynamic command"
        );
        // 分发：独立通道收事件（ToolStarted / ToolFinished / RunCompleted）
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        app.handle_slash("fakeagent", "hello", &tx2).await;
        assert!(app.agent_running, "external run marks the running state");
        let mut finished = String::new();
        let mut seen = 0;
        while seen < 3 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx2.recv())
                .await
                .expect("event within timeout")
                .expect("channel open");
            if let UiEvent::Agent(AgentEvent::ToolFinished { output, .. }) = ev {
                finished = output;
            }
            seen += 1;
        }
        assert!(finished.contains("ans:hello"), "{finished}");
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, ChatLine::User(s) if s.contains("/fakeagent hello"))),
            "the prompt is echoed into the chat"
        );
        app.agent_running = false; // RunCompleted 由事件循环处理，这里手动复位

        // /todos 与 /quit 命令分发（Enter 路径）
        let ui_tx = tokio::sync::mpsc::unbounded_channel().0;
        todo_store.replace(vec![baiji_harness::TodoItem {
            id: 1,
            content: "分析依赖".to_string(),
            status: baiji_harness::TodoStatus::Done,
            note: None,
        }]);
        app.input = "/todos".to_string();
        app.handle_key(KeyEvent::new(K::Enter, M::NONE), &ui_tx)
            .await;
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, ChatLine::System(s) if s.contains("[x] 分析依赖"))),
            "/todos prints the list"
        );
        app.input = "/quit".to_string();
        assert!(
            app.handle_key(KeyEvent::new(K::Enter, M::NONE), &ui_tx)
                .await,
            "/quit requests exit"
        );

        // 输入框灰色补全（ghost）：补全紧跟已输入文本（颜色边界即光标，
        // 不再有 ▏ 分隔），状态栏右侧临时显示用法；Tab 接受补全
        app.input = "/mo".to_string();
        terminal.clear().unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let rows = screen(&terminal);
        let input_row = rows
            .iter()
            .find(|r| r.contains("> /mo"))
            .expect("input row");
        assert!(
            input_row.contains("/model "),
            "ghost continues the typed text with no separator: {input_row}"
        );
        assert!(
            !input_row.contains('▏'),
            "no cursor artifact while ghosting"
        );
        assert!(
            rows.iter()
                .any(|r| r.replace(' ', "").contains(&"切换模型".to_string())),
            "status bar shows the ghost command's usage"
        );
        let ui_tx = tokio::sync::mpsc::unbounded_channel().0;
        app.handle_key(KeyEvent::new(K::Tab, M::NONE), &ui_tx).await;
        assert_eq!(app.input(), "/model ", "Tab accepts the ghost completion");
        // 参数区不再出补全；未知前缀也没有
        for input in ["/model glm-4.7", "/zzz", "普通消息"] {
            app.input = input.to_string();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        }

        // 裸 "/"：展示全部命令清单（灰字，宽度内尽量多列）+ Tab 补全首个
        app.input = "/".to_string();
        terminal.clear().unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let rows = screen(&terminal);
        let bare_row = rows
            .iter()
            .find(|r| r.contains("> /"))
            .expect("bare slash row");
        assert!(
            bare_row.contains("btw") && bare_row.contains("compact"),
            "bare slash lists the commands: {bare_row}"
        );
        assert!(
            bare_row.contains('…'),
            "list is width-capped with an ellipsis"
        );
        assert!(!bare_row.contains('▏'));
        app.handle_key(KeyEvent::new(K::Tab, M::NONE), &ui_tx).await;
        assert_eq!(
            app.input(),
            "/btw ",
            "Tab from bare slash completes the first command"
        );
        app.input.clear();

        // 输入单字符渲染恰好一次（回归：输入框曾把已输入文本重复渲染两遍）
        app.input = "s".to_string();
        terminal.clear().unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let rows = screen(&terminal);
        let row = rows.iter().find(|r| r.contains("> s")).expect("input row");
        assert_eq!(
            row.matches('s').count(),
            1,
            "typed char rendered exactly once: {row}"
        );
        app.input.clear();

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
    fn test_ghost_completion() {
        // 裸 "/"、非斜杠输入、参数区、无匹配：都不出补全
        assert!(ghost_completion("/").is_none());
        assert!(ghost_completion("普通消息").is_none());
        assert!(ghost_completion("/model glm-4.7").is_none());
        assert!(ghost_completion("/zzz").is_none());
        // 首个前缀匹配（大小写不敏感；字典序）
        assert_eq!(ghost_completion("/mo").unwrap().0, "model");
        assert_eq!(ghost_completion("/MO").unwrap().0, "model");
        assert_eq!(ghost_completion("/t").unwrap().0, "tasks");
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
