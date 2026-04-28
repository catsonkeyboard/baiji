use crate::llm::types::{ChatRequest, ChatResponse, StreamChunk};
use anyhow::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::sync::Arc;

/// LLM Provider trait
/// 所有 LLM 提供商需要实现此 trait
#[async_trait]
pub trait LLMProvider: Send + Sync {
    /// 发送聊天请求，返回完整响应
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// 发送聊天请求，返回流式响应
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<StreamChunk>>>;

    /// 检查是否支持工具调用
    fn supports_tools(&self) -> bool {
        true
    }

    /// 获取提供商名称
    fn provider_name(&self) -> &str;
}

/// Provider 工厂
pub struct ProviderFactory;

impl ProviderFactory {
    /// 根据配置创建对应的 Provider
    pub fn create(
        provider_name: &str,
        base_url: String,
        api_key: String,
        model: String,
    ) -> Result<Arc<dyn LLMProvider>> {
        match provider_name {
            "anthropic" => {
                use crate::llm::anthropic::AnthropicProvider;
                Ok(Arc::new(AnthropicProvider::new(base_url, api_key, model)))
            }
            _ => Err(anyhow::anyhow!(
                "Unsupported provider: {}. Supported: anthropic",
                provider_name
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_factory_anthropic() {
        let result: Result<Arc<dyn LLMProvider>> = ProviderFactory::create(
            "anthropic",
            "https://api.anthropic.com".to_string(),
            "test-key".to_string(),
            "claude-3-5-sonnet".to_string(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_provider_factory_unsupported() {
        let result: Result<Arc<dyn LLMProvider>> = ProviderFactory::create(
            "unsupported",
            "https://api.example.com".to_string(),
            "test-key".to_string(),
            "model".to_string(),
        );
        assert!(result.is_err());
    }
}
