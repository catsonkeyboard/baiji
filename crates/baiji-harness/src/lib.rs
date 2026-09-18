//! baiji-harness — AgentHarness
//!
//! 组装 Runtime 与工程化设施：
//! - [`Session`] / [`SessionTree`]：会话与分支树
//! - [`JsonlStore`]：JSONL 逐条追加持久化
//! - [`CompactionPolicy`]：token 估算 + 分层上下文压缩
//! - skills 加载与 prompt 模板
//! - run loop：用户输入 → runtime → 持久化

pub mod compaction;
pub mod memory;
pub mod persist;
pub mod session;
pub mod skills;
pub mod stub;
pub mod templates;
pub mod todo;

pub use compaction::{CompactionPolicy, compact, compact_with_llm, estimate_tokens};
pub use memory::{MemoryEntry, MemoryKind, MemoryStore, MemoryTool, memory_section, project_key};
pub use persist::{JsonlStore, Record};
pub use session::{Session, SessionMeta, SessionTree, group_by_project, new_session_id};
pub use skills::{Skill, SkillTool, filter_skills, load_skills};
pub use stub::stub_tool_results;
pub use templates::render;
pub use templates::{PromptTemplate, load_templates};
pub use todo::{TodoItem, TodoStatus, TodoStore, TodoTool, todo_section};

/// 自动接力的固定输入（用户可见、进入会话历史——下一轮模型自然衔接）
pub const AUTO_CONTINUE_PROMPT: &str = "继续，按 todo 清单推进任务";

/// 自动接力判定（TUI / headless 共用）：
/// 自主模式开 && todo 有未完成项 && 本次 run 正常完成（未被取消/失败）
/// && 累计轮次未超上限。上限按"一次用户输入触发的接力链"计，
/// 用户下一次手动输入时由调用方清零重新计。
pub fn should_auto_continue(
    enabled: bool,
    max_turns: u32,
    has_open_todos: bool,
    turns_used: u32,
    run_completed_normally: bool,
) -> bool {
    enabled && has_open_todos && run_completed_normally && turns_used < max_turns
}

use anyhow::Result;
use baiji_agent::{AgentEvent, AgentRuntime, SteeringQueue};
use baiji_ai::Message;
use baiji_telemetry::NoopTelemetry;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// 默认 coding-agent 系统提示
pub const DEFAULT_PROMPT: &str = "\
You are baiji, a terminal coding agent.

- Work in the current directory; use tools (read/write/edit/bash/grep/find/ls) to explore and modify code.
- Reading files: prefer read with mode=signatures first (symbol outline with line spans), \
then read the exact range with offset/limit. Use mode=map for directory overviews. \
Avoid mode=full on large files. If output was truncated, use the expand tool with the ctx: handle.
- Finding code: use search (BM25 keywords, no exact pattern needed) to locate relevant symbols, \
imports (direction=incoming) to see who depends on a file before changing it; grep for exact patterns.
- Prefer precise edits (edit tool) over rewriting whole files.
- Persist durable project knowledge (build commands, conventions, decisions, pitfalls) with the memory tool; it survives across sessions.
- For multi-step work, break it down with the todo tool first (one in_progress item at a time, mark done as you go). The list lives outside the conversation and is never lost to compaction.
- Verify changes by reading back or running quick commands when practical.
- Keep answers concise; show the final result and any commands the user should run.
- If a request is ambiguous, state your assumption and proceed.

## Environment
- Working directory: {{cwd}}
- Date: {{date}}
- OS: {{os}}
- Model: {{model}}";

/// AgentHarness：会话 + 持久化 + runtime 的门面
pub struct AgentHarness {
    runtime: Arc<AgentRuntime>,
    store: JsonlStore,
    session: Session,
    base_prompt: String,
    skills_prompt: Option<String>,
    /// 用户 prompt 模板（`/name 参数` 调用）
    templates: Vec<templates::PromptTemplate>,
    compaction: CompactionPolicy,
    /// 上一次 LLM 调用厂商上报的真实上下文占用（input + output token）
    last_context_tokens: Option<usize>,
    /// 用 LLM 生成压缩摘要（失败回退确定性摘要）
    llm_compaction: bool,
    /// 跨会话项目记忆（None = 未启用）
    memory: Option<(Arc<MemoryStore>, String)>,
    /// ctx store 目录（None = 禁用历史 tool result 的 stub 化）
    ctx_store: Option<PathBuf>,
    /// 会话级任务清单（与 TodoTool 共享；None = 未启用 todo 工具）
    todos: Option<Arc<TodoStore>>,
    /// usage 锚点：最近一次厂商上报的真实上下文占用 + 当时的消息数。
    /// 压缩触发估算 = usage + 锚点后新增消息的字符估算（参考 pi 的
    /// estimateContextTokens——不可见的系统提示/工具定义/tokenizer 差异
    /// 全部体现在真实值里）。
    usage_anchor: Option<UsageAnchor>,
    /// 历史版本号：压缩/stub/切换/分叉等重写历史的操作递增，锚点据此失效
    /// （防陈旧锚点：消息数恰好回到锚点值但内容已不同）
    history_version: u64,
    telemetry: Arc<dyn baiji_telemetry::Telemetry>,
}

/// usage 锚点（会话内存态，不持久化——重放后首个 run 重建）
#[derive(Debug, Clone, Copy)]
struct UsageAnchor {
    /// 厂商上报的 input + output token
    tokens: usize,
    /// 上报时的会话消息数
    message_count: usize,
    /// 上报时的历史版本
    version: u64,
}

impl AgentHarness {
    /// 创建新会话（不归属项目——测试与兼容路径）
    pub fn new(runtime: Arc<AgentRuntime>, store_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::new_with_project(runtime, store_dir, None)
    }

    /// 创建归属到项目的会话（project = project_key(workdir)，main 装配用）
    pub fn new_with_project(
        runtime: Arc<AgentRuntime>,
        store_dir: impl Into<PathBuf>,
        project: Option<String>,
    ) -> Result<Self> {
        let store = JsonlStore::new(store_dir);
        let mut session = Session::new(None);
        session.meta.project = project;
        store.append(
            &session.meta.id,
            &Record::Started {
                meta: session.meta.clone(),
            },
        )?;
        Ok(Self {
            runtime,
            store,
            session,
            base_prompt: DEFAULT_PROMPT.to_string(),
            skills_prompt: None,
            templates: Vec::new(),
            compaction: CompactionPolicy::default(),
            last_context_tokens: None,
            llm_compaction: false,
            memory: None,
            ctx_store: None,
            todos: None,
            usage_anchor: None,
            history_version: 0,
            telemetry: Arc::new(NoopTelemetry),
        })
    }

