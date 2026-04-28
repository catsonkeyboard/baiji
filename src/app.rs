use crate::agent::{AgentEvent, ReActAgent, SteeringQueue};
use crate::config::Config;
use crate::event::{Event, EventHandler};
use crate::llm::Message as LlmMessage;
use crate::mcp::McporterBridge;
use crate::ui::UI;
use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// 应用状态
pub struct App {
    /// 配置
    pub config: Config,
    /// 是否退出
    pub should_quit: bool,
    /// 当前输入的文本
    pub input: String,
    /// 输入框光标位置
    pub cursor_position: usize,
    /// 对话历史（UI 展示用）
    pub messages: Vec<Message>,
    /// 当前滚动位置
    pub scroll: usize,
    /// 当前是否正在处理（agent 运行中）
    pub is_streaming: bool,
    /// 当前轮次数（实时统计）
    pub current_turn: usize,
    /// 当前工具调用数（实时统计）
    pub tool_call_count: usize,
    /// Tool-use Agent（可选）
    agent: Option<Arc<ReActAgent>>,
    /// Agent 事件发送通道（在 run() 中初始化）
    agent_tx: Option<UnboundedSender<AgentEvent>>,
    /// 发给 LLM 的历史对话（只含 User/Assistant 轮次）
    llm_history: Vec<LlmMessage>,
    /// MCP 桥接（后台发现用）
    mcporter: Option<Arc<McporterBridge>>,
    /// Steering 消息队列（用户在 agent 运行中发送的消息）
    steering_queue: SteeringQueue,
    /// 当前运行的 agent 任务取消令牌
    cancel_token: Option<CancellationToken>,
}

/// 消息类型
#[derive(Debug, Clone)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub timestamp: chrono::DateTime<chrono::Local>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

impl App {
    /// 创建新应用实例
    pub fn new(
        config: Config,
        agent: Option<Arc<ReActAgent>>,
        mcporter: Option<Arc<McporterBridge>>,
        builtin_tool_count: usize,
    ) -> Self {
        let mcp_status = if mcporter.is_some() {
            "MCP: 后台加载中，完成后会有通知".to_string()
        } else {
            "MCP: 未配置".to_string()
        };
        let welcome_message = Message {
            role: MessageRole::System,
            content: format!(
                "欢迎使用 Rust Agent! 🦀\n\n\
                当前配置:\n\
                - Provider: {}\n\
                - Model: {}\n\
                - 内置工具: {}\n\
                - {}\n\n\
                快捷键:\n\
                - Enter: 发送消息（Agent 运行中则作为 Steering 注入）\n\
                - Esc: 中止当前 Agent 运行\n\
                - Ctrl+C / q: 退出\n\
                - ↑/↓: 滚动历史\n\
                - Ctrl+L: 清屏",
                config.llm.provider,
                config.llm.model,
                builtin_tool_count,
                mcp_status,
            ),
            timestamp: chrono::Local::now(),
        };

        Self {
            config,
            should_quit: false,
            input: String::new(),
            cursor_position: 0,
            messages: vec![welcome_message],
            scroll: 0,
            is_streaming: false,
            current_turn: 0,
            tool_call_count: 0,
            agent,
            agent_tx: None,
            llm_history: Vec::new(),
            mcporter,
            steering_queue: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            cancel_token: None,
        }
    }

    /// 运行应用主循环
    pub async fn run(&mut self) -> Result<()> {
        info!("Initializing terminal UI...");

        let mut terminal = setup_terminal()?;
        let mut event_handler = EventHandler::new(Duration::from_millis(100));

        // 创建 agent 事件通道
        let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentEvent>();
        self.agent_tx = Some(agent_tx.clone());

        // 后台 MCP 工具发现（TUI 已启动，不阻塞）
        if let Some(bridge) = self.mcporter.clone() {
            let tx = agent_tx.clone();
            tokio::spawn(async move {
                let server_names = match bridge.server_names() {
                    Ok(names) => names,
                    Err(e) => {
                        tx.send(AgentEvent::McpFailed {
                            server: "mcporter".to_string(),
                            error: e.to_string(),
                        })
                        .ok();
                        return;
                    }
                };
                for server in server_names {
                    match bridge.discover_server_tools(&server).await {
                        Ok(tools) => {
                            tx.send(AgentEvent::McpReady { server, tools }).ok();
                        }
                        Err(e) => {
                            tx.send(AgentEvent::McpFailed {
                                server,
                                error: e.to_string(),
                            })
                            .ok();
                        }
                    }
                }
            });
        }

        // 主循环
        loop {
            terminal.draw(|f| UI::draw(f, self))?;

            if self.should_quit {
                break;
            }

            tokio::select! {
                event = event_handler.next() => {
                    match event? {
                        Event::Tick => {}
                        Event::Key(key_event) => {
                            self.handle_key_event(key_event).await?;
                        }
                        Event::Mouse(_) => {}
                    }
                }
                Some(agent_event) = agent_rx.recv() => {
                    self.handle_agent_event(agent_event);
                }
            }
        }

        restore_terminal()?;
        info!("Terminal restored, exiting...");
        Ok(())
    }

