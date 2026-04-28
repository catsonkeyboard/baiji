use crate::agent::context::ContextManager;
use crate::agent::memory::{build_system_prompt, load_agents_md};
use crate::agent::prompt::AgentPrompt;
use crate::agent::retry::RetryPolicy;
use crate::agent::tool_policy::{PolicyDecision, ToolPolicy};
use crate::agent::trace::{self, ToolCallTrace, TraceRecorder};
use crate::agent::validator::ToolResultValidator;
use crate::llm::{
    ChatRequest, ChatResponse, LLMProvider, Message, Role, StreamChunk, ToolCall, ToolDefinition,
    ToolResult,
};
use crate::mcp::McporterBridge;
use anyhow::Result;
use futures::StreamExt;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// 用于向正在运行的 Agent 注入 Steering 消息（用户打断）
pub type SteeringQueue = Arc<std::sync::Mutex<VecDeque<String>>>;

/// Tool-use Agent
pub struct ReActAgent {
    llm: Arc<dyn LLMProvider>,
    tools: std::sync::Mutex<Vec<ToolDefinition>>,
    max_iterations: usize,
    mcporter: Option<Arc<McporterBridge>>,
    /// 工具策略引擎（Harness Engineering 架构约束）
    policy: Arc<ToolPolicy>,
}

/// Agent 事件（用于 UI 更新）
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 新一轮 LLM 调用开始
    TurnStart,
    /// 当前轮次结束
    TurnEnd,
    /// LLM 流式文本 delta
    TextDelta(String),
    /// 工具调用开始
    ToolExecutionStart { id: String, name: String, args: Value },
    /// 工具调用结束
    ToolExecutionEnd { id: String, name: String, result: String, is_error: bool },
    /// Agent 完成（最终答案）
    Completed(String),
    /// 被用户中断
    Interrupted,
    /// 错误
    Error(String),
    /// MCP 服务器发现成功
    McpReady { server: String, tools: Vec<ToolDefinition> },
    /// MCP 服务器发现失败
    McpFailed { server: String, error: String },
}

impl ReActAgent {
    pub fn new(llm: Arc<dyn LLMProvider>) -> Self {
        use crate::agent::tool_policy::PolicyConfig;
        Self {
            llm,
            tools: std::sync::Mutex::new(Vec::new()),
            max_iterations: 5,
            mcporter: None,
            policy: Arc::new(ToolPolicy::from_config(&PolicyConfig::default())),
        }
    }

    pub fn with_tools(self, tools: Vec<ToolDefinition>) -> Self {
        *self.tools.lock().unwrap() = tools;
        self
    }

    pub fn with_mcporter(mut self, mcporter: Arc<McporterBridge>) -> Self {
        self.mcporter = Some(mcporter);
        self
    }

    pub fn with_max_iterations(mut self, max: usize) -> Self {
        self.max_iterations = max;
        self
    }

