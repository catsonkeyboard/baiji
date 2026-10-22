//! webfetch 测试：HTML 转换器单测 + 本地 HTTP 服务器集成测试（不访问外网）

use super::*;
use crate::tools::ExpandTool;
use crate::tools::web::{client_builder, fake_lookup, send_guarded};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------------------
// 转换器单测
// ---------------------------------------------------------------------------

fn md(html: &str) -> String {
    html_to_markdown(html, None)
}

#[test]
fn strips_non_prose_elements() {
    let html = "<nav><a href='/x'>Menu</a></nav><p>Body text</p>\
                <aside>Related</aside><form><select><option>Pick</option></select></form>";
    let out = md(html);
    assert!(out.contains("Body text"));
    assert!(!out.contains("Menu"), "nav should be dropped: {out}");
    assert!(!out.contains("Related"), "aside should be dropped: {out}");
    assert!(
        !out.contains("Pick"),
        "form controls should be dropped: {out}"
    );
}

#[test]
fn strips_script_style_head_and_comments() {
    let html = "<html><head><title>T</title><style>body{color:red}</style>\
                <link rel=stylesheet href=x.css></head><body>\
                <script>var leaked = 1;</script><SCRIPT type=module>alert(1)</SCRIPT >\
                <p>Keep</p><!-- build:12345 drop me --><style>.a{}</style></body></html>";
    for out in [md(html), html_to_text(html)] {
        assert!(out.contains("Keep"), "{out}");
        for needle in ["leaked", "alert", "color:red", "drop me", ".a{}", "x.css"] {
            assert!(!out.contains(needle), "{needle} leaked into {out:?}");
        }
    }
}

#[test]
fn keeps_article_header_and_footer_content() {
    // <header> 常含标题/署名，<footer> 可含文章署名——都不算 chrome
    let html = "<article><header><h1>Real Title</h1><p>By Author</p></header>\
                <p>Body</p><footer>Published 2026</footer></article>";
    let out = md(html);
    for needle in ["# Real Title", "By Author", "Body", "Published 2026"] {
        assert!(out.contains(needle), "{needle} missing from {out}");
    }
}

#[test]
fn converts_headings_even_with_inner_whitespace() {
    let out =
        md("<h1>Title</h1>\n<h2 class=\"x\">\n   Sub   heading\n</h2><h6>Six</h6><p>text</p>");
    assert!(out.contains("# Title\n"), "{out}");
    assert!(out.contains("## Sub heading\n"), "{out}");
    assert!(out.contains("###### Six"), "{out}");
    // <header> 不会被误识别为 <h…> 标题
    let out = md("<header>Masthead</header>");
    assert_eq!(out, "Masthead");
}

#[test]
fn converts_lists_and_drops_empty_bullets() {
    let out = md("<ul>\n  <li>One</li>\n  <li class=\"x\">Two</li>\n  <li></li>\n</ul>");
    assert_eq!(out, "- One\n- Two");
    // <link> 不会被当成 <li>
    assert_eq!(md("<link rel=\"x\"><p>a</p>"), "a");
}

#[test]
fn converts_links_and_inline_formatting() {
    let html = "<p>See <a href=\"https://example.com/docs\">the <b>docs</b></a>, \
                <strong>bold</strong>, <em>it</em>, <code>x &lt; y</code> and \
                <a href='#top'>top</a>.</p><a href=\"https://e.com\"><img src=a.png></a>";
    let out = md(html);
    assert!(
        out.contains("[the docs](https://example.com/docs)"),
        "{out}"
    );
    assert!(out.contains("**bold**"), "{out}");
    assert!(out.contains("*it*"), "{out}");
    assert!(out.contains("`x < y`"), "{out}");
    // 页内锚点只保留文本；纯图片链接（无文本）整个丢弃
    assert!(out.contains("top."), "{out}");
    assert!(!out.contains("#top"), "{out}");
    assert!(!out.contains("https://e.com"), "{out}");
}

