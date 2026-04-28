use crate::llm::provider::LLMProvider;
use crate::llm::types::{
    ChatRequest, ChatResponse, Message, Role, StreamChunk, TokenUsage, ToolCall, ToolDefinition,
};
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
    /// 创建新的 Anthropic Provider
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Arc::new(Client::new()),
            base_url,
            api_key,
            model,
        }
    }

    /// 转换消息格式为 Anthropic 格式
    fn convert_messages(&self, messages: &[Message]) -> (Option<String>, Vec<AnthropicMessage>) {
        let mut system = None;
        let mut anthropic_messages = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => {
                    system = Some(msg.content.clone());
                }
                Role::User => {
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: vec![ContentBlock::text(&msg.content)],
                    });
                }
                Role::Assistant => {
                    let mut content = vec![ContentBlock::text(&msg.content)];

                    // 添加工具调用
                    if let Some(tool_calls) = &msg.tool_calls {
                        for tool_call in tool_calls {
                            content.push(ContentBlock::tool_use(
                                &tool_call.id,
                                &tool_call.name,
                                tool_call.arguments.clone(),
                            ));
                        }
                    }

                    anthropic_messages.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content,
                    });
                }
                Role::Tool => {
                    // 工具结果作为 user 消息中的 tool_result 块
                    if let Some(tool_results) = &msg.tool_results {
                        let tool_result_content: Vec<ContentBlock> = tool_results
                            .iter()
                            .map(|tr| ContentBlock::tool_result(&tr.tool_call_id, &tr.content))
                            .collect();

                        anthropic_messages.push(AnthropicMessage {
                            role: "user".to_string(),
                            content: tool_result_content,
                        });
                    }
                }
            }
        }

        (system, anthropic_messages)
    }

    /// 转换工具定义为 Anthropic 格式
    fn convert_tools(&self, tools: &[ToolDefinition]) -> Vec<AnthropicTool> {
        tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.parameters.clone(),
            })
            .collect()
    }

    /// 构建请求体
    fn build_request_body(&self, request: &ChatRequest, stream: bool) -> AnthropicRequest {
        let (system, messages) = self.convert_messages(&request.messages);

        AnthropicRequest {
            model: self.model.clone(),
            max_tokens: request.max_tokens.unwrap_or(4096),
            temperature: request.temperature,
            system,
            messages,
            tools: request.tools.as_ref().map(|t| self.convert_tools(t)),
            stream,
        }
    }
}

#[async_trait]
impl LLMProvider for AnthropicProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = self.build_request_body(&request, false);

        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Anthropic API")?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow::anyhow!("Anthropic API error: {}", error_text));
        }

        let anthropic_response: AnthropicResponse = response
            .json()
            .await
            .context("Failed to parse Anthropic response")?;

        // 解析响应内容
        let mut content = String::new();
        let mut tool_calls = Vec::new();

        for block in anthropic_response.content {
            match block.block_type.as_str() {
                "text" => {
                    content.push_str(&block.text.unwrap_or_default());
                }
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
        let body = self.build_request_body(&request, true);
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();

        let response = client
            .post(format!("{}/v1/messages", base_url))
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send streaming request to Anthropic API")?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow::anyhow!("Anthropic API error: {}", error_text));
        }

        let stream = response
            .bytes_stream()
            .eventsource()
            .map(|event| match event {
                Ok(event) => {
                    if event.data == "[DONE]" {
                        return Ok(StreamChunk::Done);
                    }

                    match serde_json::from_str::<StreamEvent>(&event.data) {
                        Ok(stream_event) => Self::parse_stream_event(stream_event),
                        Err(e) => Ok(StreamChunk::Error(format!("Parse error: {}", e))),
                    }
                }
                Err(e) => Err(anyhow::anyhow!("SSE error: {}", e)),
            })
            .boxed();

        Ok(stream)
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn provider_name(&self) -> &str {
        "anthropic"
    }
}

impl AnthropicProvider {
    /// 解析流式事件
    fn parse_stream_event(event: StreamEvent) -> Result<StreamChunk> {
        match event.event_type.as_str() {
            "content_block_delta" => {
                if let Some(delta) = event.delta {
                    match delta.delta_type.as_str() {
                        "text_delta" => {
                            if let Some(text) = delta.text {
                                return Ok(StreamChunk::Content(text));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(partial_json) = delta.partial_json {
                                return Ok(StreamChunk::Content(partial_json));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_start" => {
                if let Some(content_block) = event.content_block {
                    if content_block.block_type == "tool_use" {
                        if let (Some(id), Some(name)) = (content_block.id, content_block.name) {
                            return Ok(StreamChunk::ToolCallStart { id, name });
                        }
                    }
                }
            }
            "message_stop" => {
                return Ok(StreamChunk::Done);
            }
            _ => {}
        }

        // 对于不感兴趣的事件，返回空内容
        Ok(StreamChunk::Content(String::new()))
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

#[derive(Debug, Serialize, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// Used for tool_use blocks (the tool call ID)
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// Used for tool_result blocks (references the tool_use id)
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Vec<ToolResultContent>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ToolResultContent {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

impl ContentBlock {
    fn text(content: &str) -> Self {
        Self {
            block_type: "text".to_string(),
            text: Some(content.to_string()),
            id: None,
            tool_use_id: None,
            name: None,
            input: None,
            content: None,
        }
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Self {
        Self {
            block_type: "tool_use".to_string(),
            text: None,
            id: Some(id.to_string()),
            tool_use_id: None,
            name: Some(name.to_string()),
            input: Some(input),
            content: None,
        }
    }

    fn tool_result(tool_use_id: &str, content: &str) -> Self {
        Self {
            block_type: "tool_result".to_string(),
            text: None,
            id: None,
            tool_use_id: Some(tool_use_id.to_string()),
            name: None,
            input: None,
            content: Some(vec![ToolResultContent {
                content_type: "text".to_string(),
                text: content.to_string(),
            }]),
        }
    }
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
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// ========== Streaming Events ==========

#[derive(Debug, Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    content_block: Option<ResponseContentBlock>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(rename = "type", default)]
    delta_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}