    /// 恢复已有会话
    pub fn load(
        runtime: Arc<AgentRuntime>,
        store_dir: impl Into<PathBuf>,
        session_id: &str,
    ) -> Result<Self> {
        let store = JsonlStore::new(store_dir);
        let session = store.load(session_id)?;
        Ok(Self {
            runtime,
            store,
            session,
            base_prompt: DEFAULT_PROMPT.to_string(),
            skills_prompt: None,
            templates: Vec::new(),
            compaction: CompactionPolicy::default(),
            last_context_tokens: None,
            llm_compaction: false,
            memory: None,
            ctx_store: None,
            todos: None,
            usage_anchor: None,
            history_version: 0,
            telemetry: Arc::new(NoopTelemetry),
        })
    }

    /// 从当前会话分叉：新会话继承全部历史，parent 指向原会话
    pub fn branch(&mut self) -> Result<()> {
        self.branch_rewind(0).map(|_| ())
    }

    /// 从历史中的某个点分叉：丢掉最近 `turns_back` 轮（一轮 = 一条 user 消息及其后的
    /// 全部回复/工具往来），其余历史继承到新会话。`0` = 继承全部。
    /// 原会话不受影响——可随时切回去，这就是"回到那次提问之前重来"。
    /// 返回被丢弃的那条 user 消息（便于 UI 回填输入框供修改后重发）。
    pub fn branch_rewind(&mut self, turns_back: usize) -> Result<Option<String>> {
        let messages = &self.session.messages;
        let mut keep = messages.len();
        let mut dropped_input = None;
        if turns_back > 0 {
            let user_positions: Vec<usize> = messages
                .iter()
                .enumerate()
                .filter(|(_, m)| m.role == baiji_ai::Role::User)
                .map(|(i, _)| i)
                .collect();
            if turns_back > user_positions.len() {
                anyhow::bail!(
                    "只有 {} 轮对话，无法回退 {turns_back} 轮",
                    user_positions.len()
                );
            }
            // 在 user 消息边界切：不会拆开 tool_use 与 tool_result
            keep = user_positions[user_positions.len() - turns_back];
            dropped_input = Some(messages[keep].content.clone());
        }

        let mut child = Session::new(Some(self.session.meta.id.clone()));
        child.messages = messages[..keep].to_vec();
        child.meta.project = self.session.meta.project.clone(); // 分叉继承项目
        // 任务清单随分叉继承（父会话不受影响）
        child.todos = self.session.todos.clone();
        // 标题随 Started 一起写入（分叉时历史已知）
        child.meta.title = self.session.meta.title.clone();
        child.derive_title();
        self.store.append(
            &child.meta.id,
            &Record::Started {
                meta: child.meta.clone(),
            },
        )?;
        for message in &child.messages {
            self.store.append(
                &child.meta.id,
                &Record::Message {
                    message: message.clone(),
                },
            )?;
        }
        self.session = child;
        self.last_context_tokens = None; // 历史变短了，旧读数作废
        self.invalidate_usage_anchor();
        info!(
            "branched session -> {} (rewound {turns_back} turn(s))",
            self.session.meta.id
        );
        Ok(dropped_input)
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 当前会话归属的项目（旧会话为 None）
    pub fn current_project(&self) -> Option<String> {
        self.session.meta.project.clone()
    }

    /// 热切换 Provider（TUI 配置变更时，下一次 LLM 调用生效）
    pub fn swap_provider(&self, provider: Arc<dyn baiji_ai::Provider>) {
        self.runtime.swap_provider(provider);
    }

    pub fn set_base_prompt(&mut self, prompt: impl Into<String>) {
        self.base_prompt = prompt.into();
    }

    pub fn set_skills(&mut self, skills: &[Skill]) {
        self.skills_prompt = skills::skills_section(skills);
    }

    /// 按模型上下文窗口重设压缩阈值（启动与热切换模型时调用）
    pub fn set_context_window(&mut self, context_length: u64) {
        self.compaction = CompactionPolicy {
            keep_recent_turns: self.compaction.keep_recent_turns,
            ..CompactionPolicy::for_context(context_length, self.runtime.max_tokens())
        };
        // 运行中的预算比轮次间压缩阈值（70%）宽：85% 窗口再扣输出预留
        let in_run = ((context_length as f64 * 0.85) as u64)
            .saturating_sub(self.runtime.max_tokens() as u64)
            .max(16_000);
        self.runtime.set_context_budget(in_run as usize);
    }

    pub fn set_compaction_policy(&mut self, policy: CompactionPolicy) {
        self.compaction = policy;
    }

    /// 启用/禁用 LLM 压缩摘要（默认关闭 = 确定性摘要）
    pub fn set_llm_compaction(&mut self, enabled: bool) {
        self.llm_compaction = enabled;
    }

    /// 启用跨会话项目记忆：有效条目注入系统提示
    pub fn set_memory(&mut self, store: Arc<MemoryStore>, project: impl Into<String>) {
        self.memory = Some((store, project.into()));
    }

    /// 启用历史 tool result 的 stub 化（与 expand 工具共用同一 ctx store）
    pub fn set_ctx_store(&mut self, dir: impl Into<PathBuf>) {
        self.ctx_store = Some(dir.into());
    }

    /// 启用任务清单（与 TodoTool 共享同一存储）。
    /// 存储内容以会话当前状态为准：resume 会话后清单经此恢复进共享存储
    pub fn set_todos(&mut self, store: Arc<TodoStore>) {
        store.replace(self.session.todos.clone());
        self.todos = Some(store);
    }

    /// 是否有未完成任务（自动接力的判定条件）
    pub fn has_open_todos(&self) -> bool {
        self.todos.as_ref().is_some_and(|t| t.has_open())
    }

    /// 当前任务清单快照（UI 悬浮面板渲染用；未配置时为空）
    pub fn todos_snapshot(&self) -> Vec<TodoItem> {
        self.todos.as_ref().map(|t| t.items()).unwrap_or_default()
    }

    /// 版本与消息数均吻合的有效锚点（否则 None = 用全量字符估算）
    fn valid_usage_anchor(&self) -> Option<UsageAnchor> {
        self.usage_anchor.filter(|a| {
            a.version == self.history_version && a.message_count <= self.session.messages.len()
        })
    }

    /// 历史被重写（压缩/stub/切换/分叉）后调用：锚点作废，版本递增
    fn invalidate_usage_anchor(&mut self) {
        self.usage_anchor = None;
        self.history_version += 1;
    }

    /// 列出存储中的全部会话（供 UI 选择器）
    pub fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        self.store.list()
    }

