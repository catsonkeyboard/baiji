//! websearch 工具：免 API Key 的网页搜索，引擎按配置顺序尝试、出错或零结果时回落
//!
//! - DuckDuckGo HTML 端点：必须以 POST 表单提交（`q` + `kl`）并带浏览器 UA——
//!   普通 GET 会拿到 HTTP 202 的 anomaly 挑战页；解析 `result__a` / `result__snippet`，
//!   `//duckduckgo.com/l/?uddg=` 跳转链接用 `reqwest::Url` 的 query_pairs 解包；
//! - SearXNG（`web.searxng_url` 配置后启用）：`GET {url}/search?q=…&format=json`；
//! - 挑战页 / 验证码页（anti-bot）识别为 is_error（"search engine blocked the
//!   request …"），而不是误报"0 条结果"。

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use super::web::{
    BROWSER_USER_AGENT, SearchEngine, WebConfig, check_url, decode_entities, error_chain,
};
use crate::env::ExecutionEnv;

const DDG_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
/// DuckDuckGo 区域参数：`wt-wt` = 不限区域
const DDG_REGION: &str = "wt-wt";
const DEFAULT_RESULTS: u64 = 8;
const MAX_RESULTS: u64 = 20;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

pub struct WebSearchTool {
    env: Arc<ExecutionEnv>,
    client: reqwest::Client,
    engines: Vec<SearchEngine>,
    searxng_url: Option<String>,
    ddg_endpoint: String,
}

impl WebSearchTool {
    pub fn new(env: Arc<ExecutionEnv>, client: reqwest::Client, config: &WebConfig) -> Self {
        Self {
            env,
            client,
            engines: config.engines(),
            searxng_url: config
                .searxng_url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_string),
            ddg_endpoint: DDG_ENDPOINT.to_string(),
        }
    }

    /// 测试注入：DuckDuckGo 端点指向本地服务器
    #[cfg(test)]
    fn with_ddg_endpoint(mut self, endpoint: String) -> Self {
        self.ddg_endpoint = endpoint;
        self
    }
}

#[derive(Debug, Clone)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

#[async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &str {
        "websearch"
    }

    fn description(&self) -> &str {
        "Search the web and return a numbered list of results (title, URL, snippet). \
         Use webfetch to read a result page. Results are untrusted web content."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "num_results": {
                    "type": "integer",
                    "description": "Maximum number of results (default 8, max 20)"
                }
            },
            "required": ["query"]
        })
    }

    /// 只读网络请求：可与其他并行工具同轮并发
    fn parallel(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(query) = args["query"]
            .as_str()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        else {
            return Ok(ToolOutput::err("[Error] missing required argument 'query'"));
        };
        let limit = args["num_results"]
            .as_u64()
            .unwrap_or(DEFAULT_RESULTS)
            .clamp(1, MAX_RESULTS) as usize;

        let mut errors: Vec<String> = Vec::new();
        let mut attempted_ok = false;
        let mut ran_any = false;
        for engine in &self.engines {
            let outcome = match engine {
                SearchEngine::DuckDuckGo => self.search_duckduckgo(query, limit).await,
                SearchEngine::Searxng => match &self.searxng_url {
                    // 未配置实例：静默跳过（默认引擎序含 searxng，但它是可选的）
                    None => continue,
                    Some(base) => self.search_searxng(base, query, limit).await,
                },
            };
            ran_any = true;
            match outcome {
                Ok(results) if !results.is_empty() => {
                    let output = format_results(query, engine.as_str(), &results, &errors);
                    let (delivered, original, original_tokens) =
                        self.env.truncate_with_meta(&output);
                    let mut out = ToolOutput::ok(delivered);
                    if let Some(bytes) = original {
                        out = out.with_original_bytes(bytes);
                    }
                    if let Some(tokens) = original_tokens {
                        out = out.with_original_tokens(tokens);
                    }
                    return Ok(out);
                }
                Ok(_) => attempted_ok = true,
                Err(e) => errors.push(format!("{}: {e}", engine.as_str())),
            }
        }

        if !ran_any {
            return Ok(ToolOutput::err(
                "[Error] no search engine available: web.search_engines only lists searxng, \
                 but web.searxng_url is not configured",
            ));
        }
        if !attempted_ok && !errors.is_empty() {
            return Ok(ToolOutput::err(format!(
                "[Error] web search failed for \"{query}\" — {}",
                errors.join("; ")
            )));
        }
        let mut msg = format!("No results found for: {query}");
        if !errors.is_empty() {
            msg.push_str(&format!(" (other engines failed: {})", errors.join("; ")));
        }
        Ok(ToolOutput::ok(msg))
    }
}

