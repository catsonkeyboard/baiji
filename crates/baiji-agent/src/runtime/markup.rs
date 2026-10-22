//! Recovery for tool calls that a model emitted as XML/DSML-style markup in
//! plain assistant text instead of as provider-native structured tool calls.
//!
//! OpenAI-compatible and DeepSeek-family endpoints occasionally drift, at long
//! context, into printing their internal tool-call envelope as text:
//!
//! ```text
//! <｜DSML｜function_calls>
//! <｜DSML｜invoke name="bash">
//! <｜DSML｜parameter name="command" string="true">ls</｜DSML｜parameter>
//! </｜DSML｜invoke>
//! </｜DSML｜function_calls>
//! ```
//!
//! Variants seen in the wild drop or mangle the `｜DSML｜` prefix (`</DSML
//! parameter>`), use a bare `invoke`/`parameter` pair, or wrap the calls in
//! `calls`/`function_calls`/`tool_calls`. This module only ever looks at
//! assistant *text*, and callers only invoke it when the response carried no
//! structured tool calls, so structured tool-call arguments are never
//! inspected or rewritten.

use regex::Regex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

/// One tool call recovered from markup.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct MarkupToolCall {
    pub(super) name: String,
    pub(super) arguments: Value,
}

/// Result of scanning assistant text for tool-call markup.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum MarkupScan {
    /// No tool-call-shaped markup outside code spans.
    None,
    /// Every envelope parsed cleanly. `sanitized_text` is the text with the
    /// envelope removed (prose before and after it is kept).
    Parsed {
        calls: Vec<MarkupToolCall>,
        sanitized_text: String,
    },
    /// The text clearly contains a tool-call envelope, but it could not be
    /// turned into well-formed calls (missing tool name, orphan parameters,
    /// unterminated parameter, ...). Nothing should be executed. `reason`
    /// describes the first problem for diagnostics.
    Unparseable { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagKind {
    Wrapper,
    Invoke,
    Parameter,
}

#[derive(Debug, Clone)]
struct Tag {
    start: usize,
    end: usize,
    closing: bool,
    kind: TagKind,
    attrs: String,
}

static TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Optional namespace-ish prefix: `｜DSML｜`, `|DSML|`, `DSML ` or `antml:`.
    Regex::new(
        r"(?i)<\s*(/)?\s*(?:[｜|]\s*)?(?:DSML\s*(?:[｜|]\s*)?)?(?:antml:)?(function_calls|tool_calls|calls|invoke|parameter)\b([^<>]*)>",
    )
    .expect("valid tool markup tag regex")
});

static ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"([A-Za-z_][\w-]*)\s*=\s*"([^"]*)""#).expect("valid tool markup attr regex")
});

/// An open fenced code block: fence char, run length, column of the run and
/// blockquote depth of the opening line.
#[derive(Debug, Clone, Copy)]
struct Fence {
    ch: u8,
    len: usize,
    col: usize,
    depth: usize,
}

/// Skip a line's indentation and blockquote markers (and list markers when
/// `list_markers`): returns (byte offset of the content, its column, quote
/// depth). Container structure is not parsed; this only locates the fence.
fn container_prefix(line: &str, list_markers: bool) -> (usize, usize, usize) {
    let bytes = line.as_bytes();
    let (mut i, mut col, mut depth) = (0usize, 0usize, 0usize);
    loop {
        while let Some(&b) = bytes.get(i) {
            match b {
                b' ' => col += 1,
                b'\t' => col += 4 - col % 4,
                _ => break,
            }
            i += 1;
        }
        if bytes.get(i) == Some(&b'>') {
            depth += 1;
            i += 1;
            col += 1;
            continue;
        }
        let rest = &bytes[i..];
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        let marker = match rest.first() {
            Some(b'-' | b'+' | b'*') => 1,
            _ if (1..=9).contains(&digits) && matches!(rest.get(digits), Some(b'.' | b')')) => {
                digits + 1
            }
            _ => 0,
        };
        if list_markers && marker > 0 && matches!(rest.get(marker), Some(b' ' | b'\t')) {
            i += marker;
            col += marker;
            continue;
        }
        return (i, col, depth);
    }
}

/// CommonMark opening fence: ≥3 backticks or ≥3 tildes; a backtick fence's
/// info string must not contain backticks. Indentation is not capped at 3
/// columns because list nesting is not parsed — over-detecting code is the
/// safe direction.
fn fence_opener(line: &str) -> Option<Fence> {
    let (i, col, depth) = container_prefix(line, true);
    let rest = &line[i..];
    let ch = *rest.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = rest.bytes().take_while(|&b| b == ch).count();
    if len < 3 || (ch == b'`' && rest[len..].contains('`')) {
        return None;
    }
    Some(Fence {
        ch,
        len,
        col,
        depth,
    })
}