    /// 切换到已有会话（历史从 JSONL 重放）
    pub fn switch_session(&mut self, session_id: &str) -> Result<()> {
        self.session = self.store.load(session_id)?;
        self.invalidate_usage_anchor(); // 历史整体替换
        // 任务清单跟随切换（TodoTool 共享同一存储，随即看到新状态）
        if let Some(todos) = &self.todos {
            todos.replace(self.session.todos.clone());
        }
        info!("switched to session {session_id}");
        Ok(())
    }

    /// 开新会话（同一项目、同一 provider/配置）；当前会话完整保留在磁盘。
    /// 返回新会话 id
    pub fn start_new_session(&mut self) -> Result<String> {
        let mut session = Session::new(None);
        session.meta.project = self.session.meta.project.clone();
        let id = session.meta.id.clone();
        self.store.append(
            &id,
            &Record::Started {
                meta: session.meta.clone(),
            },
        )?;
        self.session = session;
        self.last_context_tokens = None;
        self.invalidate_usage_anchor();
        if let Some(todos) = &self.todos {
            todos.replace(Vec::new());
        }
        info!("started new session {id}");
        Ok(id)
    }

    /// 手动压缩当前会话上下文（/compact）：强制执行两级收缩（可逆 stub 化 +
    /// 摘要压缩）并落盘 Summary 记录。返回 (stub 条数, 摘要)——历史不足时摘要为 None
    pub async fn compact_now(&mut self) -> (usize, Option<String>) {
        let policy = CompactionPolicy {
            max_estimated_tokens: 0, // 强制：等同 run() 中超预算的强制压缩
            ..self.compaction.clone()
        };
        let (stubbed, _) = stub::stub_tool_results(
            &mut self.session.messages,
            self.ctx_store.as_deref(),
            &policy,
        );
        if stubbed > 0 {
            self.invalidate_usage_anchor();
        }
        let compacted = if self.llm_compaction {
            compact_with_llm(
                self.runtime.provider().as_ref(),
                &mut self.session.messages,
                &policy,
            )
            .await
        } else {
            compact(&mut self.session.messages, &policy)
        };
        if let Some(summary) = &compacted {
            self.last_context_tokens = None;
            self.invalidate_usage_anchor();
            let kept_messages = Some(self.session.messages.len().saturating_sub(1));
            if let Err(e) = self.store.append(
                &self.session.meta.id,
                &Record::Summary {
                    content: summary.clone(),
                    kept_messages,
                },
            ) {
                warn!("persist compaction summary failed: {e}");
            }
        }
        (stubbed, compacted)
    }

    pub fn set_telemetry(&mut self, telemetry: Arc<dyn baiji_telemetry::Telemetry>) {
        self.telemetry = telemetry;
    }

    /// 热切换思考级别（TUI /thinking；下一次请求生效）
    pub fn set_thinking(&self, thinking: Option<baiji_ai::ThinkingLevel>) {
        self.runtime.set_thinking(thinking);
    }

    /// 当前思考级别
    pub fn thinking_level(&self) -> Option<baiji_ai::ThinkingLevel> {
        self.runtime.thinking()
    }

    pub fn set_templates(&mut self, templates: Vec<templates::PromptTemplate>) {
        self.templates = templates;
    }

    /// 已加载的用户 prompt 模板：(调用名, 描述)
    pub fn template_list(&self) -> Vec<(String, String)> {
        self.templates
            .iter()
            .map(|t| (t.name.clone(), t.description.clone()))
            .collect()
    }

    /// 模板可用的环境变量
    fn template_vars(&self) -> std::collections::HashMap<&'static str, String> {
        let mut vars = std::collections::HashMap::new();
        vars.insert(
            "cwd",
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        vars.insert("date", chrono::Local::now().format("%Y-%m-%d").to_string());
        vars.insert("os", std::env::consts::OS.to_string());
        vars.insert("model", self.runtime.provider().model().to_string());
        vars
    }

    fn system_prompt(&self) -> String {
        // 基础 prompt（内置或用户的 system.md）是模板：{{cwd}} {{date}} {{os}} {{model}}
        let mut prompt = templates::render(&self.base_prompt, &self.template_vars());
        if let Some(skills) = &self.skills_prompt {
            prompt.push_str("\n\n");
            prompt.push_str(skills);
        }
        if let Some((store, project)) = &self.memory {
            if let Some(section) = memory_section(&store.active(project)) {
                prompt.push_str("\n\n");
                prompt.push_str(&section);
            }
        }
        // 任务清单：系统提示永不参与压缩——这是状态外置的关键性质
        if let Some(todos) = &self.todos
            && let Some(section) = todo_section(&todos.items())
        {
            prompt.push_str("\n\n");
            prompt.push_str(&section);
        }
        prompt
    }