/// 结果渲染：编号 markdown 列表 `N. **title** — url` + 缩进摘要行
fn format_results(
    query: &str,
    engine: &str,
    results: &[SearchResult],
    errors: &[String],
) -> String {
    let mut out = format!("Search results for: {query} (via {engine})\n");
    if !errors.is_empty() {
        out.push_str(&format!("(fallback after: {})\n", errors.join("; ")));
    }
    out.push('\n');
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("{}. **{}** — {}\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            out.push_str(&format!("   {}\n", r.snippet));
        }
    }
    out.trim_end().to_string()
}

impl WebSearchTool {
    async fn search_duckduckgo(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, String> {
        // DuckDuckGo HTML 端点对普通 GET 返回 anti-bot "anomaly" 挑战页（HTTP 202、
        // 无结果）；像真实页面那样以 POST 表单提交仍能拿到标准结果页（200）
        let response = self
            .client
            .post(&self.ddg_endpoint)
            .header(reqwest::header::USER_AGENT, BROWSER_USER_AGENT)
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_encode(&[("q", query), ("kl", DDG_REGION)]))
            .timeout(SEARCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| request_error(&e))?;

        let status = response.status();
        let body = response.text().await.map_err(|e| request_error(&e))?;
        if !status.is_success() {
            return Err(match detect_anti_bot_page(&body) {
                Some(reason) => blocked_message("DuckDuckGo", reason),
                None => format!("HTTP {status}"),
            });
        }
        let results = parse_ddg_results(&body, limit);
        if results.is_empty()
            && let Some(reason) = detect_anti_bot_page(&body)
        {
            return Err(blocked_message("DuckDuckGo", reason));
        }
        Ok(results)
    }

    /// 查询自托管（或可信公共）SearXNG 实例的 JSON API：请求发往用户控制的
    /// 实例，绕开 DuckDuckGo 对抓取请求的 TLS 指纹 / IP 信誉拦截
    async fn search_searxng(
        &self,
        base: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, String> {
        let mut endpoint = reqwest::Url::parse(&format!("{}/search", base.trim_end_matches('/')))
            .map_err(|e| format!("invalid web.searxng_url '{base}': {e}"))?;
        check_url(&endpoint)?;
        endpoint
            .query_pairs_mut()
            .append_pair("q", query)
            .append_pair("format", "json");

        let response = self
            .client
            .get(endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(SEARCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| request_error(&e))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "HTTP {status} from {base} — ensure the instance enables the json format \
                 (search.formats: [html, json])"
            ));
        }
        let parsed: SearxngResponse = response.json().await.map_err(|e| {
            format!(
                "non-JSON response from {base} ({}); enable the json format in the instance settings",
                error_chain(&e)
            )
        })?;
        Ok(parse_searxng_results(parsed, limit))
    }
}

fn request_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!("request timed out after {}s", SEARCH_TIMEOUT.as_secs())
    } else {
        error_chain(e)
    }
}

fn blocked_message(engine: &str, reason: &str) -> String {
    format!(
        "search engine blocked the request ({engine} served a {reason} page instead of results; \
         commonly caused by TLS fingerprinting or IP reputation). Retry later, or configure a \
         SearXNG instance via web.searxng_url"
    )
}

/// application/x-www-form-urlencoded 编码（借 `Url` 的 query 序列化，不引入新依赖）
fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://form.invalid/").expect("static url");
    {
        let mut query = url.query_pairs_mut();
        for (k, v) in pairs {
            query.append_pair(k, v);
        }
    }
    url.query().unwrap_or("").to_string()
}

#[derive(Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: Option<String>,
}

/// SearXNG JSON → 结果：丢弃空 URL，标题缺失回落为 URL，截取前 `limit` 条
fn parse_searxng_results(response: SearxngResponse, limit: usize) -> Vec<SearchResult> {
    response
        .results
        .into_iter()
        .filter(|r| !r.url.trim().is_empty())
        .take(limit)
        .map(|r| SearchResult {
            title: if r.title.trim().is_empty() {
                r.url.clone()
            } else {
                clean_text(&r.title)
            },
            snippet: r.content.as_deref().map(clean_text).unwrap_or_default(),
            url: r.url,
        })
        .collect()
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static regex")
}