/// CommonMark closing fence: same char, at least as long as the opener,
/// nothing but whitespace after it, at the opener's quote depth and indented
/// no deeper than the opener (or 3 columns).
fn is_fence_closer(line: &str, fence: Fence) -> bool {
    let (i, col, depth) = container_prefix(line, false);
    if depth != fence.depth || col > fence.col.max(3) {
        return false;
    }
    let rest = &line[i..];
    let len = rest.bytes().take_while(|&b| b == fence.ch).count();
    len >= fence.len && rest[len..].trim().is_empty()
}

fn indent_columns(line: &str) -> usize {
    let mut col = 0;
    for b in line.bytes() {
        match b {
            b' ' => col += 1,
            b'\t' => col += 4 - col % 4,
            _ => break,
        }
    }
    col
}

/// Byte ranges of fenced and indented code blocks. Markup inside them is
/// documentation the model is showing, not a call it is making.
///
/// `breaks` (sorted) are the boundaries of parameter values already accepted
/// as raw text; each also ends a line, so a value may open and close its own
/// fence (`…```</parameter>`). Block state still flows through values: a fence
/// a value leaves open keeps everything after it code.
fn block_code_ranges(text: &str, breaks: &[usize]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut fence: Option<(usize, Fence)> = None;
    let mut indented = false;
    // An indented code block may start here (it cannot interrupt a paragraph).
    let mut after_blank = true;
    let mut pending = breaks.iter().copied().peekable();
    let mut offset = 0usize;
    for physical in text.split_inclusive('\n') {
        let line_end = offset + physical.len();
        let mut start = offset;
        let mut at_break = false;
        loop {
            while pending.next_if(|&p| p <= start).is_some() {}
            let end = pending
                .peek()
                .copied()
                .filter(|&p| p < line_end)
                .unwrap_or(line_end);
            let line = &text[start..end];
            if let Some((open_start, f)) = fence {
                if is_fence_closer(line, f) {
                    ranges.push((open_start, end));
                    fence = None;
                    after_blank = true;
                }
            } else if at_break {
                // Rest of a physical line after a value boundary: never blank
                // or indented in its own right, but it may open a fence when
                // it starts a value.
                if indented {
                    ranges.push((start, end));
                } else if let Some(f) = fence_opener(line) {
                    fence = Some((start, f));
                }
                after_blank = false;
            } else if line.trim().is_empty() {
                after_blank = true;
            } else if (indented || after_blank) && indent_columns(line) >= 4 {
                ranges.push((start, end));
                indented = true;
                after_blank = false;
            } else {
                indented = false;
                after_blank = false;
                if let Some(f) = fence_opener(line) {
                    fence = Some((start, f));
                }
            }
            if end == line_end {
                break;
            }
            start = end;
            at_break = true;
        }
        offset = line_end;
    }
    if let Some((open_start, _)) = fence {
        ranges.push((open_start, text.len()));
    }
    ranges
}

/// Byte ranges of inline code spans (a run of N backticks closed by the next
/// run of exactly N backticks on the same line). Markup quoted inline is prose.
fn inline_code_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'`' {
                i += 1;
                continue;
            }
            let open_start = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            let width = i - open_start;
            let mut j = i;
            let mut closed = None;
            while j < bytes.len() {
                if bytes[j] == b'`' {
                    let run_start = j;
                    while j < bytes.len() && bytes[j] == b'`' {
                        j += 1;
                    }
                    if j - run_start == width {
                        closed = Some(j);
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            if let Some(end) = closed {
                ranges.push((offset + open_start, offset + end));
                i = end;
            }
        }
        offset += line.len();
    }
    ranges
}

/// Whether `pos` falls inside block or inline code.
fn in_code(pos: usize, blocks: &[(usize, usize)], inline: &[(usize, usize)]) -> bool {
    blocks
        .iter()
        .chain(inline)
        .any(|&(s, e)| pos >= s && pos < e)
}

