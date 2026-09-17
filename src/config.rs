//! 应用配置：厂商预设 + API Key 解析 + 策略项
//!
//! 路径：`~/.baiji/config.json`（不存在时自动生成模板）。
//! 最小配置只需 `vendor` + `api_key`，其余字段均可选：
//!
//! ```json
//! { "vendor": "glm", "api_key": "$ZHIPU_API_KEY" }
//! ```

use anyhow::{anyhow, Context, Result};
use baiji_ai::{self, Protocol, ProviderConfig, VendorPreset};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 应用配置根
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    /// 厂商 ID 或别名（openai/anthropic/openrouter/bailian/tencent/glm/kimi/deepseek/minimax/mimo/opencode/xai）
    pub vendor: String,
    /// API Key，支持 $ENV_VAR / ${ENV_VAR} 展开；缺省读厂商推荐的环境变量
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// 模型名（缺省时自动发现并取厂商首个模型）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 厂商端点选择（默认 "api" 按量付费；如 glm 的 Coding Plan：
    /// "anthropic" / "coding" / "responses"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Base URL 覆盖（默认厂商预设端点）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// 协议覆盖：anthropic / chat / responses（默认厂商预设）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 用 LLM 生成上下文压缩摘要（默认 false = 确定性摘要）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_compaction: Option<bool>,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiConfig>,
}

/// 工具策略
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyConfig {
    /// 额外允许访问的目录（相对当前目录；默认白名单 = 当前目录）
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// 需要用户确认（HITL）的工具名；非空时执行前弹确认框
    #[serde(default)]
    pub require_confirmation_tools: Vec<String>,
    /// 单次工具输出最大字节数
    #[serde(default = "default_max_output_bytes")]
    pub max_tool_output_bytes: usize,
    /// bash 默认超时（秒）
    #[serde(default = "default_bash_timeout")]
    pub bash_timeout_secs: u64,
}

fn default_max_output_bytes() -> usize {
    32 * 1024
}

fn default_bash_timeout() -> u64 {
    30
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            allowed_paths: Vec::new(),
            require_confirmation_tools: Vec::new(),
            max_tool_output_bytes: default_max_output_bytes(),
            bash_timeout_secs: default_bash_timeout(),
        }
    }
}

/// UI 配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UiConfig {
    #[serde(default = "default_theme")]
    pub theme: String,
}

fn default_theme() -> String {
    "dark".to_string()
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
        }
    }
}

/// 解析后的 Provider 接入信息
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub vendor: &'static VendorPreset,
    pub config: ProviderConfig,
    /// api_key 的来源（显式配置 / 环境变量名）
    pub key_source: String,
}

impl AppConfig {
    /// 默认配置路径 ~/.baiji/config.json；不存在时生成模板
    pub fn load() -> Result<Self> {
        let path = Self::default_path()?;
        if !path.exists() {
            let template = Self::template();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, &template)
                .with_context(|| format!("写入默认配置 {}", path.display()))?;
            // 配置会存放 API Key：仅属主可读写
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
            }
            eprintln!(
                "已生成默认配置 {}\n编辑该文件填写 api_key 后重新启动。\n支持厂商: {}",
                path.display(),
                baiji_ai::all_vendors()
                    .iter()
                    .map(|v| v.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return serde_json::from_str(&template).context("解析默认配置模板");
        }
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置 {}", path.display()))?;
        let expanded = expand_env_vars(&raw)?;
        serde_json::from_str(&expanded)
            .with_context(|| format!("解析配置失败，请检查 JSON 格式: {}", path.display()))
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(dirs::home_dir()
            .context("无法获取用户主目录")?
            .join(".baiji")
            .join("config.json"))
    }

