//! LLM 模块
//!
//! 提供统一的 LLM Provider 接口，支持多种大语言模型提供商

pub mod anthropic;
pub mod provider;
pub mod types;

// 重新导出常用类型
pub use provider::{LLMProvider, ProviderFactory};
pub use types::{
    ChatRequest, ChatResponse, Message, Role, StreamChunk, TokenUsage, ToolCall, ToolDefinition,
    ToolResult,
};
