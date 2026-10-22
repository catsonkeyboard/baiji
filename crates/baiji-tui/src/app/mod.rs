//! TUI 应用状态与主循环

use anyhow::Result;
use baiji_agent::{AgentEvent, ConfirmationDecision, SteeringQueue};
use baiji_harness::AgentHarness;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::confirm::ConfirmDialog;
use crate::settings::{self, AutoContinueConfig, RuntimeSettings};
use crate::theme::Theme;

use wizard::wizard_input_step_intercept;

/// 聊天区的一行（逻辑行）。PartialEq 供渲染缓存校验条目是否变化
#[derive(Debug, Clone, PartialEq)]
pub enum ChatLine {
    User(String),
    /// 助手正文（Markdown 渲染）
    Assistant(String),
    /// 工具活动行：`● name(摘要)` / `⎿ 结果概述`（`⎿ ✗` 失败）
    Tool(String),
    /// 工具调用行 `● name(摘要)`（带调用 id）：并行工具的结果可能乱序到达，
    /// `⎿` 结果按 id 插回本行之下，而不是按到达顺序追加
    ToolCall {
        id: String,
        text: String,
    },
    /// edit/write 的内联 diff 块（紧跟其 `●` 行；失败时 ToolFinished 标记）
    Diff(crate::diff::DiffView),
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

mod helpers;
mod history;
mod input;
mod pickers;
mod slash;
mod terminal;
mod wizard;

/// 聊天渲染（工具摘要 / 内联 diff / Markdown / 渲染缓存 / 确认弹窗 diff）的 App 级测试
#[cfg(test)]
mod chat_tests;
#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod tests;

use helpers::read_git_branch;
pub use helpers::{
    format_bytes, ghost_completion, sanitize_paste, slash_commands, split_slash,
};
pub(crate) use history::PromptHistory;
use input::InputBuffer;
// 输入框可视行布局（ui::draw_input 与 ↑↓ 行间移动共用同一套折行）
pub(crate) use input::{
    cursor_row_col as input_cursor, display_width as input_display_width,
    layout_rows as input_layout,
};
pub use pickers::{ConfirmBody, PendingConfirm, SessionPicker, SubagentsPanel};
pub use wizard::{ConfigWizard, WizardStep};

/// 粘贴守卫窗口：粘贴后这么短时间内到达的 Enter 属于粘贴内容（换行），不是提交
const PASTE_ENTER_GUARD: Duration = Duration::from_millis(150);
/// 输入框最多可见行数（超出后滚动、保持光标可见）
pub(crate) const MAX_INPUT_ROWS: usize = 8;
/// 首帧渲染前的输入框文本宽度假设（↑↓ 行间移动用；渲染后以真实宽度覆盖）
const DEFAULT_INPUT_WIDTH: usize = 76;

/// UI 事件：键盘输入 / 粘贴 / Agent 事件
/// （模型发现结果走专用通道，见事件循环 models_rx）
pub(crate) enum UiEvent {
    Key(KeyEvent),
    Paste(String),
    Agent(AgentEvent),
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
    /// 多行输入框（文本 + 光标 + 粘贴占位符）
    input: InputBuffer,
    /// 提示词历史（↑↓ 召回；`~/.baiji/prompt-history.jsonl`，测试为纯内存）
    history: PromptHistory,
    /// 最近一次粘贴的时刻（粘贴守卫：紧随其后的 Enter 视为换行）
    last_paste: Option<Instant>,
    /// 输入框可视行滚动偏移（光标保持可见；渲染时更新）
    input_scroll: usize,
    /// 输入框文本区宽度（列；渲染时更新，↑↓ 按可视行移动光标用）
    input_width: usize,
    /// 滚动偏移；usize::MAX 为「贴底」哨兵，渲染时钳制
    scroll: usize,
    /// 视图是否跟随底部：用户上翻后为 false，新内容不再把视图拽回底部；
    /// 翻回底部（渲染钳制）或提交/Ctrl+End 时恢复
    follow_bottom: bool,
    /// 最近一次渲染钳制得到的底部偏移（哨兵态下上翻的起点）
    chat_bottom: usize,
    /// 聊天区可视高度（行；渲染时更新，PageUp/PageDown 翻页用）
    chat_viewport: u16,
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
    /// 聊天区渲染缓存：已落定条目按 (宽度, 主题, 语言) 缓存可视行，免每帧重排全部历史
    chat_cache: crate::ui::ChatCache,
    /// 后台任务注册表（/tasks、/kill；headless 装配为 None）
    jobs: Option<Arc<baiji_tools::tools::jobs::JobRegistry>>,
    /// 本次会话累计工具调用次数（/usage；跨 run 累计）
    tools_total: u64,
    /// 界面文案（按 ui.language 选择；默认英文）
    strings: crate::i18n::Strings,
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
        lang: crate::i18n::Lang,
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
            input: InputBuffer::new(),
            history: PromptHistory::in_memory(),
            last_paste: None,
            input_scroll: 0,
            input_width: DEFAULT_INPUT_WIDTH,
            scroll: usize::MAX,
            follow_bottom: true,
            chat_bottom: 0,
            chat_viewport: 0,
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
            chat_cache: crate::ui::ChatCache::default(),
            jobs,
            strings: crate::i18n::Strings::for_lang(lang),
            plan_mode,
            awaiting_plan: false,
        }
    }

    /// 注入提示词历史（lib::run 传入落盘历史；默认纯内存，测试不碰真实家目录）
    pub(crate) fn set_prompt_history(&mut self, history: PromptHistory) {
        self.history = history;
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut terminal = ratatui::init();
        // bracketed paste（粘贴以事件送达）+ kitty 键盘协议（Shift+Enter 可区分）；
        // 退出与 panic 路径都会恢复（见 terminal.rs）
        terminal::enable_input_modes();
        let result = self.event_loop(&mut terminal).await;
        terminal::restore_input_modes();
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
                    // 只处理按下/重复（Windows 与 kitty 事件类型上报会额外送达松开事件）
                    CrosstermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                        Some(UiEvent::Key(key))
                    }
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
                    self.on_agent_event(agent_event, &ui_tx);
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
        // 其余只放行单行编辑键（字符/退格/光标移动进输入框；不换行、不召回历史、不补全）
        let mut wizard_input = false;
        if self.wizard.is_some() {
            let is_input_step = self.wizard.as_ref().is_some_and(|w| w.step.is_input());
            if !is_input_step {
                return self.handle_wizard_key(key, ui_tx).await;
            }
            if wizard_input_step_intercept(key.code) {
                match key.code {
                    KeyCode::Enter => self.wizard_enter(ui_tx).await,
                    // Esc：输入框里是给向导的内容（可能是 API key），随向导一起丢弃，
                    // 不能留给下一次回车当普通消息发出
                    _ => {
                        self.wizard = None;
                        self.input.clear();
                    }
                }
                return false;
            }
            wizard_input = true;
        }

        // 确认对话框优先拦截
        if self.pending.is_some() {
            self.handle_confirm_key(key);
            return false;
        }

        if wizard_input {
            self.edit_key(key);
            return false;
        }

        // 粘贴守卫：部分终端在 bracketed paste 之后补发一个回车——
        // 紧随粘贴到达的 Enter 属于粘贴内容，按换行处理而非提交
        let after_paste = self
            .last_paste
            .take()
            .is_some_and(|at| at.elapsed() < PASTE_ENTER_GUARD);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Esc => {
                if self.agent_running {
                    self.cancel.cancel();
                    self.lines.push(ChatLine::System(
                        self.strings.run_cancel_requested.to_string(),
                    ));
                } else if self.awaiting_plan {
                    self.awaiting_plan = false;
                    self.lines.push(ChatLine::System(
                        self.strings.plan_confirm_cancelled.to_string(),
                    ));
                } else if !self.input.is_empty() {
                    // 有草稿时 Esc 先清空草稿；空输入再按 Esc 才退出（防误触丢稿）
                    self.input.clear();
                    self.history.reset_draft();
                } else {
                    return true;
                }
            }
            // 换行：Shift+Enter（需 kitty 键盘协议）/ Alt+Enter / 粘贴尾随回车
            KeyCode::Enter if shift || alt || after_paste => self.input.insert_newline(),
            KeyCode::Enter => {
                // 行尾 `\` 续行：任何终端都可用的换行兜底
                if self.input.consume_backslash_continuation() {
                    return false;
                }
                return self.submit_input(ui_tx).await;
            }
            // Ctrl+J（LF）：不依赖修饰键上报的换行
            KeyCode::Char('j') if ctrl => self.input.insert_newline(),
            KeyCode::Char('o') if ctrl => {
                self.open_picker().await;
            }
            KeyCode::Tab => {
                // Tab 接受输入框里的灰色补全（ghost）——补全为完整命令 + 尾随空格
                if let Some((name, _)) = self.slash_ghost() {
                    self.input.set(format!("/{name} "));
                }
            }
            // 聊天区滚动：Shift+↑↓ 一行、PageUp/PageDown 一页、Ctrl+Home/End 顶/底
            KeyCode::Up if shift => self.scroll_up(1),
            KeyCode::Down if shift => self.scroll_down(1),
            KeyCode::PageUp => self.scroll_up(self.page_size()),
            KeyCode::PageDown => self.scroll_down(self.page_size()),
            KeyCode::Home if ctrl => self.scroll_to_top(),
            KeyCode::End if ctrl => self.jump_to_bottom(),
            // ↑↓：多行内容行间移动光标 → 到顶/底后走提示词历史；Alt/Ctrl+↑↓ 直接召回
            KeyCode::Up => self.input_up(ctrl || alt),
            KeyCode::Down => self.input_down(ctrl || alt),
            _ => {
                self.edit_key(key);
            }
        }
        false
    }

    /// Enter 提交：斜杠命令分发 / 运行中转 steering / 否则发起新 run。返回 true = 退出
    async fn submit_input(&mut self, ui_tx: &UnboundedSender<UiEvent>) -> bool {
        // 占位符展开回粘贴原文后再提交
        let text = self.input.expanded().trim().to_string();
        if text.is_empty() {
            // 计划等待态：空回车 = 批准执行（退出计划模式并发起执行 run）
            if self.awaiting_plan {
                self.approve_plan(ui_tx.clone()).await;
                self.jump_to_bottom();
            }
            return false;
        }
        self.awaiting_plan = false; // 任何输入都视为继续对话，批准作废
        self.history.record(&text);
        // 斜杠命令（/model /config /status /help …）。
        // 用户 prompt 模板（/name 参数）不在此处理：当作普通消息交给 Harness 展开
        let is_template = split_slash(&text)
            .is_some_and(|(cmd, _)| self.templates.iter().any(|(name, _)| name == cmd));
        if let Some((cmd, args)) = split_slash(&text).filter(|_| !is_template) {
            self.input.clear();
            let quit = self.handle_slash(cmd, args, ui_tx).await;
            self.jump_to_bottom();
            return quit; // /quit 请求退出
        }
        self.input.clear();
        if self.agent_running {
            // 运行中：作为 steering 注入（run 结束仍未消费的会在结束时自动补发）
            self.steering.push(&text);
            self.lines.push(ChatLine::System(crate::i18n::fill(
                self.strings.steering_prefix_tpl,
                &[&text],
            )));
        } else {
            self.lines.push(ChatLine::user(&text));
            self.auto_turns = 0; // 用户手动输入 = 新的接力链
            self.spawn_run(text, ui_tx.clone());
        }
        self.jump_to_bottom();
        false
    }

    /// 纯编辑键（主输入框与向导输入步骤共用）。返回是否处理
    fn edit_key(&mut self, key: KeyEvent) -> bool {
        let mods = key.modifiers;
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);
        let input = &mut self.input;
        match key.code {
            KeyCode::Char('a') if ctrl => input.move_line_start(),
            KeyCode::Char('e') if ctrl => input.move_line_end(),
            KeyCode::Char('u') if ctrl => {
                input.delete_to_line_start();
            }
            KeyCode::Char('k') if ctrl => {
                input.delete_to_line_end();
            }
            KeyCode::Char('w') if ctrl => {
                input.delete_word_back();
            }
            // macOS 终端 Option+←/→ 常以 ESC b / ESC f 送达
            KeyCode::Char('b') if alt && !ctrl => input.move_word_left(),
            KeyCode::Char('f') if alt && !ctrl => input.move_word_right(),
            KeyCode::Char(c) if is_text_input(c, mods) => input.insert_char(c),
            KeyCode::Backspace if ctrl || alt => {
                input.delete_word_left();
            }
            KeyCode::Backspace => {
                input.backspace();
            }
            KeyCode::Delete => {
                input.delete();
            }
            KeyCode::Left if ctrl || alt => input.move_word_left(),
            KeyCode::Left => input.move_left(),
            KeyCode::Right if ctrl || alt => input.move_word_right(),
            KeyCode::Right => input.move_right(),
            KeyCode::Home => input.move_line_start(),
            KeyCode::End => input.move_line_end(),
            _ => return false,
        }
        true
    }

    /// 在输入框可视行间移动光标（保持显示列）；已在首/末行时返回 false
    fn move_input_row(&mut self, up: bool) -> bool {
        match input::vertical_move(self.input.text(), self.input.cursor(), self.input_width, up) {
            Some(pos) => {
                self.input.set_cursor(pos);
                true
            }
            None => false,
        }
    }

    /// ↑：多行内容先在可视行间移动光标；到顶后，输入为空或正显示某条历史时召回更旧的一条；
    /// `explicit`（Alt/Ctrl+↑）不论内容直接召回，并暂存手头草稿（↓ 越过最新一条时恢复）
    fn input_up(&mut self, explicit: bool) {
        if !explicit && self.move_input_row(true) {
            return;
        }
        let current = self.input.expanded();
        let browsing = current.is_empty() || explicit || self.history.contains(&current);
        if browsing
            && let Some(entry) = self.history.older(&current, explicit)
            && entry != current
        {
            self.input.set(entry);
            return;
        }
        // 非历史草稿已在首行（或已是最旧一条）：光标到开头
        self.input.move_buffer_start();
    }

    /// ↓：多行内容先在可视行间移动光标；到底后，正显示历史时切到更新的一条，
    /// 越过最新一条恢复草稿（无草稿则清空）
    fn input_down(&mut self, explicit: bool) {
        if !explicit && self.move_input_row(false) {
            return;
        }
        let current = self.input.expanded();
        if let Some(entry) = self.history.newer(&current) {
            self.input.set(entry);
            return;
        }
        self.input.move_buffer_end();
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
                ConfirmationDecision::Allow => "allowed",
                ConfirmationDecision::AllowAll => "allowed (no more prompts this run)",
                ConfirmationDecision::Deny(_) => "denied",
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


    async fn set_plan_mode(&mut self, on: bool) {
        self.plan_mode = on;
        self.awaiting_plan = false;
        self.harness.lock().await.set_plan_mode(on);
        if on {
            self.lines
                .push(ChatLine::System(self.strings.plan_on.to_string()));
        } else {
            self.lines
                .push(ChatLine::System(self.strings.plan_off.to_string()));
        }
    }

    /// Enter 批准执行计划：退出计划模式并以固定指令发起执行 run
    async fn approve_plan(&mut self, ui_tx: UnboundedSender<UiEvent>) {
        self.awaiting_plan = false;
        self.set_plan_mode(false).await;
        self.lines.push(ChatLine::System(crate::i18n::fill(
            &self.strings.plan_execute_tpl,
            &[&baiji_harness::PLAN_EXECUTE_PROMPT.to_string()],
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
                .push(ChatLine::System("(no previous sessions)".to_string()));
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
                        let count = harness.subagents_reload().to_string();
                        self.lines.push(ChatLine::System(crate::i18n::fill(
                            &self.strings.subagents_reloaded_tpl,
                            &[&count],
                        )));
                    }
                    Err(_) => self
                        .lines
                        .push(ChatLine::System(self.strings.subagents_busy.to_string())),
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
                let title = session.meta.title.clone().unwrap_or_default();
                self.lines.push(ChatLine::System(crate::i18n::fill(
                    &self.strings.cmd_session_switched_tpl,
                    &[&self.session_id, &title],
                )));
            }
            Err(e) => {
                self.lines.push(ChatLine::System(crate::i18n::fill(
                    &self.strings.cmd_session_fail_tpl,
                    &[&e],
                )));
            }
        }
        self.jump_to_bottom();
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
                    crate::i18n::fill(&self.strings.cmd_branch_done_tpl, &[&new_id])
                } else {
                    crate::i18n::fill(
                        &self.strings.cmd_fork_done_tpl,
                        &[&new_id, &turns_back.to_string()],
                    )
                }));
                if let Some(input) = dropped {
                    self.input.set(input); // 多行原样回填
                }
            }
            Err(e) => self.lines.push(ChatLine::System(crate::i18n::fill(
                &self.strings.cmd_fork_fail_tpl,
                &[&e],
            ))),
        }
        self.jump_to_bottom();
    }

    async fn branch_session(&mut self) {
        let result = {
            let mut harness = self.harness.lock().await;
            harness.branch().map(|_| harness.session().meta.id.clone())
        };
        match result {
            Ok(new_id) => {
                self.session_id = new_id.clone();
                self.lines.push(ChatLine::System(crate::i18n::fill(
                    &self.strings.cmd_branch_done_tpl,
                    &[&new_id],
                )));
            }
            Err(e) => {
                self.lines.push(ChatLine::System(crate::i18n::fill(
                    &self.strings.cmd_fork_fail_tpl,
                    &[&e],
                )));
            }
        }
        self.jump_to_bottom();
    }

    /// 由会话历史重建聊天行（切换/分叉会话）：与实时事件同一套工具摘要与内联 diff。
    /// 历史不记录 is_error，失败与否按结果文本推断（baiji 错误前缀 / bash 非零退出码）
    fn rebuild_lines(&mut self, messages: &[baiji_ai::Message]) {
        use baiji_ai::Role;
        self.lines.clear();
        // tool_call_id → 工具名（结果行的概述按工具区分）
        let mut call_names: std::collections::HashMap<&str, &str> =
            std::collections::HashMap::new();
        for msg in messages {
            match msg.role {
                Role::User => self.lines.push(ChatLine::user(&msg.content)),
                Role::Assistant => {
                    if !msg.content.is_empty() {
                        self.lines.push(ChatLine::assistant(&msg.content));
                    }
                    for call in msg.tool_calls.iter().flatten() {
                        call_names.insert(call.id.as_str(), call.name.as_str());
                        self.push_tool_started(&call.id, &call.name, &call.arguments);
                    }
                }
                Role::Tool => {
                    for result in msg.tool_results.iter().flatten() {
                        let name = call_names
                            .get(result.tool_call_id.as_str())
                            .copied()
                            .unwrap_or("");
                        let is_error =
                            crate::tool_display::output_looks_failed(name, &result.content);
                        self.push_tool_finished(
                            &result.tool_call_id,
                            name,
                            &result.content,
                            is_error,
                        );
                    }
                }
                Role::System => {
                    self.lines.push(ChatLine::System(
                        "[conversation summary injected]".to_string(),
                    ));
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
            AgentEvent::ToolStarted { id, name, args } => {
                self.thinking.clear();
                // 本轮已流出的正文先落定：保持「正文 → 工具调用 → 下一轮正文」的时间顺序
                //（否则多轮正文首尾相接地堆在全部工具行之后）
                let text = std::mem::take(&mut self.streaming);
                if !text.trim().is_empty() {
                    self.lines.push(ChatLine::assistant(text));
                }
                self.push_tool_started(&id, &name, &args);
                self.scroll_to_bottom();
            }
            AgentEvent::ToolFinished {
                id,
                name,
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
                self.push_tool_finished(&id, &name, &output, is_error);
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
                    self.lines
                        .push(ChatLine::System(self.strings.plan_awaiting.to_string()));
                }
                self.maybe_auto_continue(ui_tx.clone());
            }
            AgentEvent::RunFailed { error } => {
                self.streaming.clear();
                self.thinking.clear();
                self.lines.push(ChatLine::System(crate::i18n::fill(
                    &self.strings.run_error_tpl,
                    &[&error],
                )));
                self.finish_run();
            }
            AgentEvent::Interrupted => {
                self.streaming.clear();
                self.thinking.clear();
                self.lines
                    .push(ChatLine::System(self.strings.run_cancelled.to_string()));
                self.run_interrupted = true; // 中断后本次接力链终止
                self.finish_run();
            }
        }
    }

    /// `● name(摘要)` 行 + edit/write 的内联 diff（建议中的改动，ToolStarted 即展示）
    fn push_tool_started(&mut self, id: &str, name: &str, args: &serde_json::Value) {
        let summary = crate::tool_display::tool_summary(name, args, &self.strings);
        self.lines.push(ChatLine::ToolCall {
            id: id.to_string(),
            text: if summary.is_empty() {
                format!("● {name}")
            } else {
                format!("● {name}({summary})")
            },
        });
        if let Some(diff) =
            crate::diff::DiffView::from_tool(id, name, args, Some(crate::diff::INLINE_DIFF_CAP))
        {
            self.lines.push(ChatLine::Diff(diff));
        }
    }

    /// `⎿` 结果概述行，按调用 id 配对：插到同 id 的 `●` 行（及其 diff）之下——
    /// 并行工具（join_all）的 ToolFinished 可能乱序到达。找不到同 id 的调用行
    /// （空 id / 历史已清空）时按到达顺序追加。失败时把该调用的 diff 标为未应用
    fn push_tool_finished(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        // 工具名已在 ● 行出现过；⎿ 行只给结果概述，出错时带 ✗ 前缀
        let summary = crate::tool_display::result_summary(name, output, is_error, &self.strings);
        let mark = if is_error { "✗ " } else { "" };
        let result = ChatLine::Tool(format!("⎿ {mark}{summary}"));
        // 最近一次同 id 的调用（部分厂商跨轮复用 call_0 之类的 id，取最新的一次）
        let call = (!id.is_empty())
            .then(|| {
                self.lines
                    .iter()
                    .rposition(|l| matches!(l, ChatLine::ToolCall { id: i, .. } if i == id))
            })
            .flatten();
        let Some(call) = call else {
            self.lines.push(result);
            return;
        };
        let mut at = call + 1;
        while let Some(ChatLine::Diff(diff)) = self.lines.get_mut(at) {
            if diff.tool_id != id {
                break;
            }
            if is_error {
                diff.failed = true;
            }
            at += 1;
        }
        self.lines.insert(at, result);
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
        // 计划模式下不接力：规划结果等待用户批准，而不是自动开跑。
        // goal / spec 实施模式 = 本次链内显式授权自主推进（仍受轮次上限约束）
        let goal_driven = self
            .harness
            .try_lock()
            .map(|h| h.goal_active())
            .unwrap_or(false);
        if (!self.auto.enabled && !goal_driven)
            || self.plan_mode
            || self.agent_running
            || self.run_interrupted
        {
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
            let max = self.auto.max_turns.to_string();
            self.lines.push(ChatLine::System(crate::i18n::fill(
                &self.strings.auto_stopped_tpl,
                &[&max],
            )));
            return;
        }
        let used = self.auto_turns.to_string();
        let max = self.auto.max_turns.to_string();
        self.lines.push(ChatLine::System(crate::i18n::fill(
            &self.strings.auto_continue_tpl,
            &[&used, &max],
        )));
        self.scroll_to_bottom();
        self.spawn_run(baiji_harness::AUTO_CONTINUE_PROMPT.to_string(), ui_tx);
    }

    /// 软贴底：视图本就跟随底部时才回底——用户上翻阅读时，流式回答/工具活动不把视图拽走
    fn scroll_to_bottom(&mut self) {
        if self.follow_bottom {
            self.scroll = usize::MAX; // 哨兵值，渲染时钳制
        }
    }

    /// 硬回底（提交 / Ctrl+End / 会话切换）：恢复跟随
    fn jump_to_bottom(&mut self) {
        self.follow_bottom = true;
        self.scroll = usize::MAX;
    }

    fn scroll_to_top(&mut self) {
        self.follow_bottom = false;
        self.scroll = 0;
    }

    /// 上翻 `rows` 行（离开底部即停止跟随）。内容不足一屏时无处可翻：保持跟随
    fn scroll_up(&mut self, rows: usize) {
        let current = if self.scroll == usize::MAX {
            self.chat_bottom
        } else {
            self.scroll
        };
        if current == 0 {
            return;
        }
        self.follow_bottom = false;
        self.scroll = current.saturating_sub(rows);
    }

    /// 下翻 `rows` 行；越过底部时由渲染钳制（set_scroll）并恢复跟随
    fn scroll_down(&mut self, rows: usize) {
        if self.scroll != usize::MAX {
            self.scroll = self.scroll.saturating_add(rows);
        }
    }

    /// 翻页步长：聊天区可视高度 − 2（保留两行上下文）
    fn page_size(&self) -> usize {
        (self.chat_viewport as usize).saturating_sub(2).max(1)
    }

    /// Agent 事件入口（事件循环调用）：先渲染/更新状态，run 结束时再做 steering 兜底
    fn on_agent_event(&mut self, event: AgentEvent, ui_tx: &UnboundedSender<UiEvent>) {
        let ends_run = matches!(
            event,
            AgentEvent::RunCompleted { .. }
                | AgentEvent::RunFailed { .. }
                | AgentEvent::Interrupted
        );
        self.handle_agent_event(event, ui_tx);
        if ends_run {
            self.resubmit_leftover_steering(ui_tx);
        }
    }

    /// run 结束（完成/失败/中断）时队列里仍有未被消费的 steering（典型：最终回答流式
    /// 输出期间输入的）：合并为下一条用户消息立即发起新 run——steering 永不丢失。
    /// 中断时同样补发：先输入纠正、再按 Esc = 「停下，改做这个」（redirect 语义）。
    /// 已因自动接力等开了新 run 时无事可做（spawn_run 已把遗留消息转入新队列）
    fn resubmit_leftover_steering(&mut self, ui_tx: &UnboundedSender<UiEvent>) -> bool {
        if self.agent_running {
            return false;
        }
        let pending = self.steering.drain();
        if pending.is_empty() {
            return false;
        }
        let text = pending.join("\n\n");
        self.lines
            .push(ChatLine::System(self.strings.steering_resubmit.to_string()));
        self.lines.push(ChatLine::user(&text));
        self.auto_turns = 0; // 用户输入发起 = 新的接力链
        self.spawn_run(text, ui_tx.clone());
        self.jump_to_bottom();
        true
    }

    /// 轮换 steering 队列：新 run 用新队列，旧队列里未消费的消息转入新队列（不丢）
    fn rotate_steering(&mut self) -> Arc<SteeringQueue> {
        let steering = Arc::new(SteeringQueue::new());
        for message in self.steering.drain() {
            steering.push(message);
        }
        self.steering = Arc::clone(&steering);
        steering
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
        // 运行中输入进新队列：外部 agent 结束（合成的 RunCompleted）时作为下一条消息补发
        self.rotate_steering();
        let cancel = CancellationToken::new();
        self.cancel = cancel.clone();
        let workdir = self.workdir.clone();
        let interrupted_msg = self.strings.external_interrupted.to_string();

        tokio::spawn(async move {
            let _ = ui_tx.send(UiEvent::Agent(AgentEvent::ToolStarted {
                id: spec.name.clone(),
                name: spec.name.clone(),
                args: serde_json::json!({"prompt": prompt}),
            }));
            let (output, is_error) = tokio::select! {
                _ = cancel.cancelled() => (
                    interrupted_msg,
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

        let steering = self.rotate_steering();
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
            self.strings.status_ready.to_string()
        }
    }

    /// 状态栏右半：会话与运行台账（各项在无数据时省略）
    pub(crate) fn status_right(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.plan_mode {
            parts.push(
                if self.awaiting_plan {
                    self.strings.status_plan_await
                } else {
                    self.strings.status_plan_readonly
                }
                .to_string(),
            );
        }
        if self.context_tokens > 0 {
            parts.push(format!("ctx {:.1}k", self.context_tokens as f64 / 1000.0));
        }
        if self.bytes_saved > 0 {
            parts.push(crate::i18n::fill(
                self.strings.status_saved_tpl,
                &[&format_bytes(self.bytes_saved), &self.tokens_saved],
            ));
        }
        if self.auto.enabled && self.auto_turns > 0 {
            parts.push(crate::i18n::fill(
                self.strings.status_auto_tpl,
                &[&self.auto_turns, &self.auto.max_turns],
            ));
        }
        parts.push(crate::i18n::fill(
            self.strings.status_session_tpl,
            &[&self.session_id],
        ));
        parts.join(" · ")
    }

    // 访问器供 ui 模块渲染
    pub(crate) fn lines(&self) -> &[ChatLine] {
        &self.lines
    }

    /// 聊天行 + 渲染缓存（拆分借用：缓存按行内容校验、增量重渲染）
    pub(crate) fn chat_view(&mut self) -> (&[ChatLine], &mut crate::ui::ChatCache) {
        (&self.lines, &mut self.chat_cache)
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
        self.input.text()
    }

    /// 输入框缓冲区（渲染：光标、占位符高亮）
    pub(crate) fn input_buffer(&self) -> &InputBuffer {
        &self.input
    }

    pub(crate) fn input_scroll(&self) -> usize {
        self.input_scroll
    }

    /// 渲染回写：输入框滚动偏移与文本区宽度
    pub(crate) fn set_input_view(&mut self, scroll: usize, width: usize) {
        self.input_scroll = scroll;
        self.input_width = width.max(1);
    }

    /// 渲染回写：聊天区可视高度（翻页步长）
    pub(crate) fn set_chat_viewport(&mut self, height: u16) {
        self.chat_viewport = height;
    }

    pub(crate) fn scroll(&self) -> usize {
        self.scroll
    }

    /// 渲染钳制回写：值变小说明已到底（贴底哨兵或越界下翻）→ 恢复跟随
    pub(crate) fn set_scroll(&mut self, value: usize) {
        if value < self.scroll {
            self.follow_bottom = true;
            self.chat_bottom = value;
        }
        self.scroll = value;
    }

    /// 是否有覆盖层（会话选择器 / 子代理面板 / 向导 / 确认弹窗）：此时不显示输入框光标
    pub(crate) fn overlay_active(&self) -> bool {
        self.picker.is_some()
            || self.subagents_panel.is_some()
            || self.wizard.is_some()
            || self.pending.is_some()
    }

    pub(crate) fn agent_running(&self) -> bool {
        self.agent_running
    }

    pub(crate) fn theme(&self) -> Theme {
        self.theme
    }

    /// 界面文案（渲染与命令输出共用）
    pub(crate) fn strings(&self) -> &crate::i18n::Strings {
        &self.strings
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
            .map(|p| p.display_rows(&self.session_id, self.strings.picker_no_title))
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
                role.model
                    .as_deref()
                    .unwrap_or(self.strings.subagents_inherit),
                role.thinking.map(|t| t.effort()).unwrap_or("-"),
                role.max_turns
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| self.strings.subagents_default.into()),
                role.tools
                    .as_ref()
                    .map(|t| t.len().to_string())
                    .unwrap_or_else(|| self.strings.subagents_all.into()),
            ));
            if i == panel.selected {
                if !role.description.is_empty() {
                    rows.push(format!("    {}", role.description));
                }
                if !role.system_prompt.is_empty() {
                    let preview: String = role.system_prompt.chars().take(160).collect();
                    rows.push(crate::i18n::fill(
                        self.strings.subagents_prompt_preview_tpl,
                        &[&preview.trim_end().to_string()],
                    ));
                }
            }
        }
        if roles.is_empty() {
            rows.push(self.strings.subagents_empty1.into());
            rows.push(self.strings.subagents_empty2.into());
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
            "(disabled)".to_string()
        } else {
            dirs.iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(" · ")
        };
        Some(crate::i18n::fill(
            self.strings.subagents_dirs_tpl,
            &[&joined],
        ))
    }

    /// 子代理面板选中项（渲染高亮用）
    pub(crate) fn subagents_selected(&self) -> Option<usize> {
        self.subagents_panel.as_ref().map(|p| p.selected)
    }

    /// 粘贴并入输入框（光标处）：保留多行结构；≥5 行显示为占位符、提交时展开。
    /// 向导输入步骤（API Key / 模型名）保持单行净化语义
    pub(crate) fn handle_paste(&mut self, text: String) {
        if self.wizard.as_ref().is_some_and(|w| w.step.is_input()) {
            self.input.insert_str(&sanitize_paste(&text));
            return;
        }
        self.last_paste = Some(Instant::now());
        let text = input::normalize_paste(&text);
        if !text.is_empty() {
            self.input
                .insert_paste(&text, self.strings.input_paste_placeholder_tpl);
        }
    }

    /// 仅测试使用（lib 构建无调用方，cfg(test) 避免 dead_code 告警）
    #[cfg(test)]
    pub(crate) fn wizard_active(&self) -> bool {
        self.wizard.is_some()
    }

    /// 全部命令（静态注册表 + 外部 agent 动态命令）：裸 `/` 的清单展示用
    pub(crate) fn command_names(&self) -> Vec<String> {
        let mut names: Vec<String> = slash_commands(crate::i18n::Lang::En)
            .iter()
            .map(|(n, _)| n.to_string())
            .collect();
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
        let input = self.input.text();
        let rest = input.strip_prefix('/')?;
        // 参数区（含空格）与多行内容不出补全
        if rest.contains(' ') || rest.contains('\n') {
            return None;
        }
        if rest.is_empty() {
            // 裸 "/"：首个命令作为 Tab 目标（完整清单由输入框 ghost 灰字展示）
            let (name, usage) = slash_commands(self.strings.lang).first()?.to_owned();
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
                crate::i18n::fill(self.strings.external_usage_tpl, &[&agent.name, &agent.name]),
            ));
        }
        ghost_completion(input, self.strings.lang).map(|(n, u)| (n.to_string(), u.to_string()))
    }

    /// 向导标题（按步骤）
    pub(crate) fn wizard_title(&self) -> Option<String> {
        let wizard = self.wizard.as_ref()?;
        Some(match wizard.step {
            WizardStep::Vendor => self.strings.wizard_vendor_title.to_string(),
            WizardStep::Endpoint => {
                crate::i18n::fill(self.strings.wizard_endpoint_title_tpl, &[&wizard.vendor])
            }
            WizardStep::Key => crate::i18n::fill(
                self.strings.wizard_key_title_tpl,
                &[&baiji_ai::find_vendor(&wizard.vendor)
                    .map(|v| v.api_key_env)
                    .unwrap_or("?")],
            ),
            WizardStep::Model => self.strings.wizard_model_title.to_string(),
            WizardStep::ModelManual => self.strings.wizard_model_manual_title.to_string(),
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
                                crate::i18n::fill(
                                    self.strings.wizard_api_endpoint_tpl,
                                    &[&preset.base_url],
                                )
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
                    rows.push("(fetching model list…)".to_string());
                } else if let Some(err) = &wizard.models_error {
                    rows.push(crate::i18n::fill(
                        self.strings.wizard_models_error_tpl,
                        &[&err],
                    ));
                    rows.push(crate::i18n::fill(
                        self.strings.wizard_fallback_hint_tpl,
                        &[&self.strings.wizard_manual_entry.to_string()],
                    ));
                } else {
                    rows.extend(wizard.models.iter().map(|m| match &m.display_name {
                        Some(d) if d != &m.id => format!("{} — {}", m.id, d),
                        _ => m.id.clone(),
                    }));
                }
                rows.push(self.strings.wizard_manual_entry.to_string());
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
            WizardStep::Key => Some(crate::i18n::fill(
                self.strings.wizard_key_prompt_tpl,
                &[&baiji_ai::find_vendor(&wizard.vendor)
                    .map(|v| v.api_key_env)
                    .unwrap_or("?")],
            )),
            WizardStep::ModelManual => Some(self.strings.wizard_manual_prompt.to_string()),
            _ => None,
        }
    }
}

/// 按键是否产生文本输入：无修饰（Shift 除外）照常输入；Ctrl+字母是快捷键不插字；
/// Alt/AltGr 组合出的非字母数字符号（部分键盘布局以 Ctrl+Alt+字符 送达）视为文本
fn is_text_input(c: char, mods: KeyModifiers) -> bool {
    if mods.intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META) {
        return false;
    }
    match (
        mods.contains(KeyModifiers::CONTROL),
        mods.contains(KeyModifiers::ALT),
    ) {
        (false, false) => true,
        (_, true) => !c.is_control() && c != ' ' && !c.is_ascii_alphanumeric(),
        (true, false) => false,
    }
}