/// Tags outside code, in order. An accepted opening parameter tag is always
/// followed by its closing tag (unless the value is unterminated): the value
/// is raw text up to the next closing parameter tag, so backticks or fences
/// inside it never hide that tag, and tags inside it are skipped.
fn collect_tags(text: &str) -> Vec<Tag> {
    let raw: Vec<Tag> = TAG_RE
        .captures_iter(text)
        .filter_map(|caps| {
            let whole = caps.get(0)?;
            let kind = match caps.get(2)?.as_str().to_ascii_lowercase().as_str() {
                "invoke" => TagKind::Invoke,
                "parameter" => TagKind::Parameter,
                _ => TagKind::Wrapper,
            };
            Some(Tag {
                start: whole.start(),
                end: whole.end(),
                closing: caps.get(1).is_some(),
                kind,
                attrs: caps
                    .get(3)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default(),
            })
        })
        .collect();
    let inline = inline_code_ranges(text);
    let mut breaks = Vec::new();
    let mut blocks = block_code_ranges(text, &breaks);
    let mut tags = Vec::new();
    let mut i = 0usize;
    while i < raw.len() {
        let tag = &raw[i];
        i += 1;
        // `<invoke>` quoted inline with backticks is prose.
        if in_code(tag.start, &blocks, &inline) || text[..tag.start].ends_with('`') {
            continue;
        }
        tags.push(tag.clone());
        if tag.kind != TagKind::Parameter || tag.closing {
            continue;
        }
        let Some(offset) = raw[i..]
            .iter()
            .position(|t| t.kind == TagKind::Parameter && t.closing)
        else {
            break;
        };
        let close = &raw[i + offset];
        tags.push(close.clone());
        // Code state before the value is unaffected; rescan the rest with the
        // value's boundaries as line breaks.
        breaks.extend([tag.end, close.start]);
        blocks = block_code_ranges(text, &breaks);
        i += offset + 1;
    }
    tags
}

fn attr(attrs: &str, key: &str) -> Option<String> {
    ATTR_RE
        .captures_iter(attrs)
        .find(|caps| caps[1].eq_ignore_ascii_case(key))
        .map(|caps| caps[2].to_string())
}

/// Convert a raw parameter body into a JSON value.
///
/// DSML marks raw strings with `string="true"` and JSON literals with
/// `string="false"`. Without the hint, scalars and containers that parse as
/// JSON (numbers, booleans, objects, arrays) keep their JSON type and
/// everything else is a string. String values are kept byte-for-byte.
fn parameter_value(raw: &str, attrs: &str) -> Value {
    let hint = attr(attrs, "string").map(|v| v.to_ascii_lowercase());
    if hint.as_deref() == Some("true") {
        return Value::String(raw.to_string());
    }
    let trimmed = raw.trim();
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::String(_)) if hint.as_deref() != Some("false") => Value::String(raw.to_string()),
        Ok(value) if !trimmed.is_empty() => value,
        _ => Value::String(raw.to_string()),
    }
}