    pub fn with_policy(mut self, policy: ToolPolicy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    /// 动态追加工具（MCP 后台发现后调用）
    pub fn add_tools(&self, new_tools: Vec<ToolDefinition>) {
        self.tools.lock().unwrap().extend(new_tools);
    }

    /// 运行 Agent（pi-mono 风格：内外双循环 + Steering + 流式输出）
    ///
    /// - Inner loop：处理工具调用 + Steering 消息，直到 LLM 返回纯文本
    /// - Outer loop：预留 follow-up 扩展（当前直接退出）
    /// - Steering：工具执行期间检测用户打断，跳过剩余工具并注入新消息
    /// - 取消：通过 CancellationToken 支持 Escape 中止
    pub async fn run(
        &self,
        history: Vec<Message>,
        question: &str,
        tx: UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
        steering: SteeringQueue,
    ) -> Result<String> {
        info!("Agent starting: {}", question);

        let agents_md = load_agents_md().await;
        let base_prompt = if self.tools.lock().unwrap().is_empty() {
            "You are a helpful AI assistant.".to_string()
        } else {
            AgentPrompt::system_prompt()
        };
        let system_prompt = build_system_prompt(&base_prompt, agents_md);

        let mut messages = vec![Message {
            role: Role::System,
            content: system_prompt,
            tool_calls: None,
            tool_results: None,
        }];
        messages.extend(history);

        let mut final_answer = String::new();
        let mut turn_count = 0;
        let mut context_mgr = ContextManager::default();
        let mut tracer = TraceRecorder::new();

        // Outer loop：预留 follow-up 扩展
        'outer: loop {
            let mut has_tool_calls = true;
            let mut pending_steering = drain_steering_queue(&steering);

            // Inner loop：处理工具调用和 Steering
            while has_tool_calls || !pending_steering.is_empty() {
                turn_count += 1;
                if turn_count > self.max_iterations {
                    warn!("Max iterations ({}) reached", self.max_iterations);
                    let err = format!("达到最大迭代次数 ({})", self.max_iterations);
                    tx.send(AgentEvent::Error(err.clone())).ok();
                    return Err(anyhow::anyhow!("Max iterations reached"));
                }

                if cancel.is_cancelled() {
                    tx.send(AgentEvent::Interrupted).ok();
                    return Ok(String::new());
                }

                tx.send(AgentEvent::TurnStart).ok();

                // 注入 Steering 消息到上下文
                for msg in pending_steering.drain(..) {
                    info!("Steering message: {}", msg);
                    messages.push(Message::user(msg));
                }

                // 智能上下文管理（替代简单截断）
                context_mgr.trim(&mut messages);

                let estimated_tokens = context_mgr.estimate_tokens(&messages);
                tracer.start_turn(turn_count, messages.len(), estimated_tokens);

                let tools_snapshot = self.tools.lock().unwrap().clone();
                let request = ChatRequest::new(messages.clone())
                    .with_tools(tools_snapshot)
                    .with_max_tokens(4096);

                debug!("LLM request (turn {}, ~{} tokens)", turn_count, estimated_tokens);

                // 流式获取 LLM 响应，带重试保护
                let retry_policy = RetryPolicy::default();
                let mut llm_attempt = 0u32;
                let response = loop {
                    match self.stream_llm_response(request.clone(), &tx, &cancel).await {
                        Ok(Some(r)) => break r,
                        Ok(None) => {
                            tx.send(AgentEvent::Interrupted).ok();
                            return Ok(String::new());
                        }
                        Err(e) => {
                            let decision = retry_policy.should_retry(llm_attempt, &e.to_string());
                            match decision {
                                crate::agent::retry::RetryDecision::Retry { delay, attempt } => {
                                    warn!(
                                        "LLM call failed (attempt {}/{}): {}. Retrying in {:?}...",
                                        llm_attempt + 1,
                                        retry_policy.max_retries + 1,
                                        e,
                                        delay
                                    );
                                    tokio::time::sleep(delay).await;
                                    llm_attempt = attempt;
                                    continue;
                                }
                                crate::agent::retry::RetryDecision::Fail => {
                                    return Err(e);
                                }
                            }
                        }
                    }
                };

                tracer.record_llm_complete();

                debug!(
                    "LLM response: {} chars, {} tool_calls",
                    response.content.len(),
                    response.tool_calls.as_ref().map(|t| t.len()).unwrap_or(0)
                );

                // 处理工具调用（顺序执行，支持 Steering 打断）
                if let Some(ref tool_calls) = response.tool_calls {
                    if !tool_calls.is_empty() {
                        let mut tool_results = Vec::new();
                        let mut steering_found = false;

                        for tool_call in tool_calls {
                            tx.send(AgentEvent::ToolExecutionStart {
                                id: tool_call.id.clone(),
                                name: tool_call.name.clone(),
                                args: tool_call.arguments.clone(),
                            })
                            .ok();

                            info!("Executing tool: {}", tool_call.name);
                            let tool_start = Instant::now();
                            let (result, is_error) = self
                                .execute_tool(&tool_call.name, tool_call.arguments.clone())
                                .await;
                            let tool_latency = tool_start.elapsed().as_millis() as u64;

                            // 验证工具结果（Harness Engineering 反馈循环）
                            let validation = ToolResultValidator::validate(
                                &tool_call.name, &result, is_error,
                            );
                            let result = if let Some(observation) =
                                ToolResultValidator::to_observation(&validation)
                            {
                                debug!("Tool validation: {:?} -> {}", validation, observation);
                                format!("{}\n{}", result, observation)
                            } else {
                                result
                            };

                            tx.send(AgentEvent::ToolExecutionEnd {
                                id: tool_call.id.clone(),
                                name: tool_call.name.clone(),
                                result: result.clone(),
                                is_error,
                            })
                            .ok();

                            tool_results.push(ToolResult {
                                tool_call_id: tool_call.id.clone(),
                                content: result.clone(),
                            });

                            // 记录工具调用追踪
                            tracer.record_tool_call(ToolCallTrace {
                                tool_name: tool_call.name.clone(),
                                latency_ms: tool_latency,
                                input_size: tool_call.arguments.to_string().len(),
                                output_size: result.len(),
                                success: !is_error,
                                retried: false,
                            });

                            // 每个工具执行后检查 Steering，发现则跳过剩余工具
                            let new_steering = drain_steering_queue(&steering);
                            if !new_steering.is_empty() {
                                info!("Steering detected, skipping remaining tools");
                                pending_steering = new_steering;
                                steering_found = true;
                                break;
                            }
                        }

                        // 添加 assistant 消息（含工具调用）
                        messages.push(Message {
                            role: Role::Assistant,
                            content: response.content.clone(),
                            tool_calls: Some(tool_calls.clone()),
                            tool_results: None,
                        });

                        // 添加工具结果消息
                        messages.push(Message {
                            role: Role::Tool,
                            content: String::new(),
                            tool_calls: None,
                            tool_results: Some(tool_results),
                        });

                        has_tool_calls = !steering_found;
                        tracer.end_turn();
                        tx.send(AgentEvent::TurnEnd).ok();
                        continue;
                    }
                }

                // 无工具调用 — 最终答案
                has_tool_calls = false;
                let answer = response.content.clone();

                if answer.is_empty() {
                    warn!("LLM returned empty response");
                    tx.send(AgentEvent::Error("LLM 返回了空响应".to_string())).ok();
                    return Err(anyhow::anyhow!("LLM returned empty response"));
                }

                final_answer = answer;
                messages.push(Message::assistant(&final_answer));
                tracer.end_turn();
                tx.send(AgentEvent::TurnEnd).ok();

                // 检查是否有 Steering 消息（允许最终答案后继续）
                pending_steering = drain_steering_queue(&steering);
            }

            break 'outer;
        }

        info!("Agent completed in {} turns", turn_count);

        // 保存追踪记录
        let agent_trace = tracer.finish();
        tokio::spawn(async move {
            trace::save_trace(&agent_trace).await;
        });

        tx.send(AgentEvent::Completed(final_answer.clone())).ok();
        Ok(final_answer)
    }

