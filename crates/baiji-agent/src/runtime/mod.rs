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
use crate::runtime::helpers::{
    CANCELLED_BY_USER, SKIPPED_BY_STEERING, SpillFn, commit, elide_old_tool_results,
    invalid_args_message, rough_token_estimate, steer_last_user, sync_new_messages,
    user_input_of,
};
use crate::hooks::HookRegistry;
use crate::queue::SteeringQueue;
use crate::tool::{ToolOutput, ToolRegistry};
use anyhow::Result;
use baiji_ai::{ChatRequest, Message, Provider, Role, StopReason, ToolCall, ToolResult};
use baiji_telemetry::{AttrValue, NoopTelemetry, Telemetry, attrs};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

mod helpers;
mod streaming;

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
    pub fn with_thinking(self, thinking: Option<baiji_ai::ThinkingLevel>) -> Self {
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
            // 轮次预算告急提醒（同样仅请求副本）：倒计时 2 轮起提示收尾，
            // 最后一轮强制"只作答不再调工具"——避免跑满预算直接报
            // "达到最大迭代次数"而拿不到任何结果（子代理尤其常见）
            let remaining = self.max_turns.saturating_sub(turn);
            if remaining == 1 {
                request_messages.push(Message::user(
                    "[system notice] You have ONE turn left after this one before the hard turn \
                     limit. Wrap up now: finish the current step only, then prepare your final \
                     answer.",
                ));
            } else if remaining == 0 {
                request_messages.push(Message::user(
                    "[system notice] FINAL TURN — the turn limit is reached. Do NOT call any \
                     tools. Give your final answer NOW based on what you have gathered.",
                ));
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
}

#[cfg(test)]
mod tests;
