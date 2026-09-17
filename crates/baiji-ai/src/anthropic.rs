//! Anthropic Messages 协议实现
//!
//! 对应 `POST {base_url}/v1/messages`，SSE 流式。
//! 工具调用块按 index 顺序流式传输：
//! `content_block_start`(tool_use) 携带 id/name，随后
//! `content_block_delta`(input_json_delta) 按同一 index 增量下发参数。

use crate::provider::{Protocol, Provider};
use crate::types::{ChatRequest, ChatResponse, Message, Role, StreamChunk, TokenUsage, ToolCall};
use anyhow::{Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// Anthropic API 版本
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Provider
pub struct AnthropicProvider {
    client: Arc<Client>,
    base_url: String,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Arc::new(crate::error::http_client()),
            base_url,
            api_key,
            model,
        }
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    /// 转换消息格式为 Anthropic 格式。
    /// 多条 System 消息（如压缩摘要）以空行拼接为单个 system 字段。
    fn convert_messages(messages: &[Message]) -> (Option<String>, Vec<AnthropicMessage>) {
        let mut system_parts: Vec<String> = Vec::new();
        let mut anthropic_messages = Vec::new();
        let turn_start = crate::types::current_turn_start(messages);

        for (position, msg) in messages.iter().enumerate() {
            match msg.role {
                Role::System => system_parts.push(msg.content.clone()),
                // Anthropic 拒绝空/纯空白的 text block（400），必须跳过
                Role::User => {
                    if !msg.content.trim().is_empty() {
                        anthropic_messages.push(AnthropicMessage {
                            role: "user".to_string(),
                            content: vec![ContentBlock::text(&msg.content)],
                        });
                    }
                }
                Role::Assistant => {
                    let mut content = Vec::new();
                    // 当前轮工具循环内的 thinking block 原样回传，且必须排在最前
                    if position > turn_start && msg.tool_calls.is_some() {
                        // 带 id 的块来自 OpenAI Responses 协议，不属于本协议
                        for block in msg.reasoning.iter().flatten().filter(|b| b.id.is_none()) {
                            if let Some(data) = &block.redacted {
                                content.push(ContentBlock::redacted_thinking(data));
                            } else if !block.text.is_empty() {
                                content.push(ContentBlock::thinking(
                                    &block.text,
                                    block.signature.as_deref(),
                                ));
                            }
                        }
                    }
                    if !msg.content.trim().is_empty() {
                        content.push(ContentBlock::text(&msg.content));
                    }
                    if let Some(tool_calls) = &msg.tool_calls {
                        for tool_call in tool_calls {
                            content.push(ContentBlock::tool_use(
                                &tool_call.id,
                                &tool_call.name,
                                tool_call.arguments.clone(),
                            ));
                        }
                    }
                    if !content.is_empty() {
                        anthropic_messages.push(AnthropicMessage {
                            role: "assistant".to_string(),
                            content,
                        });
                    }
                }
                Role::Tool => {
                    if let Some(tool_results) = &msg.tool_results {
                        let blocks: Vec<ContentBlock> = tool_results
                            .iter()
                            .map(|tr| ContentBlock::tool_result(&tr.tool_call_id, &tr.content))
                            .collect();
                        anthropic_messages.push(AnthropicMessage {
                            role: "user".to_string(),
                            content: blocks,
                        });
                    }
                }
            }
        }

        (
            (!system_parts.is_empty()).then(|| system_parts.join("\n\n")),
            anthropic_messages,
        )
    }

    fn build_request_body(request: &ChatRequest, model: &str, stream: bool) -> AnthropicRequest {
        let (system, messages) = Self::convert_messages(&request.messages);

        AnthropicRequest {
            model: model.to_string(),
            max_tokens: request.max_tokens.unwrap_or(4096),
            temperature: request.temperature,
            system,
            messages,
            tools: request.tools.as_ref().map(|tools| {
                tools
                    .iter()
                    .map(|t| AnthropicTool {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        input_schema: t.parameters.clone(),
                    })
                    .collect()
            }),
            stream,
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = Self::build_request_body(&request, &self.model, false);

        let response = self
            .client
            .post(self.messages_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Anthropic API")?;

        if !response.status().is_success() {
            return Err(crate::error::ApiError::from_response("Anthropic", response)
                .await
                .into());
        }

        let anthropic_response: AnthropicResponse = response
            .json()
            .await
            .context("Failed to parse Anthropic response")?;

        let mut content = String::new();
        let mut tool_calls = Vec::new();

        for block in anthropic_response.content {
            match block.block_type.as_str() {
                "text" => content.push_str(&block.text.unwrap_or_default()),
                "tool_use" => {
                    if let (Some(id), Some(name), Some(input)) = (block.id, block.name, block.input)
                    {
                        tool_calls.push(ToolCall {
                            id,
                            name,
                            arguments: input,
                        });
                    }
                }
                _ => {}
            }
        }

        Ok(ChatResponse {
            content,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            usage: Some(TokenUsage {
                input_tokens: anthropic_response.usage.input_tokens,
                output_tokens: anthropic_response.usage.output_tokens,
            }),
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<StreamChunk>>> {
        let body = Self::build_request_body(&request, &self.model, true);
        let url = self.messages_url();

        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to send streaming request to {}", url))?;

        if !response.status().is_success() {
            return Err(crate::error::ApiError::from_response("Anthropic", response)
                .await
                .into());
        }

        let stream = response
            .bytes_stream()
            .eventsource()
            .map(|ev| ev.map(|e| Some(e.data)).map_err(|e| e.to_string()))
            .chain(futures::stream::once(futures::future::ready(Ok(None))))
            .scan(AnthropicStreamState::default(), |state, data| {
                // None 哨兵 = SSE 流自然结束（服务端未发 message_stop 也不至于挂起）
                let result: Result<Vec<StreamChunk>> = match data {
                    Ok(Some(d)) => Ok(state.process_data(&d)),
                    Ok(None) => Ok(state.process_end()),
                    Err(e) => Err(anyhow::anyhow!("SSE error: {}", e)),
                };
                futures::future::ready(Some(result))
            })
            .flat_map(|result| {
                futures::stream::iter(match result {
                    Ok(chunks) => chunks.into_iter().map(Ok).collect::<Vec<_>>(),
                    Err(e) => vec![Err(e)],
                })
            })
            .boxed();

        Ok(stream)
    }

    fn protocol(&self) -> Protocol {
        Protocol::Anthropic
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "anthropic"
    }
}

// ========== 流式状态机 ==========

/// Anthropic SSE 事件状态机：
/// - content_block_start(tool_use) → 记录 index→id 并下发 ToolCallStart
/// - content_block_delta(input_json_delta) → 按同一 index 下发 ToolCallArguments
/// - message_stop → Done
#[derive(Debug, Default)]
pub struct AnthropicStreamState {
    /// content block index → tool_use id
    block_ids: Vec<Option<String>>,
    /// message_delta 携带的 stop_reason（message_stop 时随 Done 一并发出）
    stop_reason: Option<String>,
    usage: TokenUsage,
    finished: bool,
}

impl AnthropicStreamState {
    pub fn process_data(&mut self, data: &str) -> Vec<StreamChunk> {
        if self.finished {
            return Vec::new();
        }

        let event: StreamEvent = match serde_json::from_str(data) {
            Ok(e) => e,
            Err(e) => return vec![StreamChunk::Error(format!("Parse error: {}", e))],
        };

        match event.event_type.as_str() {
            "message_start" => {
                // 输入 token 数在 message_start，输出 token 数在 message_delta
                if let Some(usage) = event.message.and_then(|m| m.usage) {
                    self.usage.input_tokens = usage.input_tokens;
                    self.usage.output_tokens = usage.output_tokens;
                }
                Vec::new()
            }
            "content_block_start" => {
                if let Some(block) = event.content_block {
                    if block.block_type == "redacted_thinking" {
                        return block
                            .data
                            .map(|d| vec![StreamChunk::ReasoningRedacted(d)])
                            .unwrap_or_default();
                    }
                    if block.block_type == "tool_use" {
                        if let (Some(id), Some(name)) = (block.id.clone(), block.name.clone()) {
                            let index =
                                event.index.unwrap_or(self.block_ids.len() as u32) as usize;
                            while self.block_ids.len() <= index {
                                self.block_ids.push(None);
                            }
                            self.block_ids[index] = Some(id.clone());
                            return vec![StreamChunk::ToolCallStart { id, name }];
                        }
                    }
                }
                Vec::new()
            }
            "content_block_delta" => {
                if let Some(delta) = event.delta {
                    match delta.delta_type.as_str() {
                        "text_delta" => match delta.text {
                            Some(text) if !text.is_empty() => vec![StreamChunk::Content(text)],
                            _ => Vec::new(),
                        },
                        "thinking_delta" => match delta.thinking {
                            Some(text) if !text.is_empty() => vec![StreamChunk::Reasoning(text)],
                            _ => Vec::new(),
                        },
                        "signature_delta" => match delta.signature {
                            Some(sig) if !sig.is_empty() => {
                                vec![StreamChunk::ReasoningSignature(sig)]
                            }
                            _ => Vec::new(),
                        },
                        "input_json_delta" => match delta.partial_json {
                            Some(json) if !json.is_empty() => {
                                let id = event
                                    .index
                                    .and_then(|i| self.block_ids.get(i as usize))
                                    .cloned()
                                    .flatten()
                                    .unwrap_or_default();
                                vec![StreamChunk::ToolCallArguments { id, arguments: json }]
                            }
                            _ => Vec::new(),
                        },
                        _ => Vec::new(),
                    }
                } else {
                    Vec::new()
                }
            }
            "message_delta" => {
                if let Some(reason) = event.delta.and_then(|d| d.stop_reason) {
                    self.stop_reason = Some(reason);
                }
                if let Some(usage) = event.usage {
                    self.usage.output_tokens = usage.output_tokens;
                    // 部分兼容端点只在 message_delta 给出 input_tokens
                    if usage.input_tokens > 0 {
                        self.usage.input_tokens = usage.input_tokens;
                    }
                }
                Vec::new()
            }
            "message_stop" => {
                self.finished = true;
                self.done_chunks()
            }
            "error" => {
                self.finished = true;
                let message = event
                    .error
                    .and_then(|e| e.message)
                    .unwrap_or_else(|| "Anthropic stream error".to_string());
                vec![StreamChunk::Error(message)]
            }
            _ => Vec::new(),
        }
    }

    /// SSE 流自然结束（即使未收到 message_stop）时的兜底
    pub fn process_end(&mut self) -> Vec<StreamChunk> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        self.done_chunks()
    }

    fn done_chunks(&mut self) -> Vec<StreamChunk> {
        let mut out = Vec::new();
        if self.usage != TokenUsage::default() {
            out.push(StreamChunk::Usage(self.usage));
        }
        if let Some(reason) = self.stop_reason.take() {
            out.push(StreamChunk::Stop(crate::types::StopReason::parse(&reason)));
        }
        out.push(StreamChunk::Done);
        out
    }
}

// ========== Anthropic API 数据结构 ==========

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<ContentBlock>,
}

#[derive(Debug, Serialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Vec<ToolResultContent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    /// redacted_thinking 的密文
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
}

impl ContentBlock {
    fn thinking(text: &str, signature: Option<&str>) -> Self {
        Self {
            thinking: Some(text.to_string()),
            signature: signature.map(String::from),
            ..Self::empty("thinking")
        }
    }

    fn redacted_thinking(data: &str) -> Self {
        Self {
            data: Some(data.to_string()),
            ..Self::empty("redacted_thinking")
        }
    }

    fn empty(block_type: &'static str) -> Self {
        Self {
            block_type,
            text: None,
            id: None,
            name: None,
            input: None,
            tool_use_id: None,
            content: None,
            thinking: None,
            signature: None,
            data: None,
        }
    }

    fn text(content: &str) -> Self {
        Self {
            block_type: "text",
            text: Some(content.to_string()),
            id: None,
            name: None,
            input: None,
            tool_use_id: None,
            content: None,
            thinking: None,
            signature: None,
            data: None,
        }
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Self {
        Self {
            block_type: "tool_use",
            text: None,
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            input: Some(input),
            tool_use_id: None,
            content: None,
            thinking: None,
            signature: None,
            data: None,
        }
    }

    fn tool_result(tool_use_id: &str, content: &str) -> Self {
        Self {
            block_type: "tool_result",
            text: None,
            id: None,
            name: None,
            input: None,
            tool_use_id: Some(tool_use_id.to_string()),
            content: Some(vec![ToolResultContent {
                content_type: "text",
                text: content.to_string(),
            }]),
            thinking: None,
            signature: None,
            data: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct ToolResultContent {
    #[serde(rename = "type")]
    content_type: &'static str,
    text: String,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: Value,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ResponseContentBlock>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
struct ResponseContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    /// redacted_thinking 的密文
    #[serde(default)]
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    content_block: Option<ResponseContentBlock>,
    #[serde(default)]
    error: Option<ApiError>,
    /// message_start 事件
    #[serde(default)]
    message: Option<StreamMessageStart>,
    /// message_delta 事件
    #[serde(default)]
    usage: Option<StreamUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamMessageStart {
    #[serde(default)]
    usage: Option<StreamUsage>,
}

/// 流式事件里的 usage：字段可能缺失，全部宽松解析
#[derive(Debug, Deserialize)]
struct StreamUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(rename = "type", default)]
    delta_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    /// message_delta 事件
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    signature: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolResult};

    #[test]
    fn test_build_request_body_shapes() {
        let mut messages = vec![Message::system("Be helpful")];
        messages.push(Message::user("hi"));
        messages.push(
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "tu_1".to_string(),
                    name: "grep".to_string(),
                    arguments: serde_json::json!({"pattern": "foo"}),
                }]),
                tool_results: None,
                reasoning: None,
            },
        );
        messages.push(
            Message {
                role: Role::Tool,
                content: String::new(),
                tool_calls: None,
                tool_results: Some(vec![ToolResult {
                    tool_call_id: "tu_1".to_string(),
                    content: "a.rs:1:foo".to_string(),
                }]),
                reasoning: None,
            },
        );

        let request = ChatRequest::new(messages).with_max_tokens(1024);
        let body = serde_json::to_value(AnthropicProvider::build_request_body(
            &request, "claude-x", true,
        ))
        .unwrap();

        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["system"], "Be helpful");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["stream"], true);
        // 纯工具调用的 assistant 回合不得带空 text block（Anthropic 返回 400）
        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"].as_array().unwrap().len(), 1);
        assert_eq!(assistant["content"][0]["type"], "tool_use");
        // 工具结果作为 user 消息的 tool_result 块
        let last = &body["messages"][2];
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["tool_use_id"], "tu_1");
    }

    #[test]
    fn test_parse_chat_response() {
        let raw = r#"{
            "content": [
                {"type": "text", "text": "Answer"},
                {"type": "tool_use", "id": "tu_1", "name": "grep", "input": {"pattern": "foo"}}
            ],
            "usage": {"input_tokens": 3, "output_tokens": 7}
        }"#;
        let resp: AnthropicResponse = serde_json::from_str(raw).unwrap();

        let mut content = String::new();
        let mut tool_calls = Vec::new();
        for block in resp.content {
            match block.block_type.as_str() {
                "text" => content.push_str(&block.text.unwrap_or_default()),
                "tool_use" => tool_calls.push(ToolCall {
                    id: block.id.unwrap(),
                    name: block.name.unwrap(),
                    arguments: block.input.unwrap(),
                }),
                _ => {}
            }
        }
        assert_eq!(content, "Answer");
        assert_eq!(tool_calls[0].id, "tu_1");
    }

    #[test]
    fn test_stream_state_tool_use_flow() {
        let mut state = AnthropicStreamState::default();

        let chunks = state.process_data(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"grep","input":{}}}"#,
        );
        assert_eq!(
            chunks,
            vec![StreamChunk::ToolCallStart {
                id: "tu_1".to_string(),
                name: "grep".to_string(),
            }]
        );

        // 参数增量按 index 关联回 id
        let chunks = state.process_data(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\""}}"#,
        );
        assert_eq!(
            chunks,
            vec![StreamChunk::ToolCallArguments {
                id: "tu_1".to_string(),
                arguments: "{\"pattern\"".to_string(),
            }]
        );

        let chunks =
            state.process_data(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#);
        assert_eq!(chunks, vec![StreamChunk::Content("ok".to_string())]);

        let chunks = state.process_data(r#"{"type":"message_stop"}"#);
        assert_eq!(chunks, vec![StreamChunk::Done]);

        // Done 之后忽略后续事件
        assert!(state.process_data(r#"{"type":"message_stop"}"#).is_empty());
    }

    #[test]
    fn test_stream_state_end_sentinel() {
        let mut state = AnthropicStreamState::default();
        assert_eq!(
            state.process_end(), // 流自然结束哨兵
            vec![StreamChunk::Done]
        );
    }

    #[test]
    fn test_stream_state_reports_stop_reason() {
        let mut state = AnthropicStreamState::default();
        let chunks = state.process_data(
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":4096}}"#,
        );
        assert!(chunks.is_empty());
        assert_eq!(
            state.process_data(r#"{"type":"message_stop"}"#),
            vec![
                StreamChunk::Usage(TokenUsage { input_tokens: 0, output_tokens: 4096 }),
                StreamChunk::Stop(crate::types::StopReason::MaxTokens),
                StreamChunk::Done
            ]
        );
    }

    #[test]
    fn test_stream_state_thinking_and_usage() {
        let mut state = AnthropicStreamState::default();
        state.process_data(r#"{"type":"message_start","message":{"usage":{"input_tokens":120,"output_tokens":1}}}"#);
        assert_eq!(
            state.process_data(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me see"}}"#
            ),
            vec![StreamChunk::Reasoning("let me see".to_string())]
        );
        assert_eq!(
            state.process_data(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig=="}}"#
            ),
            vec![StreamChunk::ReasoningSignature("sig==".to_string())]
        );
        state.process_data(r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":45}}"#);
        assert_eq!(
            state.process_data(r#"{"type":"message_stop"}"#),
            vec![
                StreamChunk::Usage(TokenUsage { input_tokens: 120, output_tokens: 45 }),
                StreamChunk::Stop(crate::types::StopReason::ToolUse),
                StreamChunk::Done
            ]
        );
    }

    #[test]
    fn test_thinking_replayed_only_in_current_tool_loop() {
        let thinking = Some(vec![crate::types::ReasoningBlock {
            text: "plan".to_string(),
            signature: Some("sig".to_string()),
            redacted: None,
            id: None,
        }]);
        let call = |id: &str| {
            Some(vec![ToolCall {
                id: id.to_string(),
                name: "grep".to_string(),
                arguments: serde_json::json!({}),
            }])
        };
        let result = |id: &str| Message {
            tool_results: Some(vec![ToolResult {
                tool_call_id: id.to_string(),
                content: "ok".to_string(),
            }]),
            reasoning: None,
            ..Message::tool("")
        };
        let messages = vec![
            Message::user("old turn"),
            Message { tool_calls: call("t1"), reasoning: thinking.clone(), ..Message::assistant("") },
            result("t1"),
            Message::assistant("old answer"),
            Message::user("new turn"),
            Message { tool_calls: call("t2"), reasoning: thinking, ..Message::assistant("") },
            result("t2"),
        ];
        let body = serde_json::to_value(AnthropicProvider::build_request_body(
            &ChatRequest::new(messages),
            "m",
            true,
        ))
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        // 旧轮次：不带 thinking
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        // 当前轮：thinking 在最前，签名原样
        assert_eq!(msgs[5]["content"][0]["type"], "thinking");
        assert_eq!(msgs[5]["content"][0]["thinking"], "plan");
        assert_eq!(msgs[5]["content"][0]["signature"], "sig");
        assert_eq!(msgs[5]["content"][1]["type"], "tool_use");
    }
}