    /// 执行一轮对话：持久化 → 压缩 → runtime 循环 → 持久化新增消息
    pub async fn run(
        &mut self,
        user_input: impl Into<String>,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        steering: &SteeringQueue,
    ) -> Result<String> {
        let user_input = user_input.into();
        // `/name 参数` 形式且 name 是已加载的 prompt 模板 → 展开为模板正文
        let user_input =
            templates::expand_invocation(&user_input, &self.templates, &self.template_vars())
                .unwrap_or(user_input);

        // 1. 记录用户输入
        self.session.messages.push(Message::user(&user_input));
        self.store.append(
            &self.session.meta.id,
            &Record::Message {
                message: Message::user(&user_input),
            },
        )?;

        // 标题：首条用户消息确定后落盘一次（会话列表据此显示，而不是"(无标题)"）
        if self.session.meta.title.is_none() {
            self.session.derive_title();
            if let Some(title) = self.session.meta.title.clone() {
                self.store
                    .append(&self.session.meta.id, &Record::Title { title })?;
            }
        }

        // 2. 上下文压缩（超限时旧轮次折叠为摘要；会缩短消息列表，须其后取 checkpoint）
        let run_span = self.telemetry.span("harness.run", vec![]);
        // usage 锚定：锚点可用时，触发判定按「字符估算 + 偏移」进行，
        // 其中偏移 = 真实占用 − 锚点前字符估算（系统提示/工具定义/tokenizer
        // 差异）。预算按偏移收紧后，stub/compact 内部的循环判据自动等价于
        // 真实占用 + 尾部估算 ≤ 预算。锚点在历史被重写后失效（版本不符）。
        let anchored = self.valid_usage_anchor();
        let offset = anchored
            .and_then(|a| {
                compaction::anchored_estimate(&self.session.messages, a.tokens, a.message_count)
            })
            .map(|(_, offset)| offset)
            .unwrap_or(0);
        let anchored_policy = CompactionPolicy {
            max_estimated_tokens: self.compaction.max_estimated_tokens.saturating_sub(offset),
            ..self.compaction.clone()
        };
        // 2a. 先尝试 stub 化：保留窗口外的大体积 tool result 换成 ctx 句柄引用
        //     （可逆、零 LLM 成本）；预算已含锚定偏移
        let (stubbed, stub_saved) = stub::stub_tool_results(
            &mut self.session.messages,
            self.ctx_store.as_deref(),
            &anchored_policy,
        );
        if stubbed > 0 {
            info!(
                "stubbed {stubbed} old tool result(s), ~{stub_saved} tokens saved (reversible via expand)"
            );
            self.invalidate_usage_anchor(); // stub 改写了历史内容
        }
        // 强制判定只信真实值：锚定总量（锚点可用时），或厂商观察值（无锚点时，
        // 如首次 run/失效后）。纯启发式估算不触发强制（与旧行为一致）。
        let estimated_total = compaction::estimate_tokens(&self.session.messages) + offset;
        let force = if anchored.is_some() {
            estimated_total > self.compaction.max_estimated_tokens
        } else {
            self.last_context_tokens
                .is_some_and(|observed| observed > self.compaction.max_estimated_tokens)
        };
        let policy = if force {
            info!(
                "anchored/observed context ~{estimated_total} tokens exceeds budget {}, forcing compaction",
                self.compaction.max_estimated_tokens
            );
            CompactionPolicy {
                max_estimated_tokens: 0,
                ..self.compaction.clone()
            }
        } else {
            anchored_policy.clone()
        };
        let compacted = if self.llm_compaction {
            compact_with_llm(
                self.runtime.provider().as_ref(),
                &mut self.session.messages,
                &policy,
            )
            .await
        } else {
            compact(&mut self.session.messages, &policy)
        };
        if let Some(summary) = compacted {
            self.last_context_tokens = None; // 压缩后旧读数作废
            self.invalidate_usage_anchor();
            info!("context compacted (summary {} chars)", summary.len());
            // 压缩后 = [摘要] + 保留的最近消息（含刚写入的 user 输入，均已在文件中）
            let kept_messages = Some(self.session.messages.len().saturating_sub(1));
            self.store.append(
                &self.session.meta.id,
                &Record::Summary {
                    content: summary,
                    kept_messages,
                },
            )?;
        }
        let checkpoint = self.session.messages.len();

        // 3. runtime 循环（新增消息直接追加进 session.messages）。
        //    事件经 tap 转发：统计工具输出的压缩台账（Context IR 摘要）
        let (tap_tx, mut tap_rx) =
            tokio::sync::mpsc::unbounded_channel::<baiji_agent::AgentEvent>();
        let events_out = events.clone();
        let store = self.store.clone();
        let session_id = self.session.meta.id.clone();
        let forward = tokio::spawn(async move {
            let (mut calls, mut original, mut delivered) = (0u32, 0u64, 0u64);
            let (mut original_tokens, mut delivered_tokens) = (0u64, 0u64);
            // 已增量落盘的消息数；一旦写失败就停止，剩余的留给运行结束后补写（保持顺序）
            let mut persisted = 0usize;
            let mut persist_ok = true;
            let mut context_tokens: Option<usize> = None;
            while let Some(event) = tap_rx.recv().await {
                if let baiji_agent::AgentEvent::MessageCommitted { message } = event {
                    if persist_ok {
                        match store.append(&session_id, &Record::Message { message }) {
                            Ok(()) => persisted += 1,
                            Err(e) => {
                                tracing::warn!("incremental persist failed: {e}");
                                persist_ok = false;
                            }
                        }
                    }
                    continue; // 内部事件，不转发给 UI
                }
                if let baiji_agent::AgentEvent::UsageReported {
                    input_tokens,
                    output_tokens,
                } = &event
                {
                    context_tokens = Some((*input_tokens as usize) + (*output_tokens as usize));
                }
                if let baiji_agent::AgentEvent::ToolFinished {
                    output,
                    original_bytes,
                    original_tokens: orig_tokens,
                    ..
                } = &event
                {
                    calls += 1;
                    let delivered_bytes = output.len() as u64;
                    let d_tokens = baiji_agent::estimate_text_tokens(output) as u64;
                    delivered += delivered_bytes;
                    original += original_bytes.unwrap_or(delivered_bytes);
                    delivered_tokens += d_tokens;
                    original_tokens += orig_tokens.unwrap_or(d_tokens);
                }
                if events_out.send(event).is_err() {
                    break;
                }
            }
            (
                calls,
                original,
                delivered,
                persisted,
                context_tokens,
                original_tokens,
                delivered_tokens,
            )
        });

        let answer = self
            .runtime
            .run(
                &self.system_prompt(),
                &mut self.session.messages,
                &tap_tx,
                cancel,
                steering,
            )
            .await;
        drop(tap_tx);
        let (
            tool_calls,
            original_bytes,
            delivered_bytes,
            persisted,
            context_tokens,
            original_tokens,
            delivered_tokens,
        ) = forward.await.unwrap_or((0, 0, 0, 0, None, 0, 0));
        if let Some(observed) = context_tokens {
            self.last_context_tokens = context_tokens;
            // usage 锚点：消息此刻已同步完（含本次 run 的新增），真实占用代表
            // 锚点前历史；最终答案属 output token，亦已计入
            self.usage_anchor = Some(UsageAnchor {
                tokens: observed,
                message_count: self.session.messages.len(),
                version: self.history_version,
            });
        }

        // 4. 补写尚未增量落盘的新增消息（无论成败都保留进度）
        let unpersisted = (checkpoint + persisted).min(self.session.messages.len());
        for message in &self.session.messages[unpersisted..] {
            self.store.append(
                &self.session.meta.id,
                &Record::Message {
                    message: message.clone(),
                },
            )?;
        }
        // 任务清单有变更则落盘快照（run 结束时机：会话 id 稳定，无并发问题）
        if let Some(todos) = &self.todos
            && todos.take_dirty()
        {
            self.store.append(
                &self.session.meta.id,
                &Record::Todo {
                    items: todos.items(),
                },
            )?;
        }
        // 台账（有工具调用才记录）
        if tool_calls > 0 {
            self.store.append(
                &self.session.meta.id,
                &Record::Ledger {
                    tool_calls,
                    original_bytes,
                    delivered_bytes,
                    original_tokens,
                    delivered_tokens,
                },
            )?;
            info!(
                "context ledger: {tool_calls} tool calls, {} -> {} bytes ({:.1}% saved), \
                 ~{} -> ~{} tokens ({:.1}% saved)",
                original_bytes,
                delivered_bytes,
                if original_bytes > 0 {
                    original_bytes.saturating_sub(delivered_bytes) as f64 / original_bytes as f64
                        * 100.0
                } else {
                    0.0
                },
                original_tokens,
                delivered_tokens,
                if original_tokens > 0 {
                    original_tokens.saturating_sub(delivered_tokens) as f64 / original_tokens as f64
                        * 100.0
                } else {
                    0.0
                },
            );
        }
        run_span.end();

        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use baiji_agent::ToolRegistry;
    use baiji_ai::{ChatRequest, ChatResponse, Protocol, Provider, StreamChunk, TokenUsage};
    use futures::StreamExt as _;
    use futures::stream::BoxStream;

    /// 固定回答的 Mock Provider
    struct EchoProvider;

    #[async_trait]
    impl Provider for EchoProvider {
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<BoxStream<'static, Result<StreamChunk>>> {
            Ok(futures::stream::iter(vec![
                Ok(StreamChunk::Content("收到：".into())),
                Ok(StreamChunk::Done),
            ])
            .boxed())
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

    /// 上报固定 usage 的 Mock Provider（usage 锚定测试用）
    struct UsageProvider {
        input_tokens: u32,
    }

    #[async_trait]
    impl Provider for UsageProvider {
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!("runtime uses chat_stream")
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<StreamChunk>>> {
            Ok(futures::stream::iter(vec![
                Ok(StreamChunk::Usage(TokenUsage {
                    input_tokens: self.input_tokens,
                    output_tokens: 10,
                })),
                Ok(StreamChunk::Content("ok".into())),
                Ok(StreamChunk::Done),
            ])
            .boxed())
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

    /// chat() 返回固定摘要的 Mock Provider（LLM 压缩测试用）
    struct SummarizeProvider {
        fail_chat: bool,
    }

    #[async_trait]
    impl Provider for SummarizeProvider {
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse> {
            if self.fail_chat {
                anyhow::bail!("summarize API down")
            }
            Ok(ChatResponse {
                content: "· 用户在测试压缩\n· 决定使用 LLM 摘要".to_string(),
                tool_calls: None,
                usage: None,
            })
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<BoxStream<'static, Result<StreamChunk>>> {
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

    #[tokio::test]
    async fn test_harness_run_persists_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");

        let session_id = {
            let runtime = Arc::new(AgentRuntime::new(Arc::new(EchoProvider)));
            let mut harness =
                AgentHarness::new(runtime, store_dir.clone()).expect("create harness");

            assert!(harness.session().messages.is_empty());

            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let answer = harness
                .run(
                    "你好",
                    &tx,
                    &CancellationToken::new(),
                    &SteeringQueue::new(),
                )
                .await
                .expect("run");
            assert_eq!(answer, "收到：");

            // 内存态：user + assistant
            assert_eq!(harness.session().messages.len(), 2);
            harness.session().meta.id.clone()
        };

        // JSONL 回放恢复
        let runtime = Arc::new(AgentRuntime::new(Arc::new(EchoProvider)));
        let reloaded = AgentHarness::load(runtime, store_dir, &session_id).unwrap();
        assert_eq!(reloaded.session().messages.len(), 2);
        assert_eq!(reloaded.session().messages[0].content, "你好");
        assert_eq!(reloaded.session().messages[1].content, "收到：");
        assert!(reloaded.session().meta.title.as_deref().is_some());
    }

    #[tokio::test]
    async fn test_branch_inherits_history() {
        let dir = tempfile::tempdir().unwrap();
        let runtime =
            Arc::new(AgentRuntime::new(Arc::new(EchoProvider)).with_tools(ToolRegistry::new()));
        let mut harness = AgentHarness::new(runtime, dir.path().join("sessions")).unwrap();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .run(
                "first question",
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();

        let parent_id = harness.session().meta.id.clone();
        let history_len = harness.session().messages.len();

        harness.branch().unwrap();
        assert_eq!(
            harness.session().meta.parent_id.as_deref(),
            Some(parent_id.as_str())
        );
        assert_eq!(harness.session().messages.len(), history_len);

        // 分叉后继续对话，互不影响（各自 JSONL 独立）
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .run(
                "branched question",
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        assert_eq!(harness.session().messages.len(), history_len + 2);
    }

    #[tokio::test]
    async fn test_branch_rewind_and_title_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let runtime =
            Arc::new(AgentRuntime::new(Arc::new(EchoProvider)).with_tools(ToolRegistry::new()));
        let mut harness = AgentHarness::new(runtime, &sessions).unwrap();

        for question in ["q1 about alpha", "q2", "q3"] {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            harness
                .run(
                    question,
                    &tx,
                    &CancellationToken::new(),
                    &SteeringQueue::new(),
                )
                .await
                .unwrap();
        }
        let parent_id = harness.session().meta.id.clone();
        assert_eq!(harness.session().messages.len(), 6);

        // 回退 2 轮：只剩 q1 及其回答；返回被丢弃的那条提问
        let dropped = harness.branch_rewind(2).unwrap();
        assert_eq!(dropped.as_deref(), Some("q2"));
        assert_eq!(harness.session().messages.len(), 2);
        assert_eq!(
            harness.session().meta.parent_id.as_deref(),
            Some(parent_id.as_str())
        );
        assert!(
            harness.branch_rewind(5).is_err(),
            "cannot rewind past the start"
        );

        // 原会话完好；两个会话的标题都已落盘，列表不再是"(无标题)"
        let store = JsonlStore::new(&sessions);
        assert_eq!(store.load(&parent_id).unwrap().messages.len(), 6);
        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 2);
        assert!(
            metas
                .iter()
                .all(|m| m.title.as_deref() == Some("q1 about alpha"))
        );
    }

    #[tokio::test]
    async fn test_memory_injected_into_run() {
        /// 捕获 system prompt 的 Mock
        struct CaptureProvider(std::sync::Mutex<Vec<String>>);

        #[async_trait]
        impl Provider for CaptureProvider {
            async fn chat(&self, _: ChatRequest) -> Result<ChatResponse> {
                unreachable!()
            }
            async fn chat_stream(
                &self,
                request: ChatRequest,
            ) -> Result<BoxStream<'static, Result<StreamChunk>>> {
                let system = request
                    .messages
                    .iter()
                    .find(|m| m.role == baiji_ai::Role::System)
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                self.0.lock().unwrap().push(system);
                Ok(futures::stream::iter(vec![
                    Ok(StreamChunk::Content("ok".into())),
                    Ok(StreamChunk::Done),
                ])
                .boxed())
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

        let captured = Arc::new(CaptureProvider(std::sync::Mutex::new(Vec::new())));
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path().join("mem")));
        store
            .add(
                "proj",
                "构建前必须 cargo fmt",
                MemoryKind::Gotcha,
                None,
                "agent",
            )
            .unwrap();

        let runtime = Arc::new(AgentRuntime::new(captured.clone()).with_tools(ToolRegistry::new()));
        let mut harness = AgentHarness::new(runtime, dir.path().join("sessions")).unwrap();
        harness.set_memory(store, "proj");

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .run("hi", &tx, &CancellationToken::new(), &SteeringQueue::new())
            .await
            .unwrap();

        let systems = captured.0.lock().unwrap().clone();
        assert_eq!(systems.len(), 1);
        assert!(systems[0].contains("Project memory"), "{}", systems[0]);
        assert!(systems[0].contains("构建前必须 cargo fmt"));
        assert!(systems[0].contains("gotcha"));
    }

    #[tokio::test]
    async fn test_list_and_switch_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");
        let runtime =
            Arc::new(AgentRuntime::new(Arc::new(EchoProvider)).with_tools(ToolRegistry::new()));

        // 会话 A：一轮对话
        let mut harness_a = AgentHarness::new(Arc::clone(&runtime), store_dir.clone()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness_a
            .run(
                "question in A",
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        let id_a = harness_a.session().meta.id.clone();
        drop(harness_a);

        // 会话 B：另一轮
        let mut harness_b = AgentHarness::new(Arc::clone(&runtime), store_dir.clone()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness_b
            .run(
                "question in B",
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        let _id_b = harness_b.session().meta.id.clone();

        // 列表包含两个会话
        let sessions = harness_b.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);

        // 切回 A：历史恢复
        harness_b.switch_session(&id_a).unwrap();
        let contents: Vec<&str> = harness_b
            .session()
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(contents.contains(&"question in A"));
        assert_eq!(harness_b.session().meta.id, id_a);

        // 切换到不存在的会话报错
        assert!(harness_b.switch_session("sess_nope").is_err());
    }

    #[tokio::test]
    async fn test_llm_compaction_summarizes_via_provider() {
        let provider = SummarizeProvider { fail_chat: false };
        let mut messages = Vec::new();
        for i in 0..8 {
            messages.push(Message::user(format!(
                "问题 {} 一些足够长的内容用于触发 token 超限估计",
                i + 1
            )));
            messages.push(Message::assistant("简短回答"));
        }

        let summary = compact_with_llm(
            &provider,
            &mut messages,
            &CompactionPolicy {
                max_estimated_tokens: 100,
                keep_recent_turns: 2,
            },
        )
        .await
        .expect("should compact");

        assert!(summary.contains("LLM 摘要"), "LLM summary used: {summary}");
        assert!(messages[0].content.contains("[Conversation Summary]"));
        // 最近 2 轮保持原文
        assert!(messages.last().unwrap().content.contains("简短回答"));
    }

    #[tokio::test]
    async fn test_llm_compaction_falls_back_on_provider_error() {
        let messages = {
            let mut m = Vec::new();
            for i in 0..6 {
                m.push(Message::user(format!("问题 {} 内容", i + 1)));
                m.push(Message::assistant("回答"));
            }
            m
        };
        let mut messages = messages;
        let provider = SummarizeProvider { fail_chat: true };
        let summary = compact_with_llm(
            &provider,
            &mut messages,
            &CompactionPolicy {
                max_estimated_tokens: 50,
                keep_recent_turns: 2,
            },
        )
        .await
        .expect("should still compact");

        // 回退到确定性摘要
        assert!(summary.contains("Turn 1:"), "fallback summary: {summary}");
    }

    #[tokio::test]
    async fn test_run_stubs_instead_of_summarizing_when_store_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");
        let ctx_store = dir.path().join("ctx");

        let runtime = Arc::new(AgentRuntime::new(Arc::new(EchoProvider)));
        let mut harness = AgentHarness::new(runtime, store_dir.clone()).unwrap();
        harness.set_compaction_policy(CompactionPolicy {
            max_estimated_tokens: 2_000,
            keep_recent_turns: 2,
        });
        harness.set_ctx_store(ctx_store);

        // 预置大历史：8 轮，每轮带 1.2KB 工具结果（约 2.6k token > 2k 预算）
        for i in 0..8 {
            harness
                .session
                .messages
                .push(Message::user(format!("question {i}")));
            harness
                .session
                .messages
                .push(Message::assistant(format!("answer {i}")));
            harness.session.messages.push(Message {
                role: baiji_ai::Role::Tool,
                content: String::new(),
                tool_calls: None,
                tool_results: Some(vec![baiji_ai::ToolResult {
                    tool_call_id: format!("t{i}"),
                    content: "x".repeat(1200),
                }]),
                reasoning: None,
            });
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .run("go", &tx, &CancellationToken::new(), &SteeringQueue::new())
            .await
            .unwrap();
        drop(tx);
        while (rx.try_recv()).is_ok() {}

        // 旧轮次的大结果被 stub 化（带句柄），最近轮次原样
        let contents: Vec<String> = harness
            .session
            .messages
            .iter()
            .filter_map(|m| m.tool_results.clone())
            .flat_map(|rs| rs.into_iter().map(|r| r.content))
            .collect();
        assert!(
            contents.iter().any(|c| c.starts_with(stub::STUB_PREFIX)),
            "expected stubbed results"
        );
        assert_eq!(contents.last().unwrap(), &"x".repeat(1200));

        // stub 已把估算拉回预算内 → 无 Summary 记录（历史可逆，而非折叠）
        let file = store_dir.join(format!("{}.jsonl", harness.session().meta.id));
        let jsonl = std::fs::read_to_string(&file).unwrap();
        assert!(!jsonl.contains("\"summary\""), "{jsonl}");
    }

    #[test]
    fn test_should_auto_continue_truth_table() {
        let yes = should_auto_continue(true, 10, true, 5, true);
        assert!(yes);
        // 任一条件不满足即停
        assert!(!should_auto_continue(false, 10, true, 5, true)); // 开关关
        assert!(!should_auto_continue(true, 10, false, 5, true)); // todo 全完成
        assert!(!should_auto_continue(true, 10, true, 10, true)); // 轮次触顶
        assert!(!should_auto_continue(true, 10, true, 11, true)); // 超上限
        assert!(!should_auto_continue(true, 10, true, 5, false)); // 被中断
    }

    // ===== usage 锚定（T2）=====

    #[tokio::test]
    async fn test_usage_anchor_drives_compaction_and_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");

        // 第一次 run：上报巨大 usage（99_999），而历史的字符估算很小
        let runtime = Arc::new(AgentRuntime::new(Arc::new(UsageProvider {
            input_tokens: 99_999,
        })));
        let mut harness = AgentHarness::new(runtime, store_dir.clone()).unwrap();
        harness.set_compaction_policy(CompactionPolicy {
            max_estimated_tokens: 48_000,
            keep_recent_turns: 1,
        });
        for (q, a) in [("q1", "a1"), ("q2", "a2")] {
            harness.session.messages.push(Message::user(q));
            harness.session.messages.push(Message::assistant(a));
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .run("go", &tx, &CancellationToken::new(), &SteeringQueue::new())
            .await
            .unwrap();

        // 锚点已记录：消息数 = 4 预置 + "go" + 答案 = 6，版本 0
        let anchor = harness.usage_anchor.expect("anchor recorded after usage");
        assert_eq!(anchor.message_count, 6);
        assert_eq!(anchor.tokens, 99_999 + 10);

        // 第二次 run 换 EchoProvider（不再上报 usage）：锚定总量 ≈ 100k > 48k
        // → 必须强制压缩——纯启发式估算远低于预算，旧行为不会压缩
        harness.runtime.swap_provider(Arc::new(EchoProvider));
        harness
            .run(
                "again",
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        assert!(
            harness
                .session
                .messages
                .iter()
                .any(|m| m.content.contains("[Conversation Summary]")),
            "anchored estimate must trigger compaction despite small heuristic"
        );
        // 压缩使锚点失效，且本次 run 无新 usage → 保持失效
        assert!(harness.usage_anchor.is_none());
        assert_eq!(harness.history_version, 1);
    }

    #[tokio::test]
    async fn test_usage_anchor_invalidated_by_session_switch() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");

        let runtime = Arc::new(AgentRuntime::new(Arc::new(UsageProvider {
            input_tokens: 500,
        })));
        let mut harness = AgentHarness::new(runtime.clone(), store_dir.clone()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .run("go", &tx, &CancellationToken::new(), &SteeringQueue::new())
            .await
            .unwrap();
        assert!(harness.usage_anchor.is_some());
        let first_id = harness.session().meta.id.clone();

        // 同库另建一个会话并切换：锚点失效（历史整体替换）
        let mut other = AgentHarness::new(runtime, store_dir.clone()).unwrap();
        let other_id = other.session().meta.id.clone();
        drop(other);
        harness.switch_session(&other_id).unwrap();
        assert!(harness.usage_anchor.is_none());
        assert_eq!(harness.history_version, 1);

        // 切回原会话：可重新建立锚点
        harness.switch_session(&first_id).unwrap();
        harness
            .run(
                "back",
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        assert!(harness.usage_anchor.is_some());
    }

    // ===== 任务清单（todo）=====

    #[tokio::test]
    async fn test_new_session_records_project_and_branch_inherits() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");
        let runtime = Arc::new(AgentRuntime::new(Arc::new(EchoProvider)));

        // 归属项目的会话:Started 落盘带 project,load 恢复
        let mut harness = AgentHarness::new_with_project(
            runtime.clone(),
            store_dir.clone(),
            Some("demo-ab12cd34".into()),
        )
        .unwrap();
        assert_eq!(harness.current_project().as_deref(), Some("demo-ab12cd34"));
        let id = harness.session().meta.id.clone();
        let reloaded = AgentHarness::load(runtime.clone(), store_dir.clone(), &id).unwrap();
        assert_eq!(
            reloaded.current_project().as_deref(),
            Some("demo-ab12cd34"),
            "project must survive reload (resume)"
        );

        // 分叉继承项目
        harness.branch().unwrap();
        assert_eq!(harness.current_project().as_deref(), Some("demo-ab12cd34"));
    }

    #[tokio::test]
    async fn test_todo_mutations_persisted_and_restored() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");
        let todos = Arc::new(TodoStore::new());

        let runtime = Arc::new(AgentRuntime::new(Arc::new(EchoProvider)));
        let mut harness = AgentHarness::new(runtime, store_dir.clone()).unwrap();
        // set_todos 以会话状态为准（新会话为空）——工具在接线之后变更
        harness.set_todos(todos.clone());
        assert!(!harness.has_open_todos());

        todos.add("step one".into());
        todos
            .update(
                1,
                Some(TodoStatus::InProgress),
                None,
                Some(Some("wip".into())),
            )
            .unwrap();
        todos.add("step two".into());
        assert!(harness.has_open_todos());

        // run 结束时变更落盘
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .run("go", &tx, &CancellationToken::new(), &SteeringQueue::new())
            .await
            .unwrap();
        let file = store_dir.join(format!("{}.jsonl", harness.session().meta.id));
        let jsonl = std::fs::read_to_string(&file).unwrap();
        assert!(jsonl.contains("\"todo\""), "{jsonl}");

        // resume：新 harness + 新存储，清单从重放恢复
        let runtime2 = Arc::new(AgentRuntime::new(Arc::new(EchoProvider)));
        let mut restored =
            AgentHarness::load(runtime2, store_dir, &harness.session().meta.id).unwrap();
        let fresh = Arc::new(TodoStore::new());
        restored.set_todos(fresh.clone());
        let items = fresh.items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].status, TodoStatus::InProgress);
        assert_eq!(items[0].note.as_deref(), Some("wip"));

        // 系统提示包含清单（注入可见）
        let prompt = restored.system_prompt();
        assert!(prompt.contains("## Task list"), "{prompt}");
        assert!(prompt.contains("[~] 1. step one — wip"), "{prompt}");
        assert!(prompt.contains("[ ] 2. step two"), "{prompt}");
        assert!(restored.has_open_todos());
    }

    #[tokio::test]
    async fn test_todo_survives_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");
        let todos = Arc::new(TodoStore::new());

        let runtime = Arc::new(AgentRuntime::new(Arc::new(EchoProvider)));
        let mut harness = AgentHarness::new(runtime, store_dir.clone()).unwrap();
        harness.set_todos(todos.clone());
        harness.set_compaction_policy(CompactionPolicy {
            max_estimated_tokens: 10,
            keep_recent_turns: 1,
        });

        // 11 项任务 + 足以触发压缩的历史
        todos.add("the long task".into());
        for i in 1..=10 {
            todos.add(format!("step {i}"));
        }
        for i in 0..10 {
            harness
                .session
                .messages
                .push(Message::user(format!("question {i}")));
            harness.session.messages.push(Message::assistant(format!(
                "answer {i} with padding to grow context"
            )));
        }

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .run("go", &tx, &CancellationToken::new(), &SteeringQueue::new())
            .await
            .unwrap();

        // 压缩确实发生了（历史折叠为摘要）
        assert!(
            harness
                .session
                .messages
                .iter()
                .any(|m| m.content.contains("[Conversation Summary]")),
            "compaction should have fired"
        );
        // 但系统提示仍含全部任务项——清单外置于压缩不可及之处
        let prompt = harness.system_prompt();
        assert!(prompt.contains("the long task"), "{prompt}");
        assert!(prompt.contains("[ ] 11. step 10"), "{prompt}");
        assert!(prompt.matches("step ").count() >= 11, "{prompt}");
    }

    // ===== 上下文节省台账（Context IR）=====

    /// 第一轮调用 big 工具、第二轮直接回答的 Mock
    struct ToolCallProvider;

    #[async_trait]
    impl Provider for ToolCallProvider {
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<BoxStream<'static, Result<StreamChunk>>> {
            use std::sync::atomic::{AtomicU32, Ordering};
            static CALLS: AtomicU32 = AtomicU32::new(0);
            let n = CALLS.fetch_add(1, Ordering::SeqCst);
            let chunks: Vec<Result<StreamChunk>> = if n == 0 {
                vec![
                    Ok(StreamChunk::ToolCallStart {
                        id: "t1".into(),
                        name: "big".into(),
                    }),
                    Ok(StreamChunk::ToolCallArguments {
                        id: "t1".into(),
                        arguments: "{}".into(),
                    }),
                    Ok(StreamChunk::Done),
                ]
            } else {
                vec![
                    Ok(StreamChunk::Content("done".into())),
                    Ok(StreamChunk::Done),
                ]
            };
            Ok(futures::stream::iter(chunks).boxed())
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

    struct BigOutputTool;

    #[async_trait]
    impl baiji_agent::AgentTool for BigOutputTool {
        fn name(&self) -> &str {
            "big"
        }
        fn description(&self) -> &str {
            "emits a large compressed output"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<baiji_agent::ToolOutput> {
            // 模拟：原始 10KB/~2600 token 压缩到 100B/~26 token
            Ok(baiji_agent::ToolOutput::ok("x".repeat(100))
                .with_original_bytes(10_000)
                .with_original_tokens(2_600))
        }
    }

    #[tokio::test]
    async fn test_ledger_recorded_and_ignored_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(BigOutputTool));
        let runtime = Arc::new(AgentRuntime::new(Arc::new(ToolCallProvider)).with_tools(tools));
        let mut harness = AgentHarness::new(runtime, store_dir.clone()).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let collector = tokio::spawn(async move {
            let mut saved = 0u64;
            while let Some(event) = rx.recv().await {
                if let baiji_agent::AgentEvent::ToolFinished {
                    output,
                    original_bytes,
                    ..
                } = event
                {
                    saved += original_bytes
                        .unwrap_or(0)
                        .saturating_sub(output.len() as u64);
                }
            }
            saved
        });

        harness
            .run(
                "run the tool",
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        drop(tx);
        // 转发出的事件保留了原始字节数（UI 可统计）
        assert_eq!(collector.await.unwrap(), 10_000 - 100);

        // JSONL 末尾有台账记录（字节 + token 双口径）
        let file = store_dir.join(format!("{}.jsonl", harness.session().meta.id));
        let content = std::fs::read_to_string(&file).unwrap();
        let last = content.lines().last().unwrap();
        assert!(last.contains("\"ledger\""), "last line: {last}");
        assert!(last.contains("\"tool_calls\":1"));
        assert!(last.contains("\"original_tokens\":2600"));
        assert!(last.contains("\"delivered_tokens\":26"));

        // 加载时台账不进入对话历史
        let runtime2 = Arc::new(AgentRuntime::new(Arc::new(EchoProvider)));
        let reloaded = AgentHarness::load(runtime2, store_dir, &harness.session().meta.id).unwrap();
        let roles: Vec<_> = reloaded
            .session()
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(!roles.iter().any(|c| c.contains("ledger")));
    }

    #[tokio::test]
    async fn test_compact_now_forces_reduction_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");
        let runtime = Arc::new(AgentRuntime::new(Arc::new(EchoProvider)));
        let mut harness = AgentHarness::new(runtime, store_dir.clone()).unwrap();
        // 只保留最近 2 轮：4 轮后必有可压缩内容
        harness.set_compaction_policy(CompactionPolicy {
            max_estimated_tokens: 48_000,
            keep_recent_turns: 2,
        });

        let old_id = harness.session().meta.id.clone();
        for i in 0..4 {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            harness
                .run(&format!("q{i}"), &tx, &CancellationToken::new(), &SteeringQueue::new())
                .await
                .unwrap();
        }
        assert!(harness.session().messages.len() >= 8);

        let (stubbed, summary) = harness.compact_now().await;
        assert_eq!(stubbed, 0, "no ctx store: nothing to stub");
        assert!(summary.is_some(), "history beyond keep window must compact");
        // 老轮次折叠成首条 System 摘要，消息数显著减少
        assert_eq!(harness.session().messages[0].role, baiji_ai::Role::System);
        assert!(harness.session().messages.len() < 8);

        // 落盘可重放：重载后首条同样是摘要
        let loaded = JsonlStore::new(store_dir).load(&old_id).unwrap();
        assert!(loaded.messages[0].content.starts_with("[Conversation Summary]"));
    }

    #[tokio::test]
    async fn test_start_new_session_resets_and_keeps_project() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");
        let runtime = Arc::new(AgentRuntime::new(Arc::new(EchoProvider)));
        let mut harness =
            AgentHarness::new_with_project(runtime, store_dir.clone(), Some("proj-xx".to_string()))
                .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .run("q1", &tx, &CancellationToken::new(), &SteeringQueue::new())
            .await
            .unwrap();
        assert_eq!(harness.session().messages.len(), 2);

        let old_id = harness.session().meta.id.clone();
        let todos = Arc::new(TodoStore::new());
        todos.replace(vec![TodoItem {
            id: 1,
            content: "遗留任务".to_string(),
            status: TodoStatus::Pending,
            note: None,
        }]);
        harness.set_todos(todos.clone());

        let new_id = harness.start_new_session().unwrap();
        assert_ne!(new_id, old_id);
        assert!(harness.session().messages.is_empty());
        assert_eq!(
            harness.session().meta.project.as_deref(),
            Some("proj-xx"),
            "project carried over to the new session"
        );
        assert!(todos.items().is_empty(), "shared todo store cleared");

        // 旧会话在磁盘上完好
        let old = JsonlStore::new(store_dir).load(&old_id).unwrap();
        assert_eq!(old.messages.len(), 2);
    }
}