    /// 处理键盘事件
    async fn handle_key_event(&mut self, key: crossterm::event::KeyEvent) -> Result<()> {
        use crossterm::event::{KeyCode, KeyModifiers};

        debug!("Key event: {:?}", key);

        match key.code {
            // 退出（非运行中才生效）
            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                self.should_quit = true;
            }
            KeyCode::Char('q') if key.modifiers.is_empty() && !self.is_streaming => {
                self.should_quit = true;
            }

            // Esc：中止当前 agent 运行
            KeyCode::Esc => {
                if let Some(cancel) = self.cancel_token.take() {
                    info!("User cancelled agent run");
                    cancel.cancel();
                }
            }

            // 清屏
            KeyCode::Char('l') if key.modifiers == KeyModifiers::CONTROL => {
                self.scroll = 0;
            }

            // 发送消息
            KeyCode::Enter => {
                if !self.input.trim().is_empty() {
                    if self.is_streaming {
                        // Agent 运行中 → 作为 Steering 消息注入
                        self.send_steering_message();
                    } else {
                        self.send_message().await?;
                    }
                }
            }

            // 输入字符
            KeyCode::Char(c) => {
                if self.input.is_char_boundary(self.cursor_position) {
                    self.input.insert(self.cursor_position, c);
                    self.cursor_position += c.len_utf8();
                }
            }

            // 删除字符
            KeyCode::Backspace => {
                if self.cursor_position > 0 {
                    let prev_pos = self.input[..self.cursor_position]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(prev_pos);
                    self.cursor_position = prev_pos;
                }
            }
            KeyCode::Delete => {
                if self.cursor_position < self.input.len()
                    && self.input.is_char_boundary(self.cursor_position)
                {
                    self.input.remove(self.cursor_position);
                }
            }

            // 光标移动
            KeyCode::Left => {
                if self.cursor_position > 0 {
                    let prev_pos = self.input[..self.cursor_position]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.cursor_position = prev_pos;
                }
            }
            KeyCode::Right => {
                if self.cursor_position < self.input.len() {
                    let next_pos = self.input[self.cursor_position..]
                        .chars()
                        .next()
                        .map(|c| self.cursor_position + c.len_utf8())
                        .unwrap_or(self.input.len());
                    self.cursor_position = next_pos;
                }
            }
            KeyCode::Home => {
                self.cursor_position = 0;
            }
            KeyCode::End => {
                self.cursor_position = self.input.len();
            }

            // 滚动历史
            KeyCode::Up => {
                if self.scroll > 0 {
                    self.scroll -= 1;
                }
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(5);
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(5);
            }

            _ => {}
        }

