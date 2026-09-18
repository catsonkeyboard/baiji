//! 应用配置：厂商预设 + API Key 解析 + 策略项（参考 pi 的双作用域设置模型）
//!
//! 两级配置深合并（对象递归合并，数组/标量整体覆盖，项目级优先）：
//! - 全局：`~/.baiji/config.json`（不存在时自动生成模板）
//! - 项目：`./.baiji/config.json`（与 skills/prompts 同目录；可选）
//!
//! 项目级只能覆盖白名单字段（model/max_tokens/max_turns/llm_compaction/
//! compaction/retry/ui/policy 的非安全字段）。安全敏感项只认全局配置：
//! api_key/vendor/endpoint/base_url/protocol（防项目重定向把 Key 发到任意
//! 服务器）与 policy.allowed_paths/require_confirmation_tools（防恶意仓库
//! 放开路径沙箱与人工确认）。
//!
//! 最小配置只需 `vendor` + `api_key`，其余字段均可选：
//!
//! ```json
//! { "vendor": "glm", "api_key": "$ZHIPU_API_KEY" }
//! ```

use anyhow::{Context, Result, anyhow};
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
    /// 单次运行内 LLM 轮次上限（防失控循环）
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiConfig>,
    /// 项目级配置是否生效（加载时判定；不入 JSON）
    #[serde(skip)]
    pub project_config_applied: bool,
}

/// 上下文压缩设置（参考 pi 的 CompactionSettings）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompactionConfig {
    /// 总开关：关闭后两级压缩（tool result stub 化 + 摘要折叠）都不触发。
    /// 运行中的防超窗就地精简（runtime context budget）不受影响
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// token 预算覆盖（缺省 = 按模型上下文窗口推导：窗口 70% 再扣输出预留）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_estimated_tokens: Option<u64>,
    /// 摘要/stub 化时保留的最近完整轮次数
    #[serde(default = "default_keep_recent_turns")]
    pub keep_recent_turns: usize,
}

/// 瞬时错误重试设置（参考 pi 的 RetrySettings）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    /// 最大重试次数（0 = 不重试）
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// 指数退避基距（毫秒）：base × 2^attempt
    #[serde(default = "default_retry_base_delay_ms")]
    pub base_delay_ms: u64,
    /// 单次退避上限（毫秒），服务端 Retry-After 也受此约束
    #[serde(default = "default_retry_max_delay_ms")]
    pub max_delay_ms: u64,
}

fn default_max_turns() -> u32 {
    24
}

fn default_true() -> bool {
    true
}

fn default_keep_recent_turns() -> usize {
    6
}

fn default_max_retries() -> u32 {
    2
}

fn default_retry_base_delay_ms() -> u64 {
    500
}

fn default_retry_max_delay_ms() -> u64 {
    30_000
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_estimated_tokens: None,
            keep_recent_turns: default_keep_recent_turns(),
        }
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            base_delay_ms: default_retry_base_delay_ms(),
            max_delay_ms: default_retry_max_delay_ms(),
        }
    }
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
    /// 内容感知域压缩开关（JSON/表格/构建日志）。
    /// 关闭即 A/B 对照臂：只保留通用 shell 规则与截断。
    /// 环境变量 `BAIJI_COMPRESSION=off` 可免改配置临时关闭
    #[serde(default = "default_compression_enabled")]
    pub compression_enabled: bool,
    /// verbosity steer：每轮请求向最后一条 user 消息追加恒定"简洁作答"
    /// 指令（请求级注入，不改历史；实测可省约三分之一输出 token）。
    /// 环境变量 `BAIJI_VERBOSITY_STEER=on|off` 可免改配置切换
    #[serde(default)]
    pub verbosity_steer: bool,
    /// 自动接力（T4）：run 结束且 todo 仍有未完成项时，以固定输入自动继续，
    /// 直到完成或轮次上限（Esc / Ctrl-C 可停）。headless 用 --continue-until-done 开启
    #[serde(default)]
    pub auto_continue: bool,
    /// 自动接力的累计轮次上限（按一次用户输入触发的接力链计）
    #[serde(default = "default_auto_continue_max_turns")]
    pub auto_continue_max_turns: u32,
}

