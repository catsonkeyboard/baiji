//! web 工具共用设施：配置段、共享 HTTP 客户端、元数据地址防护、HTML 实体解码
//!
//! webfetch / websearch 共用一个 `reqwest::Client`（连接池复用），请求都经
//! [`send_guarded`] 发出。对云元数据 / 链路本地地址设三道防线（抓取的网页可能
//! prompt-inject agent 去访问元数据端点窃取凭据）：
//! 1. URL 校验：初始 URL 与每一跳重定向都校验字面 IP（含十进制/十六进制 IPv4
//!    写法——`Url` 解析时已归一化为点分形式）与已知元数据主机名；
//! 2. 预检 DNS（[`preflight_dns`]）：同样逐跳，主机名的任一解析结果被拦截即拒绝
//!    ——走系统代理时主机名由代理解析、第 3 道不会运行，靠它兜底；
//! 3. 连接阶段的自定义 DNS 解析器过滤解析结果（无代理时防 rebinding）。
//!
//! localhost / 内网开发服务器保持可访问（本地调试是常见用途）。

use futures::StreamExt;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

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

/// 共享 HTTP 客户端：受防护的 DNS 解析器；不自动跟随重定向——请求一律经
/// [`send_guarded`] 发出，由它逐跳校验后手动跟随
pub fn build_client() -> reqwest::Result<reqwest::Client> {
    client_builder().build()
}

pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(FETCH_USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(GuardedResolver {
            lookup: system_lookup(),
        })
        .connect_timeout(Duration::from_secs(15))
}

/// [`send_guarded`] 的失败
pub(crate) enum SendError {
    /// 防护拒绝（元数据地址，消息以 `(blocked)` 结尾）或重定向无法跟随
    /// （非 http(s) 目标、超过 [`MAX_REDIRECTS`] 跳）
    Refused(String),
    /// 底层请求失败（超时、连接、TLS、连接阶段的 DNS 过滤…）
    Request(reqwest::Error),
}

/// 发送请求并手动跟随重定向：初始 URL 与每一跳都先过 [`check_url`]（字面 IP /
/// 已知元数据主机名）与 [`preflight_dns`]（主机名的解析结果），通过后才发出。
///
/// 手动跟随是因为 reqwest 的重定向策略是同步回调，无法在其中做异步 DNS 预检。
/// 语义对齐 reqwest 的自动跟随：最多 [`MAX_REDIRECTS`] 跳；301/302 把 POST 改为
/// GET、303 改为 GET（HEAD 除外）并丢弃请求体；307/308 原样重放；跨主机跳转去掉
/// 凭据头；请求自带的超时是整条重定向链（含预检）的总预算。不附加 Referer 头。
pub(crate) async fn send_guarded(
    client: &reqwest::Client,
    mut request: reqwest::Request,
    lookup: &Lookup,
) -> Result<reqwest::Response, SendError> {
    let deadline = request.timeout().map(|t| Instant::now() + *t);
    let remaining = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
    let mut hops = 0;
    loop {
        let refused = |reason: String| {
            let prefix = if hops == 0 { "" } else { "redirect refused: " };
            SendError::Refused(format!("{prefix}{reason}"))
        };
        check_url(request.url()).map_err(refused)?;
        let dns_limit = remaining().map_or(PREFLIGHT_DNS_TIMEOUT, |left| {
            left.min(PREFLIGHT_DNS_TIMEOUT)
        });
        preflight_dns(request.url(), lookup, dns_limit)
            .await
            .map_err(refused)?;
        if let Some(left) = remaining() {
            *request.timeout_mut() = Some(left);
        }
        // 307/308 要原样重放请求体；克隆失败（流式 body）时不跟随
        let replay = request.try_clone();
        let response = client.execute(request).await.map_err(SendError::Request)?;
        let Some(next) = redirect_request(&response, replay) else {
            return Ok(response);
        };
        hops += 1;
        if hops > MAX_REDIRECTS {
            return Err(SendError::Refused(format!(
                "too many redirects (max {MAX_REDIRECTS})"
            )));
        }
        request = next;
    }
}

