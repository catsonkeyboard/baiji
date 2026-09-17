//! 主流厂商预设注册表
//!
//! 每个预设包含协议、Base URL、推荐的环境变量名与模型发现端点，
//! 用户只需提供 API Key 即可接入。所有字段都可在配置中覆盖。
//!
//! 接入地址来源（2026-09 查证）：
//! - TokenHub: https://cloud.tencent.com/document/product/1823/130079
//! - OpenCode Zen: https://opencode.ai/docs/zen/
//! - Kimi Code: https://www.kimi.com/code/docs/en/third-party-tools/claude-code.html
//! - GLM Coding Plan: https://docs.bigmodel.cn/cn/coding-plan/tool/others
//! - MiniMax: https://platform.minimax.io/docs/api-reference/text-openai-api
//! - MiMo: https://mimo.mi.com/docs/zh-CN/quick-start/summary/first-api-call

use crate::provider::Protocol;

/// 厂商的备选接入端点（如 Coding Plan 订阅专用的 Anthropic 兼容地址）
#[derive(Debug, Clone)]
pub struct EndpointVariant {
    /// 端点名（配置 `endpoint` 字段的值，如 "anthropic"）
    pub name: &'static str,
    pub protocol: Protocol,
    pub base_url: &'static str,
    /// 计费/用途说明
    pub note: &'static str,
    /// 该端点专用的模型 id（如订阅制端点只接受固定的 `kimi-for-coding`）；
    /// 未显式配置 model 时优先于模型发现
    pub default_model: Option<&'static str>,
}

/// 一个厂商的接入预设
#[derive(Debug, Clone)]
pub struct VendorPreset {
    /// 唯一 ID（配置中 `vendor` 字段的值）
    pub id: &'static str,
    /// 展示名
    pub display_name: &'static str,
    /// 默认接口协议（按量付费端点）
    pub protocol: Protocol,
    /// 默认 Base URL（按量付费端点）
    pub base_url: &'static str,
    /// 备选端点（Coding Plan / Anthropic 兼容等），配置 `endpoint` 字段选择
    pub variants: &'static [EndpointVariant],
    /// 推荐的 API Key 环境变量名
    pub api_key_env: &'static str,
    /// 是否支持模型列表自动发现（GET {base}/models 或 /v1/models）
    pub model_discovery: bool,
    /// 默认模型（发现不可用且未配置 model 时的兜底）
    pub default_model: Option<&'static str>,
    /// 自动选默认模型时的偏好（小写子串，按序匹配发现列表；只会选中真实存在的模型）
    pub model_hints: &'static [&'static str],
    /// 别名（vendor 字段的其它可接受写法）
    pub aliases: &'static [&'static str],
}

impl VendorPreset {
    /// 全部可选端点名（"api" = 默认按量付费端点）
    pub fn endpoint_names(&self) -> Vec<&'static str> {
        let mut names = vec!["api"];
        names.extend(self.variants.iter().map(|v| v.name));
        names
    }

    /// 按名取端点（"api" 返回 None = 用默认）
    pub fn find_endpoint(&self, name: &str) -> Option<&EndpointVariant> {
        self.variants.iter().find(|v| v.name == name)
    }
}

