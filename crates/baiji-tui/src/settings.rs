//! TUI 运行时设置：配置文件读写（保留未知字段）+ Provider 热重建
//!
//! 供 `/config` 向导与 `/model` 命令使用：
//! - 读：原始 JSON + `api_key` 环境变量展开
//! - 写：serde_json::Value 往返，只动 vendor/endpoint/model/api_key 四个字段
//! - 重建：resolve_vendor → build_provider（端点决定协议路由）

use anyhow::{Context, Result};
use baiji_ai::Provider;
use std::path::Path;
use std::sync::Arc;

/// 当前生效的运行时设置镜像（TUI 内可变）
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    pub vendor: String,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    /// 已展开的明文 Key（向导输入或环境变量展开而来）
    pub api_key: String,
    /// 思考级别（None = 不启用）
    pub thinking: Option<baiji_ai::ThinkingLevel>,
}

/// 自动接力配置（T4）：run 结束且 todo 未完成时自动继续
#[derive(Debug, Clone, Copy)]
pub struct AutoContinueConfig {
    pub enabled: bool,
    /// 接力链的累计轮次上限
    pub max_turns: u32,
}

impl Default for AutoContinueConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_turns: 96,
        }
    }
}

/// 状态栏提示（"智谱 GLM · glm-4.7"）
pub fn status_hint(settings: &RuntimeSettings) -> String {
    let vendor = baiji_ai::find_vendor(&settings.vendor);
    let name = vendor
        .map(|v| v.display_name)
        .unwrap_or_else(|| settings.vendor.as_str());
    let model = settings.model.as_deref().unwrap_or("自动发现");
    format!("{name} · {model}")
}

/// 从配置文件读取（api_key 展开 $ENV；缺 key 时为空串，由向导补）。
/// main 装配时用已解析的配置直接构造，此入口供测试与后续复用。
#[cfg_attr(not(test), allow(dead_code))]
pub fn load(config_path: &Path) -> Result<RuntimeSettings> {
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("读取配置 {}", config_path.display()))?;
    let expanded = baiji_ai::expand_env_vars(&raw);
    let value: serde_json::Value = serde_json::from_str(&expanded)
        .with_context(|| format!("解析配置失败: {}", config_path.display()))?;

    let api_key = value["api_key"].as_str().unwrap_or_default().to_string();
    Ok(RuntimeSettings {
        vendor: value["vendor"].as_str().unwrap_or_default().to_string(),
        endpoint: value["endpoint"].as_str().map(String::from),
        model: value["model"].as_str().map(String::from),
        api_key,
        thinking: value["thinking"]
            .as_str()
            .and_then(baiji_ai::ThinkingLevel::parse),
    })
}

/// 配置文件里 api_key 字段的处理方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyUpdate<'a> {
    /// 保持文件中的原值（如 `"$ZHIPU_API_KEY"` 引用）不动
    Keep,
    /// 用户在向导中显式输入了新 Key
    Set(&'a str),
    /// 移除字段，回退到厂商推荐的环境变量（切换厂商且未输入新 Key 时）
    Remove,
}

/// 厂商切换且未输入新 Key 时的 Key 解析：绝不把旧厂商的 Key 发给新厂商，
/// 只从新厂商推荐的环境变量取；取不到返回空串（调用方报"缺少 API Key"）。
pub fn key_for_vendor(vendor_id: &str) -> String {
    baiji_ai::find_vendor(vendor_id)
        .and_then(|v| std::env::var(v.api_key_env).ok())
        .unwrap_or_default()
}

