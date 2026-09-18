//! OpenAI Chat Completions 协议实现
//!
//! 对应 `POST {base_url}/v1/chat/completions`，
//! 兼容绝大多数 OpenAI 协议兼容服务（DeepSeek / GLM / Moonshot / vLLM 等）。

use crate::types::{ChatRequest, ChatResponse, Message, Role, StreamChunk, TokenUsage, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ========== 请求构造 ==========

#[derive(Debug, Serialize)]
pub struct ChatCompletionsRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// OpenAI 推理模型（o 系列 / gpt-5）拒绝 `max_tokens`，只认这个字段
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// 推理强度（o 系列 / gpt-5 等）；None 时不发送
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'static str>,
    pub stream: bool,
    /// 流式时请求在末尾附带 usage chunk
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

#[derive(Debug, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// OpenAI 自家的推理模型：`o1` / `o3` / `o4-mini` / `gpt-5*`。
/// 只匹配裸 id——带厂商前缀的（OpenRouter 的 `openai/gpt-5`）由网关自行归一化。
pub(crate) fn uses_max_completion_tokens(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let o_series = m
        .strip_prefix('o')
        .is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit()));
    o_series || m.starts_with("gpt-5")
}

#[derive(Debug, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 思考模型（DeepSeek / Kimi / GLM 等）在工具循环内要求回传
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCallOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatToolCallOut {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: &'static str,
    pub function: FunctionRef,
}

#[derive(Debug, Serialize)]
pub struct FunctionRef {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub struct ChatToolDef {
    #[serde(rename = "type")]
    pub tool_type: &'static str,
    pub function: FunctionDef,
}

#[derive(Debug, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

pub fn build_request(model: &str, request: &ChatRequest, stream: bool) -> ChatCompletionsRequest {
    let reasoning_model = uses_max_completion_tokens(model);
    ChatCompletionsRequest {
        model: model.to_string(),
        messages: convert_messages(&request.messages),
        tools: request.tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|t| ChatToolDef {
                    tool_type: "function",
                    function: FunctionDef {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.parameters.clone(),
                    },
                })
                .collect()
        }),
        max_tokens: request.max_tokens.filter(|_| !reasoning_model),
        max_completion_tokens: request.max_tokens.filter(|_| reasoning_model),
        // 这些模型只接受默认 temperature
        temperature: request.temperature.filter(|_| !reasoning_model),
        reasoning_effort: request.thinking.map(|level| level.effort()),
        stream,
        stream_options: stream.then_some(StreamOptions {
            include_usage: true,
        }),
    }
}

fn convert_messages(messages: &[Message]) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    let turn_start = crate::types::current_turn_start(messages);

    for (position, msg) in messages.iter().enumerate() {
        match msg.role {
            Role::System => out.push(ChatMessage {
                role: "system".to_string(),
                content: Some(msg.content.clone()),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
            }),
            Role::User => out.push(ChatMessage {
                role: "user".to_string(),
                content: Some(msg.content.clone()),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
            }),
            Role::Assistant => {
                // 纯工具调用轮次 content 为空，OpenAI 规范形式是省略（等价于 null）
                // 思考内容只在当前轮的工具循环内回传（见 current_turn_start）
                let reasoning_content = (position > turn_start && msg.tool_calls.is_some())
                    .then(|| {
                        msg.reasoning
                            .iter()
                            .flatten()
                            .map(|b| b.text.as_str())
                            .collect::<String>()
                    })
                    .filter(|text| !text.is_empty());
                out.push(ChatMessage {
                    role: "assistant".to_string(),
                    reasoning_content,
                    content: if msg.content.is_empty() {
                        None
                    } else {
                        Some(msg.content.clone())
                    },
                    tool_calls: msg.tool_calls.as_ref().map(|calls| {
                        calls
                            .iter()
                            .map(|c| ChatToolCallOut {
                                id: c.id.clone(),
                                call_type: "function",
                                function: FunctionRef {
                                    name: c.name.clone(),
                                    arguments: c.arguments.to_string(),
                                },
                            })
                            .collect()
                    }),
                    tool_call_id: None,
                });
            }
            Role::Tool => {
                if let Some(results) = &msg.tool_results {
                    for result in results {
                        out.push(ChatMessage {
                            role: "tool".to_string(),
                            reasoning_content: None,
                            content: Some(result.content.clone()),
                            tool_calls: None,
                            tool_call_id: Some(result.tool_call_id.clone()),
                        });
                    }
                }
            }
        }
    }

    out
}

// ========== 非流式响应解析 ==========

#[derive(Debug, Deserialize)]
pub struct ChatCompletionsResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub message: Option<ChoiceMessage>,
}

#[derive(Debug, Deserialize)]
pub struct ChoiceMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallIn>>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCallIn {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionIn>,
}