    /// 首次生成的模板
    pub fn template() -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "vendor": "glm",
            "api_key": "$ZHIPU_API_KEY",
            "model": null,
            "endpoint": null,
            "protocol": null,
            "base_url": null,
            "max_tokens": 8192,
            "llm_compaction": false,
            "policy": {
                "allowed_paths": [],
                "require_confirmation_tools": ["bash", "write", "edit"],
                "max_tool_output_bytes": 32768,
                "bash_timeout_secs": 30
            },
            "ui": { "theme": "dark" }
        }))
        .unwrap()
            + "\n"
    }

    /// 校验 + 解析为 Provider 配置
    pub fn resolve(&self) -> Result<ResolvedProvider> {
        let vendor = baiji_ai::find_vendor(&self.vendor).ok_or_else(|| {
            anyhow!(
                "未知的 vendor '{}'，支持: {}",
                self.vendor,
                baiji_ai::all_vendors()
                    .iter()
                    .map(|v| v.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

        // 协议校验（覆盖值必须合法）
        if let Some(protocol) = &self.protocol {
            if Protocol::parse(protocol).is_none() {
                return Err(anyhow!(
                    "未知的 protocol '{protocol}'，可选: anthropic / chat / responses"
                ));
            }
        }

        // 端点校验（未知端点直接列出可选项，避免运行期才失败）
        if let Some(endpoint) = &self.endpoint {
            if endpoint != "api" && vendor.find_endpoint(endpoint).is_none() {
                return Err(anyhow!(
                    "厂商 '{}' 无端点 '{}'，可选: {}",
                    vendor.id,
                    endpoint,
                    vendor.endpoint_names().join(", ")
                ));
            }
        }

        // API Key：显式配置 > 厂商推荐环境变量
        let (api_key, key_source) = match &self.api_key {
            Some(key) if !key.is_empty() => (key.clone(), "config".to_string()),
            _ => {
                let env = vendor.api_key_env;
                let key = std::env::var(env).unwrap_or_default();
                if key.is_empty() {
                    return Err(anyhow!(
                        "缺少 API Key：请在配置中设置 api_key，或导出环境变量 {env}"
                    ));
                }
                (key, env.to_string())
            }
        };

        let mut config = baiji_ai::resolve_vendor(
            vendor,
            self.endpoint.as_deref(),
            self.base_url.as_deref(),
            self.protocol.as_deref(),
        )?;
        config.api_key = api_key;

        Ok(ResolvedProvider {
            vendor,
            config,
            key_source,
        })
    }
}

/// 展开 $VAR / ${VAR}（未定义的变量原样保留）；实现见 baiji-ai
pub fn expand_env_vars(content: &str) -> Result<String> {
    Ok(baiji_ai::expand_env_vars(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env_vars() {
        // SAFETY: 测试进程内设置环境变量
        unsafe { std::env::set_var("BAIJI_TEST_KEY", "secret123") };

        assert_eq!(
            expand_env_vars(r#"{"api_key": "$BAIJI_TEST_KEY"}"#).unwrap(),
            r#"{"api_key": "secret123"}"#
        );
        assert_eq!(
            expand_env_vars(r#"{"api_key": "${BAIJI_TEST_KEY}"}"#).unwrap(),
            r#"{"api_key": "secret123"}"#
        );
        // 未定义变量保留原样
        assert_eq!(
            expand_env_vars(r#"{"k": "$NOT_DEFINED_VAR_XYZ"}"#).unwrap(),
            r#"{"k": "$NOT_DEFINED_VAR_XYZ"}"#
        );
        // 普通文本不受影响
        assert_eq!(
            expand_env_vars(r#"{"model": "glm-4.7", "n": 1.5}"#).unwrap(),
            r#"{"model": "glm-4.7", "n": 1.5}"#
        );
    }

    #[test]
    fn test_resolve_vendor_and_key_precedence() {
        let config = AppConfig {
            vendor: "glm".to_string(),
            api_key: Some("explicit-key".to_string()),
            model: None,
            endpoint: None,
            base_url: None,
            protocol: None,
            max_tokens: None,
            llm_compaction: None,
            policy: PolicyConfig::default(),
            ui: None,
        };
        let resolved = config.resolve().unwrap();
        assert_eq!(resolved.vendor.id, "glm");
        assert_eq!(resolved.config.api_key, "explicit-key");
        assert_eq!(resolved.config.base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert_eq!(resolved.config.protocol, Protocol::OpenAIChat);
    }

    #[test]
    fn test_resolve_unknown_vendor_and_protocol() {
        let config = AppConfig {
            vendor: "nope".to_string(),
            api_key: Some("k".to_string()),
            model: None,
            endpoint: None,
            base_url: None,
            protocol: None,
            max_tokens: None,
            llm_compaction: None,
            policy: PolicyConfig::default(),
            ui: None,
        };
        assert!(config.resolve().is_err());

        let config = AppConfig {
            vendor: "glm".to_string(),
            api_key: Some("k".to_string()),
            model: None,
            endpoint: None,
            base_url: None,
            protocol: Some("bogus".to_string()),
            max_tokens: None,
            llm_compaction: None,
            policy: PolicyConfig::default(),
            ui: None,
        };
        assert!(config.resolve().is_err());
    }

    #[test]
    fn test_resolve_missing_key_reports_env_hint() {
        let config = AppConfig {
            vendor: "kimi".to_string(),
            api_key: None,
            model: None,
            endpoint: None,
            base_url: None,
            protocol: None,
            max_tokens: None,
            llm_compaction: None,
            policy: PolicyConfig::default(),
            ui: None,
        };
        let err = config.resolve().unwrap_err().to_string();
        assert!(err.contains("MOONSHOT_API_KEY"), "unexpected: {err}");
    }

    #[test]
    fn test_resolve_coding_plan_endpoints() {
        // GLM Coding Plan：三种协议端点各自生效
        let config = AppConfig {
            vendor: "glm".to_string(),
            api_key: Some("k".to_string()),
            model: Some("glm-4.7".to_string()),
            endpoint: Some("coding".to_string()),
            base_url: None,
            protocol: None,
            max_tokens: None,
            llm_compaction: None,
            policy: PolicyConfig::default(),
            ui: None,
        };
        let resolved = config.resolve().unwrap();
        assert_eq!(resolved.config.protocol, Protocol::OpenAIChat);
        assert_eq!(
            resolved.config.base_url,
            "https://open.bigmodel.cn/api/coding/paas/v4"
        );

        let config = AppConfig {
            endpoint: Some("responses".to_string()),
            ..config
        };
        let resolved = config.resolve().unwrap();
        assert_eq!(resolved.config.protocol, Protocol::OpenAIResponses);
        assert_eq!(resolved.config.base_url, "https://open.bigmodel.cn/api/v1");

        let config = AppConfig {
            endpoint: Some("anthropic".to_string()),
            ..config
        };
        let resolved = config.resolve().unwrap();
        assert_eq!(resolved.config.protocol, Protocol::Anthropic);
        assert_eq!(
            resolved.config.base_url,
            "https://open.bigmodel.cn/api/anthropic"
        );

        // 未知端点 → 报错列出可选项
        let config = AppConfig {
            endpoint: Some("bogus".to_string()),
            ..config
        };
        let err = config.resolve().unwrap_err().to_string();
        assert!(err.contains("api, anthropic, coding, responses"), "{err}");
    }

    #[test]
    fn test_template_is_valid_config() {
        let template = AppConfig::template();
        let config: AppConfig = serde_json::from_str(&template).unwrap();
        assert_eq!(config.vendor, "glm");
    }
}