static VENDORS: &[VendorPreset] = &[
    VendorPreset {
variants: &[],
        id: "openai",
        display_name: "OpenAI",
        protocol: Protocol::OpenAIChat,
        base_url: "https://api.openai.com",
        api_key_env: "OPENAI_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["gpt-5", "gpt-4.1", "gpt-4o"],
        aliases: &[],
    },
    VendorPreset {
variants: &[],
        id: "anthropic",
        display_name: "Anthropic",
        protocol: Protocol::Anthropic,
        base_url: "https://api.anthropic.com",
        api_key_env: "ANTHROPIC_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["sonnet", "opus", "haiku"],
        aliases: &["claude"],
    },
    VendorPreset {
variants: &[],
        id: "openrouter",
        display_name: "OpenRouter",
        protocol: Protocol::OpenAIChat,
        base_url: "https://openrouter.ai/api/v1",
        api_key_env: "OPENROUTER_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["claude-sonnet", "gpt-5", "glm-4", "deepseek"],
        aliases: &["router"],
    },
    VendorPreset {
variants: &[
            EndpointVariant {
                name: "anthropic",
                protocol: Protocol::Anthropic,
                base_url: "https://dashscope.aliyuncs.com/apps/anthropic",
                note: "Anthropic 兼容（Claude Code 接入）；另有 coding.dashscope… 编程专用端点",
                default_model: None,
            },
        ],
        id: "bailian",
        display_name: "阿里云百炼 (DashScope)",
        protocol: Protocol::OpenAIChat,
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        api_key_env: "DASHSCOPE_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["qwen3-coder", "qwen3-max", "qwen-max", "qwen-plus"],
        aliases: &["dashscope", "aliyun", "qwen"],
    },
    VendorPreset {
variants: &[],
        id: "tencent",
        display_name: "腾讯云 TokenHub",
        protocol: Protocol::OpenAIChat,
        // 国际站接入点；国内各地域接入域名见 TokenHub 调用指南，可配置覆盖
        base_url: "https://tokenhub-intl.tencentmaas.com/v1",
        api_key_env: "TOKENHUB_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["hunyuan-turbos", "hunyuan", "deepseek"],
        aliases: &["tokenhub", "hunyuan"],
    },
    VendorPreset {
variants: &[
            EndpointVariant {
                name: "anthropic",
                protocol: Protocol::Anthropic,
                base_url: "https://open.bigmodel.cn/api/anthropic",
                note: "Coding Plan · Anthropic Message 协议；国际站为 https://api.z.ai/api/anthropic",
                default_model: None,
            },
            EndpointVariant {
                name: "coding",
                protocol: Protocol::OpenAIChat,
                base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
                note: "Coding Plan · OpenAI Chat Completion 协议",
                default_model: None,
            },
            EndpointVariant {
                name: "responses",
                protocol: Protocol::OpenAIResponses,
                base_url: "https://open.bigmodel.cn/api/v1",
                note: "Coding Plan · OpenAI Response 协议",
                default_model: None,
            },
        ],
        id: "glm",
        display_name: "智谱 GLM",
        protocol: Protocol::OpenAIChat,
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        api_key_env: "ZHIPU_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["glm-4.6", "glm-4.5", "glm-4"],
        aliases: &["zhipu", "bigmodel"],
    },
    VendorPreset {
variants: &[
            EndpointVariant {
                name: "anthropic",
                protocol: Protocol::Anthropic,
                base_url: "https://api.moonshot.cn/anthropic",
                note: "开放平台按量付费 · Anthropic 协议（国际站 https://api.moonshot.ai/anthropic）",
                default_model: None,
            },
            // Kimi Code 会员订阅（与开放平台是两套 Key）。地址来源：
            // https://www.kimi.com/code/docs/en/third-party-tools/claude-code.html
            EndpointVariant {
                name: "coding",
                protocol: Protocol::OpenAIChat,
                base_url: "https://api.kimi.ai/coding/v1",
                note: "Kimi Code 会员订阅 · OpenAI 协议（需 Kimi Code 专用 Key）",
                default_model: Some("kimi-for-coding"),
            },
            EndpointVariant {
                name: "coding-anthropic",
                protocol: Protocol::Anthropic,
                base_url: "https://api.kimi.ai/coding",
                note: "Kimi Code 会员订阅 · Anthropic 协议（需 Kimi Code 专用 Key）",
                default_model: Some("kimi-for-coding"),
            },
        ],
        id: "kimi",
        display_name: "月之暗面 Kimi (Moonshot)",
        protocol: Protocol::OpenAIChat,
        base_url: "https://api.moonshot.cn/v1",
        api_key_env: "MOONSHOT_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["kimi-k2", "kimi", "moonshot-v1-128k"],
        aliases: &["moonshot"],
    },
    VendorPreset {
variants: &[
            EndpointVariant {
                name: "anthropic",
                protocol: Protocol::Anthropic,
                base_url: "https://api.deepseek.com/anthropic",
                note: "Anthropic 协议兼容端点",
                default_model: None,
            },
        ],
        id: "deepseek",
        display_name: "DeepSeek",
        protocol: Protocol::OpenAIChat,
        base_url: "https://api.deepseek.com",
        api_key_env: "DEEPSEEK_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["deepseek-chat", "deepseek"],
        aliases: &[],
    },
    VendorPreset {
variants: &[
            EndpointVariant {
                name: "anthropic",
                protocol: Protocol::Anthropic,
                base_url: "https://api.minimaxi.com/anthropic",
                note: "MiniMax Coding Plan（国内）；国际站为 https://api.minimax.io/anthropic",
                default_model: None,
            },
        ],
        id: "minimax",
        display_name: "MiniMax",
        protocol: Protocol::OpenAIChat,
        base_url: "https://api.minimaxi.com/v1",
        api_key_env: "MINIMAX_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["minimax-m2", "minimax-m", "minimax"],
        aliases: &[],
    },
    VendorPreset {
variants: &[
            EndpointVariant {
                name: "anthropic",
                protocol: Protocol::Anthropic,
                base_url: "https://api.xiaomimimo.com/anthropic",
                note: "MiMo Token Plan / Claude Code 接入（Anthropic 协议）",
                default_model: None,
            },
        ],
        id: "mimo",
        display_name: "小米 MiMo",
        protocol: Protocol::OpenAIChat,
        base_url: "https://api.xiaomimimo.com/v1",
        api_key_env: "MIMO_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["mimo"],
        aliases: &["xiaomi"],
    },
    VendorPreset {
// 协议按模型家族划分（https://opencode.ai/docs/zen/）：选错端点会被拒绝
        variants: &[
            EndpointVariant {
                name: "responses",
                protocol: Protocol::OpenAIResponses,
                base_url: "https://opencode.ai/zen/v1",
                note: "GPT / Grok 系列模型（OpenAI Responses 协议）",
                default_model: None,
            },
            EndpointVariant {
                name: "anthropic",
                protocol: Protocol::Anthropic,
                base_url: "https://opencode.ai/zen",
                note: "Claude / Qwen 系列模型（Anthropic Messages 协议）",
                default_model: None,
            },
        ],
        id: "opencode",
        display_name: "OpenCode Zen",
        // 默认端点（chat）：DeepSeek / MiniMax / GLM / Kimi 等
        protocol: Protocol::OpenAIChat,
        base_url: "https://opencode.ai/zen/v1",
        api_key_env: "OPENCODE_API_KEY",
        // GET https://opencode.ai/zen/v1/models 可用（2026-09 实测返回 OpenAI 格式列表）
        model_discovery: true,
        default_model: None,
        model_hints: &[],
        aliases: &["zen"],
    },
    VendorPreset {
variants: &[],
        id: "xai",
        display_name: "xAI (Grok)",
        protocol: Protocol::OpenAIChat,
        base_url: "https://api.x.ai",
        api_key_env: "XAI_API_KEY",
        model_discovery: true,
        default_model: None,
        model_hints: &["grok-code", "grok-4", "grok"],
        aliases: &["grok"],
    },
];