/// 结果标题链接：class 含 `result__a` 的锚点（属性顺序无关，href 另行提取）
static RESULT_LINK: LazyLock<Regex> = LazyLock::new(|| {
    re(r#"(?is)<a\s([^>]*\bclass\s*=\s*"[^"]*\bresult__a\b[^"]*"[^>]*)>(.*?)</a\s*>"#)
});
static RESULT_SNIPPET: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?is)<(a|div|td|span)\s[^>]*\bclass\s*=\s*"[^"]*\bresult__snippet\b[^"]*"[^>]*>(.*?)</(?:a|div|td|span)\s*>"#,
    )
});
static HREF: LazyLock<Regex> = LazyLock::new(|| re(r#"(?i)\bhref\s*=\s*(?:"([^"]*)"|'([^']*)')"#));
static SIMPLE_TAG: LazyLock<Regex> = LazyLock::new(|| re(r"<[^>]+>"));
static WS: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));

/// 去标签 + 实体解码 + 空白折叠
fn clean_text(s: &str) -> String {
    let stripped = SIMPLE_TAG.replace_all(s, "");
    WS.replace_all(decode_entities(&stripped).trim(), " ")
        .into_owned()
}

/// 解析 DuckDuckGo HTML 结果页。摘要按"本条标题链接之后、下一条之前"的区间
/// 配对——按序号拉链配对的话，某条缺摘要时后续全部错位。
fn parse_ddg_results(html: &str, limit: usize) -> Vec<SearchResult> {
    let links: Vec<_> = RESULT_LINK.captures_iter(html).collect();
    let mut results = Vec::new();
    for (i, caps) in links.iter().enumerate() {
        if results.len() >= limit {
            break;
        }
        let attrs = caps.get(1).map_or("", |m| m.as_str());
        let Some(href) = HREF
            .captures(attrs)
            .and_then(|h| h.get(1).or_else(|| h.get(2)))
            .map(|m| m.as_str())
        else {
            continue;
        };
        let url = decode_ddg_url(href);
        // 只要外部结果：广告（duckduckgo.com/y.js）与站内链接过滤掉
        let external = reqwest::Url::parse(&url).ok().is_some_and(|u| {
            matches!(u.scheme(), "http" | "https") && !u.host_str().is_some_and(is_ddg_host)
        });
        if !external {
            continue;
        }
        let title = clean_text(caps.get(2).map_or("", |m| m.as_str()));
        let block_end = caps.get(0).map_or(0, |m| m.end());
        let next_start = links
            .get(i + 1)
            .and_then(|c| c.get(0))
            .map_or(html.len(), |m| m.start());
        let snippet = RESULT_SNIPPET
            .captures(&html[block_end..next_start])
            .and_then(|c| c.get(2))
            .map(|m| clean_text(m.as_str()))
            .unwrap_or_default();
        results.push(SearchResult {
            title: if title.is_empty() { url.clone() } else { title },
            url,
            snippet,
        });
    }
    results
}

fn is_ddg_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "duckduckgo.com" || host.ends_with(".duckduckgo.com")
}

/// 解包 DuckDuckGo 跳转链接：`//duckduckgo.com/l/?uddg=<编码后的真实 URL>&rut=…`。
/// 用 `Url::query_pairs` 解析（自动百分号解码），非跳转链接原样返回。
fn decode_ddg_url(href: &str) -> String {
    let href = decode_entities(href.trim());
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else if href.starts_with('/') {
        format!("https://duckduckgo.com{href}")
    } else {
        href
    };
    if let Ok(url) = reqwest::Url::parse(&absolute)
        && url.host_str().is_some_and(is_ddg_host)
        && url.path().starts_with("/l/")
        && let Some((_, target)) = url.query_pairs().find(|(k, _)| k == "uddg")
    {
        return target.into_owned();
    }
    absolute
}