        Ok(())
    }

    /// Agent 运行中发送 Steering 消息（用户打断）
    fn send_steering_message(&mut self) {
        let msg = self.input.trim().to_string();
        self.input.clear();
        self.cursor_position = 0;

        info!("Steering message queued: {}", msg);

        // 推送到 steering 队列（agent 在工具执行间隙会取走）
        self.steering_queue.lock().unwrap().push_back(msg.clone());

        // 在 UI 中显示用户消息（无需新 assistant 占位，TurnStart 会处理）
        self.messages.push(Message {
            role: MessageRole::User,
            content: msg,
            timestamp: chrono::Local::now(),
        });
        self.scroll_to_bottom();
    }

    /// 发送用户消息，启动 agent 任务
    async fn send_message(&mut self) -> Result<()> {
        let content = self.input.trim().to_string();

        self.messages.push(Message {
            role: MessageRole::User,
            content: content.clone(),
            timestamp: chrono::Local::now(),
        });
        self.input.clear();
        self.cursor_position = 0;

        // 添加空的 assistant 消息占位（第一个 TurnStart 会复用它）
        self.messages.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            timestamp: chrono::Local::now(),
        });
        self.is_streaming = true;
        self.scroll_to_bottom();

        info!("User message: {}", content);

        if let (Some(agent), Some(tx)) = (self.agent.clone(), self.agent_tx.clone()) {
            self.llm_history.push(LlmMessage::user(content.clone()));
            let history = self.llm_history.clone();
            let steering = self.steering_queue.clone();

            // 创建取消令牌，Esc 键可触发
            let cancel = CancellationToken::new();
            self.cancel_token = Some(cancel.clone());

            tokio::spawn(async move {
                match agent.run(history, &content, tx.clone(), cancel, steering).await {
                    Ok(_) => {}
                    Err(e) => {
                        // 输出完整错误链到日志文件
                        tracing::error!("Agent run failed: {:?}", e);
                        let mut chain = format!("{}", e);
                        let mut src = e.source();
                        while let Some(cause) = src {
                            chain.push_str(&format!("\n  caused by: {}", cause));
                            src = cause.source();
                        }
                        tracing::error!("Error chain:\n{}", chain);
                        tx.send(AgentEvent::Error(format!("Agent 执行异常: {}", e))).ok();
                    }
                }
            });
        } else {
            if let Some(last) = self.messages.last_mut() {
                last.content = "（未配置 LLM，请检查 config.json）".to_string();
            }
            self.is_streaming = false;
        }
        Ok(())
    }

    /// 处理 Agent 事件，更新 UI
    fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            // 新 turn 开始：如果没有空的 assistant 占位则新建一个
            AgentEvent::TurnStart => {
                self.current_turn += 1;
                let needs_placeholder = match self.messages.last() {
                    Some(m) if m.role == MessageRole::Assistant && m.content.is_empty() => false,
                    _ => true,
                };
                if needs_placeholder && self.is_streaming {
                    self.messages.push(Message {
                        role: MessageRole::Assistant,
                        content: String::new(),
                        timestamp: chrono::Local::now(),
                    });
                }
            }

            // Turn 结束：无需 UI 操作
            AgentEvent::TurnEnd => {}

            // 流式文本 delta：追加到最后一条 assistant 消息
            AgentEvent::TextDelta(text) => {
                self.append_to_last_assistant(&text);
            }

            // 工具调用开始
            AgentEvent::ToolExecutionStart { name, args, .. } => {
                let info = format!(
                    "[调用: {}] {}\n",
                    name,
                    serde_json::to_string(&args).unwrap_or_default()
                );
                self.append_to_last_assistant(&info);
            }

            // 工具调用结束
            AgentEvent::ToolExecutionEnd {
                name,
                result,
                is_error,
                ..
            } => {
                let label = if is_error { "错误" } else { "结果" };
                let preview = if result.len() > 300 {
                    // 找到不超过 300 字节的最近 char 边界，避免切断多字节字符
                    let end = (0..=300).rev().find(|&i| result.is_char_boundary(i)).unwrap_or(0);
                    format!("{}...", &result[..end])
                } else {
                    result.clone()
                };
                let info = format!("[{}: {}] {}\n\n", label, name, preview);
                self.append_to_last_assistant(&info);
                self.tool_call_count += 1;
            }

            // Agent 完成（所有 turn 结束后触发一次）
            AgentEvent::Completed(answer) => {
                // TextDelta 已经流式写入，若 assistant 消息为空则回退写入（容错）
                if let Some(last) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant)
                {
                    if last.content.is_empty() {
                        last.content = answer.clone();
                    }
                }
                self.llm_history.push(LlmMessage::assistant(answer));
                self.is_streaming = false;
                self.cancel_token = None;
                self.current_turn = 0;
                self.tool_call_count = 0;
                self.scroll_to_bottom();
            }

            // 被用户 Esc 中止
            AgentEvent::Interrupted => {
                if let Some(last) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant)
                {
                    if last.content.is_empty() {
                        last.content = "[已中断]".to_string();
                    } else {
                        last.content.push_str("\n[已中断]");
                    }
                }
                self.is_streaming = false;
                self.cancel_token = None;
                self.scroll_to_bottom();
            }

            AgentEvent::Error(err) => {
                if let Some(last) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant)
                {
                    if last.content.is_empty() {
                        last.content = format!("错误: {}", err);
                    } else {
                        last.content.push_str(&format!("\n错误: {}", err));
                    }
                }
                self.is_streaming = false;
                self.cancel_token = None;
            }

            AgentEvent::McpReady { server, tools } => {
                let count = tools.len();
                if let Some(agent) = &self.agent {
                    agent.add_tools(tools);
                }
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: format!("[MCP] {} 服务器已就绪，加载了 {} 个工具", server, count),
                    timestamp: chrono::Local::now(),
                });
                self.scroll_to_bottom();
            }

            AgentEvent::McpFailed { server, error } => {
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: format!("[MCP] {} 服务器启动失败: {}", server, error),
                    timestamp: chrono::Local::now(),
                });
                self.scroll_to_bottom();
            }
        }
    }

    /// 向最后一条 assistant 消息追加内容（跳过其后的 user/system 消息）
    fn append_to_last_assistant(&mut self, text: &str) {
        if let Some(last) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
        {
            last.content.push_str(text);
        }
        self.scroll_to_bottom();
    }

    /// 滚动到底部
    fn scroll_to_bottom(&mut self) {
        self.scroll = usize::MAX;
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    use crossterm::{
        terminal::{enable_raw_mode, EnterAlternateScreen},
        ExecutableCommand,
    };
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal() -> Result<()> {
    use crossterm::{
        terminal::{disable_raw_mode, LeaveAlternateScreen},
        ExecutableCommand,
    };
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(LeaveAlternateScreen)?;
    Ok(())
}
