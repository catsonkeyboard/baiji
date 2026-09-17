//! 模型列表自动发现
//!
//! - OpenAI 兼容厂商：`GET {base}/models`（Bearer 认证）
//! - Anthropic：`GET {base}/v1/models`（x-api-key + anthropic-version）
//! 两者返回结构一致（`{"data": [{"id": ...}]}`），可统一解析。
//! OpenRouter 等厂商会附带 context_length 等额外字段，尽力提取。

use crate::provider::{Protocol, ProviderConfig};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::Duration;

/// 模型元信息
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    /// 上下文窗口（token）。厂商未返回时为 None，见 [`model_limits`] 的兜底
    pub context_length: Option<u64>,
    /// 单次响应最大输出（token）
    pub max_output_tokens: Option<u64>,
    pub owned_by: Option<String>,
}

/// 模型的有效上限（发现值优先，其次内置保守兜底）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelLimits {
    pub context_length: u64,
    /// None = 未知（不对 max_tokens 做钳制）
    pub max_output_tokens: Option<u64>,
    /// context_length 是否来自厂商 API（false = 兜底估计）
    pub discovered: bool,
}

/// 发现请求超时
const TIMEOUT: Duration = Duration::from_secs(15);
/// 分页上限（防御服务端 has_more 死循环）
const MAX_PAGES: usize = 20;

/// 拉取模型列表（Anthropic 协议自动翻页）
pub async fn list_models(config: &ProviderConfig) -> Result<Vec<ModelInfo>> {
    let client = reqwest::Client::builder().timeout(TIMEOUT).build()?;

    let url = match config.protocol {
        Protocol::Anthropic => format!("{}/v1/models", config.base_url.trim_end_matches('/')),
        _ => crate::openai::endpoint(&config.base_url, "/models"),
    };

    let mut models = Vec::new();
    let mut after_id: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let request = match config.protocol {
            Protocol::Anthropic => {
                // 默认每页 20 条：拉满单页上限并按 last_id 翻页
                let mut page_url = format!("{url}?limit=1000");
                if let Some(after) = &after_id {
                    // 模型 id 只含 URL 安全字符；防御性地丢弃其它字符
                    let safe: String = after
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                        .collect();
                    page_url.push_str(&format!("&after_id={safe}"));
                }
                client
                    .get(&page_url)
                    .header("x-api-key", &config.api_key)
                    .header("anthropic-version", "2023-06-01")
            }
            _ => client.get(&url).bearer_auth(&config.api_key),
        };

        let response = request
            .send()
            .await
            .with_context(|| format!("Failed to fetch models from {}", url))?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow::anyhow!(
                "Models API error (HTTP {}): {}",
                status.as_u16(),
                error_text
            ));
        }

        let body: ModelsResponse = response
            .json()
            .await
            .context("Failed to parse models response")?;

        let next = body.has_more.then_some(body.last_id).flatten();
        models.extend(body.data.into_iter().map(ModelEntry::into_info));
        match next {
            Some(id) if config.protocol == Protocol::Anthropic => after_id = Some(id),
            _ => break,
        }
    }
    Ok(models)
}

/// 面向厂商预设的模型发现。
///
/// 选用了 Anthropic 兼容端点变体（如 `https://api.deepseek.com/anthropic`）时，
/// 这类地址通常没有 `/v1/models`：先走厂商主端点（OpenAI 兼容）发现，
/// 失败再回退到变体地址本身。
pub async fn discover_models(
    vendor: &crate::vendors::VendorPreset,
    config: &ProviderConfig,
) -> Result<Vec<ModelInfo>> {
    let via_variant = config.base_url.trim_end_matches('/') != vendor.base_url.trim_end_matches('/')
        && config.protocol == Protocol::Anthropic
        && vendor.protocol != Protocol::Anthropic;
    if !via_variant {
        return list_models(config).await;
    }

    let primary = ProviderConfig {
        protocol: vendor.protocol,
        base_url: vendor.base_url.to_string(),
        ..config.clone()
    };
    match list_models(&primary).await {
        Ok(models) if !models.is_empty() => Ok(models),
        primary_result => match list_models(config).await {
            Ok(models) => Ok(models),
            // 两条路都失败：报主端点的错误（信息量更大）
            Err(variant_err) => primary_result.and(Err(variant_err)),
        },
    }
}

