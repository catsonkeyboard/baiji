//! 聊天区工具活动的精简展示：`● name(摘要)` 的按工具摘要、`⎿` 结果的一行概述，
//! 以及按显示宽度的中间省略截断（CJK 计 2 列）。
//!
//! 摘要只做单行（换行折叠为空格）且限长存储；真正按终端宽度截断发生在渲染时。

use crate::i18n::{Strings, fill};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 存储进聊天行的摘要上限（字符数）：渲染时再按宽度中间省略
const SUMMARY_MAX_CHARS: usize = 400;

/// 工具调用摘要（`● name(…)` 括号内的部分；空串 = 不画括号）
pub fn tool_summary(name: &str, args: &Value, strings: &Strings) -> String {
    let s = |key: &str| args.get(key).and_then(Value::as_str);
    let n = |key: &str| args.get(key).and_then(Value::as_u64);
    let summary = match name {
        "bash" => s("command").map(|command| {
            let mut lines = command.trim().lines();
            let first = lines.next().unwrap_or("").trim();
            let more = lines.count();
            let mut out = format!("$ {first}");
            if more > 0 {
                out.push(' ');
                out.push_str(&fill(strings.tool_more_lines_tpl, &[&more]));
            }
            if args.get("run_in_background").and_then(Value::as_bool) == Some(true) {
                out.push_str(" (bg)");
            }
            out
        }),
        "read" => s("path").map(|path| {
            let mut out = path.to_string();
            match (n("offset"), n("limit")) {
                (Some(offset), Some(limit)) => out.push_str(&format!(":{offset}+{limit}")),
                (Some(offset), None) => out.push_str(&format!(":{offset}")),
                (None, Some(limit)) => out.push_str(&format!(":1+{limit}")),
                (None, None) => {}
            }
            if let Some(mode) = s("mode").filter(|m| *m != "full") {
                out.push_str(&format!(" mode={mode}"));
            }
            if let Some(density) = args.get("density").and_then(Value::as_f64) {
                out.push_str(&format!(" density={density}"));
            }
            out
        }),
        "grep" => s("pattern").map(|pattern| {
            let mut out = format!("\"{pattern}\"");
            if let Some(path) = s("path") {
                out.push_str(&format!(" in {path}"));
            }
            if let Some(glob) = s("glob") {
                out.push_str(&format!(" ({glob})"));
            }
            out
        }),
        "find" => s("pattern").map(|pattern| {
            let mut out = pattern.to_string();
            if let Some(path) = s("path") {
                out.push_str(&format!(" in {path}"));
            }
            if let Some(kind) = s("kind") {
                out.push_str(&format!(" ({kind})"));
            }
            out
        }),
        "ls" => Some(s("path").unwrap_or(".").to_string()),
        "edit" => s("path").or_else(|| s("file_path")).map(|path| {
            match args.get("edits").and_then(Value::as_array) {
                Some(edits) => format!("{path} {}", fill(strings.tool_edits_tpl, &[&edits.len()])),
                None => path.to_string(),
            }
        }),
        "write" => s("path").or_else(|| s("file_path")).map(str::to_string),
        "task" => s("prompt").map(|prompt| {
            let head = first_words(prompt, 12);
            match s("agent") {
                Some(agent) => format!("{agent}: {head}"),
                None => head,
            }
        }),
        "webfetch" => s("url").map(str::to_string),
        "websearch" | "search" => s("query").map(|query| {
            let mut out = format!("\"{query}\"");
            if let Some(path) = s("path") {
                out.push_str(&format!(" in {path}"));
            }
            out
        }),
        "imports" => s("target").map(|target| match s("direction") {
            Some(direction) => format!("{target} ({direction})"),
            None => target.to_string(),
        }),
        "expand" => s("handle").map(|handle| match (n("offset"), n("limit")) {
            (Some(offset), Some(limit)) => format!("{handle}:{offset}+{limit}"),
            (Some(offset), None) => format!("{handle}:{offset}"),
            _ => handle.to_string(),
        }),
        "jobs" => s("action").map(|action| match n("id") {
            Some(id) => format!("{action} #{id}"),
            None => action.to_string(),
        }),
        "todo" => s("action").map(|action| {
            if let Some(content) = s("content") {
                format!("{action} {content}")
            } else if let Some(id) = n("id") {
                match s("status") {
                    Some(status) => format!("{action} #{id} → {status}"),
                    None => format!("{action} #{id}"),
                }
            } else {
                action.to_string()
            }
        }),
        "memory" => s("action").map(|action| match s("fact").or_else(|| s("pattern")) {
            Some(text) => format!("{action} {text}"),
            None => action.to_string(),
        }),
        "skill" => s("name").map(|skill| match s("file") {
            Some(file) => format!("{skill}/{file}"),
            None => skill.to_string(),
        }),
        _ => None,
    };
    let summary = summary.unwrap_or_else(|| generic_summary(args));
    cap_chars(&single_line(&summary), SUMMARY_MAX_CHARS)
}