#[test]
fn resolves_relative_links_against_final_url() {
    let base = reqwest::Url::parse("https://example.com/guide/intro.html").unwrap();
    let out = html_to_markdown(
        "<a href=\"/api?a=1&amp;b=2\">API</a> <a href=\"next.html\">Next</a> \
         <a href=\"javascript:void(0)\">JS</a>",
        Some(&base),
    );
    assert!(
        out.contains("[API](https://example.com/api?a=1&b=2)"),
        "{out}"
    );
    assert!(
        out.contains("[Next](https://example.com/guide/next.html)"),
        "{out}"
    );
    assert!(out.contains("JS") && !out.contains("javascript:"), "{out}");
}

#[test]
fn drops_empty_links_and_overlong_targets() {
    assert_eq!(render_link("https://example.com", "", None), "");
    assert_eq!(render_link("https://example.com", "   ", None), "");
    assert_eq!(render_link("#section", "Jump", None), "Jump");
    let long = format!("https://example.com/?code={}", "a".repeat(MAX_URL_CHARS));
    assert_eq!(render_link(&long, "Run", None), "Run");
    assert_eq!(
        render_link("https://example.com", "Home", None),
        "[Home](https://example.com)"
    );
}

#[test]
fn preserves_code_blocks_verbatim() {
    let html = "<p>Example:</p><pre><code class=\"language-rust\">fn main() {\n    \
                println!(&quot;hi &lt;3&quot;);\n\n\n    <span class=\"k\">let</span>  x = 1;\n}\
                </code></pre><pre>plain   pre</pre>";
    let out = md(html);
    assert!(
        out.contains("```rust\nfn main() {\n    println!(\"hi <3\");\n\n\n    let  x = 1;\n}\n```"),
        "{out}"
    );
    // 无属性的 <pre> 也是代码块（不能被行内 code 规则抢先匹配）
    assert!(out.contains("```\nplain   pre\n```"), "{out}");
    // 纯文本模式：原样保留，不加围栏
    let text = html_to_text(html);
    assert!(text.contains("    println!(\"hi <3\");"), "{text}");
    assert!(!text.contains("```"), "{text}");
}

#[test]
fn code_blocks_stand_on_their_own_lines() {
    let out =
        md("<div>Intro<pre><code>a\n  b</code></pre><figcaption>Listing 1</figcaption></div>");
    assert_eq!(out, "Intro\n\n```\na\n  b\n```\n\nListing 1");
}

#[test]
fn sentinels_never_leak_into_output() {
    // 锚点包着标题；页面自带私用区字符与伪造哨兵的数字实体
    let html = "<a href=\"https://x.test/\"><h2>Card</h2></a><p>a\u{E004}b &#xE000;0&#xE001;</p>";
    let out = md(html);
    assert!(out.contains("[## Card](https://x.test/)"), "{out:?}");
    assert!(!out.chars().any(is_sentinel), "{out:?}");
    assert!(!html_to_text(html).chars().any(is_sentinel));
}

#[test]
fn decodes_numeric_and_named_entities() {
    let out =
        md("<p>&lt;tag&gt; &amp; &#x4e2d;&#25991; &mdash; &copy; &hellip; &nbsp;x &amp;lt;</p>");
    assert_eq!(out, "<tag> & 中文 — © … x &lt;");
}

#[test]
fn renders_tables() {
    let html = "<table><tr><th>A</th><th>B</th></tr>\n<tr><td>1</td><td>2</td></tr></table>";
    let out = md(html);
    assert!(out.contains("| A | B |\n| 1 | 2 |"), "{out}");
    let text = html_to_text(html);
    assert_eq!(text, "A\tB\n1\t2");
}

#[test]
fn block_elements_break_lines_and_whitespace_is_normalized() {
    let html = "<div>first</div><div>second</div>\n\n\n\n   <p>   spaced    out   \
                text </p><br><hr><section>third</section>";
    let out = md(html);
    assert!(out.contains("first\nsecond"), "{out}");
    assert!(out.contains("spaced out text"), "{out}");
    assert!(out.contains("---"), "{out}");
    assert!(!out.contains("\n\n\n"), "{out}");
    assert!(out.lines().all(|l| l == l.trim()), "{out}");
}

