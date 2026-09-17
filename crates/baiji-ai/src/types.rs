use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 消息角色
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 一段模型思考内容（Anthropic thinking block / OpenAI 兼容的 reasoning_content）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningBlock {
    pub text: String,
    /// Anthropic 对 thinking block 的签名：回传时必须原样带上
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// 密文形式的思考：Anthropic `redacted_thinking.data`，
    /// 或 OpenAI Responses `reasoning.encrypted_content`（此时 `id` 为该 reasoning 项的 id）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<String>,
    /// OpenAI Responses reasoning 项的 id（`rs_*`）；回传时与密文配对
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// 消息内容
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_results: Option<Vec<ToolResult>>,
    /// assistant 的思考内容。思考模型在工具循环中要求把它回传
    /// （DeepSeek / Kimi 的 reasoning_content，Anthropic 兼容端点的 thinking block）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Vec<ReasoningBlock>>,
}

/// 「当前轮」的起点：最后一条 User 消息的下标。思考内容只在其后的工具循环内回传——
/// 更早轮次的思考对模型无用，且部分厂商会因此报错（旧版 deepseek-reasoner 返回 400），
/// 换模型后旧签名也会失效。
pub fn current_turn_start(messages: &[Message]) -> usize {
    messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .unwrap_or(0)
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: None,
            tool_results: None,
            reasoning: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: None,
            tool_results: None,
            reasoning: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: None,
            tool_results: None,
            reasoning: None,
        }
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: None,
            tool_results: None,
            reasoning: None,
        }
    }

    pub fn with_tool_calls(mut self, tool_calls: Vec<ToolCall>) -> Self {
        self.tool_calls = Some(tool_calls);
        self
    }

    pub fn with_tool_results(mut self, tool_results: Vec<ToolResult>) -> Self {
        self.tool_results = Some(tool_results);
        self
    }
}

/// 工具调用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// 工具执行结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
}

/// 工具定义（用于告诉 LLM 有哪些工具可用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value, // JSON Schema
}

/// 聊天请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

impl ChatRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            tools: None,
            max_tokens: None,
            temperature: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }
}

/// 聊天响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub usage: Option<TokenUsage>,
}

/// Token 使用情况
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// 模型停止生成的原因（各协议归一化）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// 正常结束
    EndTurn,
    /// 为调用工具而停止
    ToolUse,
    /// 触及 max_tokens：输出（含工具参数 JSON）可能被截断
    MaxTokens,
    /// 其它（内容过滤、stop sequence 等），保留原始值
    Other(String),
}

impl StopReason {
    /// 归一化 Anthropic `stop_reason` / OpenAI `finish_reason` / Responses `incomplete_details.reason`
    pub fn parse(raw: &str) -> Self {
        match raw {
            "end_turn" | "stop" | "stop_sequence" => Self::EndTurn,
            "tool_use" | "tool_calls" | "function_call" => Self::ToolUse,
            "max_tokens" | "length" | "max_output_tokens" | "model_context_window_exceeded" => {
                Self::MaxTokens
            }
            other => Self::Other(other.to_string()),
        }
    }
}

/// 流式响应块
#[derive(Debug, Clone, PartialEq)]
pub enum StreamChunk {
    /// 内容增量
    Content(String),
    /// 工具调用开始
    ToolCallStart { id: String, name: String },
    /// 工具调用参数增量
    ToolCallArguments { id: String, arguments: String },
    /// 思考内容增量
    Reasoning(String),
    /// 当前思考块结束并附带签名（Anthropic）
    ReasoningSignature(String),
    /// 整块加密的思考（Anthropic redacted_thinking）
    ReasoningRedacted(String),
    /// OpenAI Responses 的 reasoning 输出项（密文 + 可选摘要）：工具循环内须原样回传
    ReasoningItem {
        id: String,
        encrypted_content: String,
        summary: String,
    },
    /// token 用量（可能多次出现，后到的覆盖先到的）
    Usage(TokenUsage),
    /// 停止原因（已知时在 `Done` 之前发出一次）
    Stop(StopReason),
    /// 流结束
    Done,
    /// 错误
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_creation() {
        let msg = Message::user("Hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "Hello");
    }

    #[test]
    fn test_chat_request_builder() {
        let req = ChatRequest::new(vec![Message::user("Hello")])
            .with_max_tokens(100)
            .with_temperature(0.5);

        assert_eq!(req.max_tokens, Some(100));
        assert_eq!(req.temperature, Some(0.5));
    }
}
