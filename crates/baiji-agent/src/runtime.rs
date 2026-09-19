//! AgentRuntime — 流式工具调用循环（ReAct）
//!
//! ```text
//! run()
//!   ├─ 注入 steering 消息
//!   ├─ loop (≤ max_turns):
//!   │    ├─ ChatRequest(system + history + tools)
//!   │    ├─ chat_stream（瞬时错误自动重试）
//!   │    │    累积 TextDelta / ToolCallStart / ToolCallArguments
//!   │    ├─ 无工具调用 → 最终答案，结束
//!   │    └─ 有工具调用 → 逐个执行：
//!   │         hooks.on_tool_call（可 Deny）→ execute → hooks.on_tool_result
//!   │         每个工具后检查 steering（发现则跳过剩余工具）
//!   └─ 新增消息（assistant/tool/steering）回写 `messages`
//! ```

use crate::confirmation::{ConfirmationDecision, ConfirmationGate, ConfirmationRequest};
use crate::event::AgentEvent;
use crate::hooks::HookRegistry;
use crate::queue::SteeringQueue;
use crate::tool::{ToolOutput, ToolRegistry};
use anyhow::Result;
use baiji_ai::{
    ChatRequest, Message, Provider, ReasoningBlock, Role, StopReason, StreamChunk, TokenUsage,
    ToolCall, ToolResult,
};
use baiji_telemetry::{AttrValue, NoopTelemetry, Telemetry, attrs};
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Agent 运行时
pub struct AgentRuntime {
    /// 可热替换的 Provider（TUI 内切换厂商/端点时 swap）
    provider: Arc<std::sync::RwLock<Arc<dyn Provider>>>,
    tools: ToolRegistry,
    hooks: HookRegistry,
    confirmation: ConfirmationGate,
    telemetry: Arc<dyn Telemetry>,
    /// 最大 LLM 轮次
    max_turns: u32,
    /// 传给 LLM 的 max_tokens
    max_tokens: u32,
    /// 运行中上下文预算（token；0 = 不限）。单个长轮次内超出时就地精简旧工具结果
    context_budget: std::sync::atomic::AtomicUsize,
    /// 瞬时错误重试次数
    max_retries: u32,
    /// 指数退避基距（base × 2^attempt）
    retry_base_delay: Duration,
    /// 单次退避上限（服务端 Retry-After 同样受约束）
    retry_max_delay: Duration,
    /// verbosity steer：向请求内最后一条 user 消息追加恒定"简洁作答"指令
    /// （请求级注入，不改会话历史；参考 lean-ctx，输出 token 实测可省约三分之一）
    verbosity_steer: bool,
    /// 思考级别（请求级推理强度；RwLock 支持运行中热切换，如 TUI 的 /thinking）。
    /// None = 不发送思考字段（协议默认行为）
    thinking: std::sync::RwLock<Option<baiji_ai::ThinkingLevel>>,
    /// 计划模式（只读规划态；AtomicBool 支持运行中热切换）。
    /// 开启后：发给模型的工具定义过滤为只读白名单，白名单外的调用直接拒绝
    plan_mode: std::sync::atomic::AtomicBool,
    /// spill 能力（可选）：运行中就地精简旧工具结果时，原文写入 ctx store
    /// 返回句柄（可逆）。闭包注入而非依赖 baiji-tools——agent 在依赖图上
    /// 位于 tools 之下，直接依赖会成环
    spill: Option<SpillFn>,
}

/// spill 闭包：内容 → ctx 句柄（main.rs 用 baiji_tools::spill_to_store 构造）
type SpillFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// 计划模式（只读规划态）可用的工具白名单。
/// 探索类只读工具 + 状态外置工具（todo/memory 不进文件系统）+ 子代理
/// （其工具集本身已是只读子集）。白名单外的工具（write/edit/bash 及全部
/// MCP 工具）在计划模式下：定义不发给模型，模型经历史发起的调用直接拒绝。
pub const PLAN_MODE_ALLOWED_TOOLS: &[&str] = &[
    "read", "grep", "find", "ls", "search", "imports", "expand", "task", "todo", "memory", "skill",
    "now",
];

