//! streaming：LLM 流式调用与重试（AgentRuntime 的流式子层）。
//! stream_with_retry / stream_llm_response 与 TurnResponse 从 mod.rs 拆出。

use crate::event::AgentEvent;
use crate::runtime::helpers::retry_delay;
use crate::runtime::AgentRuntime;
use anyhow::Result;
use baiji_ai::{ChatRequest, ReasoningBlock, StopReason, StreamChunk, ToolCall, TokenUsage};
use futures::StreamExt;
use std::collections::HashSet;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tracing::warn;

impl AgentRuntime {
    /// 流式 LLM 调用，瞬时错误自动重试（指数退避）
    pub(super) async fn stream_with_retry(
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
    pub(super) async fn stream_llm_response(
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
pub(super) struct TurnResponse {
    /// 厂商上报的本次调用上下文占用（input + output）
    pub(super) context_tokens: Option<usize>,
    pub(super) content: String,
    pub(super) reasoning: Option<Vec<ReasoningBlock>>,
    pub(super) tool_calls: Option<Vec<ToolCall>>,
    /// 参数 JSON 无效的工具调用 id（不执行）
    pub(super) invalid_args: HashSet<String>,
    pub(super) stop: Option<StopReason>,
}