/// 其它工具（MCP / 外部 coding agent 等）：紧凑 JSON；
/// 只有 `prompt` 一个字段（外部 agent）时直接展示提示词
fn generic_summary(args: &Value) -> String {
    match args {
        Value::Null => String::new(),
        Value::Object(map) if map.is_empty() => String::new(),
        Value::Object(map) if map.len() == 1 => match map.get("prompt").and_then(Value::as_str) {
            Some(prompt) => first_words(prompt, 12),
            None => args.to_string(),
        },
        _ => args.to_string(),
    }
}

/// `⎿` 结果行的一行概述。
/// - bash：成功取首个输出行，失败为 `exit N · 首个错误行`
/// - 其它：首个非空行；失败时去掉 `[Error] ` 前缀
/// - 多行输出附 `(+N lines)` 余量
pub fn result_summary(name: &str, output: &str, is_error: bool, strings: &Strings) -> String {
    if name == "bash"
        && let Some(summary) = bash_result_summary(output, is_error, strings)
    {
        return cap_chars(&summary, SUMMARY_MAX_CHARS);
    }
    let mut lines = output.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("");
    let first = if is_error {
        first
            .strip_prefix("[Error]")
            .map(str::trim_start)
            .unwrap_or(first)
    } else {
        first
    };
    let more = lines.count();
    let mut out = single_line(first);
    if more > 0 {
        out.push(' ');
        out.push_str(&fill(strings.tool_more_lines_tpl, &[&more]));
    }
    cap_chars(&out, SUMMARY_MAX_CHARS)
}

/// bash 输出格式 `exit: N\n\n[stdout]\n…\n\n[stderr]\n…` 的概述（非此格式返回 None，
/// 例如后台任务的 job id 回执）
fn bash_result_summary(output: &str, is_error: bool, strings: &Strings) -> Option<String> {
    let mut lines = output.lines();
    let code = lines.next()?.strip_prefix("exit:")?.trim().to_string();
    let (mut stdout, mut stderr): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
    let mut section = 0u8; // 0 = 头部, 1 = stdout, 2 = stderr
    for line in lines {
        match line {
            "[stdout]" => section = 1,
            "[stderr]" => section = 2,
            _ if line.trim().is_empty() => {}
            _ if section == 1 => stdout.push(line.trim()),
            _ if section == 2 => stderr.push(line.trim()),
            _ => {}
        }
    }
    if is_error || code != "0" {
        // 失败：优先含 "error" 的行（编译/测试日志里最有信息量），其次 stderr 首行、stdout 末行
        let detail = stderr
            .iter()
            .chain(stdout.iter())
            .find(|l| l.to_ascii_lowercase().contains("error"))
            .or(stderr.first())
            .or(stdout.last());
        return Some(match detail {
            Some(detail) => format!("exit {code} · {}", single_line(detail)),
            None => format!("exit {code}"),
        });
    }
    let body = if stdout.is_empty() { &stderr } else { &stdout };
    Some(match body.split_first() {
        Some((first, [])) => single_line(first),
        Some((first, rest)) => format!(
            "{} {}",
            single_line(first),
            fill(strings.tool_more_lines_tpl, &[&rest.len()])
        ),
        None => format!("exit {code}"),
    })
}

/// 回放历史时推断工具结果是否失败（会话 JSONL 不记录 is_error）：
/// 识别 baiji 自身的错误前缀与 bash 非零退出码
pub fn output_looks_failed(name: &str, output: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "[Error]",
        "[Policy denied]",
        "[Denied by",
        "[Unknown tool",
        "[Tool error]",
        "[Cancelled]",
        "[Plan mode]",
        "[Timeout]",
        "[external agent exited with",
    ];
    let trimmed = output.trim_start();
    if PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        return true;
    }
    name == "bash"
        && trimmed
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("exit:"))
            .is_some_and(|code| code.trim() != "0")
}

