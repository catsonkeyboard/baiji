use crate::agent::tool_policy::PolicyConfig;
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 应用配置根结构
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// LLM 配置
    pub llm: LLMConfig,
    /// 工具策略配置（可选，缺省使用安全默认值）
    #[serde(default)]
    pub policy: PolicyConfig,
    /// UI 配置（可选）
    #[serde(default)]
    pub ui: Option<UIConfig>,
}

/// LLM 提供商配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LLMConfig {
    /// 提供商类型: "anthropic", "openai"
    pub provider: String,
    /// API 基础 URL
    pub base_url: String,
    /// API 密钥
    pub api_key: String,
    /// 模型名称
    pub model: String,
    /// 最大 token 数（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 温度参数（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

/// UI 配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UIConfig {
    /// 主题: "dark", "light"
    #[serde(default = "default_theme")]
    pub theme: String,
    /// 是否显示思考过程
    #[serde(default = "default_show_thoughts")]
    pub show_thoughts: bool,
}

fn default_theme() -> String {
    "dark".to_string()
}

fn default_show_thoughts() -> bool {
    false
}

impl Default for UIConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            show_thoughts: default_show_thoughts(),
        }
    }
}

impl Config {
    /// 从默认路径加载配置
    /// 默认路径: ~/.baiji/config.json，不存在时自动创建默认配置
    pub fn load() -> Result<Self> {
        let config_path = Self::default_config_path()?;
        if !config_path.exists() {
            let default_config = Config::default();
            default_config.save_to_path(&config_path)?;
            eprintln!(
                "已创建默认配置文件: {}\n请编辑该文件填写 API 密钥后重新启动。",
                config_path.display()
            );
            return Ok(default_config);
        }
        Self::load_from_path(&config_path)
    }

    /// 从指定路径加载配置
    pub fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            return Err(anyhow::anyhow!(
                "配置文件不存在: {}",
                path.display()
            ));
        }

        let content = std::fs::read_to_string(path)
            .with_context(|| format!("无法读取配置文件: {}", path.display()))?;

        // 扩展环境变量
        let expanded_content = Self::expand_env_vars(&content)?;

        let config: Config = serde_json::from_str(&expanded_content)
            .with_context(|| format!("解析配置文件失败，请检查 JSON 格式: {}", path.display()))?;

        config.validate()?;

        Ok(config)
    }

    /// 获取默认配置文件路径
    /// 路径: ~/.baiji/config.json
    pub fn default_config_path() -> Result<PathBuf> {
        let config_dir = dirs::home_dir()
            .context("无法获取用户主目录")?
            .join(".baiji");

        // 确保配置目录存在
        if !config_dir.exists() {
            std::fs::create_dir_all(&config_dir)
                .with_context(|| format!("无法创建配置目录: {}", config_dir.display()))?;
        }

        Ok(config_dir.join("config.json"))
    }

    /// 验证配置有效性
    pub fn validate(&self) -> Result<()> {
        // 验证 LLM 配置
        match self.llm.provider.as_str() {
            "anthropic" | "openai" => {}
            _ => {
                return Err(anyhow::anyhow!(
                    "不支持的 LLM 提供商: {}，支持的提供商: anthropic, openai",
                    self.llm.provider
                ));
            }
        }

        if self.llm.api_key.is_empty() {
            return Err(anyhow::anyhow!("LLM API 密钥不能为空"));
        }

        if self.llm.base_url.is_empty() {
            return Err(anyhow::anyhow!("LLM base_url 不能为空"));
        }

        if self.llm.model.is_empty() {
            return Err(anyhow::anyhow!("LLM 模型名称不能为空"));
        }

        Ok(())
    }

    /// 扩展环境变量
    /// 支持格式: $VAR_NAME 或 ${VAR_NAME}
    fn expand_env_vars(content: &str) -> Result<String> {
        // 匹配 $VAR_NAME 或 ${VAR_NAME}
        let re = Regex::new(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?").context("无法编译正则表达式")?;

        let result = re.replace_all(content, |caps: &regex::Captures| {
            let var_name = &caps[1];
            match std::env::var(var_name) {
                Ok(value) => value,
                Err(_) => {
                    // 如果环境变量不存在，保留原样
                    caps[0].to_string()
                }
            }
        });

        Ok(result.to_string())
    }

    /// 保存配置到默认路径
    pub fn save(&self) -> Result<()> {
        let path = Self::default_config_path()?;
        self.save_to_path(&path)
    }

    /// 保存配置到指定路径
    pub fn save_to_path<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();

        // 确保父目录存在
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("无法创建目录: {}", parent.display()))?;
            }
        }

        let content = serde_json::to_string_pretty(self).context("无法序列化配置为 JSON")?;

        std::fs::write(path, content)
            .with_context(|| format!("无法写入配置文件: {}", path.display()))?;

        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            llm: LLMConfig::default(),
            policy: PolicyConfig::default(),
            ui: Some(UIConfig::default()),
        }
    }
}

impl Default for LLMConfig {
    fn default() -> Self {
        Self {
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            api_key: String::new(),
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(4096),
            temperature: Some(0.7),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_valid_config() {
        let config_json = r#"{
            "llm": {
                "provider": "anthropic",
                "base_url": "https://api.anthropic.com",
                "api_key": "test-api-key",
                "model": "claude-3-5-sonnet-20241022",
                "max_tokens": 4096,
                "temperature": 0.7
            }
        }"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_json.as_bytes()).unwrap();

        let config = Config::load_from_path(temp_file.path()).unwrap();

        assert_eq!(config.llm.provider, "anthropic");
        assert_eq!(config.llm.api_key, "test-api-key");
    }

    #[test]
    fn test_expand_env_vars() {
        // SAFETY: This test runs in isolation; setting env var is safe here.
        unsafe { std::env::set_var("TEST_API_KEY", "secret123"); }

        let input = r#"{"api_key": "$TEST_API_KEY"}"#;
        let result = Config::expand_env_vars(input).unwrap();

        assert!(result.contains("secret123"));
        assert!(!result.contains("$TEST_API_KEY"));
    }

    #[test]
    fn test_validate_empty_api_key() {
        let config = Config {
            llm: LLMConfig {
                provider: "anthropic".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
                api_key: "".to_string(),
                model: "claude-3-5-sonnet".to_string(),
                max_tokens: None,
                temperature: None,
            },
            policy: PolicyConfig::default(),
            ui: None,
        };

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_unsupported_provider() {
        let config = Config {
            llm: LLMConfig {
                provider: "unsupported".to_string(),
                base_url: "https://api.example.com".to_string(),
                api_key: "test".to_string(),
                model: "test-model".to_string(),
                max_tokens: None,
                temperature: None,
            },
            policy: PolicyConfig::default(),
            ui: None,
        };

        assert!(config.validate().is_err());
    }
}