/// 写回配置文件：只更新四个字段，保留其它字段与顺序无关内容。
///
/// `settings.api_key` 是**已展开的明文**，绝不直接回写；
/// api_key 字段只按 `key` 指示处理。文件权限收紧为 0600。
pub fn save(config_path: &Path, settings: &RuntimeSettings, key: KeyUpdate<'_>) -> Result<()> {
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("读取配置 {}", config_path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("解析配置失败: {}", config_path.display()))?;

    let map = value.as_object_mut().context("配置根必须是 JSON 对象")?;
    map.insert("vendor".into(), serde_json::json!(settings.vendor));
    match &settings.endpoint {
        Some(e) => {
            map.insert("endpoint".into(), serde_json::json!(e));
        }
        None => {
            map.remove("endpoint");
        }
    }
    match &settings.model {
        Some(m) => {
            map.insert("model".into(), serde_json::json!(m));
        }
        None => {
            map.remove("model");
        }
    }
    match settings.thinking {
        Some(level) => {
            map.insert("thinking".into(), serde_json::json!(level.effort()));
        }
        None => {
            map.remove("thinking");
        }
    }
    match key {
        KeyUpdate::Keep => {}
        KeyUpdate::Set(typed) => {
            map.insert("api_key".into(), serde_json::json!(typed));
        }
        KeyUpdate::Remove => {
            map.remove("api_key");
        }
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_private(config_path, &(serde_json::to_string_pretty(&value)? + "\n"))
}

/// 以 0600 权限写文件（配置可能含明文 Key）；已存在的文件也收紧权限
fn write_private(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("写入配置 {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(content.as_bytes())?;
    Ok(())
}

/// 校验并构建 Provider（端点变体决定协议与地址）
pub fn build_provider(settings: &RuntimeSettings) -> Result<Arc<dyn Provider>> {
    if settings.vendor.trim().is_empty() {
        anyhow::bail!("vendor 未设置");
    }
    if settings.api_key.trim().is_empty() {
        anyhow::bail!("缺少 API Key（在向导中输入，或导出厂商推荐的环境变量）");
    }
    let vendor = baiji_ai::find_vendor(&settings.vendor)
        .with_context(|| format!("未知厂商 '{}'", settings.vendor))?;
    // 未显式选端点时，聚合厂商（OpenCode Zen）按模型家族自动选协议
    let endpoint = settings.endpoint.as_deref().or_else(|| {
        settings
            .model
            .as_deref()
            .and_then(|model| baiji_ai::auto_endpoint(vendor, model))
    });
    let config = baiji_ai::resolve_vendor(vendor, endpoint, None, None)?;
    if settings.model.as_deref().unwrap_or("").is_empty() {
        anyhow::bail!("model 未设置（用 /model <名称> 或在向导中选择）");
    }
    let config = baiji_ai::ProviderConfig {
        model: settings.model.clone().unwrap(),
        api_key: settings.api_key.clone(),
        ..config
    };
    baiji_ai::build_provider(config)
}

/// 拉取厂商模型列表（向导模型选择用；endpoint 变体参与 URL 路由）
pub async fn discover_models_async(settings: &RuntimeSettings) -> Result<Vec<baiji_ai::ModelInfo>> {
    if settings.api_key.trim().is_empty() {
        anyhow::bail!("缺少 API Key，无法获取模型列表");
    }
    let vendor = baiji_ai::find_vendor(&settings.vendor)
        .with_context(|| format!("未知厂商 '{}'", settings.vendor))?;
    let config = baiji_ai::resolve_vendor(vendor, settings.endpoint.as_deref(), None, None)?;
    let config = baiji_ai::ProviderConfig {
        api_key: settings.api_key.clone(),
        ..config
    };
    baiji_ai::discover_models(vendor, &config).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(path: &Path, vendor: &str, api_key: &str) {
        std::fs::write(
            path,
            format!(
                r#"{{"vendor": "{vendor}", "api_key": "{api_key}", "policy": {{"bash_timeout_secs": 99}}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn test_load_expands_env_and_save_preserves_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // SAFETY: 测试进程内设置环境变量
        unsafe { std::env::set_var("BAIJI_TUI_TEST_KEY", "sk-live") };
        write_config(&path, "glm", "$BAIJI_TUI_TEST_KEY");

        let settings = load(&path).unwrap();
        assert_eq!(settings.vendor, "glm");
        assert_eq!(settings.api_key, "sk-live"); // 已展开

        // 保存：更新 vendor/endpoint/model，未知字段 policy 保留
        let updated = RuntimeSettings {
            vendor: "glm".to_string(),
            endpoint: Some("coding".to_string()),
            model: Some("glm-4.7".to_string()),
            api_key: "sk-live".to_string(),
            thinking: None,
        };
        save(&path, &updated, KeyUpdate::Keep).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["endpoint"], "coding");
        assert_eq!(value["model"], "glm-4.7");
        // 展开后的明文 Key 绝不回写，$ENV 引用保持原样
        assert_eq!(value["api_key"], "$BAIJI_TUI_TEST_KEY");
        assert!(!raw.contains("sk-live"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(value["policy"]["bash_timeout_secs"], 99, "未知字段保留");

        // endpoint/model 置 None → 字段移除
        let cleared = RuntimeSettings {
            endpoint: None,
            model: None,
            ..updated
        };
        save(&path, &cleared, KeyUpdate::Keep).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value.get("endpoint").is_none());
        assert!(value.get("model").is_none());

        // 显式输入的新 Key 才写入
        save(&path, &cleared, KeyUpdate::Set("sk-typed")).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["api_key"], "sk-typed");

        // 切换厂商未输入 Key → 移除字段（回退到新厂商的环境变量）
        save(&path, &cleared, KeyUpdate::Remove).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value.get("api_key").is_none());
    }

    #[test]
    fn test_build_provider_routes_endpoint() {
        let settings = RuntimeSettings {
            vendor: "glm".to_string(),
            endpoint: Some("coding".to_string()),
            model: Some("glm-4.7".to_string()),
            api_key: "sk-test".to_string(),
            thinking: None,
        };
        let provider = build_provider(&settings).unwrap();
        assert_eq!(provider.protocol(), baiji_ai::Protocol::OpenAIChat);
        assert_eq!(provider.model(), "glm-4.7");

        // 缺 Key / 缺 model 的报错
        let bad = RuntimeSettings {
            api_key: String::new(),
            thinking: None,
            ..settings.clone()
        };
        assert!(build_provider(&bad).is_err());
        let bad = RuntimeSettings {
            model: None,
            ..settings
        };
        assert!(build_provider(&bad).is_err());
    }

    #[test]
    fn test_status_hint() {
        let settings = RuntimeSettings {
            vendor: "glm".to_string(),
            endpoint: None,
            model: Some("glm-4.7".to_string()),
            api_key: String::new(),
            thinking: None,
        };
        assert_eq!(status_hint(&settings), "智谱 GLM · glm-4.7");
        let settings = RuntimeSettings {
            model: None,
            ..settings
        };
        assert!(status_hint(&settings).contains("自动发现"));
    }
}