fn default_auto_continue_max_turns() -> u32 {
    96
}

fn default_max_output_bytes() -> usize {
    32 * 1024
}

fn default_bash_timeout() -> u64 {
    30
}

fn default_compression_enabled() -> bool {
    true
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            allowed_paths: Vec::new(),
            require_confirmation_tools: Vec::new(),
            max_tool_output_bytes: default_max_output_bytes(),
            bash_timeout_secs: default_bash_timeout(),
            compression_enabled: default_compression_enabled(),
            verbosity_steer: false,
            auto_continue: false,
            auto_continue_max_turns: default_auto_continue_max_turns(),
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

/// 项目级配置可覆盖的顶层字段白名单
const PROJECT_ALLOWED_TOP: &[&str] = &[
    "model",
    "max_tokens",
    "max_turns",
    "llm_compaction",
    "policy",
    "compaction",
    "retry",
    "ui",
];
/// policy 内项目级可覆盖的子字段（安全字段全局专用）
const PROJECT_ALLOWED_POLICY: &[&str] = &[
    "max_tool_output_bytes",
    "bash_timeout_secs",
    "compression_enabled",
    "verbosity_steer",
    "auto_continue",
    "auto_continue_max_turns",
];

/// 深合并：对象递归合并，数组/标量整体覆盖（overlay 优先）——与 pi 的语义一致
fn deep_merge(base: serde_json::Value, overlay: serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(mut b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                let merged = match b.remove(&k) {
                    Some(bv) => deep_merge(bv, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            serde_json::Value::Object(b)
        }
        (_, overlay) => overlay,
    }
}

/// 项目级白名单过滤：安全敏感字段被移除并警告（不失败——仓库里放什么都不能崩）
fn filter_project_scope(mut project: serde_json::Value, path: &Path) -> serde_json::Value {
    let Some(map) = project.as_object_mut() else {
        return project;
    };
    let keys: Vec<String> = map.keys().cloned().collect();
    for k in keys {
        if !PROJECT_ALLOWED_TOP.contains(&k.as_str()) {
            eprintln!(
                "警告: 项目配置 {} 忽略安全敏感字段 '{k}'（仅全局配置可设置）",
                path.display()
            );
            map.remove(&k);
        }
    }
    if let Some(policy) = map
        .get_mut("policy")
        .and_then(serde_json::Value::as_object_mut)
    {
        let keys: Vec<String> = policy.keys().cloned().collect();
        for k in keys {
            if !PROJECT_ALLOWED_POLICY.contains(&k.as_str()) {
                eprintln!(
                    "警告: 项目配置 {} 忽略 'policy.{k}'（安全敏感，仅全局配置可设置）",
                    path.display()
                );
                policy.remove(&k);
            }
        }
    }
    project
}

/// 已知顶层字段（未知字段警告用；拼写错误早发现）
const KNOWN_TOP_FIELDS: &[&str] = &[
    "vendor",
    "api_key",
    "model",
    "endpoint",
    "base_url",
    "protocol",
    "max_tokens",
    "llm_compaction",
    "max_turns",
    "policy",
    "compaction",
    "retry",
    "ui",
];

fn warn_unknown_fields(value: &serde_json::Value, source: &str) {
    let Some(map) = value.as_object() else {
        return;
    };
    for k in map.keys() {
        if !KNOWN_TOP_FIELDS.contains(&k.as_str()) {
            eprintln!("警告: {source} 的未知字段 '{k}' 被忽略");
        }
    }
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
            let config: AppConfig = serde_json::from_str(&template).context("解析默认配置模板")?;
            return Ok(config.with_env_overrides());
        }
        Self::load_from(&path)
    }

    /// 全局配置 + 当前目录的项目级配置（`./.baiji/config.json`，可选）
    pub fn load_from(path: &Path) -> Result<Self> {
        let project = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".baiji")
            .join("config.json");
        Self::load_with_project(path, &project)
    }

    /// 分层加载：全局 → 项目（白名单过滤后深合并，项目优先）→ 环境变量覆盖
    pub fn load_with_project(global_path: &Path, project_path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(global_path)
            .with_context(|| format!("读取配置 {}", global_path.display()))?;
        let global: serde_json::Value = serde_json::from_str(&expand_env_vars(&raw)?)
            .with_context(|| {
                format!("解析配置失败，请检查 JSON 格式: {}", global_path.display())
            })?;
        warn_unknown_fields(&global, &format!("配置 {}", global_path.display()));

        let mut merged = global;
        let mut project_applied = false;
        if project_path.exists() {
            let praw = std::fs::read_to_string(project_path)
                .with_context(|| format!("读取项目配置 {}", project_path.display()))?;
            let project: serde_json::Value = serde_json::from_str(&expand_env_vars(&praw)?)
                .with_context(|| {
                    format!(
                        "解析项目配置失败，请检查 JSON 格式: {}",
                        project_path.display()
                    )
                })?;
            let filtered = filter_project_scope(project, project_path);
            warn_unknown_fields(&filtered, &format!("项目配置 {}", project_path.display()));
            project_applied = true;
            merged = deep_merge(merged, filtered);
        }

        let mut config: AppConfig = serde_json::from_value(merged)
            .with_context(|| format!("解析配置失败: {}", global_path.display()))?;
        config.project_config_applied = project_applied;
        Ok(config.with_env_overrides())
    }

    /// 映射为 harness 的压缩策略（结合模型窗口推导默认预算）
    pub fn compaction_policy(
        &self,
        context_length: u64,
        max_output_reserve: u32,
    ) -> baiji_harness::CompactionPolicy {
        if !self.compaction.enabled {
            // 总开关关闭：预算无限大 → stub 化与摘要都不会触发
            return baiji_harness::CompactionPolicy {
                max_estimated_tokens: usize::MAX,
                keep_recent_turns: self.compaction.keep_recent_turns,
            };
        }
        let mut policy =
            baiji_harness::CompactionPolicy::for_context(context_length, max_output_reserve);
        if let Some(budget) = self.compaction.max_estimated_tokens {
            policy.max_estimated_tokens = budget as usize;
        }
        policy.keep_recent_turns = self.compaction.keep_recent_turns;
        policy
    }

    /// 供 /status 展示的运行时设置摘要
    pub fn runtime_summary(&self) -> String {
        let on_off = |b: bool| if b { "on" } else { "off" };
        let compaction = if !self.compaction.enabled {
            "off".to_string()
        } else {
            match self.compaction.max_estimated_tokens {
                Some(b) => format!("on (budget {b})"),
                None => "on (auto)".to_string(),
            }
        };
        format!(
            "max_tokens: {} · max_turns: {} · compaction: {compaction} (keep {} turns) · \
             retry: {}×{}ms (cap {}ms) · compression: {} · verbosity_steer: {} · \
             auto_continue: {} (cap {}) · project config: {}",
            self.max_tokens
                .map(|v| v.to_string())
                .unwrap_or_else(|| "默认".into()),
            self.max_turns,
            self.compaction.keep_recent_turns,
            self.retry.max_retries,
            self.retry.base_delay_ms,
            self.retry.max_delay_ms,
            on_off(self.policy.compression_enabled),
            on_off(self.policy.verbosity_steer),
            on_off(self.policy.auto_continue),
            self.policy.auto_continue_max_turns,
            if self.project_config_applied {
                "生效"
            } else {
                "无"
            },
        )
    }

    /// 环境变量覆盖（A/B 对照免改配置）：`BAIJI_COMPRESSION=off|0|false|no` 关闭域压缩；
    /// `BAIJI_VERBOSITY_STEER=on|1|true|yes` 开启（其余值关闭）
    fn with_env_overrides(mut self) -> Self {
        if let Ok(v) = std::env::var("BAIJI_COMPRESSION") {
            let off = matches!(v.to_lowercase().as_str(), "off" | "0" | "false" | "no");
            self.policy.compression_enabled = !off;
        }
        if let Ok(v) = std::env::var("BAIJI_VERBOSITY_STEER") {
            let on = matches!(v.to_lowercase().as_str(), "on" | "1" | "true" | "yes");
            self.policy.verbosity_steer = on;
        }
        self
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
            "max_turns": 24,
            "compaction": {
                "enabled": true,
                "max_estimated_tokens": null,
                "keep_recent_turns": 6
            },
            "retry": {
                "max_retries": 2,
                "base_delay_ms": 500,
                "max_delay_ms": 30000
            },
            "policy": {
                "allowed_paths": [],
                "require_confirmation_tools": ["bash", "write", "edit"],
                "max_tool_output_bytes": 32768,
                "bash_timeout_secs": 30,
                "compression_enabled": true,
                "verbosity_steer": false,
                "auto_continue": false,
                "auto_continue_max_turns": 96
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
    fn test_verbosity_steer_env_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"vendor":"glm","api_key":"k"}"#).unwrap();
        // 环境变量开启（免改配置的 A/B 开关）
        unsafe { std::env::set_var("BAIJI_VERBOSITY_STEER", "on") };
        let cfg = AppConfig::load_from(&path).unwrap();
        unsafe { std::env::remove_var("BAIJI_VERBOSITY_STEER") };
        assert!(cfg.policy.verbosity_steer);
        // 未设置时：配置默认关闭
        let cfg = AppConfig::load_from(&path).unwrap();
        assert!(!cfg.policy.verbosity_steer);
    }

    #[test]
    fn test_policy_compression_defaults_and_env_override() {
        // 旧配置缺失 compression_enabled 字段 → serde 默认开启
        let cfg: AppConfig =
            serde_json::from_str(r#"{"vendor":"glm","api_key":"k","policy":{}}"#).unwrap();
        assert!(cfg.policy.compression_enabled);

        // BAIJI_COMPRESSION=off 免改配置关闭域压缩
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"vendor":"glm","api_key":"k"}"#).unwrap();
        unsafe { std::env::set_var("BAIJI_COMPRESSION", "off") };
        let cfg = AppConfig::load_from(&path).unwrap();
        unsafe { std::env::remove_var("BAIJI_COMPRESSION") };
        assert!(!cfg.policy.compression_enabled);

        // 未设置环境变量时保持配置值
        let cfg = AppConfig::load_from(&path).unwrap();
        assert!(cfg.policy.compression_enabled);
    }

    #[test]
    fn test_project_config_merges_with_whitelist() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.json");
        let project = dir.path().join("project.json");
        std::fs::write(
            &global,
            r#"{"vendor":"glm","api_key":"global-key","model":"glm-4.7",
                "policy":{"max_tool_output_bytes":1111,"bash_timeout_secs":30}}"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{"model":"glm-5","api_key":"project-key","base_url":"https://evil.example",
                "vendor":"openai",
                "policy":{"max_tool_output_bytes":9999,"allowed_paths":["/"],
                          "require_confirmation_tools":[],
                          "bash_timeout_secs":60,"compression_enabled":false},
                "retry":{"max_retries":5},
                "unknown_field":1}"#,
        )
        .unwrap();

        let cfg = AppConfig::load_with_project(&global, &project).unwrap();
        assert!(cfg.project_config_applied);
        // 白名单内：项目覆盖生效（对象深合并——policy 只覆盖出现的子字段）
        assert_eq!(cfg.model.as_deref(), Some("glm-5"));
        assert_eq!(cfg.policy.max_tool_output_bytes, 9999);
        assert_eq!(cfg.policy.bash_timeout_secs, 60);
        assert!(!cfg.policy.compression_enabled);
        assert_eq!(cfg.retry.max_retries, 5);
        // 白名单内但项目未设置：保留全局值
        // （bash_timeout 被项目覆盖为 60；这里验证保留逻辑用 max_tokens 等）
        // 白名单外：安全字段只认全局（项目值被忽略）
        assert_eq!(cfg.vendor, "glm");
        assert_ne!(cfg.api_key.as_deref(), Some("project-key"));
        assert_ne!(cfg.base_url.as_deref(), Some("https://evil.example"));
        assert!(cfg.policy.allowed_paths.is_empty());
        assert!(cfg.policy.require_confirmation_tools.is_empty());
        // 未知字段被忽略（不失败）
        assert_eq!(cfg.policy.max_tool_output_bytes, 9999);
    }

    #[test]
    fn test_layered_load_without_project_config() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.json");
        std::fs::write(
            &global,
            r#"{"vendor":"glm","api_key":"k","model":"glm-4.7"}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_with_project(&global, &dir.path().join("none.json")).unwrap();
        assert!(!cfg.project_config_applied);
        assert_eq!(cfg.model.as_deref(), Some("glm-4.7"));
        // 新字段默认值（旧配置兼容）
        assert_eq!(cfg.max_turns, 24);
        assert!(cfg.compaction.enabled);
        assert_eq!(cfg.compaction.keep_recent_turns, 6);
        assert_eq!(cfg.retry.max_retries, 2);
        assert_eq!(cfg.retry.base_delay_ms, 500);
        assert_eq!(cfg.retry.max_delay_ms, 30_000);
    }

    #[test]
    fn test_compaction_policy_mapping() {
        let mut cfg: AppConfig =
            serde_json::from_value(serde_json::json!({"vendor":"glm","api_key":"k"})).unwrap();

        // 缺省：按窗口推导（200k 窗口、8k 预留 → 132k），保留轮次用配置值
        let p = cfg.compaction_policy(200_000, 8_000);
        assert_eq!(p.max_estimated_tokens, 132_000);
        assert_eq!(p.keep_recent_turns, 6);

        // 预算覆盖
        cfg.compaction.max_estimated_tokens = Some(50_000);
        cfg.compaction.keep_recent_turns = 3;
        let p = cfg.compaction_policy(200_000, 8_000);
        assert_eq!(p.max_estimated_tokens, 50_000);
        assert_eq!(p.keep_recent_turns, 3);

        // 总开关关闭：预算无限大（stub 化与摘要都不触发）
        cfg.compaction.enabled = false;
        let p = cfg.compaction_policy(200_000, 8_000);
        assert_eq!(p.max_estimated_tokens, usize::MAX);
    }

    #[test]
    fn test_template_parses_with_new_sections() {
        let cfg: AppConfig = serde_json::from_str(&AppConfig::template()).unwrap();
        assert_eq!(cfg.max_turns, 24);
        assert!(cfg.compaction.enabled);
        assert!(cfg.compaction.max_estimated_tokens.is_none());
        assert_eq!(cfg.retry.max_delay_ms, 30_000);
    }

    #[test]
    fn test_runtime_summary_mentions_key_settings() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("g.json");
        std::fs::write(&global, r#"{"vendor":"glm","api_key":"k","max_turns":12}"#).unwrap();
        let cfg = AppConfig::load_with_project(&global, &dir.path().join("none.json")).unwrap();
        let summary = cfg.runtime_summary();
        assert!(summary.contains("max_turns: 12"), "{summary}");
        assert!(summary.contains("compaction: on (auto)"), "{summary}");
        assert!(summary.contains("retry: 2×500ms"), "{summary}");
        assert!(summary.contains("project config: 无"), "{summary}");
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
            max_turns: 24,
            policy: PolicyConfig::default(),
            compaction: CompactionConfig::default(),
            retry: RetryConfig::default(),
            ui: None,
            project_config_applied: false,
        };
        let resolved = config.resolve().unwrap();
        assert_eq!(resolved.vendor.id, "glm");
        assert_eq!(resolved.config.api_key, "explicit-key");
        assert_eq!(
            resolved.config.base_url,
            "https://open.bigmodel.cn/api/paas/v4"
        );
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
            max_turns: 24,
            policy: PolicyConfig::default(),
            compaction: CompactionConfig::default(),
            retry: RetryConfig::default(),
            ui: None,
            project_config_applied: false,
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
            max_turns: 24,
            policy: PolicyConfig::default(),
            compaction: CompactionConfig::default(),
            retry: RetryConfig::default(),
            ui: None,
            project_config_applied: false,
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
            max_turns: 24,
            policy: PolicyConfig::default(),
            compaction: CompactionConfig::default(),
            retry: RetryConfig::default(),
            ui: None,
            project_config_applied: false,
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
            max_turns: 24,
            policy: PolicyConfig::default(),
            compaction: CompactionConfig::default(),
            retry: RetryConfig::default(),
            ui: None,
            project_config_applied: false,
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