    /// 流式调用 LLM，边收流边推送 TextDelta 事件，同时收集完整响应。
    /// 若被 cancel，返回 None。
    async fn stream_llm_response(
        &self,
        request: ChatRequest,
        tx: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<Option<ChatResponse>> {
        let mut stream = self.llm.chat_stream(request).await?;
        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        // (id, name, accumulated_args_json)
        let mut current_tool: Option<(String, String, String)> = None;
        let mut in_tool_call = false;

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Ok(None);
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(StreamChunk::Content(t))) => {
                            if in_tool_call {
                                // Content after ToolCallStart = tool argument JSON fragments
                                if let Some((_, _, ref mut args)) = current_tool {
                                    args.push_str(&t);
                                }
                            } else if !t.is_empty() {
                                text.push_str(&t);
                                tx.send(AgentEvent::TextDelta(t)).ok();
                            }
                        }
                        Some(Ok(StreamChunk::ToolCallStart { id, name })) => {
                            // Finalize previous tool call if any
                            if let Some((prev_id, prev_name, prev_args)) = current_tool.take() {
                                let args = serde_json::from_str(&prev_args)
                                    .unwrap_or(serde_json::json!({}));
                                tool_calls.push(ToolCall { id: prev_id, name: prev_name, arguments: args });
                            }
                            current_tool = Some((id, name, String::new()));
                            in_tool_call = true;
                        }
                        Some(Ok(StreamChunk::ToolCallArguments { id: _, arguments })) => {
                            if let Some((_, _, ref mut args)) = current_tool {
                                args.push_str(&arguments);
                            }
                        }
                        Some(Ok(StreamChunk::Done)) | None => {
                            // Finalize last tool call
                            if let Some((id, name, args)) = current_tool.take() {
                                let parsed = serde_json::from_str(&args)
                                    .unwrap_or(serde_json::json!({}));
                                tool_calls.push(ToolCall { id, name, arguments: parsed });
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

        Ok(Some(ChatResponse {
            content: text,
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            usage: None,
        }))
    }

    /// 执行工具，返回 (结果, 是否出错)
    async fn execute_tool(&self, tool_name: &str, arguments: Value) -> (String, bool) {
        // 策略检查：是否允许执行该工具
        match self.policy.check_tool(tool_name) {
            PolicyDecision::Deny(reason) => {
                warn!("Tool '{}' blocked by policy: {}", tool_name, reason);
                return (format!("[Blocked] {}", reason), true);
            }
            PolicyDecision::RequireConfirmation => {
                info!("Tool '{}' requires confirmation (auto-approved in current version)", tool_name);
                // TODO: 未来通过 AgentEvent 请求用户确认
            }
            PolicyDecision::Allow => {}
        }

        if tool_name.starts_with("builtin__") {
            let result = super::builtin_tools::execute_builtin_tool(
                tool_name, &arguments, &self.policy,
            ).await;
            return (result, false);
        }
        if let Some(ref bridge) = self.mcporter {
            match bridge.execute_tool(tool_name, arguments).await {
                Ok(result) => {
                    // 策略截断 MCP 工具的输出
                    let result = self.policy.truncate_output(&result);
                    (result, false)
                }
                Err(e) => {
                    warn!("Tool '{}' failed: {}", tool_name, e);
                    (format!("Tool execution failed: {}", e), true)
                }
            }
        } else {
            (format!("[Mock] Tool '{}' called", tool_name), false)
        }
    }

    /// 裁剪上下文（已废弃，由 ContextManager 替代）
    #[allow(dead_code)]
    fn trim_context(&self, messages: &mut Vec<Message>) {
        let max_messages = 30;
        let keep_recent = 10;

        if messages.len() <= max_messages {
            return;
        }

        info!("Trimming context from {} messages", messages.len());
        let core = messages.drain(0..2).collect::<Vec<_>>();
        let start = messages.len().saturating_sub(keep_recent);
        let recent = messages.drain(start..).collect::<Vec<_>>();
        messages.clear();
        messages.extend(core);
        messages.extend(recent);
        debug!("Context trimmed to {} messages", messages.len());
    }
}

fn drain_steering_queue(steering: &SteeringQueue) -> Vec<String> {
    steering.lock().unwrap().drain(..).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ProviderFactory;

    fn make_agent() -> ReActAgent {
        ReActAgent::new(
            ProviderFactory::create(
                "anthropic",
                "https://api.anthropic.com".to_string(),
                "test".to_string(),
                "claude-3-5-sonnet-20241022".to_string(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn test_agent_creation() {
        let agent = make_agent();
        assert!(agent.mcporter.is_none());
        assert!(agent.tools.lock().unwrap().is_empty());
    }

    #[test]
    fn test_agent_with_tools() {
        let tools = vec![ToolDefinition {
            name: "search".to_string(),
            description: "Search the web".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let agent = make_agent().with_tools(tools);
        assert_eq!(agent.tools.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_agent_max_iterations() {
        let agent = make_agent().with_max_iterations(5);
        assert_eq!(agent.max_iterations, 5);
    }

    #[test]
    fn test_steering_queue() {
        let queue: SteeringQueue = Arc::new(std::sync::Mutex::new(VecDeque::new()));
        queue.lock().unwrap().push_back("test message".to_string());
        let drained = drain_steering_queue(&queue);
        assert_eq!(drained, vec!["test message"]);
        assert!(drain_steering_queue(&queue).is_empty());
    }
}
