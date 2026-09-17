//! OpenAI Responses 协议实现
//!
//! 对应 `POST {base_url}/v1/responses`（OpenAI 新一代 API）。
//! 与 Chat Completions 的主要差异：
//! - 系统提示通过顶层 `instructions` 字段传递
//! - 工具调用是独立输出项 `function_call`，结果通过 `function_call_output` 回传
//! - 工具定义是扁平结构（不嵌套在 `function` 下）
//! - 流式事件带 `type` 字段，工具参数增量通过 `response.function_call_arguments.delta`

use crate::types::{ChatRequest, ChatResponse, Role, StreamChunk, TokenUsage, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ========== 请求构造 ==========

#[derive(Debug, Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<InputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    pub stream: bool,
    /// 推理模型：`["reasoning.encrypted_content"]`，让服务端把思考以密文形式返回
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<&'static str>>,
    /// 推理模型：`false`。我们不用 `previous_response_id`，而是自己回传密文思考；
    /// 不关掉的话服务端会白存一份，且下一次请求拿不到上一轮的思考
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
}

/// 输入项。消息使用无 type 标签的「简化输入」形式（content 为纯字符串）；
/// function_call 系列使用带 type 标签的完整形式。untagged 序列化按各变体自身字段输出。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum InputItem {
    Message(InputMessage),
    FunctionCall(InputFunctionCall),
    FunctionCallOutput(InputFunctionCallOutput),
    Reasoning(InputReasoning),
}

/// 回传上一轮的 reasoning 项（密文原样带回，模型才能在工具循环中延续思路）
#[derive(Debug, Serialize)]
pub struct InputReasoning {
    #[serde(rename = "type")]
    pub item_type: &'static str, // "reasoning"
    pub id: String,
    pub summary: Vec<ReasoningSummaryPart>,
    pub encrypted_content: String,
}

#[derive(Debug, Serialize)]
pub struct ReasoningSummaryPart {
    #[serde(rename = "type")]
    pub part_type: &'static str, // "summary_text"
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct InputMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct InputFunctionCall {
    #[serde(rename = "type")]
    pub item_type: &'static str, // "function_call"
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub struct InputFunctionCallOutput {
    #[serde(rename = "type")]
    pub item_type: &'static str, // "function_call_output"
    pub call_id: String,
    pub output: String,
}

#[derive(Debug, Serialize)]
pub struct ResponsesToolDef {
    #[serde(rename = "type")]
    pub tool_type: &'static str, // "function"
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

pub fn build_request(model: &str, request: &ChatRequest, stream: bool) -> ResponsesRequest {
    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<InputItem> = Vec::new();
    let reasoning_model = super::chat_completions::uses_max_completion_tokens(model);
    let turn_start = crate::types::current_turn_start(&request.messages);

    for (position, msg) in request.messages.iter().enumerate() {
        match msg.role {
            Role::System => instructions.push(msg.content.clone()),
            Role::User => input.push(InputItem::Message(InputMessage {
                role: "user".to_string(),
                content: msg.content.clone(),
            })),
            Role::Assistant => {
                // 当前轮工具循环内的 reasoning 项必须排在它产生的 function_call 之前
                if reasoning_model && position > turn_start && msg.tool_calls.is_some() {
                    for block in msg.reasoning.iter().flatten() {
                        if let (Some(id), Some(encrypted)) = (&block.id, &block.redacted) {
                            input.push(InputItem::Reasoning(InputReasoning {
                                item_type: "reasoning",
                                id: id.clone(),
                                summary: if block.text.is_empty() {
                                    Vec::new()
                                } else {
                                    vec![ReasoningSummaryPart {
                                        part_type: "summary_text",
                                        text: block.text.clone(),
                                    }]
                                },
                                encrypted_content: encrypted.clone(),
                            }));
                        }
                    }
                }
                if !msg.content.is_empty() {
                    input.push(InputItem::Message(InputMessage {
                        role: "assistant".to_string(),
                        content: msg.content.clone(),
                    }));
                }
                if let Some(calls) = &msg.tool_calls {
                    for call in calls {
                        input.push(InputItem::FunctionCall(InputFunctionCall {
                            item_type: "function_call",
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.to_string(),
                        }));
                    }
                }
            }
            Role::Tool => {
                if let Some(results) = &msg.tool_results {
                    for result in results {
                        input.push(InputItem::FunctionCallOutput(InputFunctionCallOutput {
                            item_type: "function_call_output",
                            call_id: result.tool_call_id.clone(),
                            output: result.content.clone(),
                        }));
                    }
                }
            }
        }
    }

    ResponsesRequest {
        model: model.to_string(),
        instructions: if instructions.is_empty() {
            None
        } else {
            Some(instructions.join("\n"))
        },
        input,
        tools: request.tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|t| ResponsesToolDef {
                    tool_type: "function",
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                })
                .collect()
        }),
        max_output_tokens: request.max_tokens,
        // 推理模型只接受默认 temperature
        temperature: request.temperature.filter(|_| !reasoning_model),
        stream,
        include: reasoning_model.then(|| vec!["reasoning.encrypted_content"]),
        store: reasoning_model.then_some(false),
    }
}

