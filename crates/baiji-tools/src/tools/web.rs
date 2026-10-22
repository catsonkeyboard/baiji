//! web 工具共用设施：配置段、共享 HTTP 客户端、元数据地址防护、HTML 实体解码
//!
//! webfetch / websearch 共用一个 `reqwest::Client`（连接池复用）。客户端对
//! 云元数据 / 链路本地地址设两道防线（抓取的网页可能 prompt-inject agent
//! 去访问元数据端点窃取凭据）：
//! 1. URL 校验 + 重定向策略逐跳校验字面 IP（含十进制/十六进制 IPv4 写法——
//!    `Url` 解析时已归一化为点分形式）；
//! 2. 自定义 DNS 解析器过滤解析结果（防 `metadata.google.internal` 之类域名
//!    或恶意 DNS 把任意域名指向 169.254.169.254）。
//!
//! localhost / 内网开发服务器保持可访问（本地调试是常见用途）。

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::LazyLock;
use std::time::Duration;

/// 重定向跳数上限
pub const MAX_REDIRECTS: usize = 5;

/// 通用 UA：多数站点对 "compatible" 形态的 UA 放行
pub(crate) const FETCH_USER_AGENT: &str =
    "Mozilla/5.0 (compatible; baiji/0.2; +terminal coding agent)";

/// 浏览器 UA：DuckDuckGo HTML 端点对非浏览器 UA 更容易下发 anomaly 挑战页
pub(crate) const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// `web` 配置段（全局专用：项目级配置会被白名单丢弃——恶意仓库不能把
/// 搜索查询重定向到自己的服务器）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebConfig {
    /// 总开关：false 时 webfetch / websearch 都不注册（环境变量 `BAIJI_WEB=off` 同效）
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 搜索引擎尝试顺序（出错或零结果时回落下一个）：`duckduckgo`（别名 `ddg`）/
    /// `searxng`（别名 `searx`，需配置 `searxng_url`，未配置时跳过）
    #[serde(default = "default_search_engines")]
    pub search_engines: Vec<String>,
    /// SearXNG 实例根地址（如 `https://searx.example.org`；实例需开启 JSON 格式）
    #[serde(default)]
    pub searxng_url: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_search_engines() -> Vec<String> {
    vec!["duckduckgo".to_string(), "searxng".to_string()]
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            search_engines: default_search_engines(),
            searxng_url: None,
        }
    }
}

/// 本地可执行的搜索引擎
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngine {
    DuckDuckGo,
    Searxng,
}

impl SearchEngine {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" | "ddg" => Some(Self::DuckDuckGo),
            "searxng" | "searx" => Some(Self::Searxng),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::Searxng => "searxng",
        }
    }
}

impl WebConfig {
    /// 解析引擎顺序（去重；未知名告警跳过；全部无效时回落 DuckDuckGo）
    pub fn engines(&self) -> Vec<SearchEngine> {
        let mut engines = Vec::new();
        for name in &self.search_engines {
            match SearchEngine::parse(name) {
                Some(engine) if !engines.contains(&engine) => engines.push(engine),
                Some(_) => {}
                None => tracing::warn!("web.search_engines: unknown engine '{name}' ignored"),
            }
        }
        if engines.is_empty() {
            engines.push(SearchEngine::DuckDuckGo);
        }
        engines
    }
}

/// 共享 HTTP 客户端：受防护的重定向策略 + DNS 解析器
pub fn build_client() -> reqwest::Result<reqwest::Client> {
    client_builder().build()
}

pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(FETCH_USER_AGENT)
        .redirect(guarded_redirect_policy())
        .dns_resolver(GuardedResolver)
        .connect_timeout(Duration::from_secs(15))
}

/// 重定向策略：最多 [`MAX_REDIRECTS`] 跳，每一跳的目标都过 [`check_url`]
fn guarded_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        // previous 首项是初始 URL（不算跳数）——与 Policy::limited 同口径
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error(format!("too many redirects (max {MAX_REDIRECTS})"));
        }
        match check_url(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(format!("redirect refused: {reason}")),
        }
    })
}

/// 已知元数据主机名（解析器也会拦住它们的解析结果；这里给出更明确的报错）
const BLOCKED_HOSTNAMES: &[&str] = &[
    "metadata.google.internal",
    "metadata.goog",
    "metadata.tencentyun.com",
];

/// 校验 URL：仅 http/https，且主机不是链路本地 / 云元数据地址
pub(crate) fn check_url(url: &reqwest::Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported URL scheme '{other}' (only http/https)"
            ));
        }
    }
    let Some(host) = url.host_str() else {
        return Err("URL has no host".to_string());
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(format!(
                "host {host} is a link-local / cloud-metadata address (blocked)"
            ));
        }
        return Ok(());
    }
    let domain = bare.trim_end_matches('.').to_ascii_lowercase();
    if BLOCKED_HOSTNAMES.contains(&domain.as_str()) {
        return Err(format!(
            "host {host} is a cloud-metadata endpoint (blocked)"
        ));
    }
    Ok(())
}