/// 明显不是对话模型的 id（embedding / 语音 / 图像 / 审核 / 重排等）
fn is_chat_model(id: &str) -> bool {
    const NON_CHAT: &[&str] = &[
        "embed", "whisper", "tts", "dall-e", "image", "moderation", "audio", "realtime",
        "transcribe", "rerank", "babbage", "davinci", "speech", "video", "ocr", "sora",
        "asr", "wanx", "cosyvoice", "paraformer", "vl-ocr", "guard",
    ];
    let lower = id.to_lowercase();
    !NON_CHAT.iter().any(|kw| lower.contains(kw))
}

/// 从发现结果里挑默认模型（只会返回列表中真实存在的 id）：
/// 1. 厂商预设的 `default_model`（若在列表中）
/// 2. 按厂商 `model_hints` 顺序，第一个命中的对话模型
/// 3. 第一个对话模型
///
/// 不再盲取 `models[0]`——OpenAI 的列表首项可能是 embedding / 图像模型。
pub fn pick_default_model<'a>(
    vendor: &crate::vendors::VendorPreset,
    models: &'a [ModelInfo],
) -> Option<&'a ModelInfo> {
    if let Some(default) = vendor.default_model
        && let Some(found) = models.iter().find(|m| m.id == default)
    {
        return Some(found);
    }
    let chat: Vec<&ModelInfo> = models.iter().filter(|m| is_chat_model(&m.id)).collect();
    for hint in vendor.model_hints {
        if let Some(found) = chat.iter().find(|m| m.id.to_lowercase().contains(hint)) {
            return Some(found);
        }
    }
    chat.first().copied()
}

/// 未知模型的上下文窗口兜底
const FALLBACK_CONTEXT: u64 = 64_000;

/// 内置兜底表：(id 子串, 上下文窗口)。**刻意取保守下界**——低估只会让上下文压缩
/// 提前触发，高估则会让请求因超长被拒。厂商 API 返回了真实值时不使用此表。
/// 更具体的条目排在前面。
const CONTEXT_FALLBACKS: &[(&str, u64)] = &[
    ("claude", 200_000),
    ("gpt-4.1", 1_000_000),
    ("gpt-4o", 128_000),
    ("gpt-5", 272_000),
    ("o1", 128_000),
    ("o3", 200_000),
    ("o4", 200_000),
    ("glm-4.6", 200_000),
    ("glm-4", 128_000),
    ("kimi-k2", 128_000),
    ("moonshot-v1-8k", 8_000),
    ("moonshot-v1-32k", 32_000),
    ("moonshot-v1-128k", 128_000),
    ("deepseek", 64_000),
    ("qwen3-coder", 256_000),
    ("qwen", 128_000),
    ("minimax", 200_000),
    ("grok-4", 256_000),
    ("grok", 128_000),
    ("mimo", 128_000),
];

/// 解析模型的有效上限：发现值 > 兜底表 > 全局保守默认
pub fn model_limits(model_id: &str, discovered: Option<&ModelInfo>) -> ModelLimits {
    let max_output_tokens = discovered.and_then(|m| m.max_output_tokens);
    if let Some(context_length) = discovered.and_then(|m| m.context_length).filter(|c| *c > 0) {
        return ModelLimits {
            context_length,
            max_output_tokens,
            discovered: true,
        };
    }
    let lower = model_id.to_lowercase();
    let context_length = CONTEXT_FALLBACKS
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, ctx)| *ctx)
        .unwrap_or(FALLBACK_CONTEXT);
    ModelLimits {
        context_length,
        max_output_tokens,
        discovered: false,
    }
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
    /// Anthropic 分页
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