// ========== 非流式响应解析 ==========

#[derive(Debug, Deserialize)]
pub struct ResponsesApiResponse {
    #[serde(default)]
    pub output: Vec<OutputItem>,
    #[serde(default)]
    pub usage: Option<ResponsesUsage>,
}

#[derive(Debug, Deserialize)]
pub struct OutputItem {
    #[serde(rename = "type", default)]
    pub item_type: String,
    /// 输出项自身 ID（function_call 为 fc_*，message 为 msg_*）
    #[serde(default)]
    pub id: Option<String>,
    /// message 项内容：output_text 部分数组（个别兼容实现为纯字符串）
    #[serde(default)]
    pub content: Option<Value>,
    /// function_call 项的调用 ID，function_call_output 通过它回关联
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
    /// reasoning 项：密文思考（请求了 include 才有）
    #[serde(default)]
    pub encrypted_content: Option<String>,
    /// reasoning 项：摘要分段 `[{type:"summary_text", text}]`
    #[serde(default)]
    pub summary: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ResponsesUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

pub fn parse_response(resp: ResponsesApiResponse) -> ChatResponse {
    let mut content = String::new();
    let mut tool_calls = Vec::new();

    for item in resp.output {
        match item.item_type.as_str() {
            "message" => content.push_str(&extract_text(item.content.as_ref())),
            "function_call" => {
                let id = item.call_id.or(item.id).unwrap_or_default();
                if id.is_empty() {
                    continue;
                }
                let arguments = item
                    .arguments
                    .and_then(|args| serde_json::from_str(&args).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
                tool_calls.push(ToolCall {
                    id,
                    name: item.name.unwrap_or_default(),
                    arguments,
                });
            }
            // reasoning / web_search_call 等其他输出项忽略
            _ => {}
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
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        }),
    }
}

/// 提取 message 项的文本：兼容 output_text 部分数组与纯字符串两种形式
fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

// ========== 流式解析 ==========
//
// 与 Chat Completions 相同的缓冲策略：文本增量立即下发，
// 工具调用按 item_id 缓冲、在 response.completed / 流终止时统一下发。

#[derive(Debug, Default)]
pub struct ResponsesStreamState {
    /// (item_id, 缓冲的工具调用)，按 output_item.added 到达顺序排列
    tools: Vec<(String, BufferedTool)>,
    /// response.incomplete 的原因（如 max_output_tokens）
    incomplete_reason: Option<String>,
    /// 是否收到 response.completed / response.incomplete
    saw_terminal: bool,
    usage: Option<crate::types::TokenUsage>,
    finished: bool,
}

#[derive(Debug)]
struct BufferedTool {
    call_id: String,
    name: String,
    arguments: String,
}

impl ResponsesStreamState {
    /// 处理一条 SSE data。返回空 Vec 表示事件被忽略。
    pub fn process_data(&mut self, data: &str) -> Vec<StreamChunk> {
        if self.finished {
            return Vec::new();
        }

        if data.trim() == "[DONE]" {
            return self.finish();
        }

        let event: ResponsesStreamEvent = match serde_json::from_str(data) {
            Ok(e) => e,
            Err(e) => return vec![StreamChunk::Error(format!("Parse error: {}", e))],
        };

        match event.event_type.as_str() {
            "response.output_text.delta" => match event.delta {
                Some(d) if !d.is_empty() => vec![StreamChunk::Content(d)],
                _ => Vec::new(),
            },
            "response.output_item.done" => {
                // reasoning 项完成：密文此时才完整
                match event.item {
                    Some(item) if item.item_type == "reasoning" => {
                        match (item.id, item.encrypted_content) {
                            (Some(id), Some(encrypted_content)) if !encrypted_content.is_empty() => {
                                let summary = item
                                    .summary
                                    .as_ref()
                                    .and_then(Value::as_array)
                                    .map(|parts| {
                                        parts
                                            .iter()
                                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    })
                                    .unwrap_or_default();
                                vec![StreamChunk::ReasoningItem {
                                    id,
                                    encrypted_content,
                                    summary,
                                }]
                            }
                            _ => Vec::new(),
                        }
                    }
                    _ => Vec::new(),
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.item {
                    if item.item_type == "function_call" {
                        self.tools.push((
                            item.id.unwrap_or_default(),
                            BufferedTool {
                                call_id: item.call_id.unwrap_or_default(),
                                name: item.name.unwrap_or_default(),
                                arguments: String::new(),
                            },
                        ));
                    }
                }
                Vec::new()
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.delta {
                    if !delta.is_empty() {
                        let idx = event
                            .item_id
                            .as_deref()
                            .and_then(|id| self.tools.iter().rposition(|(iid, _)| iid == id))
                            .unwrap_or_else(|| self.tools.len().saturating_sub(1));
                        if let Some((_, tool)) = self.tools.get_mut(idx) {
                            tool.arguments.push_str(&delta);
                        }
                    }
                }
                Vec::new()
            }
            "response.completed" => {
                self.saw_terminal = true;
                self.capture_usage(event.response.as_ref());
                self.finish()
            }
            "response.incomplete" => {
                let reason = event
                    .response
                    .as_ref()
                    .and_then(|r| r.pointer("/incomplete_details/reason").and_then(Value::as_str))
                    .unwrap_or("incomplete");
                self.incomplete_reason = Some(reason.to_string());
                self.capture_usage(event.response.as_ref());
                self.saw_terminal = true;
                self.finish()
            }
            "response.failed" | "error" => {
                self.finished = true;
                // response.failed 的信息在 response.error.message；顶层 error 事件在 message
                let message = event
                    .response
                    .as_ref()
                    .and_then(|r| r.pointer("/error/message").and_then(Value::as_str))
                    .or(event.message.as_deref())
                    .unwrap_or("LLM response failed")
                    .to_string();
                vec![StreamChunk::Error(message)]
            }
            // response.created / response.in_progress / *.done 等事件忽略
            _ => Vec::new(),
        }
    }

    /// SSE 流终止（即使服务端未发 response.completed）时的兜底 flush
    pub fn process_end(&mut self) -> Vec<StreamChunk> {
        if self.finished {
            return Vec::new();
        }
        self.finish()
    }

    fn capture_usage(&mut self, response: Option<&Value>) {
        let read = |key: &str| {
            response
                .and_then(|r| r.pointer(&format!("/usage/{key}")))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
        };
        if let (Some(input_tokens), Some(output_tokens)) =
            (read("input_tokens"), read("output_tokens"))
        {
            self.usage = Some(crate::types::TokenUsage {
                input_tokens,
                output_tokens,
            });
        }
    }

    fn finish(&mut self) -> Vec<StreamChunk> {
        self.finished = true;
        let mut out = Vec::new();

        let tools = std::mem::take(&mut self.tools);
        for (i, (_, mut tool)) in tools.into_iter().enumerate() {
            if tool.name.is_empty() {
                continue;
            }
            if tool.call_id.is_empty() {
                tool.call_id = format!("call_{}", i);
            }
            out.push(StreamChunk::ToolCallStart {
                id: tool.call_id.clone(),
                name: tool.name,
            });
            out.push(StreamChunk::ToolCallArguments {
                id: tool.call_id,
                arguments: tool.arguments,
            });
        }
        let had_tools = out.iter().any(|c| matches!(c, StreamChunk::ToolCallStart { .. }));
        let stop = match self.incomplete_reason.take() {
            Some(reason) => match crate::types::StopReason::parse(&reason) {
                // "incomplete" 等未知原因一律按截断处理：输出不完整
                crate::types::StopReason::Other(_) => crate::types::StopReason::MaxTokens,
                known => known,
            },
            None if had_tools => crate::types::StopReason::ToolUse,
            None => crate::types::StopReason::EndTurn,
        };
        // 只有收到终止事件才知道原因；连接中断（process_end）不伪造
        if let Some(usage) = self.usage.take() {
            out.push(StreamChunk::Usage(usage));
        }
        if self.saw_terminal {
            out.push(StreamChunk::Stop(stop));
        }
        out.push(StreamChunk::Done);
        out
    }
}

#[derive(Debug, Deserialize)]
struct ResponsesStreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    /// output_text.delta / function_call_arguments.delta 携带的增量文本
    #[serde(default)]
    delta: Option<String>,
    /// function_call_arguments.delta 通过 item_id（fc_*）定位调用
    #[serde(default)]
    item_id: Option<String>,
    /// output_item.added 携带的输出项
    #[serde(default)]
    item: Option<OutputItem>,
    /// response.failed 时错误信息位于 response.error.message
    #[serde(default)]
    response: Option<Value>,
    /// 顶层 `error` 事件的错误信息
    #[serde(default)]
    message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, ToolCall, ToolDefinition, ToolResult};

