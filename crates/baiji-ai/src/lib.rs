//! baiji-ai — AI Provider 接入层
//!
//! - 统一消息/请求/响应/流式块类型
//! - `Provider` trait 与 Anthropic Messages、OpenAI 兼容
//!   （Chat Completions / Responses）双协议实现
//! - 主流厂商预设注册表（只需 API Key 即可接入）
//! - 模型列表自动发现

pub mod anthropic;
pub mod error;
pub mod models;
pub mod openai;
pub mod provider;
pub mod types;
pub mod vendors;

pub use error::{http_client, is_transient_error, retry_after, ApiError};
pub use models::{
    discover_models, list_models, model_limits, pick_default_model, ModelInfo, ModelLimits,
};
pub use provider::{build_provider, Protocol, Provider, ProviderConfig};
pub use types::{current_turn_start, ReasoningBlock, StopReason, 
    ChatRequest, ChatResponse, Message, Role, StreamChunk, TokenUsage, ToolCall, ToolDefinition,
    ToolResult,
};
pub use vendors::{all_vendors, auto_endpoint, expand_env_vars, find_vendor, resolve_vendor, EndpointVariant, VendorPreset};