/// 3xx 响应 → 下一跳请求。不是可跟随的重定向、缺 Location、Location 无法解析
/// 或无法重放时返回 None——调用方原样拿到这个响应
fn redirect_request(
    response: &reqwest::Response,
    replay: Option<reqwest::Request>,
) -> Option<reqwest::Request> {
    use reqwest::header::{
        AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, LOCATION,
        PROXY_AUTHORIZATION, TRANSFER_ENCODING,
    };
    use reqwest::{Method, StatusCode};

    let status = response.status();
    let to_get = match status {
        StatusCode::SEE_OTHER => true,
        StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND => {
            replay.as_ref().is_some_and(|r| r.method() == Method::POST)
        }
        StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => false,
        _ => return None,
    };
    let location = response.headers().get(LOCATION)?.to_str().ok()?;
    let next_url = response.url().join(location).ok()?;
    let mut next = replay?;
    if to_get {
        if next.method() != Method::HEAD {
            *next.method_mut() = Method::GET;
        }
        *next.body_mut() = None;
        for name in [
            CONTENT_TYPE,
            CONTENT_LENGTH,
            CONTENT_ENCODING,
            TRANSFER_ENCODING,
        ] {
            next.headers_mut().remove(name);
        }
    }
    let cross_host = next_url.host_str() != next.url().host_str()
        || next_url.port_or_known_default() != next.url().port_or_known_default();
    if cross_host {
        for name in [AUTHORIZATION, COOKIE, PROXY_AUTHORIZATION] {
            next.headers_mut().remove(name);
        }
    }
    *next.url_mut() = next_url;
    Some(next)
}

/// 预检 DNS 的时限（另受请求总预算约束）：超时按"解析失败"处理，即放行。
/// 有代理时预检本就防不了 rebinding（见 [`preflight_dns`]），封顶只是避免本地
/// DNS 不通的代理用户每次抓取都等满系统解析超时。
const PREFLIGHT_DNS_TIMEOUT: Duration = Duration::from_secs(3);

/// 预检 DNS：目标是主机名时先用系统解析器解析，**任一**地址是链路本地 / 云元数据
/// 地址即拒绝（字面 IP 由 [`check_url`] 负责）。[`send_guarded`] 对初始请求与每一跳
/// 重定向都执行一次。
///
/// 为什么需要：走代理（`HTTP(S)_PROXY` 环境变量或 macOS 系统代理，如本机 Clash）
/// 时，目标主机名交给代理解析，连接阶段的 [`GuardedResolver`] 根本不会被调用——
/// 只剩字面 IP 校验，`*.nip.io` 一类域名或恶意 DNS 就能把请求导向
/// 169.254.169.254 / 100.100.100.200（阿里云 ECS 上无需额外请求头即可读元数据）。
///
/// 始终执行，而不只在有代理时：reqwest 不暴露"某个 URL 会不会走代理"（系统代理
/// 来自环境变量与操作系统设置），自行判断既不完整又易错；无代理时它只是多一次
/// （通常命中系统缓存的）解析，且比连接阶段更严格（任一地址被拦截即拒绝，而非剔除）。
///
/// 残余风险（如实记录）：
/// - 解析失败或超时时放行：代理常能解析本地 DNS 解析不了的名字，一律拒绝会让
///   依赖代理上网的用户用不了 webfetch；
/// - DNS rebinding：有代理时，代理稍后自己的解析结果可能与预检不同（TTL=0 的恶意
///   DNS 先答公网地址、再答元数据地址），这条路径无法兜底；无代理时连接阶段的
///   [`GuardedResolver`] 会对实际连接的地址再过滤一次。
pub(crate) async fn preflight_dns(
    url: &reqwest::Url,
    lookup: &Lookup,
    limit: Duration,
) -> Result<(), String> {
    // domain() 对字面 IP 返回 None
    let Some(host) = url.domain() else {
        return Ok(());
    };
    let addrs = match tokio::time::timeout(limit, lookup(host.to_string())).await {
        Ok(Ok(addrs)) => addrs,
        Ok(Err(e)) => {
            tracing::debug!("web preflight: cannot resolve '{host}' ({e}); proceeding");
            return Ok(());
        }
        Err(_) => {
            tracing::debug!("web preflight: resolving '{host}' timed out; proceeding");
            return Ok(());
        }
    };
    match addrs.into_iter().find(|ip| is_blocked_ip(*ip)) {
        Some(ip) => Err(format!(
            "host '{host}' resolves to {ip}, a link-local / cloud-metadata address (blocked)"
        )),
        None => Ok(()),
    }
}