#[derive(Debug, Deserialize)]
pub struct FunctionIn {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

pub fn parse_response(resp: ChatCompletionsResponse) -> ChatResponse {
    let mut content = String::new();
    let mut tool_calls = Vec::new();

    if let Some(message) = resp.choices.into_iter().next().and_then(|c| c.message) {
        if let Some(text) = message.content {
            content.push_str(&text);
        }

        if let Some(calls) = message.tool_calls {
            for call in calls {
                if let Some(function) = call.function {
                    // arguments 在 Chat Completions 中是 JSON 字符串
                    let arguments = function
                        .arguments
                        .and_then(|args| serde_json::from_str(&args).ok())
                        .unwrap_or_else(|| serde_json::json!({}));
                    tool_calls.push(ToolCall {
                        id: call.id.unwrap_or_default(),
                        name: function.name.unwrap_or_default(),
                        arguments,
                    });
                }
            }
        }
    }

    ChatResponse {
        content,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        usage: resp.usage.map(|u| TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        }),
    }
}

// ========== 流式解析 ==========
//
// OpenAI 流式的 tool_calls 以 index 定位、分片到达（首片携带 id/name，
// 后续片只有 arguments 增量），且多个并行调用的分片可能交错。
// Agent 侧按「当前工具」追加参数，无法表达交错，因此这里按 index
// 缓冲，文本增量立即下发、工具调用在流结束时按序一次性下发。

#[derive(Debug, Default)]
pub struct ChatStreamState {
    tools: Vec<BufferedTool>,
    /// 最后一个 choice chunk 携带的 finish_reason（随 Done 一并发出）
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
    finished: bool,
}

#[derive(Debug)]
struct BufferedTool {
    id: String,
    name: String,
    arguments: String,
}

impl ChatStreamState {
    /// 处理一条 SSE data。返回空 Vec 表示事件被忽略。
    pub fn process_data(&mut self, data: &str) -> Vec<StreamChunk> {
        if self.finished {
            return Vec::new();
        }

        if data.trim() == "[DONE]" {
            return self.finish();
        }

        let chunk: ChatStreamChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(e) => return vec![StreamChunk::Error(format!("Parse error: {}", e))],
        };

        if let Some(err) = chunk.error {
            self.finished = true;
            return vec![StreamChunk::Error(
                err.message
                    .unwrap_or_else(|| "Unknown API error".to_string()),
            )];
        }

