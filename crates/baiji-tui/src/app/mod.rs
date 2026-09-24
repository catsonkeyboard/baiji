//! TUI 应用状态与主循环

use anyhow::Result;
use baiji_agent::{AgentEvent, ConfirmationDecision, SteeringQueue};
use baiji_harness::AgentHarness;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::confirm::ConfirmDialog;
use crate::settings::{self, AutoContinueConfig, RuntimeSettings};
use crate::theme::Theme;

use wizard::wizard_input_step_intercept;

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

mod helpers;
mod pickers;
mod slash;
mod wizard;

#[cfg(test)]
mod tests;

use helpers::read_git_branch;
pub use helpers::{
    format_bytes, ghost_completion, sanitize_paste, slash_commands, split_slash,
};
pub use pickers::{PendingConfirm, SessionPicker, SubagentsPanel};
pub use wizard::{ConfigWizard, WizardStep};

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
            strings: crate::i18n::Strings::for_lang(lang),
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
                    self.lines.push(ChatLine::System(
                        self.strings.run_cancel_requested.to_string(),
                    ));
                } else if self.awaiting_plan {
                    self.awaiting_plan = false;
                    self.lines.push(ChatLine::System(
                        self.strings.plan_confirm_cancelled.to_string(),
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
                    self.lines.push(ChatLine::System(crate::i18n::fill(
                        self.strings.steering_prefix_tpl,
                        &[&text],
                    )));
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
                    crate::i18n::fill(&self.strings.cmd_branch_done_tpl, &[&new_id])
                } else {
                    crate::i18n::fill(
                        &self.strings.cmd_fork_done_tpl,
                        &[&new_id, &turns_back.to_string()],
                    )
                }));
                if let Some(input) = dropped {
                    self.input = sanitize_paste(&input);
                }
            }
            Err(e) => self.lines.push(ChatLine::System(crate::i18n::fill(
                &self.strings.cmd_fork_fail_tpl,
                &[&e],
            ))),
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

    /// 粘贴并入输入框（换行折叠为空格，保持单行输入语义）
    pub(crate) fn handle_paste(&mut self, text: String) {
        self.input.push_str(&sanitize_paste(&text));
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
        let input = self.input.as_str();
        let rest = input.strip_prefix('/')?;
        if rest.contains(' ') {
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