/// 各厂商字段名不统一，全部按 alias 宽松解析
#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    /// Anthropic / 部分兼容厂商使用 display_name
    #[serde(default)]
    display_name: Option<String>,
    /// OpenRouter 使用 name
    #[serde(default)]
    name: Option<String>,
    #[serde(
        default,
        alias = "context_window",
        alias = "max_context_length",
        alias = "max_model_len",
        alias = "max_input_tokens",
        alias = "input_token_limit"
    )]
    context_length: Option<u64>,
    #[serde(
        default,
        alias = "max_tokens",
        alias = "max_completion_tokens",
        alias = "output_token_limit"
    )]
    max_output_tokens: Option<u64>,
    /// OpenRouter：`top_provider.{context_length,max_completion_tokens}`
    #[serde(default)]
    top_provider: Option<TopProvider>,
    #[serde(default)]
    owned_by: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TopProvider {
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

impl ModelEntry {
    fn into_info(self) -> ModelInfo {
        let top = self.top_provider;
        ModelInfo {
            id: self.id,
            display_name: self.display_name.or(self.name),
            context_length: self
                .context_length
                .or(top.as_ref().and_then(|t| t.context_length)),
            max_output_tokens: self
                .max_output_tokens
                .or(top.as_ref().and_then(|t| t.max_completion_tokens)),
            owned_by: self.owned_by,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_models_response() {
        let raw = r#"{
            "data": [
                {"id": "glm-4.7", "display_name": "GLM-4.7", "owned_by": "zhipu"},
                {"id": "qwen3-coder", "name": "Qwen3 Coder", "context_length": 1000000}
            ]
        }"#;
        let body: ModelsResponse = serde_json::from_str(raw).unwrap();
        let models: Vec<ModelInfo> = body.data.into_iter().map(ModelEntry::into_info).collect();

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].display_name.as_deref(), Some("GLM-4.7"));
        assert_eq!(models[1].context_length, Some(1_000_000));
    }

    #[test]
    fn test_parse_limits_across_vendor_shapes() {
        let raw = r#"{
            "data": [
                {"id": "or/model", "context_length": 200000,
                 "top_provider": {"context_length": 200000, "max_completion_tokens": 64000}},
                {"id": "vllm-model", "max_model_len": 32768},
                {"id": "a-model", "max_input_tokens": 1000000, "max_tokens": 128000}
            ],
            "has_more": true, "last_id": "a-model"
        }"#;
        let body: ModelsResponse = serde_json::from_str(raw).unwrap();
        assert!(body.has_more);
        assert_eq!(body.last_id.as_deref(), Some("a-model"));
        let models: Vec<ModelInfo> = body.data.into_iter().map(ModelEntry::into_info).collect();
        assert_eq!(models[0].max_output_tokens, Some(64_000));
        assert_eq!(models[1].context_length, Some(32_768));
        assert_eq!(models[2].context_length, Some(1_000_000));
        assert_eq!(models[2].max_output_tokens, Some(128_000));
    }

    fn info(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_pick_default_model_skips_non_chat_models() {
        let openai = crate::vendors::find_vendor("openai").unwrap();
        let models = vec![
            info("text-embedding-3-small"),
            info("dall-e-3"),
            info("whisper-1"),
            info("gpt-4o-mini-tts"),
            info("gpt-4o"),
            info("gpt-5"),
        ];
        // 按 hints 顺序优先 gpt-5，而不是列表首项（embedding）
        assert_eq!(pick_default_model(openai, &models).unwrap().id, "gpt-5");

        // 无 hint 命中 → 第一个对话模型
        let models = vec![info("text-embedding-3-small"), info("some-new-chat-model")];
        assert_eq!(
            pick_default_model(openai, &models).unwrap().id,
            "some-new-chat-model"
        );
        // 全是非对话模型 → None
        assert!(pick_default_model(openai, &[info("whisper-1")]).is_none());
    }

    #[test]
    fn test_model_limits_prefers_discovered_then_fallback() {
        let discovered = ModelInfo {
            context_length: Some(1_000_000),
            max_output_tokens: Some(32_000),
            ..info("x")
        };
        let limits = model_limits("x", Some(&discovered));
        assert!(limits.discovered);
        assert_eq!(limits.context_length, 1_000_000);
        assert_eq!(limits.max_output_tokens, Some(32_000));

        let limits = model_limits("claude-sonnet-4-5", None);
        assert!(!limits.discovered);
        assert_eq!(limits.context_length, 200_000);
        // 具体条目优先于宽泛条目
        assert_eq!(model_limits("glm-4.6", None).context_length, 200_000);
        assert_eq!(model_limits("glm-4.5-air", None).context_length, 128_000);
        assert_eq!(model_limits("totally-unknown", None).context_length, FALLBACK_CONTEXT);
    }

    #[test]
    fn test_models_url_uses_versioned_base() {
        // 智谱 /v4 基址 → /v4/models（不重复加 /v1）
        assert_eq!(
            crate::openai::endpoint("https://open.bigmodel.cn/api/paas/v4", "/models"),
            "https://open.bigmodel.cn/api/paas/v4/models"
        );
        // 裸根地址 → /v1/models
        assert_eq!(
            crate::openai::endpoint("https://api.deepseek.com", "/models"),
            "https://api.deepseek.com/v1/models"
        );
    }
}
