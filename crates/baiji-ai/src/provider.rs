//! Provider 抽象：trait、协议枚举与构建工厂

use crate::anthropic::AnthropicProvider;
use crate::openai::{ApiType, OpenAIProvider};
use crate::types::{ChatRequest, ChatResponse, StreamChunk};
use anyhow::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::sync::Arc;

/// 接口协议
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Anthropic Messages API（/v1/messages）
    Anthropic,
    /// OpenAI 兼容 Chat Completions（/v1/chat/completions）
    OpenAIChat,
    /// OpenAI Responses API（/v1/responses）
    OpenAIResponses,
}

impl Protocol {
    /// 解析配置值（大小写不敏感），接受常用别名
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "anthropic" | "messages" => Some(Self::Anthropic),
            "chat" | "chat_completions" | "openai" | "openai-chat" => Some(Self::OpenAIChat),
            "responses" | "openai-responses" => Some(Self::OpenAIResponses),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAIChat => "chat",
            Self::OpenAIResponses => "responses",
        }
    }
}

/// LLM Provider 统一接口
#[async_trait]
pub trait Provider: Send + Sync {
    /// 非流式请求
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// 流式请求
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<StreamChunk>>>;

    /// 使用的协议
    fn protocol(&self) -> Protocol;

    /// 模型名
    fn model(&self) -> &str;

    /// 展示名（如 "anthropic"、"openai"）
    fn provider_name(&self) -> &str;
}

/// Provider 构建配置
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl ProviderConfig {
    pub fn new(
        protocol: Protocol,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            protocol,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }
}

/// 根据配置构建 Provider
pub fn build_provider(config: ProviderConfig) -> Result<Arc<dyn Provider>> {
    match config.protocol {
        Protocol::Anthropic => Ok(Arc::new(AnthropicProvider::new(
            config.base_url,
            config.api_key,
            config.model,
        ))),
        Protocol::OpenAIChat => Ok(Arc::new(OpenAIProvider::new(
            ApiType::ChatCompletions,
            config.base_url,
            config.api_key,
            config.model,
        ))),
        Protocol::OpenAIResponses => Ok(Arc::new(OpenAIProvider::new(
            ApiType::Responses,
            config.base_url,
            config.api_key,
            config.model,
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_parse() {
        assert_eq!(Protocol::parse("anthropic"), Some(Protocol::Anthropic));
        assert_eq!(Protocol::parse("Messages"), Some(Protocol::Anthropic));
        assert_eq!(Protocol::parse("chat"), Some(Protocol::OpenAIChat));
        assert_eq!(Protocol::parse("openai-chat"), Some(Protocol::OpenAIChat));
        assert_eq!(Protocol::parse("responses"), Some(Protocol::OpenAIResponses));
        assert_eq!(Protocol::parse("other"), None);
    }

    #[test]
    fn test_build_provider_all_protocols() {
        for protocol in [
            Protocol::Anthropic,
            Protocol::OpenAIChat,
            Protocol::OpenAIResponses,
        ] {
            let provider = build_provider(ProviderConfig::new(
                protocol,
                "https://example.com",
                "key",
                "model-x",
            ))
            .unwrap();
            assert_eq!(provider.protocol(), protocol);
            assert_eq!(provider.model(), "model-x");
        }
    }
}