/// 全部厂商预设
pub fn all_vendors() -> &'static [VendorPreset] {
    VENDORS
}

/// 按 ID 或别名查找厂商预设（大小写不敏感）
pub fn find_vendor(name: &str) -> Option<&'static VendorPreset> {
    let lower = name.to_ascii_lowercase();
    VENDORS.iter().find(|v| v.id == lower || v.aliases.contains(&lower.as_str()))
}

/// 协议随模型家族而定的聚合厂商：按模型 id 推断应走的端点名（"api" = 默认端点）。
/// 目前只有 OpenCode Zen（https://opencode.ai/docs/zen/）：
/// Claude / Qwen → Anthropic Messages，GPT / Grok → Responses，其余 → Chat Completions。
/// 用户显式配置了 `endpoint` 时不应调用本函数覆盖。
pub fn auto_endpoint(vendor: &VendorPreset, model: &str) -> Option<&'static str> {
    if vendor.id != "opencode" {
        return None;
    }
    let m = model.to_ascii_lowercase();
    Some(if m.starts_with("claude") || m.starts_with("qwen") {
        "anthropic"
    } else if m.starts_with("gpt") || m.starts_with("grok") || m.starts_with("muse") {
        "responses"
    } else {
        "api"
    })
}

/// 厂商预设 + 端点选择 + 用户覆盖项 → Provider 配置。
/// 优先级：base_url/protocol 显式覆盖 > endpoint 变体 > 默认端点。
pub fn resolve_vendor(
    preset: &VendorPreset,
    endpoint: Option<&str>,
    base_url_override: Option<&str>,
    protocol_override: Option<&str>,
) -> anyhow::Result<crate::provider::ProviderConfig> {
    let (mut protocol, mut base_url) = (preset.protocol, preset.base_url);
    if let Some(name) = endpoint.filter(|n| !n.is_empty() && *n != "api") {
        let variant = preset.find_endpoint(name).ok_or_else(|| {
            anyhow::anyhow!(
                "厂商 '{}' 无端点 '{}'，可选: {}",
                preset.id,
                name,
                preset.endpoint_names().join(", ")
            )
        })?;
        protocol = variant.protocol;
        base_url = variant.base_url;
    }
    if let Some(s) = protocol_override {
        protocol = crate::provider::Protocol::parse(s)
            .ok_or_else(|| anyhow::anyhow!("未知的协议 '{}'，可选: anthropic / chat / responses", s))?;
    }
    if let Some(s) = base_url_override.filter(|s| !s.trim().is_empty()) {
        base_url = s;
    }
    Ok(crate::provider::ProviderConfig {
        protocol,
        base_url: base_url.to_string(),
        api_key: String::new(), // 由调用方解析填充
        model: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_vendor_by_id_and_alias() {
        assert_eq!(find_vendor("glm").unwrap().id, "glm");
        assert_eq!(find_vendor("GLM").unwrap().id, "glm");
        assert_eq!(find_vendor("zhipu").unwrap().id, "glm");
        assert_eq!(find_vendor("moonshot").unwrap().id, "kimi");
        assert_eq!(find_vendor("tokenhub").unwrap().id, "tencent");
        assert_eq!(find_vendor("zen").unwrap().id, "opencode");
        assert_eq!(find_vendor("xiaomi").unwrap().id, "mimo");
        assert!(find_vendor("nonexistent").is_none());
    }

    #[test]
    fn test_vendor_registry_completeness() {
        assert_eq!(all_vendors().len(), 12);
        // id 唯一
        let mut ids: Vec<_> = all_vendors().iter().map(|v| v.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), all_vendors().len());
        // 所有厂商都有接入要素
        for v in all_vendors() {
            assert!(v.base_url.starts_with("https://"), "{} base_url", v.id);
            assert!(!v.api_key_env.is_empty(), "{} api_key_env", v.id);
        }
    }

    #[test]
    fn test_resolve_vendor_with_overrides() {
        let preset = find_vendor("glm").unwrap();
        let cfg = resolve_vendor(preset, None, None, None).unwrap();
        assert_eq!(cfg.base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert_eq!(cfg.protocol, Protocol::OpenAIChat);

        let cfg = resolve_vendor(preset, None, Some("https://relay.local/v1"), Some("anthropic"))
            .unwrap();
        assert_eq!(cfg.base_url, "https://relay.local/v1");
        assert_eq!(cfg.protocol, Protocol::Anthropic);

        assert!(resolve_vendor(preset, None, None, Some("bogus")).is_err());
    }

    #[test]
    fn test_endpoint_variants() {
        // 双轨厂商：anthropic 端点切协议与地址（Coding Plan）
        let cases = [
            ("glm", "https://open.bigmodel.cn/api/anthropic"),
            ("deepseek", "https://api.deepseek.com/anthropic"),
            ("kimi", "https://api.moonshot.cn/anthropic"),
            ("minimax", "https://api.minimaxi.com/anthropic"),
            ("mimo", "https://api.xiaomimimo.com/anthropic"),
            ("bailian", "https://dashscope.aliyuncs.com/apps/anthropic"),
        ];
        for (vendor, expected_url) in cases {
            let preset = find_vendor(vendor).unwrap();
            let cfg = resolve_vendor(preset, Some("anthropic"), None, None)
                .unwrap_or_else(|e| panic!("{vendor}: {e}"));
            assert_eq!(cfg.protocol, Protocol::Anthropic, "{vendor}");
            assert_eq!(cfg.base_url, expected_url, "{vendor}");
            assert!(preset.endpoint_names().contains(&"anthropic"));
        }

        // GLM Coding Plan 三协议端点
        let glm = find_vendor("glm").unwrap();
        let cfg = resolve_vendor(glm, Some("coding"), None, None).unwrap();
        assert_eq!(cfg.protocol, Protocol::OpenAIChat);
        assert_eq!(cfg.base_url, "https://open.bigmodel.cn/api/coding/paas/v4");
        let cfg = resolve_vendor(glm, Some("responses"), None, None).unwrap();
        assert_eq!(cfg.protocol, Protocol::OpenAIResponses);
        assert_eq!(cfg.base_url, "https://open.bigmodel.cn/api/v1");

        // "api" = 默认端点；未知端点报错并列出可选项
        let cfg = resolve_vendor(glm, Some("api"), None, None).unwrap();
        assert_eq!(cfg.protocol, Protocol::OpenAIChat);

        let err = resolve_vendor(glm, Some("coding-plan"), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("api, anthropic"), "{err}");

        // 显式 base_url 覆盖优先于端点变体
        let cfg = resolve_vendor(
            glm,
            Some("anthropic"),
            Some("https://api.z.ai/api/anthropic"),
            None,
        )
        .unwrap();
        assert_eq!(cfg.base_url, "https://api.z.ai/api/anthropic");

        // 单端点厂商无变体
        let openai = find_vendor("openai").unwrap();
        assert!(openai.variants.is_empty());
        assert_eq!(openai.endpoint_names(), vec!["api"]);
    }
}

/// 展开 `$VAR` / `${VAR}`（未定义的变量原样保留）——API Key 配置约定
pub fn expand_env_vars(content: &str) -> String {
    // 手写扫描，避免为一个小功能引入 regex 依赖
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        let (braced, body) = match after.strip_prefix('{') {
            Some(b) => (true, b),
            None => (false, after),
        };
        let name_end = body
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(body.len());
        let name = &body[..name_end];
        if name.is_empty() {
            out.push('$');
            rest = after;
            continue;
        }
        match std::env::var(name) {
            Ok(value) => out.push_str(&value),
            Err(_) => {
                out.push('$');
                if braced {
                    out.push('{');
                    out.push_str(name);
                    out.push('}');
                    rest = &body[name_end + 1.min(body.len() - name_end)..];
                } else {
                    out.push_str(name);
                    rest = &body[name_end..];
                }
                continue;
            }
        }
        rest = if braced {
            &body[name_end + 1.min(body.len() - name_end)..]
        } else {
            &body[name_end..]
        };
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod env_tests {
    use super::*;

    #[test]
    fn test_expand_env_vars() {
        // SAFETY: 测试进程内设置环境变量
        unsafe { std::env::set_var("BAIJI_AI_TEST_KEY", "secret123") };

        assert_eq!(expand_env_vars("$BAIJI_AI_TEST_KEY"), "secret123");
        assert_eq!(expand_env_vars("${BAIJI_AI_TEST_KEY}"), "secret123");
        // 未定义保留原样
        assert_eq!(expand_env_vars("$NOT_DEFINED_XYZ_1"), "$NOT_DEFINED_XYZ_1");
        assert_eq!(
            expand_env_vars("${NOT_DEFINED_XYZ_1}"),
            "${NOT_DEFINED_XYZ_1}"
        );
        // 普通文本 / 边界
        assert_eq!(expand_env_vars("no vars here"), "no vars here");
        assert_eq!(expand_env_vars("k=$A v=${B} end"), "k=$A v=${B} end");
        assert_eq!(expand_env_vars("$"), "$");
        assert_eq!(expand_env_vars("a$b"), "a$b");
    }

    #[test]
    fn test_auto_endpoint_routes_opencode_by_model_family() {
        let zen = find_vendor("opencode").unwrap();
        assert_eq!(auto_endpoint(zen, "claude-opus-5"), Some("anthropic"));
        assert_eq!(auto_endpoint(zen, "gpt-5.5"), Some("responses"));
        assert_eq!(auto_endpoint(zen, "glm-4.6"), Some("api"));
        // 路由到的端点都真实存在
        for name in ["anthropic", "responses"] {
            assert!(zen.find_endpoint(name).is_some());
        }
        // Anthropic 变体拼出的地址 = 官方文档的 /zen/v1/messages
        let cfg = resolve_vendor(zen, Some("anthropic"), None, None).unwrap();
        assert_eq!(cfg.base_url, "https://opencode.ai/zen");
        // 其它厂商不自动改道
        assert_eq!(auto_endpoint(find_vendor("glm").unwrap(), "claude-x"), None);
    }

    #[test]
    fn test_kimi_code_variants() {
        let kimi = find_vendor("kimi").unwrap();
        let coding = kimi.find_endpoint("coding").unwrap();
        assert_eq!(coding.base_url, "https://api.kimi.ai/coding/v1");
        assert_eq!(coding.default_model, Some("kimi-for-coding"));
        assert_eq!(
            kimi.find_endpoint("coding-anthropic").unwrap().protocol,
            Protocol::Anthropic
        );
    }
}