/// 按显示宽度中间省略：保留头尾、中间以 `…` 连接，结果宽度 ≤ max_width
pub fn truncate_middle_display(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let remaining = max_width - 1;
    let head = remaining / 2 + remaining % 2;
    let tail = remaining / 2;
    format!("{}…{}", display_prefix(s, head), display_suffix(s, tail))
}

/// 按显示宽度尾部截断（超出时以 `…` 结尾，结果宽度 ≤ max_width）
pub fn truncate_end_display(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    format!("{}…", display_prefix(s, max_width - 1))
}

/// 显示宽度不超过 max_width 的最长前缀
fn display_prefix(s: &str, max_width: usize) -> &str {
    let mut used = 0usize;
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        let w = ch.width().unwrap_or(0);
        if used + w > max_width {
            break;
        }
        used += w;
        end = idx + ch.len_utf8();
    }
    &s[..end]
}

/// 显示宽度不超过 max_width 的最长后缀
fn display_suffix(s: &str, max_width: usize) -> &str {
    let mut used = 0usize;
    let mut start = s.len();
    for (idx, ch) in s.char_indices().rev() {
        let w = ch.width().unwrap_or(0);
        if used + w > max_width {
            break;
        }
        used += w;
        start = idx;
    }
    &s[start..]
}

/// 折叠为单行：换行/制表/控制字符 → 空格（防止转义序列混入聊天区），连续空白合一
fn single_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for c in s.trim().chars() {
        if c.is_whitespace() || c.is_control() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out
}

/// 首行的前 n 个词（task / 外部 agent 提示词预览）
fn first_words(s: &str, n: usize) -> String {
    let first = s.trim().lines().next().unwrap_or("");
    let words: Vec<&str> = first.split_whitespace().collect();
    if words.len() > n {
        format!("{}…", words[..n].join(" "))
    } else {
        words.join(" ")
    }
}