#[test]
fn does_not_leak_attributes_containing_angle_brackets() {
    // Parsoid 风格标签把 JSON 放在属性里；朴素 `<[^>]+>` 会在值内首个 `>` 处
    // 停下，把剩余部分当正文吐出
    let html = r#"<span data-mw='{"wt":"[[a]] > [[b]]"}'>Visible</span>"#;
    assert_eq!(html_to_text(html), "Visible");
    assert_eq!(md(html), "Visible");
    let html = r#"<p><span data-x="a > b" title='c > d'>Shown</span></p>"#;
    assert_eq!(md(html), "Shown");
}

#[test]
fn extracts_title() {
    let html = "<html><head><title>\n  Rust &amp; Cargo  </title></head><body>x</body></html>";
    assert_eq!(extract_title(html).as_deref(), Some("Rust & Cargo"));
    assert_eq!(extract_title("<p>none</p>"), None);
}

#[test]
fn binary_content_types_are_detected() {
    for ct in [
        "image/png",
        "image/jpeg",
        "application/pdf",
        "application/octet-stream",
        "application/zip",
        "application/gzip",
        "video/mp4",
        "font/woff2",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "APPLICATION/PDF; charset=binary",
    ] {
        assert!(is_binary_content_type(ct), "{ct} should be binary");
    }
    for ct in [
        "",
        "text/html; charset=utf-8",
        "application/json",
        "application/ld+json",
        "text/plain",
        "image/svg+xml",
        "application/xml",
        "application/javascript",
    ] {
        assert!(!is_binary_content_type(ct), "{ct} should be text");
    }
    assert!(looks_binary(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
    assert!(!looks_binary("plain text, 中文".as_bytes()));
}

#[test]
fn decodes_non_utf8_charsets() {
    let (gbk, _, _) = encoding_rs::GBK.encode("<p>中文内容</p>");
    assert_eq!(
        decode_body(&gbk, "text/html; charset=GBK"),
        "<p>中文内容</p>"
    );
    // 头部未声明 → 读 <meta charset>
    let (page, _, _) =
        encoding_rs::GBK.encode("<html><head><meta charset=\"gb2312\"></head><p>你好</p>");
    assert!(decode_body(&page, "text/html").contains("你好"));
    // 缺省 UTF-8；非法字节 → U+FFFD 不 panic
    assert_eq!(decode_body("héllo".as_bytes(), ""), "héllo");
    assert!(decode_body(&[0x66, 0xff, 0x6f], "text/plain").contains('\u{FFFD}'));
}

#[test]
fn html_detection() {
    assert!(is_html("text/html; charset=utf-8", ""));
    assert!(is_html("application/xhtml+xml", ""));
    assert!(is_html("", "  <!DOCTYPE html><html>"));
    assert!(is_html("text/plain", "<html><body>x</body></html>"));
    assert!(!is_html("text/plain", "just <b>text</b>"));
    assert!(!is_html("application/json", "<html>"));
}

// ---------------------------------------------------------------------------
// 参数与策略（不发请求）
// ---------------------------------------------------------------------------

fn test_client() -> reqwest::Client {
    // 测试只连本地服务器：绕过系统代理，防止本机代理配置干扰
    client_builder().no_proxy().build().unwrap()
}

fn tool_with(env: ExecutionEnv) -> (WebFetchTool, Arc<ExecutionEnv>) {
    let env = Arc::new(env);
    (WebFetchTool::new(env.clone(), test_client()), env)
}

#[tokio::test]
async fn refuses_metadata_hosts_and_bad_args() {
    let (tool, _) = tool_with(ExecutionEnv::new("."));
    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://[fd00:ec2::254]/latest/meta-data/",
        "http://metadata.google.internal/computeMetadata/v1/",
    ] {
        let out = tool.execute(serde_json::json!({"url": url})).await.unwrap();
        assert!(out.is_error, "{url}");
        assert!(
            out.content.starts_with("[Policy denied]"),
            "{}",
            out.content
        );
    }
    let out = tool
        .execute(serde_json::json!({"url": "file:///etc/passwd"}))
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("scheme"),
        "{}",
        out.content
    );
    let out = tool
        .execute(serde_json::json!({"url": "not a url"}))
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("invalid URL"),
        "{}",
        out.content
    );
    let out = tool.execute(serde_json::json!({})).await.unwrap();
    assert!(
        out.is_error && out.content.contains("missing"),
        "{}",
        out.content
    );
    let out = tool
        .execute(serde_json::json!({"url": "https://example.com", "format": "pdf"}))
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("unknown format"),
        "{}",
        out.content
    );
}