/// Scan `text` for tool-call markup and try to turn it into tool calls.
pub(super) fn scan_tool_call_markup(text: &str) -> MarkupScan {
    let tags = collect_tags(text);
    let relevant = tags
        .iter()
        .filter(|tag| tag.kind != TagKind::Wrapper)
        .count();
    // A single stray tag is far more likely to be prose than a call.
    if relevant < 2 {
        return MarkupScan::None;
    }
    // Prose that merely mentions the tag names ("put <parameter> tags inside an
    // <invoke> block") must not trigger a re-issue. Require a structural signal:
    // a wrapper tag, a named invoke, or a named parameter closed by an invoke.
    let named = |tag: &Tag| attr(&tag.attrs, "name").is_some_and(|n| !n.trim().is_empty());
    let strong = tags.iter().any(|t| t.kind == TagKind::Wrapper)
        || tags
            .iter()
            .any(|t| t.kind == TagKind::Invoke && !t.closing && named(t))
        || (tags
            .iter()
            .any(|t| t.kind == TagKind::Parameter && !t.closing && named(t))
            && tags.iter().any(|t| t.kind == TagKind::Invoke && t.closing));
    if !strong {
        return MarkupScan::None;
    }

    let mut calls = Vec::new();
    let mut problem: Option<String> = None;
    let mut current: Option<(Option<String>, Map<String, Value>)> = None;
    let mut i = 0usize;
    while i < tags.len() {
        let tag = &tags[i];
        match (tag.kind, tag.closing) {
            (TagKind::Wrapper, _) => {}
            (TagKind::Invoke, false) => {
                if current.is_some() && problem.is_none() {
                    problem = Some("nested or unterminated invoke".to_string());
                }
                current = Some((
                    attr(&tag.attrs, "name").filter(|n| !n.trim().is_empty()),
                    Map::new(),
                ));
            }
            (TagKind::Invoke, true) => match current.take() {
                Some((Some(name), arguments)) => calls.push(MarkupToolCall {
                    name: name.trim().to_string(),
                    arguments: Value::Object(arguments),
                }),
                Some((None, _)) => {
                    problem.get_or_insert_with(|| "invoke without a tool name".to_string());
                }
                None => {
                    problem.get_or_insert_with(|| {
                        "closing invoke without a matching opening invoke".to_string()
                    });
                }
            },
            (TagKind::Parameter, false) => {
                // Parameter bodies are raw: the body ends at the next closing
                // parameter tag regardless of any markup inside it.
                let Some(close_idx) = (i + 1..tags.len())
                    .find(|&j| tags[j].kind == TagKind::Parameter && tags[j].closing)
                else {
                    problem.get_or_insert_with(|| "unterminated parameter".to_string());
                    break;
                };
                let body = &text[tag.end..tags[close_idx].start];
                match (&mut current, attr(&tag.attrs, "name")) {
                    (Some((_, arguments)), Some(name)) if !name.trim().is_empty() => {
                        arguments
                            .insert(name.trim().to_string(), parameter_value(body, &tag.attrs));
                    }
                    (Some(_), _) => {
                        problem.get_or_insert_with(|| "parameter without a name".to_string());
                    }
                    (None, _) => {
                        problem.get_or_insert_with(|| {
                            "parameter outside of an invoke block".to_string()
                        });
                    }
                }
                i = close_idx;
            }
            (TagKind::Parameter, true) => {
                problem.get_or_insert_with(|| "closing parameter without an opening".to_string());
            }
        }
        i += 1;
    }
    if current.is_some() {
        problem.get_or_insert_with(|| "unterminated invoke".to_string());
    }
    if let Some(reason) = problem {
        return MarkupScan::Unparseable { reason };
    }
    if calls.is_empty() {
        return MarkupScan::Unparseable {
            reason: "no complete invoke block".to_string(),
        };
    }

    let first = tags.first().map(|t| t.start).unwrap_or(0);
    let last = tags.last().map(|t| t.end).unwrap_or(text.len());
    let prefix = text[..first].trim_end();
    let suffix = text[last..].trim();
    let mut sanitized_text = prefix.to_string();
    if !suffix.is_empty() {
        if !sanitized_text.is_empty() {
            sanitized_text.push('\n');
        }
        sanitized_text.push_str(suffix);
    }
    MarkupScan::Parsed {
        calls,
        sanitized_text,
    }
}

/// Model-facing notice (request copy only) when an envelope could not be parsed.
pub(super) fn unparsed_markup_notice(reason: &str) -> String {
    format!(
        "[system notice] Your previous response contained a tool call written as plain-text \
         markup (invoke/parameter envelope) instead of a native structured tool call. It could \
         not be parsed ({reason}), so nothing was executed. Re-issue the intended tool call now \
         using the native tool-calling interface, with the tool name and a complete JSON object \
         of arguments. Do not print tool-call markup as text and do not repeat completed work."
    )
}

/// `to=functions.NAME {json}`: the OpenAI-harmony call envelope leaked as text.
/// Returns (prose before, tool name, arguments, prose after).
///
/// Strict on purpose — prose explaining the format must never execute: the
/// marker must sit outside fenced/inline code, the JSON object must follow the
/// tool name directly (whitespace only), and it must end the message.
pub(super) fn parse_text_wrapped_tool_call(text: &str) -> Option<(String, String, Value, String)> {
    let marker = "to=functions.";
    let blocks = block_code_ranges(text, &[]);
    let inline = inline_code_ranges(text);
    let marker_idx = text
        .match_indices(marker)
        .map(|(idx, _)| idx)
        .filter(|idx| !in_code(*idx, &blocks, &inline))
        .last()?;
    let after_marker = &text[marker_idx + marker.len()..];
    let tool_name_end = after_marker
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || *ch == '_')
        .map(|(idx, ch)| idx + ch.len_utf8())
        .last()?;
    let tool_name = after_marker[..tool_name_end].to_string();
    let remaining = &after_marker[tool_name_end..];
    let json_start = remaining.len() - remaining.trim_start().len();
    let slice = &remaining[json_start..];
    if !slice.starts_with('{') {
        return None;
    }
    let mut stream = serde_json::Deserializer::from_str(slice).into_iter::<Value>();
    let Some(Ok(parsed)) = stream.next() else {
        return None;
    };
    if !parsed.is_object() || !slice[stream.byte_offset()..].trim().is_empty() {
        return None;
    }
    let prefix = text[..marker_idx].trim_end().to_string();
    Some((prefix, tool_name, parsed, String::new()))
}

#[cfg(test)]
#[path = "markup_tests.rs"]
mod tests;