/// 字符数封顶（存储上限，不是显示截断）
fn cap_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn en() -> Strings {
        Strings::en()
    }

    #[test]
    fn test_tool_summary_per_tool() {
        let s = en();
        assert_eq!(
            tool_summary("bash", &json!({"command": "ls -la"}), &s),
            "$ ls -la"
        );
        assert_eq!(
            tool_summary(
                "bash",
                &json!({"command": "cat <<EOF > a\nx\ny\nEOF", "run_in_background": true}),
                &s
            ),
            "$ cat <<EOF > a (+3 lines) (bg)"
        );
        assert_eq!(
            tool_summary("read", &json!({"path": "src/lib.rs"}), &s),
            "src/lib.rs"
        );
        assert_eq!(
            tool_summary(
                "read",
                &json!({"path": "a.rs", "offset": 10, "limit": 20}),
                &s
            ),
            "a.rs:10+20"
        );
        assert_eq!(
            tool_summary("read", &json!({"path": "src", "mode": "map"}), &s),
            "src mode=map"
        );
        assert_eq!(
            tool_summary(
                "grep",
                &json!({"pattern": "fn main", "path": "src", "glob": "*.rs"}),
                &s
            ),
            "\"fn main\" in src (*.rs)"
        );
        assert_eq!(tool_summary("grep", &json!({"pattern": "x"}), &s), "\"x\"");
        assert_eq!(
            tool_summary(
                "find",
                &json!({"pattern": "mod", "path": "crates", "kind": "file"}),
                &s
            ),
            "mod in crates (file)"
        );
        assert_eq!(tool_summary("ls", &json!({}), &s), ".");
        assert_eq!(tool_summary("ls", &json!({"path": "src"}), &s), "src");
        assert_eq!(
            tool_summary(
                "edit",
                &json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
                &s
            ),
            "a.rs"
        );
        assert_eq!(
            tool_summary(
                "edit",
                &json!({"path": "a.rs", "edits": [{"old_string": "a", "new_string": "b"}, {"old_string": "c", "new_string": "d"}]}),
                &s
            ),
            "a.rs (2 edits)"
        );
        assert_eq!(
            tool_summary("write", &json!({"path": "out.txt", "content": "hi"}), &s),
            "out.txt"
        );
        assert_eq!(
            tool_summary(
                "task",
                &json!({"agent": "explorer", "prompt": "Find every caller of foo and report back with paths and lines please"}),
                &s
            ),
            "explorer: Find every caller of foo and report back with paths and lines…"
        );
        assert_eq!(
            tool_summary("webfetch", &json!({"url": "https://x.dev"}), &s),
            "https://x.dev"
        );
        assert_eq!(
            tool_summary("websearch", &json!({"query": "rust tui"}), &s),
            "\"rust tui\""
        );
        // MCP 等未知工具：紧凑 JSON
        assert_eq!(
            tool_summary("mcp.server.tool", &json!({"a": 1, "b": "x"}), &s),
            r#"{"a":1,"b":"x"}"#
        );
        // 外部 agent：只有 prompt 时直接展示
        assert_eq!(
            tool_summary("codex", &json!({"prompt": "fix it"}), &s),
            "fix it"
        );
        assert_eq!(tool_summary("now", &json!({}), &s), "");
        // 中文包
        let zh = Strings::zh();
        assert_eq!(
            tool_summary("bash", &json!({"command": "a\nb"}), &zh),
            "$ a (+1 行)"
        );
    }

    #[test]
    fn test_summary_single_line_and_capped() {
        let s = en();
        // 控制字符不进聊天区
        assert_eq!(
            tool_summary("grep", &json!({"pattern": "a\u{1b}[2Jb"}), &s),
            "\"a [2Jb\""
        );
        let long = "x".repeat(2000);
        let summary = tool_summary("bash", &json!({"command": long}), &s);
        assert!(summary.chars().count() <= SUMMARY_MAX_CHARS + 1);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn test_result_summary() {
        let s = en();
        assert_eq!(
            result_summary("bash", "exit: 0\n\n[stdout]\nhello\nworld\n", false, &s),
            "hello (+1 lines)"
        );
        assert_eq!(result_summary("bash", "exit: 0\n", false, &s), "exit 0");
        assert_eq!(
            result_summary(
                "bash",
                "exit: 101\n\n[stdout]\nok\n\n[stderr]\n   Compiling x\nerror[E0425]: cannot find value\n",
                true,
                &s
            ),
            "exit 101 · error[E0425]: cannot find value"
        );
        assert_eq!(
            result_summary("bash", "exit: 2\n\n[stderr]\nls: nope\n", true, &s),
            "exit 2 · ls: nope"
        );
        assert_eq!(
            result_summary("edit", "[Error] old_string not found in 'a.rs'.", true, &s),
            "old_string not found in 'a.rs'."
        );
        assert_eq!(
            result_summary("read", "line1\nline2\nline3", false, &s),
            "line1 (+2 lines)"
        );
        assert_eq!(result_summary("read", "", false, &s), "");
    }

    #[test]
    fn test_output_looks_failed() {
        assert!(output_looks_failed("edit", "[Error] old_string not found"));
        assert!(output_looks_failed("bash", "exit: 1\n\n[stderr]\nx"));
        assert!(!output_looks_failed("bash", "exit: 0\n"));
        assert!(output_looks_failed(
            "write",
            "[Denied by user] user declined"
        ));
        assert!(!output_looks_failed("read", "fn main() {}"));
    }

    #[test]
    fn test_truncate_middle_display() {
        assert_eq!(truncate_middle_display("hello", 10), "hello");
        assert_eq!(truncate_middle_display("abcdefghij", 5), "ab…ij");
        assert_eq!(
            UnicodeWidthStr::width(truncate_middle_display("abcdefghij", 6).as_str()),
            6
        );
        // CJK 按 2 列计宽，结果不超预算
        let cut = truncate_middle_display("中文中文中文中文", 7);
        assert!(UnicodeWidthStr::width(cut.as_str()) <= 7);
        assert_eq!(cut, "中…文");
        assert_eq!(truncate_middle_display("ab中文cd", 6), "ab…cd");
        assert_eq!(truncate_middle_display("abc", 1), "…");
        assert_eq!(truncate_middle_display("abc", 0), "");
        assert_eq!(truncate_end_display("abcdef", 4), "abc…");
        assert_eq!(truncate_end_display("你好世界", 5), "你好…");
    }
}