/// 判定 HTML 是否为 anti-bot / 验证码挑战页而非真实结果页。DuckDuckGo 等以
/// HTTP 200/202 下发这类页面，"状态成功 + 零结果"本身有歧义，需要此检查。
/// 只在解析出零条结果时调用（结果摘要里出现 "captcha" 一词不算）。
fn detect_anti_bot_page(html: &str) -> Option<&'static str> {
    let lowered = html.to_ascii_lowercase();
    const MARKERS: &[(&str, &str)] = &[
        ("anomaly-modal", "anomaly challenge"),
        ("anomaly.js", "anomaly challenge"),
        ("dpn=1", "anomaly challenge"),
        ("g-recaptcha", "recaptcha"),
        ("h-captcha", "hcaptcha"),
        ("captcha", "captcha"),
        ("are you a robot", "bot check"),
        ("unusual traffic", "bot check"),
        ("verify you are human", "human verification"),
        ("challenge-platform", "cloudflare challenge"),
        ("cf-challenge", "cloudflare challenge"),
    ];
    MARKERS
        .iter()
        .find(|(needle, _)| lowered.contains(needle))
        .map(|(_, reason)| *reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::web::client_builder;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // 真实抓取的 DuckDuckGo 页面（fixtures）
    const DDG_RESULTS: &str = include_str!("testdata/ddg_results.html");
    const DDG_ANOMALY: &str = include_str!("testdata/ddg_anomaly.html");

    #[test]
    fn parses_ddg_html_results() {
        // 模拟 POST 表单返回的结构：标题与摘要含 <b> 高亮标签与实体
        let html = r#"
            <div class="result results_links results_links_deep web-result">
              <a class="result__a" href="https://rust-lang.org/"><b>Rust</b> Language</a>
              <a class="result__snippet" href="https://rust-lang.org/">A <b>systems</b> programming language &amp; more.</a>
            </div>
            <div class="result results_links results_links_deep web-result">
              <a rel="nofollow" href="https://en.wikipedia.org/wiki/Rust" class="result__a">Rust on Wikipedia</a>
              <a class="result__snippet" href="https://en.wikipedia.org/wiki/Rust">Encyclopedia <b>entry</b>.</a>
            </div>
        "#;
        let results = parse_ddg_results(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Language");
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert_eq!(results[0].snippet, "A systems programming language & more.");
        // class 在 href 之后也能识别
        assert_eq!(results[1].url, "https://en.wikipedia.org/wiki/Rust");
        assert_eq!(results[1].snippet, "Encyclopedia entry.");
    }

    #[test]
    fn missing_snippet_does_not_misalign_following_results() {
        let html = r#"
            <a class="result__a" href="https://one.test/">One</a>
            <a class="result__a" href="https://two.test/">Two</a>
            <a class="result__snippet" href="https://two.test/">Second snippet</a>
        "#;
        let results = parse_ddg_results(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].snippet, "");
        assert_eq!(results[1].snippet, "Second snippet");
    }

    #[test]
    fn unwraps_uddg_redirect_links_and_filters_ads() {
        assert_eq!(
            decode_ddg_url(
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1%26c%3D2&amp;rut=abc"
            ),
            "https://example.com/a?b=1&c=2"
        );
        assert_eq!(
            decode_ddg_url("https://duckduckgo.com/l/?kh=-1&uddg=https%3A%2F%2Fdocs.rs%2F"),
            "https://docs.rs/"
        );
        assert_eq!(
            decode_ddg_url("https://plain.test/x"),
            "https://plain.test/x"
        );

        let html = r#"
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Freal.test%2F&amp;rut=1">Real</a>
            <a class="result__a" href="https://duckduckgo.com/y.js?ad_domain=ads.test&amp;u3=x">Ad</a>
            <a class="result__a" href="/html/?q=more">Internal</a>
            <a class="result__a" href="https://github.com/someone/duckduckgo.com-clone">Clone repo</a>
        "#;
        let results = parse_ddg_results(html, 10);
        let urls: Vec<_> = results.iter().map(|r| r.url.as_str()).collect();
        // 只按主机过滤：路径里出现 duckduckgo.com 的外部结果保留
        assert_eq!(
            urls,
            vec![
                "https://real.test/",
                "https://github.com/someone/duckduckgo.com-clone"
            ]
        );
    }

    #[test]
    fn respects_result_limit() {
        let results = parse_ddg_results(DDG_RESULTS, 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn real_captured_ddg_results_fixture_parses() {
        assert_eq!(detect_anti_bot_page(DDG_RESULTS), None);
        let results = parse_ddg_results(DDG_RESULTS, 20);
        assert_eq!(results.len(), 10, "{results:#?}");
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert_eq!(results[0].title, "Rust Programming Language");
        assert!(
            results[0].snippet.starts_with("Rust is a fast, reliable"),
            "{:?}",
            results[0]
        );
        // 摘要里的 &quot; 实体被解码
        let github = results
            .iter()
            .find(|r| r.url.contains("github.com"))
            .unwrap();
        assert!(
            github.snippet.contains("(the \"Rust Trademarks\")"),
            "{github:?}"
        );
        assert!(
            results
                .iter()
                .all(|r| !r.title.is_empty() && !r.title.contains('<'))
        );
    }

    #[test]
    fn real_captured_ddg_anomaly_fixture_is_detected() {
        // 被标记的请求：HTML 端点返回 anomaly 挑战页、零结果
        assert!(parse_ddg_results(DDG_ANOMALY, 10).is_empty());
        assert_eq!(detect_anti_bot_page(DDG_ANOMALY), Some("anomaly challenge"));
    }

    #[test]
    fn detects_generic_captcha_pages() {
        let html = r#"<html><body><div class="g-recaptcha"></div>
            Please verify you are human.</body></html>"#;
        assert_eq!(detect_anti_bot_page(html), Some("recaptcha"));
        let cf = r#"<script src="/cdn-cgi/challenge-platform/h/b/orchestrate"></script>"#;
        assert_eq!(detect_anti_bot_page(cf), Some("cloudflare challenge"));
        assert_eq!(detect_anti_bot_page("<p>No results.</p>"), None);
    }

    #[test]
    fn parses_searxng_json_results() {
        let body = serde_json::json!({
            "query": "rust",
            "results": [
                {"url": "https://www.rust-lang.org/", "title": "Rust Programming Language",
                 "content": "A language empowering <b>everyone</b>."},
                {"url": "https://doc.rust-lang.org/book/", "title": "The Rust Book", "content": "Learn Rust."},
                {"url": "", "title": "junk"},
                {"url": "https://crates.io", "title": ""}
            ]
        });
        let parsed: SearxngResponse = serde_json::from_value(body).unwrap();
        let results = parse_searxng_results(parsed, 10);
        assert_eq!(results.len(), 3, "empty-url entry should be dropped");
        assert_eq!(results[0].snippet, "A language empowering everyone.");
        assert_eq!(results[2].title, "https://crates.io");
        assert_eq!(results[2].snippet, "");

        let many = serde_json::json!({
            "results": (0..10).map(|i| serde_json::json!({"url": format!("https://x/{i}"), "title": "t"})).collect::<Vec<_>>()
        });
        let parsed: SearxngResponse = serde_json::from_value(many).unwrap();
        assert_eq!(parse_searxng_results(parsed, 3).len(), 3);
    }

    #[test]
    fn form_encoding_escapes_reserved_characters() {
        assert_eq!(
            form_encode(&[("q", "rust & c++ 中文"), ("kl", "wt-wt")]),
            "q=rust+%26+c%2B%2B+%E4%B8%AD%E6%96%87&kl=wt-wt"
        );
    }

    #[test]
    fn output_is_numbered_markdown_list() {
        let results = vec![
            SearchResult {
                title: "One".into(),
                url: "https://one.test".into(),
                snippet: "first".into(),
            },
            SearchResult {
                title: "Two".into(),
                url: "https://two.test".into(),
                snippet: String::new(),
            },
        ];
        let out = format_results("q", "duckduckgo", &results, &[]);
        assert_eq!(
            out,
            "Search results for: q (via duckduckgo)\n\n\
             1. **One** — https://one.test\n   first\n\
             2. **Two** — https://two.test"
        );
    }

    // ---- 本地服务器：引擎回落与挑战页报错（不访问外网）----

    /// 按路径返回固定响应；记录收到的请求原文（校验 POST 表单）
    async fn spawn_server(
        routes: Vec<(&'static str, String, String)>,
    ) -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        let routes = Arc::new(routes);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let routes = routes.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut received = Vec::new();
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => received.extend_from_slice(&buf[..n]),
                        }
                        let text = String::from_utf8_lossy(&received);
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            let len = head
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if body.len() >= len {
                                break;
                            }
                        }
                    }
                    let request = String::from_utf8_lossy(&received).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    log.lock().unwrap().push(request);
                    let (status, ctype, body) = routes
                        .iter()
                        .find(|(p, _, _)| path.starts_with(p))
                        .map(|(_, ct, body)| ("200 OK", ct.as_str(), body.as_str()))
                        .unwrap_or(("404 Not Found", "text/plain", "nope"));
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (addr, seen)
    }

    fn tool(config: &WebConfig, ddg: String) -> WebSearchTool {
        let client = client_builder().no_proxy().build().unwrap();
        WebSearchTool::new(Arc::new(ExecutionEnv::new(".")), client, config).with_ddg_endpoint(ddg)
    }

    #[tokio::test]
    async fn ddg_post_form_results_end_to_end() {
        let (addr, seen) =
            spawn_server(vec![("/html/", "text/html".into(), DDG_RESULTS.into())]).await;
        let t = tool(&WebConfig::default(), format!("http://{addr}/html/"));
        let out = t
            .execute(serde_json::json!({"query": "rust lang", "num_results": 2}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content
                .starts_with("Search results for: rust lang (via duckduckgo)")
        );
        assert!(
            out.content
                .contains("1. **Rust Programming Language** — https://rust-lang.org/")
        );
        assert!(out.content.contains("\n2. **"), "{}", out.content);
        assert!(!out.content.contains("\n3. **"), "{}", out.content);
        // POST 表单 + 浏览器 UA
        let request = seen.lock().unwrap()[0].clone();
        assert!(request.starts_with("POST /html/ HTTP/1.1"), "{request}");
        assert!(request.contains("q=rust+lang&kl=wt-wt"), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("user-agent: mozilla/5.0 (windows"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn anomaly_page_is_an_error_not_zero_results() {
        let (addr, _) =
            spawn_server(vec![("/html/", "text/html".into(), DDG_ANOMALY.into())]).await;
        let t = tool(&WebConfig::default(), format!("http://{addr}/html/"));
        let out = t
            .execute(serde_json::json!({"query": "rust"}))
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("search engine blocked the request"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("No results"), "{}", out.content);
    }

    #[tokio::test]
    async fn falls_back_to_searxng_when_ddg_is_blocked() {
        let searx_body = serde_json::json!({
            "results": [{"url": "https://searx.result/", "title": "From SearXNG", "content": "ok"}]
        })
        .to_string();
        let (addr, seen) = spawn_server(vec![
            ("/html/", "text/html".into(), DDG_ANOMALY.into()),
            ("/searx/search", "application/json".into(), searx_body),
        ])
        .await;
        let config = WebConfig {
            searxng_url: Some(format!("http://{addr}/searx/")),
            ..WebConfig::default()
        };
        let t = tool(&config, format!("http://{addr}/html/"));
        let out = t
            .execute(serde_json::json!({"query": "a b"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("(via searxng)"), "{}", out.content);
        assert!(
            out.content
                .contains("fallback after: duckduckgo: search engine blocked")
        );
        assert!(
            out.content
                .contains("1. **From SearXNG** — https://searx.result/\n   ok")
        );
        let requests = seen.lock().unwrap().clone();
        assert!(
            requests
                .iter()
                .any(|r| r.starts_with("GET /searx/search?q=a+b&format=json")),
            "{requests:?}"
        );
    }

    #[tokio::test]
    async fn searxng_first_when_configured_and_errors_aggregate() {
        // searxng 在前且返回非 JSON → 回落 ddg 也失败（404）→ is_error 汇总两条原因
        let (addr, _) = spawn_server(vec![(
            "/searx/search",
            "text/html".into(),
            "<html>nope</html>".into(),
        )])
        .await;
        let config = WebConfig {
            search_engines: vec!["searxng".into(), "ddg".into()],
            searxng_url: Some(format!("http://{addr}/searx")),
            ..WebConfig::default()
        };
        let t = tool(&config, format!("http://{addr}/missing/"));
        let out = t.execute(serde_json::json!({"query": "x"})).await.unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("searxng: non-JSON response"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("duckduckgo: HTTP 404"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn empty_results_and_bad_args() {
        let (addr, _) = spawn_server(vec![(
            "/html/",
            "text/html".into(),
            "<html><body><div class=\"no-results\">No results.</div></body></html>".into(),
        )])
        .await;
        let t = tool(&WebConfig::default(), format!("http://{addr}/html/"));
        let out = t
            .execute(serde_json::json!({"query": "zzzz"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "No results found for: zzzz");

        let out = t.execute(serde_json::json!({"query": "  "})).await.unwrap();
        assert!(out.is_error);
        assert_eq!(t.name(), "websearch");
        assert!(t.parallel());

        // 只配 searxng 却没给地址：明确报错，而不是"无结果"
        let only_searx = WebConfig {
            search_engines: vec!["searxng".into()],
            ..WebConfig::default()
        };
        let t = tool(&only_searx, format!("http://{addr}/html/"));
        let out = t.execute(serde_json::json!({"query": "x"})).await.unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("searxng_url is not configured"),
            "{}",
            out.content
        );
    }
}