/// 已知元数据主机名（预检与解析器也会拦住它们的解析结果；这里给出更明确的报错）
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

/// 主机名 → 地址列表的解析函数。默认 [`system_lookup`]；测试注入假解析表
/// （把 `evil.test` 指向 169.254.169.254），不依赖网络。
pub(crate) type Lookup =
    Arc<dyn Fn(String) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> + Send + Sync>;

/// 系统解析器（getaddrinfo，经 tokio 阻塞线程池）
pub(crate) fn system_lookup() -> Lookup {
    Arc::new(
        |host: String| -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
            Box::pin(async move {
                let addrs = tokio::net::lookup_host((host.as_str(), 0)).await?;
                Ok(addrs.map(|addr| addr.ip()).collect())
            })
        },
    )
}

/// 测试用假解析：`(主机名, [地址…])` 表；表外主机名 → NotFound 错误
#[cfg(test)]
pub(crate) fn fake_lookup(table: &[(&str, &[&str])]) -> Lookup {
    let table: std::collections::HashMap<String, Vec<IpAddr>> = table
        .iter()
        .map(|(host, ips)| {
            let ips = ips.iter().map(|ip| ip.parse().expect("test IP")).collect();
            (host.to_string(), ips)
        })
        .collect();
    Arc::new(
        move |host: String| -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
            let result = table.get(&host).cloned().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no such host: {host}"),
                )
            });
            Box::pin(async move { result })
        },
    )
}

/// DNS 解析器（连接阶段）：解析后剔除被拦截的地址；全部被拦截时报错。
/// 走代理时 reqwest 不为目标主机调用它（主机名交给代理解析）——那条路径靠
/// [`preflight_dns`] 预检兜底。
struct GuardedResolver {
    lookup: Lookup,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let lookup = self.lookup.clone();
        Box::pin(async move {
            let resolved = lookup(host.clone()).await?;
            // 端口 0：reqwest 换成 URL 端口或协议默认端口
            let allowed: Vec<SocketAddr> = resolved
                .iter()
                .filter(|ip| !is_blocked_ip(**ip))
                .map(|ip| SocketAddr::new(*ip, 0))
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

/// 流式读取响应体，至多 `max` 字节（无 Content-Length 的分块响应也受控）；
/// 返回 (正文, 是否在上限处截断)
pub(crate) async fn read_body_capped(
    response: reqwest::Response,
    max: usize,
) -> reqwest::Result<(Vec<u8>, bool)> {
    let mut body: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let room = max.saturating_sub(body.len());
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
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

    #[tokio::test]
    async fn test_guarded_resolver_filters_blocked_addresses() {
        use reqwest::dns::Resolve;
        let resolver = GuardedResolver {
            lookup: fake_lookup(&[
                ("evil.test", &["169.254.169.254", "fe80::1"]),
                (
                    "mixed.test",
                    &["169.254.169.254", "93.184.216.34", "::ffff:100.100.100.200"],
                ),
                ("local.test", &["127.0.0.1", "::1"]),
            ]),
        };
        let resolve = |host: &str| resolver.resolve(host.parse().expect("dns name"));
        let addrs: Vec<SocketAddr> = resolve("mixed.test").await.unwrap().collect();
        assert_eq!(addrs, vec!["93.184.216.34:0".parse().unwrap()]);
        let addrs: Vec<SocketAddr> = resolve("local.test").await.unwrap().collect();
        assert_eq!(addrs.len(), 2, "loopback stays reachable: {addrs:?}");
        let err = resolve("evil.test").await.err().expect("all blocked");
        assert!(err.to_string().contains("(blocked)"), "{err}");
        // 解析失败原样上报（连接阶段不放行未知结果）
        assert!(resolve("missing.test").await.is_err());
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