        if let Some(usage) = chunk.usage {
            self.usage = Some(TokenUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            });
        }

        let mut out = Vec::new();
        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason.filter(|r| !r.is_empty()) {
                self.finish_reason = Some(reason);
            }
            let Some(delta) = choice.delta else { continue };

            if let Some(text) = delta.reasoning_content {
                if !text.is_empty() {
                    out.push(StreamChunk::Reasoning(text));
                }
            }
            if let Some(text) = delta.content {
                if !text.is_empty() {
                    out.push(StreamChunk::Content(text));
                }
            }

            if let Some(fragments) = delta.tool_calls {
                for fragment in fragments {
                    self.absorb(fragment);
                }
            }
        }
        out
    }

    /// SSE 流终止（即使服务端未发 [DONE]）时的兜底 flush，避免缓冲的工具调用丢失
    pub fn process_end(&mut self) -> Vec<StreamChunk> {
        if self.finished {
            return Vec::new();
        }
        self.finish()
    }

    fn finish(&mut self) -> Vec<StreamChunk> {
        self.finished = true;
        let mut out = Vec::new();

        let tools = std::mem::take(&mut self.tools);
        for (i, mut tool) in tools.into_iter().enumerate() {
            if tool.name.is_empty() {
                continue;
            }
            if tool.id.is_empty() {
                tool.id = format!("call_{}", i);
            }
            out.push(StreamChunk::ToolCallStart {
                id: tool.id.clone(),
                name: tool.name,
            });
            out.push(StreamChunk::ToolCallArguments {
                id: tool.id,
                arguments: tool.arguments,
            });
        }
        if let Some(usage) = self.usage.take() {
            out.push(StreamChunk::Usage(usage));
        }
        if let Some(reason) = self.finish_reason.take() {
            out.push(StreamChunk::Stop(crate::types::StopReason::parse(&reason)));
        }
        out.push(StreamChunk::Done);
        out
    }

    fn absorb(&mut self, fragment: StreamToolCallFragment) {
        let index = fragment.index.unwrap_or(self.tools.len() as u32) as usize;
        while self.tools.len() <= index {
            self.tools.push(BufferedTool {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
        }

        let tool = &mut self.tools[index];
        if let Some(id) = fragment.id {
            if !id.is_empty() {
                tool.id = id;
            }
        }
        if let Some(function) = fragment.function {
            if let Some(name) = function.name {
                if !name.is_empty() {
                    tool.name = name;
                }
            }
            if let Some(args) = function.arguments {
                tool.arguments.push_str(&args);
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatStreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    /// include_usage 时末尾 chunk 携带（此时 choices 为空）
    #[serde(default)]
    usage: Option<StreamUsage>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct StreamUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    /// DeepSeek / Kimi / GLM / Qwen 用 reasoning_content，OpenRouter 用 reasoning
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallFragment>>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCallFragment {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionFragment>,
}

#[derive(Debug, Deserialize)]
struct StreamFunctionFragment {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, ThinkingLevel, ToolCall, ToolDefinition, ToolResult};

    #[test]
    fn test_reasoning_effort_field() {
        // 启用：reasoning_effort 下发（对推理模型）
        let request = sample_request().with_thinking(Some(ThinkingLevel::Medium));
        let body = serde_json::to_value(build_request("o3", &request, true)).unwrap();
        assert_eq!(body["reasoning_effort"], "medium");

        // 未启用：字段不出现（非推理模型不接受该字段）
        let body = serde_json::to_value(build_request("gpt-4o", &sample_request(), true)).unwrap();
        assert!(body.get("reasoning_effort").is_none());
    }

    fn sample_request() -> ChatRequest {
        let mut messages = vec![Message::system("You are helpful")];
        messages.push(Message::user("grep for foo"));
        messages.push(Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                name: "builtin__grep".to_string(),
                arguments: serde_json::json!({"pattern": "foo"}),
            }]),
            tool_results: None,
            reasoning: None,
        });
        messages.push(Message {
            role: Role::Tool,
            content: String::new(),
            tool_calls: None,
            tool_results: Some(vec![ToolResult {
                tool_call_id: "call_1".to_string(),
                content: "src/main.rs:1:foo".to_string(),
            }]),
            reasoning: None,
        });

        ChatRequest::new(messages)
            .with_tools(vec![ToolDefinition {
                name: "builtin__grep".to_string(),
                description: "Search files".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }])
            .with_max_tokens(1024)
    }

    #[test]
    fn test_build_request_message_shapes() {
        let body = serde_json::to_value(build_request("gpt-4o", &sample_request(), true)).unwrap();

        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 1024);
        assert!(body.get("temperature").is_none());

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[0],
            serde_json::json!({"role": "system", "content": "You are helpful"})
        );
        assert_eq!(
            messages[1],
            serde_json::json!({"role": "user", "content": "grep for foo"})
        );

        // 纯工具调用轮次：content 省略，tool_calls 展开且 arguments 是 JSON 字符串
        let assistant = &messages[2];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant.get("content").is_none());
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["name"],
            "builtin__grep"
        );
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            serde_json::json!(r#"{"pattern":"foo"}"#)
        );

        assert_eq!(
            messages[3],
            serde_json::json!({"role": "tool", "tool_call_id": "call_1", "content": "src/main.rs:1:foo"})
        );

        // 工具定义嵌套在 function 下
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "builtin__grep");
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let raw = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Let me search",
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "builtin__grep", "arguments": "{\"pattern\":\"foo\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }"#;

        let resp: ChatCompletionsResponse = serde_json::from_str(raw).unwrap();
        let parsed = parse_response(resp);

        assert_eq!(parsed.content, "Let me search");
        let calls = parsed.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_9");
        assert_eq!(calls[0].name, "builtin__grep");
        assert_eq!(calls[0].arguments, serde_json::json!({"pattern": "foo"}));
        let usage = parsed.usage.unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (10, 5));
    }

    #[test]
    fn test_stream_buffers_tool_calls_until_done() {
        let mut state = ChatStreamState::default();

        // 文本增量应立即下发
        let chunks = state.process_data(r#"{"choices":[{"delta":{"content":"Hi"}}]}"#);
        assert_eq!(chunks, vec![StreamChunk::Content("Hi".to_string())]);

        // 工具调用分片：首片 id+name，随后两片 arguments 增量
        state.process_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"builtin__grep","arguments":""}}]}}]}"#,
        );
        state.process_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pattern\""}}]}}]}"#,
        );
        let chunks = state.process_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"foo\"}"}}]}}]}"#,
        );
        assert!(
            chunks.is_empty(),
            "buffered tool call must not emit mid-stream"
        );

        // [DONE] 触发一次性下发，arguments 是完整 JSON 字符串
        let chunks = state.process_data("[DONE]");
        assert_eq!(
            chunks,
            vec![
                StreamChunk::ToolCallStart {
                    id: "call_a".to_string(),
                    name: "builtin__grep".to_string(),
                },
                StreamChunk::ToolCallArguments {
                    id: "call_a".to_string(),
                    arguments: r#"{"pattern":"foo"}"#.to_string(),
                },
                StreamChunk::Done,
            ]
        );
    }

    #[test]
    fn test_stream_end_without_done_marker_flushes() {
        let mut state = ChatStreamState::default();
        state.process_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"builtin__read","arguments":"{}"}}]}}]}"#,
        );

        // 服务端未发 [DONE] 直接断流：process_end 兜底 flush
        let chunks = state.process_end();
        assert!(matches!(chunks[0], StreamChunk::ToolCallStart { .. }));
        assert!(matches!(chunks.last(), Some(StreamChunk::Done)));
    }

    #[test]
    fn test_stream_error_event() {
        let mut state = ChatStreamState::default();
        let chunks = state.process_data(
            r#"{"error":{"message":"Invalid API key","type":"invalid_request_error"}}"#,
        );

        assert_eq!(
            chunks,
            vec![StreamChunk::Error("Invalid API key".to_string())]
        );
    }

    #[test]
    fn test_stream_reports_length_finish_reason() {
        let mut state = ChatStreamState::default();
        state.process_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write","arguments":"{\"path\":\"a"}}]}}]}"#,
        );
        state.process_data(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#);
        let chunks = state.process_data("[DONE]");
        assert_eq!(
            &chunks[chunks.len() - 2..],
            &[
                StreamChunk::Stop(crate::types::StopReason::MaxTokens),
                StreamChunk::Done
            ]
        );
    }

    #[test]
    fn test_reasoning_models_use_max_completion_tokens() {
        let request = ChatRequest::new(vec![Message::user("hi")])
            .with_max_tokens(2048)
            .with_temperature(0.2);
        for model in ["gpt-5", "gpt-5-mini", "o3", "o4-mini", "o1-preview"] {
            let body = serde_json::to_value(build_request(model, &request, true)).unwrap();
            assert_eq!(body["max_completion_tokens"], 2048, "{model}");
            assert!(body.get("max_tokens").is_none(), "{model}");
            assert!(body.get("temperature").is_none(), "{model}");
        }
        for model in [
            "gpt-4o",
            "deepseek-chat",
            "glm-4.6",
            "openai/gpt-5",
            "olmo-2",
        ] {
            let body = serde_json::to_value(build_request(model, &request, true)).unwrap();
            assert_eq!(body["max_tokens"], 2048, "{model}");
            assert!(body.get("max_completion_tokens").is_none(), "{model}");
        }
        // 只有流式请求才带 stream_options
        let body = serde_json::to_value(build_request("gpt-4o", &request, true)).unwrap();
        assert_eq!(body["stream_options"]["include_usage"], true);
        let body = serde_json::to_value(build_request("gpt-4o", &request, false)).unwrap();
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn test_stream_reasoning_and_usage() {
        let mut state = ChatStreamState::default();
        assert_eq!(
            state.process_data(r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#),
            vec![StreamChunk::Reasoning("hmm".to_string())]
        );
        // OpenRouter 的字段名
        assert_eq!(
            state.process_data(r#"{"choices":[{"delta":{"reasoning":"ok"}}]}"#),
            vec![StreamChunk::Reasoning("ok".to_string())]
        );
        state
            .process_data(r#"{"choices":[],"usage":{"prompt_tokens":900,"completion_tokens":30}}"#);
        let chunks = state.process_data("[DONE]");
        assert_eq!(
            chunks,
            vec![
                StreamChunk::Usage(TokenUsage {
                    input_tokens: 900,
                    output_tokens: 30
                }),
                StreamChunk::Done
            ]
        );
    }

    #[test]
    fn test_reasoning_content_replayed_only_in_current_tool_loop() {
        let reasoning = Some(vec![crate::types::ReasoningBlock {
            text: "plan".to_string(),
            signature: None,
            redacted: None,
            id: None,
        }]);
        let call = Some(vec![ToolCall {
            id: "c1".to_string(),
            name: "grep".to_string(),
            arguments: serde_json::json!({}),
        }]);
        let messages = vec![
            Message::user("old"),
            Message {
                tool_calls: call.clone(),
                reasoning: reasoning.clone(),
                ..Message::assistant("")
            },
            Message {
                reasoning: reasoning.clone(),
                ..Message::assistant("old answer")
            },
            Message::user("new"),
            Message {
                tool_calls: call,
                reasoning,
                ..Message::assistant("")
            },
        ];
        let body = serde_json::to_value(build_request(
            "deepseek-chat",
            &ChatRequest::new(messages),
            true,
        ))
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs[1].get("reasoning_content").is_none(), "old turn");
        assert!(
            msgs[2].get("reasoning_content").is_none(),
            "final answers never replay"
        );
        assert_eq!(msgs[4]["reasoning_content"], "plan");
    }
}