#[test]
fn schema_and_parallel_flag() {
    let (tool, _) = tool_with(ExecutionEnv::new("."));
    assert_eq!(tool.name(), "webfetch");
    assert!(tool.parallel());
    let schema = tool.parameters();
    assert_eq!(schema["required"], serde_json::json!(["url"]));
    for key in ["url", "format", "timeout_secs"] {
        assert!(schema["properties"][key].is_object(), "{key}");
    }
}

// ---------------------------------------------------------------------------
// 本地 HTTP 服务器集成测试
// ---------------------------------------------------------------------------

/// 一个响应：状态行、额外头、正文；`content_length: None` = 不发长度、写完关连接
struct Reply {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    content_length: Option<usize>,
}

impl Reply {
    fn new(status: &'static str, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        let body = body.into();
        Self {
            status,
            headers: vec![("Content-Type", content_type.to_string())],
            content_length: Some(body.len()),
            body,
        }
    }

    fn redirect(status: &'static str, location: String) -> Self {
        Self {
            status,
            headers: vec![("Location", location)],
            body: Vec::new(),
            content_length: Some(0),
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {}\r\n", self.status);
        for (k, v) in &self.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(len) = self.content_length {
            head.push_str(&format!("Content-Length: {len}\r\n"));
        }
        head.push_str("Connection: close\r\n\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

const PAGE: &str = "<!DOCTYPE html><html><head><title>Test &amp; Page</title>\
    <script>var tracking = 1;</script></head><body>\
    <nav><a href=\"/home\">Home nav</a></nav>\
    <h1>Hello</h1><p>Read the <a href=\"/docs/intro\">intro</a> first.</p>\
    <pre><code>let x = 1;</code></pre></body></html>";

fn big_page() -> String {
    let mut html = String::from("<html><body><h1>Big</h1>");
    for i in 0..3000 {
        html.push_str(&format!(
            "<p>paragraph number {i} with some filler text</p>\n"
        ));
    }
    html.push_str("<p>THE-END-MARKER</p></body></html>");
    html
}

fn route(path: &str, addr: SocketAddr) -> Reply {
    match path {
        "/page" => Reply::new("200 OK", "text/html; charset=utf-8", PAGE),
        "/data.json" => {
            let items: Vec<_> = (0..20)
                .map(|i| serde_json::json!({"id": i, "name": format!("item-{i}"), "debug": null}))
                .collect();
            let body = serde_json::to_string_pretty(&serde_json::json!({"items": items})).unwrap();
            Reply::new("200 OK", "application/json", body)
        }
        "/r1" => Reply::redirect("302 Found", "/r2".into()),
        "/r2" => Reply::redirect("301 Moved Permanently", format!("http://{addr}/page")),
        "/r307" => Reply::redirect("307 Temporary Redirect", "/page".into()),
        "/to-metadata" => Reply::redirect(
            "302 Found",
            "http://169.254.169.254/latest/meta-data/".into(),
        ),
        // 主机名形式的元数据跳转：evil.test 由测试注入的假解析表指向 169.254.169.254
        "/to-evil" => Reply::redirect("302 Found", "http://evil.test/latest/meta-data/".into()),
        // 作为假代理时代"元数据服务"应答：内容出现在输出里即说明防护被绕过
        "/latest/meta-data/" => Reply::new("200 OK", "text/plain", "SECRET-METADATA"),
        "/big" => Reply::new("200 OK", "text/html", big_page()),
        "/image.png" => Reply::new("200 OK", "image/png", b"\x89PNG\r\n\x1a\n\0\0".to_vec()),
        "/nul" => {
            let body = b"MZ\0\0\x03\0binary".to_vec();
            Reply {
                status: "200 OK",
                headers: Vec::new(),
                content_length: Some(body.len()),
                body,
            }
        }
        "/huge-declared" => Reply {
            status: "200 OK",
            headers: vec![("Content-Type", "text/plain".into())],
            body: Vec::new(),
            content_length: Some(MAX_BODY_BYTES + 1),
        },
        "/stream-huge" => Reply {
            status: "200 OK",
            headers: vec![("Content-Type", "text/plain".into())],
            body: vec![b'a'; MAX_BODY_BYTES + 512 * 1024],
            content_length: None,
        },
        "/gbk" => {
            let (bytes, _, _) =
                encoding_rs::GBK.encode("<html><body><p>中文页面</p></body></html>");
            Reply::new("200 OK", "text/html; charset=gbk", bytes.into_owned())
        }
        "/missing" => Reply::new(
            "404 Not Found",
            "text/html",
            "<html><body><h1>Not Found</h1><p>no such page</p></body></html>",
        ),
        "/empty" => Reply::new("200 OK", "text/plain", ""),
        p if p.starts_with("/loop/") => {
            let n: usize = p["/loop/".len()..].parse().unwrap_or(0);
            Reply::redirect("302 Found", format!("/loop/{}", n + 1))
        }
        _ => Reply::new("404 Not Found", "text/plain", "unknown route"),
    }
}

/// 极简 HTTP/1.1 服务器：每连接读请求头 → 按路径路由 → 写响应 → 关闭
async fn spawn_server() -> SocketAddr {
    spawn_logged_server().await.0
}

/// 同 [`spawn_server`]，并记录收到的 `方法 目标`。也可充当正向代理：absolute-form
/// 目标（`GET http://host/path`）按其路径路由——经代理的请求，主机名由代理
/// （这里即本服务器）负责，客户端自己的 DNS 解析器不会运行。
async fn spawn_logged_server() -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let log = log.clone();
            tokio::spawn(async move {
                let mut received = Vec::new();
                let mut buf = [0u8; 2048];
                while !received.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => received.extend_from_slice(&buf[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&received);
                let mut words = request.split_whitespace();
                let method = words.next().unwrap_or("").to_string();
                let target = words.next().unwrap_or("/").to_string();
                log.lock().unwrap().push(format!("{method} {target}"));
                let path = match target.split_once("://") {
                    Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
                    None => target.as_str(),
                };
                // 客户端提前断开（如超限截断）时写失败是预期内的
                let _ = stream.write_all(&route(path, addr).encode()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (addr, seen)
}

async fn fetch_ok(tool: &WebFetchTool, url: String, extra: Value) -> ToolOutput {
    let mut args = serde_json::json!({"url": url});
    if let (Some(args), Some(extra)) = (args.as_object_mut(), extra.as_object()) {
        args.extend(extra.clone());
    }
    tool.execute(args).await.unwrap()
}

#[tokio::test]
async fn fetches_html_as_markdown_text_and_raw() {
    let addr = spawn_server().await;
    let (tool, _) = tool_with(ExecutionEnv::new("."));
    let url = format!("http://{addr}/page");

    let out = fetch_ok(&tool, url.clone(), serde_json::json!({})).await;
    assert!(!out.is_error, "{}", out.content);
    let first = out.content.lines().next().unwrap();
    assert!(
        first.starts_with(&format!("[webfetch] {url} · HTTP 200 · text/html")),
        "{first}"
    );
    assert!(first.ends_with("→ markdown"), "{first}");
    assert!(
        out.content.contains("Title: Test & Page"),
        "{}",
        out.content
    );
    assert!(out.content.contains("# Hello"), "{}", out.content);
    // 相对链接按最终 URL 解析为绝对地址
    assert!(
        out.content
            .contains(&format!("[intro](http://{addr}/docs/intro)")),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("```\nlet x = 1;\n```"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("tracking"), "{}", out.content);
    assert!(!out.content.contains("Home nav"), "{}", out.content);
    // 格式转换不计入台账节省
    assert!(out.original_bytes.is_none());

    let out = fetch_ok(&tool, url.clone(), serde_json::json!({"format": "text"})).await;
    assert!(
        out.content.contains("Hello\n\nRead the intro first."),
        "{}",
        out.content
    );
    assert!(!out.content.contains("[intro]"), "{}", out.content);

    let out = fetch_ok(&tool, url, serde_json::json!({"format": "html"})).await;
    assert!(out.content.contains("<h1>Hello</h1>"), "{}", out.content);
    assert!(
        out.content
            .lines()
            .next()
            .unwrap()
            .ends_with("→ html (raw)")
    );
}

#[tokio::test]
async fn json_body_is_compacted() {
    let addr = spawn_server().await;
    let (tool, _) = tool_with(ExecutionEnv::new("."));
    let out = fetch_ok(
        &tool,
        format!("http://{addr}/data.json"),
        serde_json::json!({}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("application/json"), "{}", out.content);
    assert!(out.content.contains("→ as-is"), "{}", out.content);
    // 无损紧凑：单行、null 字段被删
    let body = out.content.split_once("\n\n").unwrap().1;
    assert!(!body.contains('\n'), "{body}");
    assert!(body.contains("\"name\":\"item-19\""), "{body}");
    assert!(!body.contains("debug"), "{body}");
    // 域压缩计入台账
    assert!(out.original_bytes.unwrap() > out.content.len() as u64);
    assert!(out.original_tokens.is_some());
}

#[tokio::test]
async fn follows_redirect_chain_and_reports_final_url() {
    let addr = spawn_server().await;
    let (tool, _) = tool_with(ExecutionEnv::new("."));
    let out = fetch_ok(&tool, format!("http://{addr}/r1"), serde_json::json!({})).await;
    assert!(!out.is_error, "{}", out.content);
    let first = out.content.lines().next().unwrap();
    assert!(
        first.starts_with(&format!("[webfetch] http://{addr}/page · HTTP 200")),
        "{first}"
    );
    assert!(
        first.contains(&format!("redirected from http://{addr}/r1")),
        "{first}"
    );
    assert!(out.content.contains("# Hello"));

    // 超过 5 跳
    let out = fetch_ok(
        &tool,
        format!("http://{addr}/loop/0"),
        serde_json::json!({}),
    )
    .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("too many redirects"),
        "{}",
        out.content
    );

    // 重定向到元数据地址：逐跳校验拦截（不会真的去连 169.254.169.254）
    let out = fetch_ok(
        &tool,
        format!("http://{addr}/to-metadata"),
        serde_json::json!({}),
    )
    .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.starts_with("[Policy denied]"),
        "{}",
        out.content
    );
}

// ---------------------------------------------------------------------------
// 预检 DNS：走代理时主机名由代理解析，连接阶段的 GuardedResolver 根本不运行
// ---------------------------------------------------------------------------

/// 假解析表：evil/ali/v6 指向元数据地址；mixed 公网 + 元数据混合（任一被拦截
/// 即拒绝）；safe 指向公网地址；表外主机名解析失败
fn metadata_lookup() -> Lookup {
    fake_lookup(&[
        ("evil.test", &["169.254.169.254"]),
        ("ali.test", &["100.100.100.200"]),
        ("v6.test", &["fd00:ec2::254"]),
        ("mixed.test", &["93.184.216.34", "169.254.169.254"]),
        ("safe.test", &["93.184.216.34"]),
    ])
}

/// 所有 http 请求都经 `proxy` 转发的客户端（显式代理同时关闭系统代理探测）
fn proxied_tool(proxy: SocketAddr) -> WebFetchTool {
    let client = client_builder()
        .proxy(reqwest::Proxy::http(format!("http://{proxy}")).unwrap())
        .build()
        .unwrap();
    WebFetchTool::new(Arc::new(ExecutionEnv::new(".")), client).with_lookup(metadata_lookup())
}

#[tokio::test]
async fn preflight_refuses_metadata_hostnames_behind_proxy() {
    let (proxy, seen) = spawn_logged_server().await;
    let tool = proxied_tool(proxy);
    for host in ["evil.test", "ali.test", "v6.test", "mixed.test"] {
        let url = format!("http://{host}/latest/meta-data/");
        let out = fetch_ok(&tool, url, serde_json::json!({})).await;
        assert!(out.is_error, "{host}: {}", out.content);
        assert!(
            out.content.starts_with("[Policy denied]"),
            "{host}: {}",
            out.content
        );
        assert!(out.content.contains(host), "{}", out.content);
        assert!(!out.content.contains("SECRET-METADATA"), "{}", out.content);
    }
    // 预检在发请求之前：代理一条请求都没收到
    assert!(
        seen.lock().unwrap().is_empty(),
        "{:?}",
        seen.lock().unwrap()
    );
}

#[tokio::test]
async fn preflight_checks_redirect_hops_behind_proxy() {
    // 首跳 safe.test 预检通过；重定向到 evil.test 时拦截，第二跳不会发出
    let (proxy, seen) = spawn_logged_server().await;
    let tool = proxied_tool(proxy);
    let out = fetch_ok(
        &tool,
        "http://safe.test/to-evil".into(),
        serde_json::json!({}),
    )
    .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.starts_with("[Policy denied] redirect refused"),
        "{}",
        out.content
    );
    assert!(out.content.contains("evil.test"), "{}", out.content);
    assert!(!out.content.contains("SECRET-METADATA"), "{}", out.content);
    assert_eq!(*seen.lock().unwrap(), vec!["GET http://safe.test/to-evil"]);
}

#[tokio::test]
async fn preflight_checks_redirect_hops_without_proxy() {
    // 不走代理时同样预检（不依赖连接阶段的系统解析结果）
    let addr = spawn_server().await;
    let tool = WebFetchTool::new(Arc::new(ExecutionEnv::new(".")), test_client())
        .with_lookup(metadata_lookup());
    let out = fetch_ok(
        &tool,
        format!("http://{addr}/to-evil"),
        serde_json::json!({}),
    )
    .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.starts_with("[Policy denied] redirect refused"),
        "{}",
        out.content
    );
    assert!(out.content.contains("evil.test"), "{}", out.content);
}

#[tokio::test]
async fn guarded_redirects_follow_reqwest_method_rules() {
    // 手动跟随（websearch 的 POST 表单也走这里）：302/301 把 POST 改为 GET，
    // 307 原样重放 POST
    let (addr, seen) = spawn_logged_server().await;
    let client = test_client();
    for path in ["/r1", "/r307"] {
        let request = client
            .post(format!("http://{addr}{path}"))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body("q=rust")
            .build()
            .unwrap();
        let response = match send_guarded(&client, request, &metadata_lookup()).await {
            Ok(response) => response,
            Err(_) => panic!("{path}: redirect chain should be followed"),
        };
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.url().path(), "/page", "{path}");
    }
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "POST /r1",
            "GET /r2",
            "GET /page",
            "POST /r307",
            "POST /page"
        ]
    );
}

#[tokio::test]
async fn preflight_passes_clean_and_unresolvable_hosts_to_proxy() {
    // 预检解析干净 → 放行；本地解析失败 → 放行（代理可能解析得了本地 DNS
    // 解析不了的名字，常见于需要代理才能访问的网络环境）
    let (proxy, seen) = spawn_logged_server().await;
    let tool = proxied_tool(proxy);
    for url in ["http://safe.test/page", "http://unknown.test/page"] {
        let out = fetch_ok(&tool, url.into(), serde_json::json!({})).await;
        assert!(!out.is_error, "{url}: {}", out.content);
        assert!(out.content.contains("# Hello"), "{}", out.content);
    }
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn oversized_output_spills_and_is_recoverable_with_expand() {
    let addr = spawn_server().await;
    let store = tempfile::tempdir().unwrap();
    let (tool, env) = tool_with(
        ExecutionEnv::new(".")
            .with_ctx_store(store.path())
            .with_max_output_bytes(4096),
    );
    let out = fetch_ok(&tool, format!("http://{addr}/big"), serde_json::json!({})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("# Big"), "head is delivered");
    assert!(!out.content.contains("THE-END-MARKER"), "tail is cut");
    assert!(out.content.len() < 4096 + 300, "{}", out.content.len());
    let original = out.original_bytes.expect("truncation recorded in ledger");
    assert!(original > 4096);
    assert!(out.original_tokens.unwrap() > 1000);

    // 标记携带句柄；完整转换结果可取回
    let handle = out
        .content
        .split("ctx:")
        .nth(1)
        .map(|s| s.chars().take(16).collect::<String>())
        .expect("ctx handle in marker");
    let full = env.retrieve(&handle).unwrap();
    assert_eq!(full.len() as u64, original);
    assert!(full.contains("THE-END-MARKER"));
    assert!(full.starts_with("[webfetch] "));

    // expand 工具按行分页取回尾部
    let total_lines = full.lines().count();
    let expand = ExpandTool::new(env.clone());
    let page = expand
        .execute(serde_json::json!({
            "handle": format!("ctx:{handle}"),
            "offset": total_lines,
            "limit": 1
        }))
        .await
        .unwrap();
    assert!(!page.is_error, "{}", page.content);
    assert!(page.content.contains("THE-END-MARKER"), "{}", page.content);
}

#[tokio::test]
async fn refuses_binary_and_oversized_bodies() {
    let addr = spawn_server().await;
    let (tool, _) = tool_with(ExecutionEnv::new("."));

    let out = fetch_ok(
        &tool,
        format!("http://{addr}/image.png"),
        serde_json::json!({}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.content.contains("binary content (image/png)"),
        "{}",
        out.content
    );

    // 未声明类型但内容含 NUL → 嗅探为二进制
    let out = fetch_ok(&tool, format!("http://{addr}/nul"), serde_json::json!({})).await;
    assert!(out.is_error);
    assert!(out.content.contains("binary content"), "{}", out.content);

    // Content-Length 超限：读正文前拒绝
    let out = fetch_ok(
        &tool,
        format!("http://{addr}/huge-declared"),
        serde_json::json!({}),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.contains("too large"), "{}", out.content);

    // 无长度声明的超长流：读到上限截断并在头部注明
    let out = fetch_ok(
        &tool,
        format!("http://{addr}/stream-huge"),
        serde_json::json!({}),
    )
    .await;
    assert!(
        !out.is_error,
        "{}",
        &out.content[..200.min(out.content.len())]
    );
    let first = out.content.lines().next().unwrap();
    assert!(
        first.contains(&format!("{MAX_BODY_BYTES} bytes")),
        "{first}"
    );
    assert!(first.contains("body cut at"), "{first}");
}

#[tokio::test]
async fn http_errors_charsets_and_empty_bodies() {
    let addr = spawn_server().await;
    let (tool, _) = tool_with(ExecutionEnv::new("."));

    let out = fetch_ok(
        &tool,
        format!("http://{addr}/missing"),
        serde_json::json!({}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.content.starts_with("[Error] HTTP 404 Not Found"),
        "{}",
        out.content
    );
    assert!(out.content.contains("no such page"), "{}", out.content);

    let out = fetch_ok(&tool, format!("http://{addr}/gbk"), serde_json::json!({})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("中文页面"), "{}", out.content);

    let out = fetch_ok(&tool, format!("http://{addr}/empty"), serde_json::json!({})).await;
    assert!(!out.is_error);
    assert!(
        out.content.contains("(empty response body)"),
        "{}",
        out.content
    );
}