    fn sample_request() -> ChatRequest {
        let mut messages = vec![Message::system("You are helpful")];
        messages.push(Message::user("grep for foo"));
        messages.push(
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "builtin__grep".to_string(),
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
                    tool_call_id: "call_1".to_string(),
                    content: "src/main.rs:1:foo".to_string(),
                }]),
                reasoning: None,
            },
        );

        ChatRequest::new(messages)
            .with_tools(vec![ToolDefinition {
                name: "builtin__grep".to_string(),
                description: "Search files".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }])
            .with_max_tokens(1024)
    }

    #[test]
    fn test_build_request_shapes() {
        let body = serde_json::to_value(build_request("gpt-5", &sample_request(), true)).unwrap();

        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["instructions"], "You are helpful");
        assert_eq!(body["max_output_tokens"], 1024);
        assert_eq!(body["stream"], true);

        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        // 简化输入形式：无 type 标签，content 为纯字符串
        assert_eq!(input[0], serde_json::json!({"role": "user", "content": "grep for foo"}));
        // 纯工具调用轮次：assistant 文本消息被省略，仅剩 function_call 项
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["name"], "builtin__grep");
        assert_eq!(input[1]["arguments"], serde_json::json!(r#"{"pattern":"foo"}"#));
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "src/main.rs:1:foo");

        // 工具定义为扁平结构
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "builtin__grep");
        assert_eq!(tool["description"], "Search files");
    }

    #[test]
    fn test_parse_response() {
        let raw = r#"{
            "output": [
                {"type": "reasoning", "id": "rs_1"},
                {"type": "message", "id": "msg_1", "role": "assistant",
                 "content": [{"type": "output_text", "text": "Answer"}]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                 "name": "builtin__grep", "arguments": "{\"pattern\":\"foo\"}"}
            ],
            "usage": {"input_tokens": 12, "output_tokens": 8}
        }"#;

        let resp: ResponsesApiResponse = serde_json::from_str(raw).unwrap();
        let parsed = parse_response(resp);

        assert_eq!(parsed.content, "Answer");
        let calls = parsed.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        // 内部 ID 采用 call_id（function_call_output 通过它回关联）
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].arguments, serde_json::json!({"pattern": "foo"}));
        let usage = parsed.usage.unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (12, 8));
    }

    #[test]
    fn test_stream_function_call_events() {
        let mut state = ResponsesStreamState::default();

        // 文本增量立即下发
        let chunks =
            state.process_data(r#"{"type":"response.output_text.delta","delta":"Hello"}"#);
        assert_eq!(chunks, vec![StreamChunk::Content("Hello".to_string())]);

        // function_call 项加入 + 参数增量（按 item_id 定位），均缓冲不下发
        state.process_data(
            r#"{"type":"response.output_item.added","output_index":1,
                "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"builtin__grep","arguments":""}}"#,
        );
        let chunks = state.process_data(
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"pattern\""}"#,
        );
        assert!(chunks.is_empty());
        state.process_data(
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":":\"foo\"}"}"#,
        );

        // completed 触发一次性下发
        let chunks = state.process_data(r#"{"type":"response.completed","response":{}}"#);
        assert_eq!(
            chunks,
            vec![
                StreamChunk::ToolCallStart {
                    id: "call_1".to_string(),
                    name: "builtin__grep".to_string(),
                },
                StreamChunk::ToolCallArguments {
                    id: "call_1".to_string(),
                    arguments: r#"{"pattern":"foo"}"#.to_string(),
                },
                StreamChunk::Stop(crate::types::StopReason::ToolUse),
                StreamChunk::Done,
            ]
        );
    }

    #[test]
    fn test_stream_end_without_completed_flushes() {
        let mut state = ResponsesStreamState::default();
        state.process_data(
            r#"{"type":"response.output_item.added",
                "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"builtin__read","arguments":""}}"#,
        );
        state.process_data(
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{}"}"#,
        );

        let chunks = state.process_end();
        assert!(matches!(chunks[0], StreamChunk::ToolCallStart { .. }));
        assert!(matches!(chunks.last(), Some(StreamChunk::Done)));
    }

    #[test]
    fn test_stream_failed_event() {
        let mut state = ResponsesStreamState::default();
        let chunks = state.process_data(
            r#"{"type":"response.failed","response":{"error":{"code":null,"message":"Model overloaded"}}}"#,
        );

        assert_eq!(
            chunks,
            vec![StreamChunk::Error("Model overloaded".to_string())]
        );
    }

    #[test]
    fn test_stream_incomplete_reports_max_tokens() {
        let mut state = ResponsesStreamState::default();
        let chunks = state.process_data(
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
        );
        assert_eq!(
            chunks,
            vec![
                StreamChunk::Stop(crate::types::StopReason::MaxTokens),
                StreamChunk::Done
            ]
        );
    }

    #[test]
    fn test_stream_top_level_error_message() {
        let mut state = ResponsesStreamState::default();
        let chunks =
            state.process_data(r#"{"type":"error","code":"rate_limit","message":"slow down"}"#);
        assert_eq!(chunks, vec![StreamChunk::Error("slow down".to_string())]);
    }

    #[test]
    fn test_reasoning_item_roundtrip() {
        // 流：reasoning 项完成 → ReasoningItem
        let mut state = ResponsesStreamState::default();
        let chunks = state.process_data(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1",
                "summary":[{"type":"summary_text","text":"plan the grep"}],"encrypted_content":"ENC=="}}"#,
        );
        assert_eq!(
            chunks,
            vec![StreamChunk::ReasoningItem {
                id: "rs_1".into(),
                encrypted_content: "ENC==".into(),
                summary: "plan the grep".into(),
            }]
        );
        // function_call 的 done 事件不受影响
        assert!(state
            .process_data(r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1"}}"#)
            .is_empty());

        // 请求：推理模型在当前轮工具循环内回传，且排在 function_call 之前
        let reasoning = Some(vec![crate::types::ReasoningBlock {
            text: "plan the grep".into(),
            signature: None,
            redacted: Some("ENC==".into()),
            id: Some("rs_1".into()),
        }]);
        let messages = vec![
            crate::types::Message::user("find foo"),
            crate::types::Message {
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    name: "grep".into(),
                    arguments: serde_json::json!({"pattern": "foo"}),
                }]),
                reasoning,
                ..crate::types::Message::assistant("")
            },
        ];
        let request = ChatRequest::new(messages).with_temperature(0.3);
        let body = serde_json::to_value(build_request("gpt-5", &request, true)).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert!(body.get("temperature").is_none());
        assert_eq!(body["input"][1]["type"], "reasoning");
        assert_eq!(body["input"][1]["id"], "rs_1");
        assert_eq!(body["input"][1]["encrypted_content"], "ENC==");
        assert_eq!(body["input"][1]["summary"][0]["text"], "plan the grep");
        assert_eq!(body["input"][2]["type"], "function_call");

        // 非推理模型（含其它厂商的 responses 端点）：请求形状不变
        let body = serde_json::to_value(build_request("glm-4.6", &request, true)).unwrap();
        assert!(body.get("store").is_none() && body.get("include").is_none());
        assert_eq!(body["input"][1]["type"], "function_call");
    }
}