/// 链路本地 / 云元数据地址判定：
/// IPv4 169.254.0.0/16（AWS/GCP/Azure/腾讯云 IMDS）、100.100.100.200（阿里云）；
/// IPv6 fe80::/10、fd00:ec2::254（AWS IMDS IPv6），以及其 IPv4 映射 /
/// NAT64（64:ff9b::/96）嵌入形式。回环与私网段不在此列。
pub(crate) fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_v4(v4);
            }
            let seg = v6.segments();
            if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let embedded = Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    seg[6] as u8,
                    (seg[7] >> 8) as u8,
                    seg[7] as u8,
                );
                return is_blocked_v4(embedded);
            }
            (seg[0] & 0xffc0) == 0xfe80 || v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)
        }
    }
}

fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    v4.is_link_local() || v4 == Ipv4Addr::new(100, 100, 100, 200)
}

/// DNS 解析器：系统解析后剔除被拦截的地址；全部被拦截时报错
struct GuardedResolver;

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let resolved: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = resolved
                .iter()
                .copied()
                .filter(|addr| !is_blocked_ip(addr.ip()))
                .collect();
            if allowed.is_empty() && !resolved.is_empty() {
                return Err(format!(
                    "host '{host}' resolves to a link-local / cloud-metadata address (blocked)"
                )
                .into());
            }
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// 展开错误链（reqwest::Error 的 Display 只给顶层，DNS/重定向拒绝原因在 source 里）
pub(crate) fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = cause.source();
    }
    out
}

static ENTITY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"&(?:#([0-9]{1,7})|#[xX]([0-9a-fA-F]{1,6})|([A-Za-z][A-Za-z0-9]{1,31}));")
        .expect("static regex")
});

/// HTML 实体解码（单遍扫描，`&amp;lt;` 正确还原为 `&lt;` 而非 `<`）：
/// 数字实体 `&#NNN;` / `&#xHH;`（非法码点 → U+FFFD）+ 常见命名实体；
/// 未知命名实体原样保留。
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    ENTITY
        .replace_all(s, |caps: &regex::Captures<'_>| {
            if let Some(dec) = caps.get(1) {
                return numeric_entity(dec.as_str().parse().ok());
            }
            if let Some(hex) = caps.get(2) {
                return numeric_entity(u32::from_str_radix(hex.as_str(), 16).ok());
            }
            let name = caps.get(3).map_or("", |m| m.as_str());
            match named_entity(name) {
                Some(text) => text.to_string(),
                None => caps[0].to_string(),
            }
        })
        .into_owned()
}

fn numeric_entity(code: Option<u32>) -> String {
    // U+E000–U+E004 是 HTML 转换管道的内部哨兵（私用区），页面实体不得伪造
    match code
        .filter(|&c| c != 0 && !(0xE000..=0xE004).contains(&c))
        .and_then(char::from_u32)
    {
        Some(c) => c.to_string(),
        None => '\u{FFFD}'.to_string(),
    }
}

fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" | "AMP" => "&",
        "lt" | "LT" => "<",
        "gt" | "GT" => ">",
        "quot" | "QUOT" => "\"",
        "apos" => "'",
        "nbsp" | "ensp" | "emsp" | "thinsp" => " ",
        "shy" | "zwj" | "zwnj" | "lrm" | "rlm" => "",
        "ndash" => "–",
        "mdash" => "—",
        "hellip" => "…",
        "lsquo" => "‘",
        "rsquo" => "’",
        "sbquo" => "‚",
        "ldquo" => "“",
        "rdquo" => "”",
        "bdquo" => "„",
        "laquo" => "«",
        "raquo" => "»",
        "lsaquo" => "‹",
        "rsaquo" => "›",
        "bull" => "•",
        "middot" => "·",
        "prime" => "′",
        "Prime" => "″",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "deg" => "°",
        "plusmn" => "±",
        "times" => "×",
        "divide" => "÷",
        "minus" => "−",
        "le" => "≤",
        "ge" => "≥",
        "ne" => "≠",
        "asymp" => "≈",
        "infin" => "∞",
        "micro" => "µ",
        "para" => "¶",
        "sect" => "§",
        "dagger" => "†",
        "Dagger" => "‡",
        "permil" => "‰",
        "frac12" => "½",
        "frac14" => "¼",
        "frac34" => "¾",
        "sup1" => "¹",
        "sup2" => "²",
        "sup3" => "³",
        "cent" => "¢",
        "pound" => "£",
        "yen" => "¥",
        "euro" => "€",
        "curren" => "¤",
        "iexcl" => "¡",
        "iquest" => "¿",
        "larr" => "←",
        "rarr" => "→",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "lArr" => "⇐",
        "rArr" => "⇒",
        "hArr" => "⇔",
        "check" => "✓",
        "hearts" => "♥",
        "star" => "☆",
        "tab" | "Tab" => "\t",
        "NewLine" => "\n",
        "grave" => "`",
        "lpar" => "(",
        "rpar" => ")",
        "lsqb" | "lbrack" => "[",
        "rsqb" | "rbrack" => "]",
        "lcub" | "lbrace" => "{",
        "rcub" | "rbrace" => "}",
        "colon" => ":",
        "semi" => ";",
        "comma" => ",",
        "period" => ".",
        "excl" => "!",
        "quest" => "?",
        "num" => "#",
        "dollar" => "$",
        "percnt" => "%",
        "ast" => "*",
        "plus" => "+",
        "equals" => "=",
        "sol" => "/",
        "bsol" => "\\",
        "verbar" | "vert" => "|",
        "lowbar" => "_",
        "Hat" => "^",
        "commat" => "@",
        "auml" => "ä",
        "ouml" => "ö",
        "uuml" => "ü",
        "Auml" => "Ä",
        "Ouml" => "Ö",
        "Uuml" => "Ü",
        "szlig" => "ß",
        "eacute" => "é",
        "egrave" => "è",
        "ecirc" => "ê",
        "aacute" => "á",
        "agrave" => "à",
        "acirc" => "â",
        "iacute" => "í",
        "oacute" => "ó",
        "uacute" => "ú",
        "ntilde" => "ñ",
        "ccedil" => "ç",
        "Eacute" => "É",
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "lambda" => "λ",
        "mu" => "μ",
        "pi" => "π",
        "sigma" => "σ",
        "omega" => "ω",
        "Delta" => "Δ",
        "Sigma" => "Σ",
        "Omega" => "Ω",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn test_metadata_hosts_blocked() {
        for blocked in [
            "http://169.254.169.254/latest/meta-data/",
            "http://169.254.0.23/",
            // 十进制 / 十六进制 IPv4 写法由 Url 归一化后同样拦截
            "http://2852039166/",
            "http://0xA9FEA9FE/",
            "http://[fd00:ec2::254]/",
            "http://[fe80::1]/",
            "http://[::ffff:169.254.169.254]/",
            "http://[64:ff9b::a9fe:a9fe]/",
            "http://100.100.100.200/latest/meta-data/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://METADATA.GOOGLE.INTERNAL./",
        ] {
            assert!(
                check_url(&url(blocked)).is_err(),
                "{blocked} must be blocked"
            );
        }
    }

    #[test]
    fn test_local_and_public_hosts_allowed() {
        for allowed in [
            "http://localhost:3000/",
            "http://127.0.0.1:8080/api",
            "http://[::1]/",
            "http://192.168.1.10/",
            "http://10.0.0.5/",
            "https://example.com/page",
            "https://docs.rs/reqwest",
        ] {
            assert!(
                check_url(&url(allowed)).is_ok(),
                "{allowed} must be allowed"
            );
        }
    }

    #[test]
    fn test_non_http_schemes_rejected() {
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "data:text/plain,hi",
        ] {
            let err = check_url(&url(bad)).unwrap_err();
            assert!(
                err.contains("scheme") || err.contains("host"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn test_decode_entities_numeric_and_named() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;div&gt;"), "<div>");
        // 单遍解码：不会把 &amp;lt; 二次解码成 <
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
        assert_eq!(decode_entities("&#39;q&#x27;"), "'q'");
        assert_eq!(decode_entities("&#8212;&#x2014;&mdash;"), "———");
        assert_eq!(decode_entities("&#20013;&#x6587;"), "中文");
        assert_eq!(decode_entities("&copy; 2026 &hellip;"), "© 2026 …");
        assert_eq!(decode_entities("x&nbsp;y"), "x y");
        // 非法码点 → U+FFFD；未知命名实体原样保留；裸 & 不动
        assert_eq!(decode_entities("&#0;&#xD800;"), "\u{FFFD}\u{FFFD}");
        assert_eq!(decode_entities("&bogus; & AT&T"), "&bogus; & AT&T");
    }

    #[test]
    fn test_engine_order_parsing() {
        let cfg = WebConfig {
            search_engines: vec![
                "SearX".into(),
                "ddg".into(),
                "bing".into(),
                "searxng".into(),
            ],
            ..WebConfig::default()
        };
        assert_eq!(
            cfg.engines(),
            vec![SearchEngine::Searxng, SearchEngine::DuckDuckGo]
        );
        let empty = WebConfig {
            search_engines: vec!["google".into()],
            ..WebConfig::default()
        };
        assert_eq!(empty.engines(), vec![SearchEngine::DuckDuckGo]);
        assert_eq!(
            WebConfig::default().engines(),
            vec![SearchEngine::DuckDuckGo, SearchEngine::Searxng]
        );
    }

    #[test]
    fn test_web_config_serde_defaults() {
        let cfg: WebConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.search_engines, vec!["duckduckgo", "searxng"]);
        assert!(cfg.searxng_url.is_none());
        let cfg: WebConfig =
            serde_json::from_str(r#"{"enabled":false,"searxng_url":"https://s.example"}"#).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.searxng_url.as_deref(), Some("https://s.example"));
    }
}