impl AgentRuntime {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider: Arc::new(std::sync::RwLock::new(provider)),
            tools: ToolRegistry::new(),
            hooks: HookRegistry::new(),
            confirmation: ConfirmationGate::default(),
            telemetry: Arc::new(NoopTelemetry),
            max_turns: 24,
            max_tokens: 8192,
            context_budget: std::sync::atomic::AtomicUsize::new(0),
            max_retries: 2,
            retry_base_delay: Duration::from_millis(500),
            retry_max_delay: Duration::from_millis(30_000),
            verbosity_steer: false,
            thinking: std::sync::RwLock::new(None),
            plan_mode: std::sync::atomic::AtomicBool::new(false),
            spill: None,
        }
    }

    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_hooks(mut self, hooks: HookRegistry) -> Self {
        self.hooks = hooks;
        self
    }

    /// 设置 HITL 确认门控（命中的工具执行前需审批）
    pub fn with_confirmation(mut self, confirmation: ConfirmationGate) -> Self {
        self.confirmation = confirmation;
        self
    }

    pub fn with_telemetry(mut self, telemetry: Arc<dyn Telemetry>) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// 单次 LLM 响应的输出上限（对应配置 `max_tokens`）
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens.max(1);
        self
    }

    /// 启用 verbosity steer（配置 `policy.verbosity_steer`）。
    /// 每轮请求向最后一条 user 消息追加恒定的简洁指令
    pub fn with_verbosity_steer(mut self, enabled: bool) -> Self {
        self.verbosity_steer = enabled;
        self
    }

    /// 设置思考级别（对应配置 `thinking`；None = 不启用）
    pub fn with_thinking(mut self, thinking: Option<baiji_ai::ThinkingLevel>) -> Self {
        *self.thinking.write().unwrap() = thinking;
        self
    }

    /// 以计划模式启动（headless `--plan`；运行中仍可用 set_plan_mode 切换）
    pub fn with_plan_mode(self, on: bool) -> Self {
        self.set_plan_mode(on);
        self
    }

    /// 热切换思考级别（TUI /thinking；下一次请求生效）
    pub fn set_thinking(&self, thinking: Option<baiji_ai::ThinkingLevel>) {
        *self.thinking.write().unwrap() = thinking;
    }

    /// 当前思考级别
    pub fn thinking(&self) -> Option<baiji_ai::ThinkingLevel> {
        *self.thinking.read().unwrap()
    }

    /// 热切换计划模式（TUI /plan；下一次请求与工具调用生效）
    pub fn set_plan_mode(&self, on: bool) {
        self.plan_mode
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// 是否处于计划模式（只读规划态）
    pub fn plan_mode(&self) -> bool {
        self.plan_mode.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 发给模型的工具定义：计划模式下只保留只读白名单（模型看不到写工具，
    /// 执行路径另有拒绝兜底）
    fn request_tool_definitions(&self) -> Vec<baiji_ai::ToolDefinition> {
        if self.plan_mode() {
            self.tools
                .definitions()
                .into_iter()
                .filter(|d| PLAN_MODE_ALLOWED_TOOLS.contains(&d.name.as_str()))
                .collect()
        } else {
            self.tools.definitions()
        }
    }

    /// 注入 spill 能力：运行中精简旧工具结果时原文可逆（ctx 句柄 + expand 取回）
    pub fn with_spill(mut self, spill: SpillFn) -> Self {
        self.spill = Some(spill);
        self
    }

    /// 瞬时错误重试策略（配置 `retry` 段）：次数 + 指数退避基距 + 单次上限
    pub fn with_retry(mut self, max_retries: u32, base_delay_ms: u64, max_delay_ms: u64) -> Self {
        self.max_retries = max_retries;
        self.retry_base_delay = Duration::from_millis(base_delay_ms.max(1));
        self.retry_max_delay = Duration::from_millis(max_delay_ms.max(1));
        self
    }

    pub fn with_limits(mut self, max_turns: u32, max_tokens: u32) -> Self {
        self.max_turns = max_turns;
        self.max_tokens = max_tokens;
        self
    }

    /// 设置运行中的上下文预算（token）。可在热切换模型时调用。
    pub fn set_context_budget(&self, tokens: usize) {
        self.context_budget
            .store(tokens, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// 当前 Provider（克隆句柄）
    pub fn provider(&self) -> Arc<dyn Provider> {
        self.provider.read().unwrap().clone()
    }

    /// 热切换 Provider（下一次 LLM 调用生效）
    pub fn swap_provider(&self, provider: Arc<dyn Provider>) {
        *self.provider.write().unwrap() = provider;
    }

    /// 执行一次 Agent 运行。
    ///
    /// - `system_prompt`：系统提示（由 Harness 组装：基础 prompt + skills 等）
    /// - `messages`：持久化会话历史（不含 system；**已包含**本次 user 输入）。
    ///   运行中产生的新消息（steering/assistant/tool）会追加到该列表
    /// - 返回最终答案文本；用户取消时返回空串并发送 [`AgentEvent::Interrupted`]
    pub async fn run(
        &self,
        system_prompt: &str,
        messages: &mut Vec<Message>,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        steering: &SteeringQueue,
    ) -> Result<String> {
        let history_len = messages.len();

        self.hooks.run_start(&user_input_of(messages)).await?;
        let provider = self.provider();
        let run_span = self.telemetry.span(
            "agent.run",
            attrs(&[("model", AttrValue::from(provider.model().to_string()))]),
        );

        // 工作上下文 = system + 完整历史
        let mut convo = vec![Message::system(system_prompt)];
        convo.extend(messages.iter().cloned());

        let mut final_answer = String::new();
        let result = self
            .run_loop(&mut convo, events, cancel, steering, &mut final_answer)
            .await;

        // 无论成功、取消还是失败，新增消息只在此处回写一次（失败时保留部分进度）
        sync_new_messages(&convo, history_len, messages);

        match result {
            Ok(()) => {
                self.hooks.run_end(&final_answer).await?;
                run_span.end();
                events
                    .send(AgentEvent::RunCompleted {
                        answer: final_answer.clone(),
                    })
                    .ok();
                Ok(final_answer)
            }
            Err(e) => {
                self.hooks.run_end(&final_answer).await?;
                run_span.end_with_error(&e.to_string());
                events
                    .send(AgentEvent::RunFailed {
                        error: e.to_string(),
                    })
                    .ok();
                Err(e)
            }
        }
    }

    async fn run_loop(
        &self,
        convo: &mut Vec<Message>,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        steering: &SteeringQueue,
        final_answer: &mut String,
    ) -> Result<()> {
        let mut turn: u32 = 0;

        loop {
            if cancel.is_cancelled() {
                events.send(AgentEvent::Interrupted).ok();
                return Ok(());
            }

            turn += 1;
            if turn > self.max_turns {
                let msg = format!("达到最大迭代次数 ({})", self.max_turns);
                warn!("{}", msg);
                return Err(anyhow::anyhow!("{}", msg));
            }

            // 注入 steering 消息（用户运行中的新指令）
            for msg in steering.drain() {
                info!("Steering message: {}", msg);
                commit(convo, events, Message::user(msg));
            }

            self.hooks.turn_start(turn).await?;
            events.send(AgentEvent::TurnStarted { turn }).ok();
            let turn_span = self.telemetry.span(
                "agent.turn",
                attrs(&[("turn", AttrValue::Uint(turn as u64))]),
            );

            // LLM 流式调用（带重试）。verbosity steer 只注入请求副本，
            // 会话历史（convo / messages）不受影响，也不会被持久化
            let mut request_messages = convo.clone();
            if self.verbosity_steer {
                steer_last_user(&mut request_messages);
            }
            let request = ChatRequest::new(request_messages)
                .with_tools(self.request_tool_definitions())
                .with_max_tokens(self.max_tokens)
                .with_thinking(self.thinking());
            let response = match self.stream_with_retry(request, events, cancel).await? {
                Some(response) => response,
                None => {
                    // 用户取消
                    events.send(AgentEvent::Interrupted).ok();
                    turn_span.end();
                    return Ok(());
                }
            };

            debug!(
                "LLM response: {} chars, {} tool_calls",
                response.content.len(),
                response.tool_calls.as_ref().map(|t| t.len()).unwrap_or(0)
            );

            match response.tool_calls {
                Some(tool_calls) if !tool_calls.is_empty() => {
                    // assistant 消息（含工具调用）+ 工具结果
                    let mut tool_results = Vec::new();

                    // 并行批次：本轮全部调用都是 parallel 工具（如多个 task
                    // 子代理，各自独立上下文）时并发执行，结果按原顺序配对。
                    // steering/取消只在批次级生效——已并发的兄弟调用无法
                    // 中途跳过（并行语义的固有代价）
                    let all_parallel = tool_calls.len() > 1
                        && tool_calls.iter().all(|c| {
                            !response.invalid_args.contains(&c.id)
                                && self.tools.get(&c.name).is_some_and(|t| t.parallel())
                        });
                    if all_parallel {
                        info!(
                            "running {} parallel tool calls concurrently",
                            tool_calls.len()
                        );
                        let outputs = futures::future::join_all(
                            tool_calls
                                .iter()
                                .map(|tc| self.execute_tool_with_hooks(tc, events, cancel)),
                        )
                        .await;
                        for (tool_call, output) in tool_calls.iter().zip(outputs) {
                            // 与串行路径同语义：工具级 Err 中止 run（is_error 走结果）
                            let output = output?;
                            tool_results.push(ToolResult {
                                tool_call_id: tool_call.id.clone(),
                                content: output.content.clone(),
                            });
                        }
                        if !steering.is_empty() {
                            info!("Steering detected after parallel batch");
                        }
                    } else {
                        let mut skip_rest = false;
                        for tool_call in &tool_calls {
                            // 每个 tool_call 都必须有对应的 tool_result，
                            // 否则严格的 API（如 Anthropic）会拒绝下一次请求
                            if skip_rest || cancel.is_cancelled() {
                                let reason = if cancel.is_cancelled() {
                                    CANCELLED_BY_USER
                                } else {
                                    SKIPPED_BY_STEERING
                                };
                                tool_results.push(ToolResult {
                                    tool_call_id: tool_call.id.clone(),
                                    content: reason.to_string(),
                                });
                                continue;
                            }

                            // 参数 JSON 无效（多为触及 max_tokens 被截断）：绝不以 `{}` 执行，
                            // 把原因告诉模型让它重试
                            if response.invalid_args.contains(&tool_call.id) {
                                let content = invalid_args_message(
                                    &tool_call.name,
                                    response.stop.as_ref(),
                                    self.max_tokens,
                                );
                                warn!("tool '{}' not executed: {}", tool_call.name, content);
                                events
                                    .send(AgentEvent::ToolStarted {
                                        id: tool_call.id.clone(),
                                        name: tool_call.name.clone(),
                                        args: tool_call.arguments.clone(),
                                    })
                                    .ok();
                                events
                                    .send(AgentEvent::ToolFinished {
                                        id: tool_call.id.clone(),
                                        name: tool_call.name.clone(),
                                        output: content.clone(),
                                        is_error: true,
                                        duration_ms: 0,
                                        original_bytes: None,
                                        original_tokens: None,
                                    })
                                    .ok();
                                tool_results.push(ToolResult {
                                    tool_call_id: tool_call.id.clone(),
                                    content,
                                });
                                continue;
                            }

                            let output = self
                                .execute_tool_with_hooks(tool_call, events, cancel)
                                .await?;
                            tool_results.push(ToolResult {
                                tool_call_id: tool_call.id.clone(),
                                content: output.content.clone(),
                            });

                            // 每个工具执行后检查 steering：发现则跳过剩余工具
                            if !steering.is_empty() {
                                info!("Steering detected, skipping remaining tools");
                                skip_rest = true;
                            }
                        }
                    }

                    commit(
                        convo,
                        events,
                        Message {
                            role: Role::Assistant,
                            content: response.content.clone(),
                            tool_calls: Some(tool_calls.clone()),
                            tool_results: None,
                            // 思考模型要求在工具循环内回传（由各 provider 决定如何带上）
                            reasoning: response.reasoning.clone(),
                        },
                    );
                    commit(
                        convo,
                        events,
                        Message {
                            role: Role::Tool,
                            content: String::new(),
                            tool_calls: None,
                            tool_results: Some(tool_results),
                            reasoning: None,
                        },
                    );

                    // 单个长轮次内上下文逼近窗口：就地精简旧的工具结果。
                    // （轮次之间的压缩由 Harness 负责，但它只在 run 开始前执行）
                    let budget = self
                        .context_budget
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if budget > 0 {
                        let observed = response
                            .context_tokens
                            .unwrap_or(0)
                            .max(rough_token_estimate(convo));
                        if observed > budget {
                            let freed = elide_old_tool_results(convo, self.spill.as_ref());
                            if freed > 0 {
                                warn!(
                                    "context ~{observed} tokens over budget {budget}: elided {freed} bytes of old tool results"
                                );
                            }
                        }
                    }

                    events.send(AgentEvent::TurnFinished { turn }).ok();
                    turn_span.end();
                    // 继续下一轮（新一轮开头注入 steering）
                }
                _ => {
                    // 无工具调用 — 最终答案
                    if response.content.is_empty() {
                        let msg = "LLM 返回了空响应";
                        warn!("{}", msg);
                        return Err(anyhow::anyhow!("{}", msg));
                    }
                    let mut answer = response.content.clone();
                    if response.stop == Some(StopReason::MaxTokens) {
                        warn!("final answer truncated at max_tokens={}", self.max_tokens);
                        // 标记进入答案与历史：用户看到截断提示，
                        // 模型下一轮也能看到并从中断处续写
                        answer.push_str(&format!(
                            "\n\n[答案因 max_tokens={} 被截断 — 可发送\u{201c}继续\u{201d}获取剩余部分]",
                            self.max_tokens
                        ));
                    }
                    *final_answer = answer.clone();
                    commit(convo, events, Message::assistant(answer));
                    events.send(AgentEvent::TurnFinished { turn }).ok();
                    turn_span.end();
                    break;
                }
            }
        }

        Ok(())
    }

    /// 工具执行 + hooks + HITL 确认 + 事件 + 遥测
    async fn execute_tool_with_hooks(
        &self,
        tool_call: &ToolCall,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput> {
        let name = tool_call.name.as_str();
        events
            .send(AgentEvent::ToolStarted {
                id: tool_call.id.clone(),
                name: name.to_string(),
                args: tool_call.arguments.clone(),
            })
            .ok();

        let started = std::time::Instant::now();
        let tool_span = self.telemetry.span(
            "agent.tool",
            attrs(&[("name", AttrValue::from(name.to_string()))]),
        );

        // 计划模式门控：白名单外的工具不执行（定义已过滤，这里兜底历史里的
        // 调用与热切换竞态），拒绝原因作为 is_error 结果回传给模型
        if self.plan_mode() && !PLAN_MODE_ALLOWED_TOOLS.contains(&name) {
            let output = ToolOutput::err(format!(
                "[Plan mode] tool '{name}' is not allowed while planning — this session is \
                 read-only. Explore with read/search/grep/task, then present your \
                 implementation plan as the final answer; the user approves it before execution."
            ));
            tool_span.set_attribute("duration_ms", AttrValue::Uint(0));
            tool_span.set_attribute("is_error", AttrValue::Bool(true));
            tool_span.set_attribute("denied", AttrValue::from("plan_mode"));
            tool_span.end();
            events
                .send(AgentEvent::ToolFinished {
                    id: tool_call.id.clone(),
                    name: name.to_string(),
                    output: output.content.clone(),
                    is_error: true,
                    duration_ms: 0,
                    original_bytes: None,
                    original_tokens: None,
                })
                .ok();
            return Ok(output);
        }

        // Hook 拦截/改写 + HITL 确认。
        // hook 自身报错按拒绝处理（fail closed），而不是中止整个 run：
        // 中止会丢掉本轮已执行工具的结果，留下不配对的 tool_use。
        let decision = match self.hooks.tool_call(name, &tool_call.arguments).await {
            Ok(decision) => decision,
            Err(e) => {
                warn!("hook failed on tool '{}': {e}", name);
                crate::HookDecision::Deny(format!("hook error: {e}"))
            }
        };
        let output = match decision {
            crate::HookDecision::Deny(reason) => {
                warn!("Tool '{}' denied by hook: {}", name, reason);
                ToolOutput::err(format!("[Denied by hook] {}", reason))
            }
            allowed => {
                // 改写后的参数：人工确认看到的、工具执行的都是它
                let args = match allowed {
                    crate::HookDecision::Modify(new_args) => {
                        info!("Tool '{}' arguments rewritten by hook", name);
                        new_args
                    }
                    _ => tool_call.arguments.clone(),
                };
                let denied = if self.confirmation.needs(name) {
                    match self
                        .confirmation
                        .confirm(
                            ConfirmationRequest {
                                tool_name: name.to_string(),
                                args: args.clone(),
                            },
                            cancel,
                        )
                        .await
                    {
                        ConfirmationDecision::Deny(reason) => Some(reason),
                        _ => None,
                    }
                } else {
                    None
                };
                match denied {
                    Some(reason) => {
                        warn!("Tool '{}' denied by user: {}", name, reason);
                        ToolOutput::err(format!("[Denied by user] {}", reason))
                    }
                    None => self.run_tool(name, &args, cancel).await,
                }
            }
        };

        let duration_ms = started.elapsed().as_millis() as u64;
        tool_span.set_attribute("duration_ms", AttrValue::Uint(duration_ms));
        tool_span.set_attribute("is_error", AttrValue::Bool(output.is_error));
        tool_span.set_attribute(
            "bytes_delivered",
            AttrValue::Uint(output.content.len() as u64),
        );
        tool_span.set_attribute(
            "tokens_delivered",
            AttrValue::Uint(crate::estimate_text_tokens(&output.content) as u64),
        );
        if let Some(original) = output.original_bytes {
            tool_span.set_attribute("bytes_original", AttrValue::Uint(original));
            tool_span.set_attribute("bytes_saved", AttrValue::Uint(output.bytes_saved()));
        }
        if let Some(tokens) = output.original_tokens {
            tool_span.set_attribute("tokens_original", AttrValue::Uint(tokens));
            tool_span.set_attribute("tokens_saved", AttrValue::Uint(output.tokens_saved()));
        }
        tool_span.end();

        // 结果流过 hooks（可改写，如脱敏）；hook 报错不丢结果
        let output = match self.hooks.tool_result(name, output.clone()).await {
            Ok(rewritten) => rewritten,
            Err(e) => {
                warn!("tool_result hook failed on '{}': {e}", name);
                output
            }
        };

        events
            .send(AgentEvent::ToolFinished {
                id: tool_call.id.clone(),
                name: name.to_string(),
                output: output.content.clone(),
                is_error: output.is_error,
                duration_ms,
                original_bytes: output.original_bytes,
                original_tokens: output.original_tokens,
            })
            .ok();

        Ok(output)
    }

    /// 执行工具本体（查找 + 调用，错误包装为 ToolOutput）。
    ///
    /// 取消 = 丢弃工具的 future：bash 的进程组守卫会随之 SIGKILL 整个进程组，
    /// 其它工具的 IO 在下一个 await 点停止。
    async fn run_tool(
        &self,
        name: &str,
        args: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let Some(tool) = self.tools.get(name) else {
            return ToolOutput::err(format!("[Unknown tool: {}]", name));
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => ToolOutput::err(CANCELLED_BY_USER),
            result = tool.execute(args.clone()) => match result {
                Ok(output) => output,
                Err(e) => ToolOutput::err(format!("[Tool error] {}", e)),
            },
        }
    }

    /// 流式 LLM 调用，瞬时错误自动重试（指数退避）
    async fn stream_with_retry(
        &self,
        request: ChatRequest,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<Option<TurnResponse>> {
        let mut attempt: u32 = 0;
        loop {
            let mut streamed_text = false;
            match self
                .stream_llm_response(request.clone(), events, cancel, &mut streamed_text)
                .await
            {
                Ok(response) => return Ok(response),
                Err(e) => {
                    // 按状态码 / 传输层错误类型判定（不再对错误文本做子串匹配）
                    if !baiji_ai::is_transient_error(&e) || attempt >= self.max_retries {
                        return Err(e);
                    }
                    // 已经流出的半截回答作废：通知 UI 清掉，否则重试后文本会出现两遍
                    if streamed_text {
                        events.send(AgentEvent::StreamRestarted).ok();
                    }
                    let delay = retry_delay(
                        self.retry_base_delay,
                        attempt,
                        self.retry_max_delay,
                        baiji_ai::retry_after(&e),
                    );
                    warn!(
                        "LLM call failed (attempt {}/{}): {}; retrying in {:?}",
                        attempt + 1,
                        self.max_retries,
                        e,
                        delay
                    );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Ok(None),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    attempt += 1;
                }
            }
        }
    }

    /// 流式调用 LLM，边收流边推送 TextDelta 事件，同时收集完整响应。
    /// 取消时返回 None。
    async fn stream_llm_response(
        &self,
        request: ChatRequest,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        streamed_text: &mut bool,
    ) -> Result<Option<TurnResponse>> {
        let mut stream = self.provider().chat_stream(request).await?;
        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut invalid_args: HashSet<String> = HashSet::new();
        let mut stop: Option<StopReason> = None;
        let mut usage: Option<TokenUsage> = None;
        // 思考内容：thinking 累积当前块，遇到签名（Anthropic）即封块
        let mut thinking = String::new();
        let mut reasoning: Vec<ReasoningBlock> = Vec::new();
        // (id, name, accumulated_args_json)
        let mut current_tool: Option<(String, String, String)> = None;

        // 收尾一个工具调用：空参数 = 无参工具（合法）；非空但解析失败 = 无效，标记后不执行
        let mut finish_tool = |(id, name, args): (String, String, String),
                               tool_calls: &mut Vec<ToolCall>| {
            let arguments = if args.trim().is_empty() {
                serde_json::json!({})
            } else {
                match serde_json::from_str::<serde_json::Value>(&args) {
                    Ok(value) if value.is_object() => value,
                    _ => {
                        invalid_args.insert(id.clone());
                        // 历史里仍须是合法对象，否则下一次请求会被 API 拒绝
                        serde_json::json!({})
                    }
                }
            };
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        };

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Ok(None);
                }
                chunk = stream.next() => {
                    match chunk {
                        // 文本增量始终是文本（工具参数走 ToolCallArguments）
                        Some(Ok(StreamChunk::Content(t))) => {
                            if !t.is_empty() {
                                text.push_str(&t);
                                *streamed_text = true;
                                events.send(AgentEvent::TextDelta { text: t }).ok();
                            }
                        }
                        Some(Ok(StreamChunk::ToolCallStart { id, name })) => {
                            // 结束上一个工具调用
                            if let Some(prev) = current_tool.take() {
                                finish_tool(prev, &mut tool_calls);
                            }
                            current_tool = Some((id, name, String::new()));
                        }
                        Some(Ok(StreamChunk::ToolCallArguments { id: _, arguments })) => {
                            if let Some((_, _, args)) = current_tool.as_mut() {
                                args.push_str(&arguments);
                            }
                        }
                        Some(Ok(StreamChunk::Stop(reason))) => stop = Some(reason),
                        Some(Ok(StreamChunk::Reasoning(t))) => {
                            thinking.push_str(&t);
                            events.send(AgentEvent::ReasoningDelta { text: t }).ok();
                        }
                        Some(Ok(StreamChunk::ReasoningSignature(signature))) => {
                            reasoning.push(ReasoningBlock {
                                text: std::mem::take(&mut thinking),
                                signature: Some(signature),
                                redacted: None,
                                id: None,
                            });
                        }
                        Some(Ok(StreamChunk::ReasoningRedacted(data))) => {
                            reasoning.push(ReasoningBlock {
                                text: String::new(),
                                signature: None,
                                redacted: Some(data),
                                id: None,
                            });
                        }
                        Some(Ok(StreamChunk::ReasoningItem { id, encrypted_content, summary })) => {
                            reasoning.push(ReasoningBlock {
                                text: summary,
                                signature: None,
                                redacted: Some(encrypted_content),
                                id: Some(id),
                            });
                        }
                        Some(Ok(StreamChunk::Usage(u))) => usage = Some(u),
                        Some(Ok(StreamChunk::Done)) | None => {
                            if let Some(last) = current_tool.take() {
                                finish_tool(last, &mut tool_calls);
                            }
                            break;
                        }
                        Some(Ok(StreamChunk::Error(e))) => {
                            return Err(anyhow::anyhow!("Stream error: {}", e));
                        }
                        Some(Err(e)) => return Err(e),
                    }
                }
            }
        }

        if !thinking.is_empty() {
            reasoning.push(ReasoningBlock {
                text: thinking,
                signature: None,
                redacted: None,
                id: None,
            });
        }
        if let Some(usage) = usage {
            events
                .send(AgentEvent::UsageReported {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                })
                .ok();
        }

        Ok(Some(TurnResponse {
            context_tokens: usage.map(|u| (u.input_tokens + u.output_tokens) as usize),
            reasoning: (!reasoning.is_empty()).then_some(reasoning),
            content: text,
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            invalid_args,
            stop,
        }))
    }
}

/// 一轮 LLM 流式调用的汇总结果
struct TurnResponse {
    /// 厂商上报的本次调用上下文占用（input + output）
    context_tokens: Option<usize>,
    content: String,
    reasoning: Option<Vec<ReasoningBlock>>,
    tool_calls: Option<Vec<ToolCall>>,
    /// 参数 JSON 无效的工具调用 id（不执行）
    invalid_args: HashSet<String>,
    stop: Option<StopReason>,
}

/// 最近这么多条工具结果消息保持原样（模型正在用）
const KEEP_RECENT_TOOL_MESSAGES: usize = 3;
/// 小于此字节数的工具结果不值得精简
const ELIDE_MIN_BYTES: usize = 1024;
const ELIDED_NOTE: &str = "[elided to save context — this older tool output was removed; re-run the tool if you need it again]";

/// 粗略 token 估算（厂商未上报 usage 时的兜底）：ASCII ≈ 4 字符/token，其它 ≈ 2
fn rough_token_estimate(messages: &[Message]) -> usize {
    let estimate = |s: &str| {
        let ascii = s.bytes().filter(u8::is_ascii).count();
        let other = s.chars().count().saturating_sub(ascii);
        ascii / 4 + other / 2
    };
    messages
        .iter()
        .map(|m| {
            estimate(&m.content)
                + m.tool_calls
                    .iter()
                    .flatten()
                    .map(|c| estimate(&c.arguments.to_string()))
                    .sum::<usize>()
                + m.tool_results
                    .iter()
                    .flatten()
                    .map(|r| estimate(&r.content))
                    .sum::<usize>()
        })
        .sum()
}

/// 把较早的大块工具结果替换为占位说明，返回释放的字节数。
/// 消息条数与 tool_call ↔ tool_result 的配对保持不变（API 要求严格配对）。
/// spill 可用时占位携带 ctx 句柄（可逆，expand 取回）；否则回退不可逆占位。
fn elide_old_tool_results(convo: &mut [Message], spill: Option<&SpillFn>) -> usize {
    let tool_positions: Vec<usize> = convo
        .iter()
        .enumerate()
        .filter(|(_, m)| m.tool_results.is_some())
        .map(|(i, _)| i)
        .collect();
    let cutoff = tool_positions
        .len()
        .saturating_sub(KEEP_RECENT_TOOL_MESSAGES);
    let mut freed = 0;
    for &position in &tool_positions[..cutoff] {
        for result in convo[position].tool_results.iter_mut().flatten() {
            if result.content.len() >= ELIDE_MIN_BYTES {
                let bytes = result.content.len();
                let replacement = match spill.and_then(|f| f(&result.content)) {
                    Some(handle) => format!(
                        "[ctx stub: {bytes} bytes of older tool output elided; \
                         full content handle: ctx:{handle} — call the expand tool with this handle to retrieve it]"
                    ),
                    None => ELIDED_NOTE.to_string(),
                };
                freed += bytes.saturating_sub(replacement.len());
                result.content = replacement;
            }
        }
    }
    freed
}

/// 新消息进入工作上下文，并通知持久化层增量落盘
fn commit(convo: &mut Vec<Message>, events: &UnboundedSender<AgentEvent>, message: Message) {
    events
        .send(AgentEvent::MessageCommitted {
            message: message.clone(),
        })
        .ok();
    convo.push(message);
}

/// 重试退避延迟：优先服务端 Retry-After，否则 base×2^attempt；
/// 一律不超过上限（服务端要求过长等待也截断，避免悬挂）
fn retry_delay(
    base: Duration,
    attempt: u32,
    cap: Duration,
    server_retry_after: Option<Duration>,
) -> Duration {
    let raw = server_retry_after.unwrap_or_else(|| base.saturating_mul(1u32 << attempt.min(16)));
    raw.min(cap)
}

/// verbosity steer 的恒定指令文本。逐字节恒定：同一会话内每轮请求的追加
/// 内容相同，此前的前缀在 provider 侧的自动前缀缓存中仍然命中。
const STEER_SUFFIX: &str = "\n\n[System note: Be concise. Answer directly without restating the \
question or adding pleasantries; skip filler and long summaries unless asked for detail. Keep \
code, commands and facts complete.]";

/// 向最后一条 user 消息追加 verbosity 指令（只作用于请求副本；无 user 消息不注入）
fn steer_last_user(messages: &mut [Message]) {
    for m in messages.iter_mut().rev() {
        if m.role == Role::User {
            m.content.push_str(STEER_SUFFIX);
            return;
        }
    }
}

/// steering 打断后，未执行的工具调用的占位结果
const SKIPPED_BY_STEERING: &str = "[Skipped] 用户在运行中发来了新指令，此工具调用未执行";

/// 用户取消后，未执行/被中断的工具调用的结果
const CANCELLED_BY_USER: &str = "[Cancelled] 用户取消了本次运行，此工具调用未完成";

fn invalid_args_message(tool: &str, stop: Option<&StopReason>, max_tokens: u32) -> String {
    if stop == Some(&StopReason::MaxTokens) {
        format!(
            "[Error] tool '{tool}' was NOT executed: the response hit the output limit \
             (max_tokens={max_tokens}) and the arguments JSON was cut off. Retry with smaller \
             arguments — e.g. write the file in several smaller write/edit calls."
        )
    } else {
        format!(
            "[Error] tool '{tool}' was NOT executed: its arguments were not a valid JSON object \
             (the response was probably truncated). Send the call again with complete arguments."
        )
    }
}

fn user_input_of(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// 把 convo 中新增的消息（index > 1 + history_len）追加到持久化历史
fn sync_new_messages(convo: &[Message], history_len: usize, messages: &mut Vec<Message>) {
    let new_start = 1 + history_len; // 跳过 system + 原有历史
    messages.truncate(history_len); // 幂等：重复调用不会产生重复消息
    if convo.len() > new_start {
        messages.extend(convo[new_start..].iter().cloned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{AgentTool, ToolOutput};
    use async_trait::async_trait;
    use baiji_ai::{ChatResponse, Protocol, StreamChunk};

    // ===== verbosity steer =====

    /// 记录每次请求消息与工具定义的捕获型 Provider
    struct CapturingProvider {
        requests: std::sync::Mutex<Vec<Vec<Message>>>,
        tool_names: std::sync::Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!("runtime uses chat_stream")
        }
        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<StreamChunk>>> {
            self.requests.lock().unwrap().push(request.messages.clone());
            self.tool_names.lock().unwrap().push(
                request
                    .tools
                    .iter()
                    .flatten()
                    .map(|t| t.name.clone())
                    .collect(),
            );
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

    #[test]
    fn test_retry_delay_exponential_with_cap() {
        let base = Duration::from_millis(500);
        let cap = Duration::from_millis(30_000);
        // 指数退避：500ms → 1s → 2s → 4s
        assert_eq!(retry_delay(base, 0, cap, None), Duration::from_millis(500));
        assert_eq!(
            retry_delay(base, 1, cap, None),
            Duration::from_millis(1_000)
        );
        assert_eq!(
            retry_delay(base, 2, cap, None),
            Duration::from_millis(2_000)
        );
        assert_eq!(
            retry_delay(base, 3, cap, None),
            Duration::from_millis(4_000)
        );
        // 封顶：2^6×500 = 32s > 30s cap
        assert_eq!(retry_delay(base, 6, cap, None), cap);
        // 服务端 Retry-After 优先，但同样受上限约束
        assert_eq!(
            retry_delay(base, 0, cap, Some(Duration::from_secs(2))),
            Duration::from_secs(2)
        );
        assert_eq!(
            retry_delay(base, 0, cap, Some(Duration::from_secs(120))),
            cap
        );
    }

    #[test]
    fn test_steer_last_user_targets_last_user_only() {
        let mut msgs = vec![
            Message::user("q1"),
            Message::assistant("a1"),
            Message::user("q2"),
            Message::assistant("a2"),
        ];
        steer_last_user(&mut msgs);
        assert_eq!(msgs[0].content, "q1");
        assert_eq!(msgs[2].content.strip_suffix(STEER_SUFFIX), Some("q2"));

        // 无 user 消息：不注入
        let mut none = vec![Message::system("s"), Message::assistant("a")];
        steer_last_user(&mut none);
        assert_eq!(none[0].content, "s");
    }

    #[tokio::test]
    async fn test_verbosity_steer_injects_into_request_copy_only() {
        let provider = Arc::new(CapturingProvider {
            requests: std::sync::Mutex::new(Vec::new()),
            tool_names: std::sync::Mutex::new(Vec::new()),
        });
        let runtime = AgentRuntime::new(provider.clone()).with_verbosity_steer(true);
        let mut history = vec![Message::user("原始问题")];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        runtime
            .run(
                "sys",
                &mut history,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();

        // 请求副本：最后一条 user 消息带恒定后缀
        {
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let last_user = requests[0]
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .unwrap();
            assert_eq!(
                last_user.content.strip_suffix(STEER_SUFFIX),
                Some("原始问题")
            );
        }
        // 会话历史未被污染（注入不持久化、不进上下文）
        assert_eq!(history[0].content, "原始问题");

        // 第二轮：后缀落在新的最后一条 user 消息上，旧 user 消息保持干净
        history.push(Message::user("第二个问题"));
        runtime
            .run(
                "sys",
                &mut history,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        {
            let requests = provider.requests.lock().unwrap();
            let users: Vec<&Message> = requests[1]
                .iter()
                .filter(|m| m.role == Role::User)
                .collect();
            assert_eq!(users.len(), 2);
            assert_eq!(users[0].content, "原始问题");
            assert_eq!(
                users[1].content.strip_suffix(STEER_SUFFIX),
                Some("第二个问题")
            );
        }
        assert_eq!(history[0].content, "原始问题");
        assert_eq!(history[2].content, "第二个问题");
    }

    #[tokio::test]
    async fn test_verbosity_steer_disabled_by_default() {
        let provider = Arc::new(CapturingProvider {
            requests: std::sync::Mutex::new(Vec::new()),
            tool_names: std::sync::Mutex::new(Vec::new()),
        });
        let runtime = AgentRuntime::new(provider.clone());
        let mut history = vec![Message::user("q")];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        runtime
            .run(
                "sys",
                &mut history,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        let requests = provider.requests.lock().unwrap();
        assert!(!requests[0].iter().any(|m| m.content.contains(STEER_SUFFIX)));
    }
    use baiji_telemetry::RecordingTelemetry;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ===== Mock Provider：按预设脚本吐流式块 =====

    #[derive(Clone)]
    enum Script {
        ToolThenAnswer {
            tool_id: &'static str,
            tool_name: &'static str,
            args: &'static str,
            answer: &'static str,
        },
        AnswerOnly(&'static str),
        /// 第一轮一次返回三个工具调用，之后直接回答
        ThreeToolsThenAnswer,
        /// 每一轮都返回工具调用（用于触发 max_turns）
        AlwaysTool,
        /// 第一轮：工具参数在 max_tokens 处被截断；之后直接回答
        TruncatedToolThenAnswer,
        /// 最终答案在 max_tokens 处被截断（无工具调用）
        TruncatedAnswer(&'static str),
    }

    fn tool_call_chunks(id: &str) -> Vec<Result<StreamChunk>> {
        vec![
            Ok(StreamChunk::ToolCallStart {
                id: id.to_string(),
                name: "append".to_string(),
            }),
            Ok(StreamChunk::ToolCallArguments {
                id: id.to_string(),
                arguments: r#"{"text":"x"}"#.to_string(),
            }),
        ]
    }

    struct MockProvider {
        script: Script,
        calls: AtomicUsize,
    }

    impl MockProvider {
        fn new(script: Script) -> Self {
            Self {
                script,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            unreachable!("runtime uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<StreamChunk>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let n = self.calls.load(Ordering::SeqCst);
            let chunks: Vec<Result<StreamChunk>> = match &self.script {
                Script::ToolThenAnswer {
                    tool_id,
                    tool_name,
                    args,
                    answer,
                } => {
                    if n == 1 {
                        vec![
                            Ok(StreamChunk::Content("Let me check.".into())),
                            Ok(StreamChunk::ToolCallStart {
                                id: tool_id.to_string(),
                                name: tool_name.to_string(),
                            }),
                            Ok(StreamChunk::ToolCallArguments {
                                id: tool_id.to_string(),
                                arguments: args.to_string(),
                            }),
                            Ok(StreamChunk::Done),
                        ]
                    } else {
                        vec![
                            Ok(StreamChunk::Content(answer.to_string())),
                            Ok(StreamChunk::Done),
                        ]
                    }
                }
                Script::AnswerOnly(text) => vec![
                    Ok(StreamChunk::Content(text.to_string())),
                    Ok(StreamChunk::Done),
                ],
                Script::TruncatedAnswer(text) => vec![
                    Ok(StreamChunk::Content(text.to_string())),
                    Ok(StreamChunk::Stop(StopReason::MaxTokens)),
                    Ok(StreamChunk::Done),
                ],
                Script::ThreeToolsThenAnswer => {
                    if n == 1 {
                        let mut chunks = Vec::new();
                        for id in ["a", "b", "c"] {
                            chunks.extend(tool_call_chunks(id));
                        }
                        chunks.push(Ok(StreamChunk::Done));
                        chunks
                    } else {
                        vec![
                            Ok(StreamChunk::Content("done".to_string())),
                            Ok(StreamChunk::Done),
                        ]
                    }
                }
                Script::TruncatedToolThenAnswer => {
                    if n == 1 {
                        vec![
                            Ok(StreamChunk::ToolCallStart {
                                id: "t1".to_string(),
                                name: "append".to_string(),
                            }),
                            Ok(StreamChunk::ToolCallArguments {
                                id: "t1".to_string(),
                                arguments: r#"{"text":"cut of"#.to_string(),
                            }),
                            Ok(StreamChunk::Stop(StopReason::MaxTokens)),
                            Ok(StreamChunk::Done),
                        ]
                    } else {
                        vec![
                            Ok(StreamChunk::Content("done".to_string())),
                            Ok(StreamChunk::Done),
                        ]
                    }
                }
                Script::AlwaysTool => {
                    let mut chunks = tool_call_chunks(&format!("t{n}"));
                    chunks.push(Ok(StreamChunk::Done));
                    chunks
                }
            };
            Ok(futures::stream::iter(chunks).boxed())
        }

        fn protocol(&self) -> Protocol {
            Protocol::Anthropic
        }
        fn model(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    // ===== 测试工具 =====

    struct AppendTool;

    #[async_trait]
    impl AgentTool for AppendTool {
        fn name(&self) -> &str {
            "append"
        }
        fn description(&self) -> &str {
            "append text"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
            Ok(ToolOutput::ok(format!(
                "appended:{}",
                args["text"].as_str().unwrap_or("")
            )))
        }
    }

    fn runtime_with(provider: MockProvider) -> AgentRuntime {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(AppendTool));
        AgentRuntime::new(Arc::new(provider)).with_tools(tools)
    }

    #[test]
    fn test_thinking_level_builder_and_hot_swap() {
        use baiji_ai::ThinkingLevel;
        let runtime = runtime_with(MockProvider::new(Script::AnswerOnly("ok")));
        assert_eq!(runtime.thinking(), None, "default off");
        let runtime = runtime.with_thinking(Some(ThinkingLevel::High));
        assert_eq!(runtime.thinking(), Some(ThinkingLevel::High));
        // 热切换（TUI /thinking 路径）：&self 即可修改，下一次请求生效
        runtime.set_thinking(Some(ThinkingLevel::Minimal));
        assert_eq!(runtime.thinking(), Some(ThinkingLevel::Minimal));
        runtime.set_thinking(None);
        assert_eq!(runtime.thinking(), None);
    }

    // ===== 计划模式 =====

    #[tokio::test]
    async fn test_plan_mode_denies_non_readonly_tool() {
        let provider = MockProvider::new(Script::ToolThenAnswer {
            tool_id: "t1",
            tool_name: "append",
            args: r#"{"text":"x"}"#,
            answer: "planned",
        });
        let runtime = runtime_with(provider).with_plan_mode(true);

        let mut messages = vec![Message::user("hi")];
        let events = drive(&runtime, &mut messages).await;

        // 工具被计划模式门控拒绝（is_error 结果回传，循环继续到最终答案）
        let result = &messages[2].tool_results.as_ref().unwrap()[0];
        assert!(result.content.contains("[Plan mode]"), "{}", result.content);
        assert_eq!(messages.last().unwrap().content, "planned");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolFinished { is_error: true, .. }))
        );
    }

    #[tokio::test]
    async fn test_plan_mode_filters_tool_definitions_and_hot_toggles() {
        struct NamedTool(&'static str);
        #[async_trait]
        impl AgentTool for NamedTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "stub"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
                Ok(ToolOutput::ok("ran"))
            }
        }

        let provider = Arc::new(CapturingProvider {
            requests: std::sync::Mutex::new(Vec::new()),
            tool_names: std::sync::Mutex::new(Vec::new()),
        });
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(NamedTool("append"))); // 白名单外
        tools.register(Arc::new(NamedTool("read"))); // 白名单内
        let runtime = AgentRuntime::new(provider.clone())
            .with_tools(tools)
            .with_plan_mode(true);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let steering = SteeringQueue::new();
        let mut messages = vec![Message::user("plan this")];
        runtime
            .run(
                "sys",
                &mut messages,
                &tx,
                &CancellationToken::new(),
                &steering,
            )
            .await
            .unwrap();
        {
            let names = provider.tool_names.lock().unwrap();
            assert_eq!(names[0], vec!["read"], "plan mode hides non-readonly tools");
        }

        // 热切换关闭（TUI Enter 执行计划路径）：下一次请求恢复全量定义
        runtime.set_plan_mode(false);
        messages.push(Message::user("execute"));
        runtime
            .run(
                "sys",
                &mut messages,
                &tx,
                &CancellationToken::new(),
                &steering,
            )
            .await
            .unwrap();
        let names = provider.tool_names.lock().unwrap();
        assert_eq!(
            names[1],
            vec!["append", "read"],
            "registration order restored"
        );
    }

    #[tokio::test]
    async fn test_plan_mode_denies_before_hook_gate() {
        // 门控顺序回归：计划模式拒绝必须发生在 hook 门控之前
        //（不该为注定被拒的调用咨询 hook / 弹 HITL 确认）
        struct RecordingHook(std::sync::Mutex<Vec<String>>);

        #[async_trait]
        impl crate::hooks::Hook for RecordingHook {
            fn name(&self) -> &str {
                "recording"
            }
            async fn on_tool_call(
                &self,
                name: &str,
                _args: &serde_json::Value,
            ) -> anyhow::Result<crate::HookDecision> {
                self.0.lock().unwrap().push(name.to_string());
                Ok(crate::HookDecision::Proceed)
            }
        }

        let seen = Arc::new(RecordingHook(std::sync::Mutex::new(Vec::new())));
        let provider = MockProvider::new(Script::ToolThenAnswer {
            tool_id: "t1",
            tool_name: "append",
            args: r#"{"text":"x"}"#,
            answer: "planned",
        });
        let mut hooks = HookRegistry::new();
        hooks.register(seen.clone());
        let runtime = runtime_with(provider)
            .with_hooks(hooks)
            .with_plan_mode(true);

        let mut messages = vec![Message::user("hi")];
        drive(&runtime, &mut messages).await;

        assert!(
            seen.0.lock().unwrap().is_empty(),
            "plan-mode denial must short-circuit before the hook gate"
        );
        let result = &messages[2].tool_results.as_ref().unwrap()[0];
        assert!(result.content.contains("[Plan mode]"));
    }

    // ===== 并行工具编排 =====

    /// 第一轮发两个指定名字的工具调用，第二轮给最终答案
    struct TwoCallsProvider {
        names: [&'static str; 2],
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for TwoCallsProvider {
        async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
            unreachable!("runtime uses chat_stream")
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<StreamChunk>>>
        {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let chunks: Vec<anyhow::Result<StreamChunk>> = if n == 1 {
                let mut v = Vec::new();
                for (i, name) in self.names.iter().enumerate() {
                    v.push(Ok(StreamChunk::ToolCallStart {
                        id: format!("t{i}"),
                        name: name.to_string(),
                    }));
                    v.push(Ok(StreamChunk::ToolCallArguments {
                        id: format!("t{i}"),
                        arguments: "{}".to_string(),
                    }));
                }
                v.push(Ok(StreamChunk::Done));
                v
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

    /// 探针工具：并发时两个探针在 barrier 相遇；串行时各自超时（overlap 恒 false）
    struct BarrierProbe {
        name: &'static str,
        barrier: Arc<tokio::sync::Barrier>,
        overlap: Arc<std::sync::atomic::AtomicBool>,
        parallel: bool,
        /// 单独等待 barrier 的超时（并行相遇在毫秒级；串行路径靠超时放行）
        wait_ms: u64,
    }

    #[async_trait]
    impl AgentTool for BarrierProbe {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "probe"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn parallel(&self) -> bool {
            self.parallel
        }
        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
            let met = tokio::time::timeout(
                std::time::Duration::from_millis(self.wait_ms),
                self.barrier.wait(),
            )
            .await
            .is_ok();
            if met {
                self.overlap.store(true, Ordering::Relaxed);
            }
            Ok(ToolOutput::ok(format!("{} met={met}", self.name)))
        }
    }

    #[tokio::test]
    async fn test_parallel_tools_run_concurrently() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let overlap = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(BarrierProbe {
            name: "probe_a",
            barrier: barrier.clone(),
            overlap: overlap.clone(),
            parallel: true,
            wait_ms: 2000,
        }));
        tools.register(Arc::new(BarrierProbe {
            name: "probe_b",
            barrier,
            overlap: overlap.clone(),
            parallel: true,
            wait_ms: 2000,
        }));
        let runtime = AgentRuntime::new(Arc::new(TwoCallsProvider {
            names: ["probe_a", "probe_b"],
            calls: AtomicUsize::new(0),
        }))
        .with_tools(tools);

        let mut history = vec![Message::user("go")];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let answer = runtime
            .run(
                "sys",
                &mut history,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        assert_eq!(answer, "done");

        assert!(
            overlap.load(Ordering::Relaxed),
            "two parallel-capable tools in one turn must run concurrently"
        );
        // 结果按原 id 顺序配对（tool_use/tool_result 稳定）
        let results_msg = history.iter().find(|m| m.tool_results.is_some()).unwrap();
        let results = results_msg.tool_results.as_ref().unwrap();
        assert_eq!(results[0].tool_call_id, "t0");
        assert_eq!(results[1].tool_call_id, "t1");
        assert!(results[0].content.contains("probe_a"));
        assert!(results[1].content.contains("probe_b"));
    }

    #[tokio::test]
    async fn test_mixed_tools_fall_back_to_sequential() {
        // 一轮里有非 parallel 工具 → 整轮回退串行（in-flight 计数恒 ≤ 1；结果仍配对）
        use std::sync::atomic::{AtomicUsize as AU, Ordering as O};

        struct Tracker {
            name: &'static str,
            parallel: bool,
            active: Arc<AU>,
            max_active: Arc<AU>,
        }

        #[async_trait]
        impl AgentTool for Tracker {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &str {
                "tracker"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn parallel(&self) -> bool {
                self.parallel
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
                let n = self.active.fetch_add(1, O::SeqCst) + 1;
                self.max_active.fetch_max(n, O::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                self.active.fetch_sub(1, O::SeqCst);
                Ok(ToolOutput::ok(self.name))
            }
        }

        let active = Arc::new(AU::new(0));
        let max_active = Arc::new(AU::new(0));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(Tracker {
            name: "para",
            parallel: true,
            active: active.clone(),
            max_active: max_active.clone(),
        }));
        tools.register(Arc::new(Tracker {
            name: "seq",
            parallel: false,
            active,
            max_active: max_active.clone(),
        }));
        let runtime = AgentRuntime::new(Arc::new(TwoCallsProvider {
            names: ["para", "seq"],
            calls: AtomicUsize::new(0),
        }))
        .with_tools(tools);

        let mut history = vec![Message::user("go")];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        runtime
            .run(
                "sys",
                &mut history,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            max_active.load(O::SeqCst),
            1,
            "mixed turn must fall back to sequential execution"
        );
        let results_msg = history.iter().find(|m| m.tool_results.is_some()).unwrap();
        assert_eq!(results_msg.tool_results.as_ref().unwrap().len(), 2);
    }

    async fn drive(runtime: &AgentRuntime, messages: &mut Vec<Message>) -> Vec<AgentEvent> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let steering = SteeringQueue::new();
        let handle = tokio::spawn(async move {
            let mut collected = Vec::new();
            while let Some(event) = rx.recv().await {
                collected.push(event);
            }
            collected
        });
        runtime
            .run(
                "You are a test agent.",
                messages,
                &tx,
                &CancellationToken::new(),
                &steering,
            )
            .await
            .unwrap();
        drop(tx);
        handle.await.unwrap()
    }

    #[tokio::test]
    async fn test_tool_loop_and_message_history() {
        let provider = MockProvider::new(Script::ToolThenAnswer {
            tool_id: "t1",
            tool_name: "append",
            args: r#"{"text":"hello"}"#,
            answer: "All done.",
        });
        let runtime = runtime_with(provider);

        let mut messages = vec![Message::user("run the tool")];
        let events = drive(&runtime, &mut messages).await;

        // 消息历史：user + assistant(tool_calls) + tool + assistant(final)
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].name, "append");
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(
            messages[2].tool_results.as_ref().unwrap()[0].content,
            "appended:hello"
        );
        assert_eq!(messages[3].content, "All done.");

        // 事件序列
        let kinds: Vec<_> = events.iter().map(|e| e.kind()).collect();
        assert!(kinds.contains(&"text_delta"));
        assert!(kinds.contains(&"tool_started"));
        assert!(kinds.contains(&"tool_finished"));
        assert!(kinds.contains(&"run_completed"));
    }

    #[tokio::test]
    async fn test_answer_only_single_turn() {
        let provider = MockProvider::new(Script::AnswerOnly("直接回答"));
        let runtime = runtime_with(provider);

        let mut messages = vec![Message::user("hi")];
        let events = drive(&runtime, &mut messages).await;

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content, "直接回答");
        assert_eq!(events.last().unwrap().kind(), "run_completed");
    }

    #[tokio::test]
    async fn test_hook_denies_tool() {
        struct DenyHook;

        #[async_trait]
        impl crate::Hook for DenyHook {
            fn name(&self) -> &str {
                "deny-append"
            }
            async fn on_tool_call(
                &self,
                _name: &str,
                _args: &serde_json::Value,
            ) -> Result<crate::HookDecision> {
                Ok(crate::HookDecision::Deny("not allowed".to_string()))
            }
        }

        let provider = MockProvider::new(Script::ToolThenAnswer {
            tool_id: "t1",
            tool_name: "append",
            args: r#"{"text":"x"}"#,
            answer: "ok",
        });
        let mut hooks = HookRegistry::new();
        hooks.register(Arc::new(DenyHook));
        let runtime = runtime_with(provider).with_hooks(hooks);

        let mut messages = vec![Message::user("hi")];
        drive(&runtime, &mut messages).await;

        // 工具结果被替换为 Deny 文案，但循环继续并给出最终答案
        assert!(
            messages[2].tool_results.as_ref().unwrap()[0]
                .content
                .contains("[Denied by hook]")
        );
        assert_eq!(messages[3].content, "ok");
    }

    #[tokio::test]
    async fn test_cancel_interrupts() {
        struct PendingProvider;

        #[async_trait]
        impl Provider for PendingProvider {
            async fn chat(&self, _: ChatRequest) -> Result<ChatResponse> {
                unreachable!()
            }
            async fn chat_stream(
                &self,
                _: ChatRequest,
            ) -> Result<futures::stream::BoxStream<'static, Result<StreamChunk>>> {
                // 永不结束的流，等待被取消
                Ok(futures::stream::pending().boxed())
            }
            fn protocol(&self) -> Protocol {
                Protocol::Anthropic
            }
            fn model(&self) -> &str {
                "mock"
            }
            fn provider_name(&self) -> &str {
                "mock"
            }
        }

        let runtime = AgentRuntime::new(Arc::new(PendingProvider));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let steering = SteeringQueue::new();

        let mut messages = vec![Message::user("hi")];
        tokio::spawn({
            let cancel = cancel.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                cancel.cancel();
            }
        });

        let answer = runtime
            .run("sys", &mut messages, &tx, &cancel, &steering)
            .await
            .unwrap();
        assert_eq!(answer, "");
    }

    #[tokio::test]
    async fn test_steering_injected_between_turns() {
        // 第一轮带工具调用，第二轮直接回答
        let provider = MockProvider::new(Script::ToolThenAnswer {
            tool_id: "t1",
            tool_name: "append",
            args: r#"{"text":"x"}"#,
            answer: "steered answer",
        });
        let runtime = runtime_with(provider);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let steering = SteeringQueue::new();
        steering.push("stop and answer directly");

        let mut messages = vec![Message::user("hi")];
        runtime
            .run(
                "sys",
                &mut messages,
                &tx,
                &CancellationToken::new(),
                &steering,
            )
            .await
            .unwrap();
        drop(tx);

        // steering 消息被注入到持久化历史（user 消息）
        let steered = messages
            .iter()
            .any(|m| m.role == Role::User && m.content == "stop and answer directly");
        assert!(steered, "steering message should be persisted");

        // 事件流以 run_completed 收尾
        let mut kinds = Vec::new();
        while let Some(event) = rx.recv().await {
            kinds.push(event.kind());
        }
        assert_eq!(kinds.last(), Some(&"run_completed"));
    }

    #[tokio::test]
    async fn test_steering_skipped_tools_still_get_results() {
        /// 执行时模拟用户插话：向 steering 队列推入一条消息
        struct SteeringTool(Arc<SteeringQueue>);

        #[async_trait]
        impl AgentTool for SteeringTool {
            fn name(&self) -> &str {
                "append"
            }
            fn description(&self) -> &str {
                "append text"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
                self.0.push("change of plan");
                Ok(ToolOutput::ok("appended:x"))
            }
        }

        let steering = Arc::new(SteeringQueue::new());
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(SteeringTool(steering.clone())));
        let runtime = AgentRuntime::new(Arc::new(MockProvider::new(Script::ThreeToolsThenAnswer)))
            .with_tools(tools);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let mut messages = vec![Message::user("hi")];
        runtime
            .run(
                "sys",
                &mut messages,
                &tx,
                &CancellationToken::new(),
                &steering,
            )
            .await
            .unwrap();

        let call_ids: Vec<String> = messages
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .flatten()
            .map(|c| c.id.clone())
            .collect();
        let results: Vec<&ToolResult> = messages
            .iter()
            .filter_map(|m| m.tool_results.as_ref())
            .flatten()
            .collect();
        let result_ids: Vec<String> = results.iter().map(|r| r.tool_call_id.clone()).collect();

        assert_eq!(call_ids, vec!["a", "b", "c"]);
        assert_eq!(result_ids, call_ids, "every tool_call needs a tool_result");
        assert!(results[0].content.starts_with("appended:"));
        assert_eq!(results[1].content, SKIPPED_BY_STEERING);
        assert_eq!(results[2].content, SKIPPED_BY_STEERING);
    }

    #[tokio::test]
    async fn test_truncated_tool_args_are_not_executed() {
        static EXECUTED: AtomicUsize = AtomicUsize::new(0);
        struct CountingTool;

        #[async_trait]
        impl AgentTool for CountingTool {
            fn name(&self) -> &str {
                "append"
            }
            fn description(&self) -> &str {
                "append text"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
                EXECUTED.fetch_add(1, Ordering::SeqCst);
                Ok(ToolOutput::ok("ran"))
            }
        }

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(CountingTool));
        let runtime =
            AgentRuntime::new(Arc::new(MockProvider::new(Script::TruncatedToolThenAnswer)))
                .with_tools(tools);

        let mut messages = vec![Message::user("hi")];
        let events = drive(&runtime, &mut messages).await;

        assert_eq!(
            EXECUTED.load(Ordering::SeqCst),
            0,
            "must not run with `{{}}`"
        );
        let result = &messages[2].tool_results.as_ref().unwrap()[0];
        assert_eq!(result.tool_call_id, "t1");
        assert!(result.content.contains("NOT executed"));
        assert!(result.content.contains("max_tokens"));
        // 历史中的参数仍是合法对象，UI 收到错误结果，运行继续到最终答案
        assert!(
            messages[1].tool_calls.as_ref().unwrap()[0]
                .arguments
                .is_object()
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolFinished { is_error: true, .. }))
        );
        assert_eq!(messages.last().unwrap().content, "done");
    }

    #[tokio::test]
    async fn test_cancel_interrupts_running_tool() {
        struct SlowTool;

        #[async_trait]
        impl AgentTool for SlowTool {
            fn name(&self) -> &str {
                "append"
            }
            fn description(&self) -> &str {
                "slow"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(ToolOutput::ok("never"))
            }
        }

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(SlowTool));
        let runtime = AgentRuntime::new(Arc::new(MockProvider::new(Script::ThreeToolsThenAnswer)))
            .with_tools(tools);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });

        let mut messages = vec![Message::user("hi")];
        let started = std::time::Instant::now();
        let answer = runtime
            .run("sys", &mut messages, &tx, &cancel, &SteeringQueue::new())
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "tool was not interrupted"
        );
        assert_eq!(answer, "");

        // 三个调用都有结果：被中断的 + 两个未执行的
        let results = messages[2].tool_results.as_ref().unwrap();
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.content == CANCELLED_BY_USER));
    }

    #[tokio::test]
    async fn test_max_turns_does_not_duplicate_messages() {
        let runtime = runtime_with(MockProvider::new(Script::AlwaysTool)).with_limits(2, 4096);

        let mut messages = vec![Message::user("hi")];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let result = runtime
            .run(
                "sys",
                &mut messages,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await;
        assert!(result.is_err());

        // user + 2 × (assistant + tool)；部分进度保留且不重复
        assert_eq!(messages.len(), 5);
        let mut ids: Vec<String> = messages
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .flatten()
            .map(|c| c.id.clone())
            .collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total, "tool_call ids must be unique");
    }

    #[tokio::test]
    async fn test_telemetry_spans_emitted() {
        let provider = MockProvider::new(Script::AnswerOnly("ok"));
        let telemetry = RecordingTelemetry::new();
        let runtime = runtime_with(provider).with_telemetry(telemetry.shared());

        let mut messages = vec![Message::user("hi")];
        drive(&runtime, &mut messages).await;

        let spans = telemetry.span_names();
        assert!(spans.contains(&"agent.run".to_string()));
        assert!(spans.contains(&"agent.turn".to_string()));
    }

    #[tokio::test]
    async fn test_confirmation_gate_denies_tool() {
        struct StrictApprover;

        #[async_trait]
        impl crate::Approver for StrictApprover {
            async fn confirm(
                &self,
                _request: crate::ConfirmationRequest,
                _cancel: &CancellationToken,
            ) -> crate::ConfirmationDecision {
                crate::ConfirmationDecision::Deny("user said no".to_string())
            }
        }

        let provider = MockProvider::new(Script::ToolThenAnswer {
            tool_id: "t1",
            tool_name: "append",
            args: r#"{"text":"x"}"#,
            answer: "ok",
        });
        let gate =
            crate::ConfirmationGate::new(vec!["append".to_string()], Arc::new(StrictApprover));
        let runtime = runtime_with(provider).with_confirmation(gate);

        let mut messages = vec![Message::user("hi")];
        drive(&runtime, &mut messages).await;

        // 工具被用户拒绝，结果回传 Deny 文案，循环继续到最终答案
        assert!(
            messages[2].tool_results.as_ref().unwrap()[0]
                .content
                .contains("[Denied by user]")
        );
        assert_eq!(messages[3].content, "ok");
    }

    #[tokio::test]
    async fn test_confirmation_allow_all_skips_reprompts() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AllowAllApprover {
            prompts: AtomicUsize,
        }

        #[async_trait]
        impl crate::Approver for AllowAllApprover {
            async fn confirm(
                &self,
                _request: crate::ConfirmationRequest,
                _cancel: &CancellationToken,
            ) -> crate::ConfirmationDecision {
                self.prompts.fetch_add(1, Ordering::SeqCst);
                crate::ConfirmationDecision::AllowAll
            }
        }

        let provider = MockProvider::new(Script::ToolThenAnswer {
            tool_id: "t1",
            tool_name: "append",
            args: r#"{"text":"x"}"#,
            answer: "ok",
        });
        let approver = Arc::new(AllowAllApprover {
            prompts: AtomicUsize::new(0),
        });
        let gate = crate::ConfirmationGate::new(vec!["append".to_string()], approver.clone());
        let runtime = runtime_with(provider).with_confirmation(gate);

        let mut messages = vec![Message::user("hi")];
        drive(&runtime, &mut messages).await;

        // 工具正常执行（AllowAll 放行），且只询问一次
        assert_eq!(
            messages[2].tool_results.as_ref().unwrap()[0].content,
            "appended:x"
        );
        assert_eq!(approver.prompts.load(Ordering::SeqCst), 1);
    }

    /// 前 `failures` 次调用：先流出半截文本，再以给定 HTTP 状态失败；之后正常回答
    struct FlakyProvider {
        status: u16,
        failures: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for FlakyProvider {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            unreachable!("runtime uses chat_stream")
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<StreamChunk>>> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let chunks: Vec<Result<StreamChunk>> = if n < self.failures {
                vec![
                    Ok(StreamChunk::Content("partial ".into())),
                    Err(anyhow::Error::new(baiji_ai::ApiError {
                        provider: "mock",
                        status: self.status,
                        body: "{}".into(),
                        retry_after: Some(Duration::from_millis(1)),
                    })),
                ]
            } else {
                vec![
                    Ok(StreamChunk::Content("full answer".into())),
                    Ok(StreamChunk::Done),
                ]
            };
            Ok(futures::stream::iter(chunks).boxed())
        }
        fn protocol(&self) -> Protocol {
            Protocol::Anthropic
        }
        fn model(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test]
    async fn test_retry_on_transient_status_resets_partial_stream() {
        let runtime = AgentRuntime::new(Arc::new(FlakyProvider {
            status: 503,
            failures: 1,
            calls: AtomicUsize::new(0),
        }));
        let mut messages = vec![Message::user("hi")];
        let events = drive(&runtime, &mut messages).await;

        assert_eq!(messages.last().unwrap().content, "full answer");
        // 半截文本作废的通知必须出现在重试的文本之前
        let restart = events
            .iter()
            .position(|e| matches!(e, AgentEvent::StreamRestarted))
            .expect("StreamRestarted emitted");
        let full = events
            .iter()
            .position(|e| matches!(e, AgentEvent::TextDelta { text } if text == "full answer"))
            .unwrap();
        assert!(restart < full);
    }

    #[tokio::test]
    async fn test_no_retry_on_client_error() {
        let provider = Arc::new(FlakyProvider {
            status: 401,
            failures: 5,
            calls: AtomicUsize::new(0),
        });
        let runtime = AgentRuntime::new(provider.clone());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut messages = vec![Message::user("hi")];
        let result = runtime
            .run(
                "sys",
                &mut messages,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("401"));
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "401 must not be retried"
        );
    }

    #[test]
    fn test_elide_old_tool_results_keeps_recent_and_pairing() {
        let tool_msg = |id: &str, size: usize| Message {
            tool_results: Some(vec![ToolResult {
                tool_call_id: id.to_string(),
                content: "x".repeat(size),
            }]),
            ..Message::tool("")
        };
        let mut convo = vec![Message::system("sys"), Message::user("go")];
        for i in 0..5 {
            convo.push(Message::assistant(format!("step {i}")));
            convo.push(tool_msg(&format!("t{i}"), 5000));
        }
        convo.push(tool_msg("small", 10)); // 第 6 条工具消息，很小

        let before = convo.len();
        let freed = elide_old_tool_results(&mut convo, None);
        assert!(freed > 0);
        assert_eq!(convo.len(), before, "message count must not change");

        let contents: Vec<&str> = convo
            .iter()
            .filter_map(|m| m.tool_results.as_ref())
            .map(|r| r[0].content.as_str())
            .collect();
        // 6 条里最早 3 条被精简，最近 3 条原样
        assert!(contents[..3].iter().all(|c| *c == ELIDED_NOTE));
        assert!(contents[3..5].iter().all(|c| c.len() == 5000));
        assert_eq!(contents[5].len(), 10);
        // id 配对不变
        let ids: Vec<&str> = convo
            .iter()
            .filter_map(|m| m.tool_results.as_ref())
            .map(|r| r[0].tool_call_id.as_str())
            .collect();
        assert_eq!(ids, vec!["t0", "t1", "t2", "t3", "t4", "small"]);
        // 幂等
        assert_eq!(elide_old_tool_results(&mut convo, None), 0);
    }

    #[test]
    fn test_elide_with_spill_is_reversible() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("ctx");
        // 生产同款 spill 实现（main.rs 注入的就是它）
        let spill: SpillFn =
            Arc::new(move |content: &str| baiji_tools::spill_to_store(&store, content));
        let original = "meaningful old tool output\n".repeat(80); // 2.2KB
        let tool_msg = |id: &str, content: String| Message {
            tool_results: Some(vec![ToolResult {
                tool_call_id: id.to_string(),
                content,
            }]),
            ..Message::tool("")
        };
        let mut convo = vec![Message::system("sys"), Message::user("go")];
        convo.push(tool_msg("t0", original.clone()));
        // 凑满 KEEP_RECENT_TOOL_MESSAGES(3) 窗口，让 t0 落入精简区
        for id in ["t1", "t2", "t3"] {
            convo.push(tool_msg(id, "recent".into()));
        }

        let freed = elide_old_tool_results(&mut convo, Some(&spill));
        assert!(freed > 0);
        let stub = &convo[2].tool_results.as_ref().unwrap()[0].content;
        assert!(stub.starts_with("[ctx stub:"), "{stub}");
        assert!(stub.contains("ctx:"), "must carry recovery handle");
        // 原文可凭句柄逐字取回
        let handle = &stub[stub.find("ctx:").unwrap() + 4..][..16];
        let retrieved = std::fs::read_to_string(dir.path().join("ctx").join(handle)).unwrap();
        assert_eq!(retrieved, original);
        // 幂等：占位已小于阈值
        assert_eq!(elide_old_tool_results(&mut convo, Some(&spill)), 0);
    }

    #[tokio::test]
    async fn test_truncated_final_answer_carries_marker() {
        let provider = Arc::new(MockProvider::new(Script::TruncatedAnswer(
            "这是一段被截断的回答",
        )));
        let runtime = AgentRuntime::new(provider).with_max_tokens(100);
        let mut history = vec![Message::user("问个长问题")];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let answer = runtime
            .run(
                "sys",
                &mut history,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        // 答案带明示标记（用户可见，模型下轮可续写）
        assert!(answer.contains("这是一段被截断的回答"), "{answer}");
        assert!(answer.contains("被截断"), "{answer}");
        assert!(answer.contains("max_tokens=100"), "{answer}");
        // 历史中的 assistant 消息同样携带标记
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, answer);

        // 正常完成的答案不带标记
        let provider = Arc::new(MockProvider::new(Script::AnswerOnly("正常回答")));
        let runtime = AgentRuntime::new(provider);
        let mut history = vec![Message::user("q")];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let answer = runtime
            .run(
                "sys",
                &mut history,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        assert_eq!(answer, "正常回答");
    }
}
