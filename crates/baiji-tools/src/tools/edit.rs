//! edit 工具：精确字符串替换（old_string 必须唯一，或显式 replace_all）
//!
//! - 单次形式：`old_string` / `new_string`（+ `replace_all`）
//! - 批量形式：`edits: [{old_string, new_string, replace_all?}, …]`——同一文件的多处修改，
//!   按序作用于演进中的内容，**全有或全无**（任一条失败则不写盘，错误指明失败条目与原因）
//! - 匹配失败给出诊断提示（行尾空白 / 缩进 / 近似行），绝不静默套用模糊匹配
//! - 成功后附带结果片段（每处改动 ±3 行，行号与 read mode=full 一致），省去一次重读

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::sync::Arc;
use tokio::fs;

use crate::env::ExecutionEnv;

/// 结果片段：改动处上下各保留的行数
const SNIPPET_CONTEXT: usize = 3;
/// 结果片段总行数上限（超出的改动只计数不展示）
const MAX_SNIPPET_LINES: usize = 40;
/// 片段单行字符上限（压缩文件/超长行不撑爆输出）
const MAX_SNIPPET_LINE_CHARS: usize = 240;
/// 提示里引用文本的截断长度
const HINT_TEXT_CHARS: usize = 120;
/// 近似行提示的最低相似度（字符二元组 Dice 系数）
const NEAR_MISS_THRESHOLD: f64 = 0.5;
/// 近似行检查最多考察的 old_string 行数
const NEAR_MISS_MAX_OLD_LINES: usize = 30;

pub struct EditTool {
    env: Arc<ExecutionEnv>,
}

impl EditTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }
}

/// 一条替换
struct EditOp {
    old: String,
    new: String,
    replace_all: bool,
}

/// 全部替换成功后的结果
struct Applied {
    content: String,
    /// 总替换次数
    replaced: usize,
    /// 改动区域：最终内容中的字节区间 [start, end)（纯删除时 start == end）
    regions: Vec<(usize, usize)>,
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing exact strings. Pass either old_string/new_string (one change) \
         or edits (an array of {old_string, new_string, replace_all?} for several changes to the \
         same file, applied in order; all succeed or nothing is written). Each old_string must \
         match exactly once (include surrounding context to disambiguate), or set replace_all=true. \
         On a mismatch the error explains why (whitespace/indentation/closest line). On success \
         a numbered snippet of the result around each change is returned (same line numbers \
         as read), so no re-read is needed."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to edit"},
                "old_string": {"type": "string", "description": "Exact text to replace (single-change form; omit when using edits)"},
                "new_string": {"type": "string", "description": "Replacement text (single-change form)"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (single-change form, default false)"},
                "edits": {
                    "type": "array",
                    "description": "Several changes to this file, applied in order against the evolving content; all-or-nothing. Use instead of old_string/new_string.",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string", "description": "Exact text to replace"},
                            "new_string": {"type": "string", "description": "Replacement text"},
                            "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(path) = args["path"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'path'"));
        };
        let (ops, batch) = match parse_ops(&args) {
            Ok(parsed) => parsed,
            Err(message) => return Ok(ToolOutput::err(message)),
        };

        let resolved = match self.env.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        let content = match fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "[Error] reading '{}': {e}",
                    resolved.display()
                )));
            }
        };

        let display = resolved.display().to_string();
        let applied = match apply_ops(&content, &ops, &display, batch) {
            Ok(applied) => applied,
            Err(message) => return Ok(ToolOutput::err(message)),
        };

        if let Err(e) = crate::env::atomic_write(&resolved, applied.content.as_bytes()).await {
            return Ok(ToolOutput::err(format!("[Error] writing '{display}': {e}")));
        }

        let mut message = format!(
            "Edited {display}: replaced {} occurrence(s)",
            applied.replaced
        );
        if batch {
            message.push_str(&format!(" across {} edits", ops.len()));
        }
        let snippet = render_snippet(&applied.content, &applied.regions);
        if !snippet.is_empty() {
            message.push('\n');
            message.push_str(&snippet);
        }
        Ok(ToolOutput::ok(message))
    }
}

/// 解析参数：单次形式或 `edits` 数组（互斥）。返回 (替换列表, 是否批量形式)；
/// Err 为直接回传模型的错误文本。
fn parse_ops(args: &Value) -> std::result::Result<(Vec<EditOp>, bool), String> {
    let edits = &args["edits"];
    if !edits.is_null() {
        if !args["old_string"].is_null() || !args["new_string"].is_null() {
            return Err(
                "[Error] use either old_string/new_string or edits, not both in one call"
                    .to_string(),
            );
        }
        if !args["replace_all"].is_null() {
            return Err(
                "[Error] top-level replace_all only applies to old_string/new_string; \
                 set replace_all inside each edits entry instead"
                    .to_string(),
            );
        }
        let Some(list) = edits.as_array() else {
            return Err(
                "[Error] 'edits' must be an array of {old_string, new_string, replace_all?} objects"
                    .to_string(),
            );
        };
        if list.is_empty() {
            return Err("[Error] 'edits' must contain at least one entry".to_string());
        }
        let mut ops = Vec::with_capacity(list.len());
        for (i, entry) in list.iter().enumerate() {
            let (Some(old), Some(new)) =
                (entry["old_string"].as_str(), entry["new_string"].as_str())
            else {
                return Err(format!(
                    "[Error] edits[{i}] needs string fields 'old_string' and 'new_string'; \
                     nothing was written"
                ));
            };
            ops.push(EditOp {
                old: old.to_string(),
                new: new.to_string(),
                replace_all: entry["replace_all"].as_bool().unwrap_or(false),
            });
        }
        return Ok((ops, true));
    }

    let Some(old) = args["old_string"].as_str() else {
        return Err(
            "[Error] missing required argument 'old_string' (or pass an 'edits' array)".to_string(),
        );
    };
    let Some(new) = args["new_string"].as_str() else {
        return Err("[Error] missing required argument 'new_string'".to_string());
    };
    Ok((
        vec![EditOp {
            old: old.to_string(),
            new: new.to_string(),
            replace_all: args["replace_all"].as_bool().unwrap_or(false),
        }],
        false,
    ))
}

/// 按序在内存副本上应用全部替换；任一条失败则整体失败（文件不会被改成半成品）。
/// 失败条目全部列出（后续条目基于"跳过失败条目后"的内容校验），一次修正到位。
fn apply_ops(
    original: &str,
    ops: &[EditOp],
    display: &str,
    batch: bool,
) -> std::result::Result<Applied, String> {
    let mut content = original.to_string();
    let mut regions: Vec<(usize, usize)> = Vec::new();
    let mut replaced = 0;
    let mut failures = Vec::new();
    // 批量形式的错误头已点名文件，条目原因里不再重复路径
    let location = if batch {
        String::new()
    } else {
        format!(" in '{display}'")
    };

    for (i, op) in ops.iter().enumerate() {
        match locate(&content, op, &location) {
            Ok((old, new, positions)) => {
                replaced += positions.len();
                content = replace_at(&content, &positions, old.len(), &new, &mut regions);
            }
            Err(reason) => failures.push((i, reason)),
        }
    }

    if failures.is_empty() {
        return Ok(Applied {
            content,
            replaced,
            regions,
        });
    }
    if !batch {
        return Err(format!("[Error] {}", failures.remove(0).1));
    }
    let mut message = format!(
        "[Error] No changes written to '{display}': {} of {} edits failed:",
        failures.len(),
        ops.len()
    );
    for (i, reason) in &failures {
        message.push_str(&format!("\nedits[{i}]: {reason}"));
    }
    message.push_str(
        "\n(Edits apply in order: each entry is matched against the content produced by the \
         entries before it. Fix the failing entries and resend the whole call.)",
    );
    Err(message)
}

/// 单条替换的校验与定位：返回实际使用的 (old, new)（CRLF 适配后）与全部匹配位置
/// （非 replace_all 时恰好一个）。Err 为不带 `[Error]` 前缀的原因；
/// `location` 是拼进原因里的 ` in '<path>'`（批量形式为空）。
fn locate(
    content: &str,
    op: &EditOp,
    location: &str,
) -> std::result::Result<(String, String, Vec<usize>), String> {
    if op.old.is_empty() {
        return Err(
            "old_string must not be empty (use the write tool to create a file)".to_string(),
        );
    }
    if op.old == op.new {
        return Err("old_string and new_string are identical; nothing to change".to_string());
    }

    // CRLF 文件：read 工具按行展示时已去掉 \r，模型给出的多行 old_string 只含 \n。
    // 直接匹配不上时，按文件的换行风格转换后重试（new_string 同步转换，保持风格一致）
    let (old, new) = if !content.contains(op.old.as_str())
        && content.contains("\r\n")
        && op.old.contains('\n')
        && !op.old.contains('\r')
    {
        (
            op.old.replace('\n', "\r\n"),
            op.new.replace("\r\n", "\n").replace('\n', "\r\n"),
        )
    } else {
        (op.old.clone(), op.new.clone())
    };

    let positions: Vec<usize> = content
        .match_indices(old.as_str())
        .map(|(pos, _)| pos)
        .collect();
    match positions.len() {
        0 => {
            let mut reason = format!(
                "old_string not found{location}. Verify the exact text (including whitespace)."
            );
            if let Some(hint) = mismatch_hint(content, &op.old) {
                reason.push_str("\nHint: ");
                reason.push_str(&hint);
            }
            Err(reason)
        }
        n if n > 1 && !op.replace_all => Err(format!(
            "old_string matches {n} locations{location}. Add surrounding context to make it \
             unique, or set replace_all=true."
        )),
        _ if op.replace_all => Ok((old, new, positions)),
        _ => Ok((old, new, positions[..1].to_vec())),
    }
}

/// 在给定位置（升序、互不重叠）替换，同时把已有改动区域平移到新坐标并登记新区域
fn replace_at(
    content: &str,
    positions: &[usize],
    old_len: usize,
    new: &str,
    regions: &mut Vec<(usize, usize)>,
) -> String {
    let new_len = new.len();
    let mut out = String::with_capacity(content.len() + positions.len() * new_len);
    let mut last = 0;
    for &pos in positions {
        out.push_str(&content[last..pos]);
        out.push_str(new);
        last = pos + old_len;
    }
    out.push_str(&content[last..]);

    // 第 k 个匹配在新内容中的起点：前面 k 个匹配各自伸缩了 new_len - old_len
    let shifted = |pos: usize, k: usize| pos - k * old_len + k * new_len;
    // 旧坐标 → 新坐标；落在被替换段内部的坐标吸附到替换段边界
    let remap = |x: usize, is_end: bool| -> usize {
        let mut before = 0;
        for (k, &pos) in positions.iter().enumerate() {
            if pos + old_len <= x {
                before = k + 1;
            } else if pos < x {
                let start = shifted(pos, k);
                return if is_end { start + new_len } else { start };
            } else {
                break;
            }
        }
        x - before * old_len + before * new_len
    };
    for region in regions.iter_mut() {
        *region = (remap(region.0, false), remap(region.1, true));
    }
    for (k, &pos) in positions.iter().enumerate() {
        let start = shifted(pos, k);
        regions.push((start, start + new_len));
    }
    out
}

// ========== 结果片段 ==========

/// 改动处 ±SNIPPET_CONTEXT 行的带行号片段（`N\t行`，与 read mode=full 一致）：
/// 重叠窗口合并，总行数封顶，超出的改动只计数。
fn render_snippet(content: &str, regions: &[(usize, usize)]) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() || regions.is_empty() {
        return String::new();
    }
    // 每行起始字节偏移（二分定位行号；replace_all 大量区域时避免反复扫描）
    let mut line_starts = vec![0usize];
    line_starts.extend(
        content
            .bytes()
            .enumerate()
            .filter(|(_, b)| *b == b'\n')
            .map(|(i, _)| i + 1),
    );
    let total = lines.len();
    let line_of = |byte: usize| -> usize {
        let idx = line_starts.partition_point(|&start| start <= byte);
        idx.clamp(1, total)
    };

    // 每处改动的 (首行, 末行)
    let spans: Vec<(usize, usize)> = regions
        .iter()
        .map(|&(start, end)| {
            let first = line_of(start);
            let last = if end > start { line_of(end - 1) } else { first };
            (first, last.max(first))
        })
        .collect();
    let mut windows: Vec<(usize, usize)> = spans
        .iter()
        .map(|&(first, last)| {
            (
                first.saturating_sub(SNIPPET_CONTEXT).max(1),
                (last + SNIPPET_CONTEXT).min(total),
            )
        })
        .collect();
    windows.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for window in windows {
        match merged.last_mut() {
            Some(prev) if window.0 <= prev.1 + 1 => prev.1 = prev.1.max(window.1),
            _ => merged.push(window),
        }
    }

    let mut out: Vec<String> = Vec::new();
    // 实际展示的行区间（统计被省略的改动）
    let mut shown: Vec<(usize, usize)> = Vec::new();
    let mut used = 0;
    for (first, last) in merged {
        if used >= MAX_SNIPPET_LINES {
            break;
        }
        if !out.is_empty() {
            out.push("…".to_string());
        }
        let len = last - first + 1;
        let take = len.min(MAX_SNIPPET_LINES - used);
        for n in first..first + take {
            out.push(format!("{n}\t{}", clip_line(lines[n - 1])));
        }
        if take < len {
            out.push(format!("[… {} more lines]", len - take));
        }
        used += take;
        shown.push((first, first + take - 1));
    }
    let elided = spans
        .iter()
        .filter(|(first, _)| !shown.iter().any(|(a, b)| a <= first && first <= b))
        .count();
    if elided > 0 {
        out.push(format!(
            "[snippet capped at {MAX_SNIPPET_LINES} lines: {elided} more change(s) not shown — \
             read the file to view them]"
        ));
    }
    out.join("\n")
}

/// 片段行截断（按字符边界）
fn clip_line(line: &str) -> String {
    let count = line.chars().count();
    if count <= MAX_SNIPPET_LINE_CHARS {
        return line.to_string();
    }
    let head: String = line.chars().take(MAX_SNIPPET_LINE_CHARS).collect();
    format!("{head}… [+{} chars]", count - MAX_SNIPPET_LINE_CHARS)
}

// ========== 匹配失败诊断 ==========
// 只诊断、不套用：提示里给出行号与差异，模型据此一次修正调用。

/// 依次尝试：忽略行尾空白 → 忽略缩进 → 近似行。返回第一条成立的提示。
fn mismatch_hint(content: &str, old: &str) -> Option<String> {
    if old.trim().is_empty() {
        return None;
    }
    let file_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old.lines().collect();
    if file_lines.is_empty() || old_lines.is_empty() {
        return None;
    }
    trailing_whitespace_hint(&file_lines, &old_lines)
        .or_else(|| indentation_hint(&file_lines, &old_lines))
        .or_else(|| near_miss_hint(&file_lines, &old_lines))
}

/// 忽略每行行尾空白（含 \r 与末尾换行）后能否匹配
fn trailing_whitespace_hint(file_lines: &[&str], old_lines: &[&str]) -> Option<String> {
    let strip = |lines: &[&str]| {
        lines
            .iter()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let file = strip(file_lines);
    let old = strip(old_lines);
    let pos = file.find(&old)?;
    let first = file[..pos].matches('\n').count() + 1;
    let last = first + old.matches('\n').count();
    Some(format!(
        "old_string matches {} if trailing whitespace and line endings are ignored — the file \
         differs only in trailing spaces/tabs or the final newline. Copy the exact text from \
         the file (no fuzzy match was applied).",
        fmt_lines(first, last)
    ))
}

/// 忽略每行首尾空白后能否匹配（多行：首行允许后缀匹配、末行允许前缀匹配）。
/// 命中时报告 1-based 行号与文件实际缩进。
fn indentation_hint(file_lines: &[&str], old_lines: &[&str]) -> Option<String> {
    let n = old_lines.len();
    if n > file_lines.len() {
        return None;
    }
    let old_trimmed: Vec<&str> = old_lines.iter().map(|l| l.trim()).collect();
    let window_matches = |window: &[&str]| -> bool {
        if n == 1 {
            return window[0].trim().contains(old_trimmed[0]);
        }
        window
            .iter()
            .zip(&old_trimmed)
            .enumerate()
            .all(|(k, (file, old))| {
                let file = file.trim();
                if k == 0 {
                    file.ends_with(old)
                } else if k == n - 1 {
                    file.starts_with(old)
                } else {
                    file == *old
                }
            })
    };

    let mut first = None;
    let mut locations = 0;
    for start in 0..=file_lines.len() - n {
        if window_matches(&file_lines[start..start + n]) {
            locations += 1;
            first.get_or_insert(start);
        }
    }
    let start = first?;

    // 第一处"整行内容相同但缩进不同"的行：报告双方实际缩进
    let detail = (0..n).find_map(|k| {
        let file = file_lines[start + k];
        let old = old_lines[k];
        let (file_ws, old_ws) = (leading_ws(file), leading_ws(old));
        (!old.trim().is_empty() && file.trim() == old.trim() && file_ws != old_ws).then(|| {
            format!(
                "file line {} is indented with {} but old_string has {}",
                start + k + 1,
                describe_indent(file_ws),
                describe_indent(old_ws)
            )
        })
    });
    let mut hint = format!(
        "old_string matches {} if indentation/surrounding whitespace is ignored",
        fmt_lines(start + 1, start + n)
    );
    if locations > 1 {
        hint.push_str(&format!(" (first of {locations} such locations)"));
    }
    match detail {
        Some(detail) => hint.push_str(&format!(": {detail}.")),
        None => hint.push('.'),
    }
    hint.push_str(
        " Re-indent old_string (and new_string) to match the file exactly (no fuzzy match was applied).",
    );
    Some(hint)
}

/// 近似行：找出第一条在文件中不存在的 old_string 行，给出最相似的文件行
fn near_miss_hint(file_lines: &[&str], old_lines: &[&str]) -> Option<String> {
    let file_trimmed: Vec<&str> = file_lines.iter().map(|l| l.trim()).collect();
    let candidates: Vec<(usize, &str)> = old_lines
        .iter()
        .enumerate()
        .map(|(i, l)| (i, l.trim()))
        .filter(|(_, t)| t.chars().count() >= 3)
        .take(NEAR_MISS_MAX_OLD_LINES)
        .collect();
    let first = *candidates.first()?;
    let label = |i: usize| {
        if old_lines.len() > 1 {
            format!("old_string line {}", i + 1)
        } else {
            "old_string".to_string()
        }
    };

    let missing = candidates
        .iter()
        .find(|(_, text)| !file_trimmed.iter().any(|f| f.contains(text)));
    match missing {
        Some(&(i, text)) => Some(match closest_line(&file_trimmed, text) {
            Some((idx, score)) if score >= NEAR_MISS_THRESHOLD => format!(
                "{} `{}` does not appear in the file; the closest line is {} ({:.0}% similar): `{}`",
                label(i),
                clip_hint(text),
                idx + 1,
                score * 100.0,
                clip_hint(file_lines[idx].trim_end())
            ),
            _ => format!(
                "{} `{}` does not appear anywhere in the file — re-read the file (it may have \
                 changed since you last read it).",
                label(i),
                clip_hint(text)
            ),
        }),
        // 每行单独都存在，但不是连续的这一段
        None if old_lines.len() > 1 => {
            let found = file_trimmed.iter().position(|f| f.contains(first.1))? + 1;
            Some(format!(
                "every line of old_string exists in the file, but not consecutively in this \
                 order (its first line appears at line {found}) — lines may be missing, extra \
                 or reordered; re-read that region."
            ))
        }
        None => None,
    }
}

/// 与 `target` 最相似的文件行：(0-based 下标, 相似度)
fn closest_line(file_trimmed: &[&str], target: &str) -> Option<(usize, f64)> {
    let target_grams = bigrams(target);
    if target_grams.is_empty() {
        return None;
    }
    let mut best: Option<(usize, f64)> = None;
    for (idx, line) in file_trimmed.iter().enumerate() {
        // 长度悬殊时 Dice 上界 2·min/(a+b) 已低于阈值：跳过，省掉二元组计算
        let (a, b) = (target.len(), line.len());
        if a.min(b) * 2 < (a + b) / 2 {
            continue;
        }
        let score = dice(&target_grams, &bigrams(line));
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((idx, score));
        }
    }
    best
}

/// 字符二元组（排序后便于归并求交）；超长行只取前 400 字符
fn bigrams(text: &str) -> Vec<(char, char)> {
    let chars: Vec<char> = text.chars().take(400).collect();
    let mut grams: Vec<(char, char)> = chars.windows(2).map(|w| (w[0], w[1])).collect();
    grams.sort_unstable();
    grams
}

/// Sørensen–Dice 系数（多重集交）
fn dice(a: &[(char, char)], b: &[(char, char)]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    2.0 * common as f64 / (a.len() + b.len()) as f64
}

fn leading_ws(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// 缩进描述：`4 spaces` / `1 tab` / `1 tab + 2 spaces` / `no indentation`
fn describe_indent(ws: &str) -> String {
    let tabs = ws.chars().filter(|c| *c == '\t').count();
    let spaces = ws.chars().filter(|c| *c == ' ').count();
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    match (tabs, spaces) {
        (0, 0) => "no indentation".to_string(),
        (0, s) => plural(s, "space"),
        (t, 0) => plural(t, "tab"),
        (t, s) => format!("{} + {}", plural(t, "tab"), plural(s, "space")),
    }
}

fn fmt_lines(first: usize, last: usize) -> String {
    if first == last {
        format!("at line {first}")
    } else {
        format!("at lines {first}-{last}")
    }
}

fn clip_hint(text: &str) -> String {
    if text.chars().count() <= HINT_TEXT_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(HINT_TEXT_CHARS).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup(content: &str) -> (tempfile::TempDir, EditTool) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.rs"), content).await.unwrap();
        let tool = EditTool::new(Arc::new(ExecutionEnv::new(dir.path())));
        (dir, tool)
    }

    fn read(dir: &tempfile::TempDir, name: &str) -> String {
        std::fs::read_to_string(dir.path().join(name)).unwrap()
    }

    #[tokio::test]
    async fn test_edit_unique_replacement() {
        let (dir, tool) = setup("fn main() {\n    println!(\"hi\");\n}\n").await;
        tool.execute(serde_json::json!({
            "path": "f.rs",
            "old_string": "println!(\"hi\");",
            "new_string": "println!(\"bye\");"
        }))
        .await
        .unwrap();

        let updated = fs::read_to_string(dir.path().join("f.rs")).await.unwrap();
        assert!(updated.contains("bye"));
        assert!(!updated.contains("hi"));
    }

    #[tokio::test]
    async fn test_edit_ambiguous_requires_replace_all() {
        let (_dir, tool) = setup("let a = 1;\nlet b = 1;\n").await;

        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "old_string": "1",
                "new_string": "2"
            }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("2 locations"));

        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "old_string": "1",
                "new_string": "2",
                "replace_all": true
            }))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("2 occurrence(s)"));
    }

    #[tokio::test]
    async fn test_edit_not_found() {
        let (_dir, tool) = setup("content\n").await;
        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "old_string": "nope",
                "new_string": "x"
            }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not found"));
    }

    #[tokio::test]
    async fn test_edit_crlf_file_with_lf_old_string() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("win.txt");
        std::fs::write(&file, "line one\r\nline two\r\nline three\r\n").unwrap();
        let tool = EditTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({
                "path": "win.txt",
                "old_string": "line one\nline two",
                "new_string": "LINE 1\nLINE 2"
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // 换行风格保持 CRLF
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "LINE 1\r\nLINE 2\r\nline three\r\n"
        );
    }

    #[tokio::test]
    async fn test_edit_rejects_empty_and_identical() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "abc").unwrap();
        let tool = EditTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        for (old, new) in [("", "x"), ("abc", "abc")] {
            let out = tool
                .execute(serde_json::json!({
                    "path": "a.txt", "old_string": old, "new_string": new, "replace_all": true
                }))
                .await
                .unwrap();
            assert!(out.is_error);
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "abc"
        );
        // 原子写不留临时文件
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("baiji-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    // ===== edits 数组 =====

    #[tokio::test]
    async fn test_edits_array_applies_in_order() {
        let (dir, tool) = setup("alpha\nbeta\ngamma\nbeta\n").await;
        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "edits": [
                    {"old_string": "alpha", "new_string": "temp"},
                    // 后续条目看到前面条目的结果
                    {"old_string": "temp", "new_string": "ALPHA"},
                    {"old_string": "beta", "new_string": "B", "replace_all": true}
                ]
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(read(&dir, "f.rs"), "ALPHA\nB\ngamma\nB\n");
        let first_line = out.content.lines().next().unwrap();
        assert!(first_line.starts_with("Edited "), "{first_line}");
        assert!(
            first_line.ends_with("replaced 4 occurrence(s) across 3 edits"),
            "{first_line}"
        );
    }

    #[tokio::test]
    async fn test_edits_array_failure_rolls_back_everything() {
        let original = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let (dir, tool) = setup(original).await;
        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "edits": [
                    {"old_string": "fn a()", "new_string": "fn a2()"},
                    {"old_string": "fn missing()", "new_string": "fn x()"},
                    {"old_string": "fn", "new_string": "pub fn"},
                    {"old_string": "fn c()", "new_string": "fn c2()"}
                ]
            }))
            .await
            .unwrap();
        assert!(out.is_error);
        // 文件原样未动，错误指明失败条目与原因
        assert_eq!(read(&dir, "f.rs"), original);
        assert!(
            out.content.contains("No changes written"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("2 of 4 edits failed"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("edits[1]: old_string not found"),
            "{}",
            out.content
        );
        assert!(
            out.content
                .contains("edits[2]: old_string matches 3 locations"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("edits[0]:"), "{}", out.content);
        assert!(!out.content.contains("edits[3]:"), "{}", out.content);
    }

    #[tokio::test]
    async fn test_edits_array_rejects_both_forms_and_bad_entries() {
        let original = "abc\n";
        let (dir, tool) = setup(original).await;
        for args in [
            serde_json::json!({
                "path": "f.rs", "old_string": "abc", "new_string": "x",
                "edits": [{"old_string": "abc", "new_string": "y"}]
            }),
            serde_json::json!({
                "path": "f.rs", "new_string": "x",
                "edits": [{"old_string": "abc", "new_string": "y"}]
            }),
            serde_json::json!({
                "path": "f.rs", "replace_all": true,
                "edits": [{"old_string": "abc", "new_string": "y"}]
            }),
            serde_json::json!({"path": "f.rs", "edits": []}),
            serde_json::json!({"path": "f.rs", "edits": "abc"}),
            serde_json::json!({"path": "f.rs", "edits": [{"old_string": "abc"}]}),
            serde_json::json!({"path": "f.rs"}),
        ] {
            let out = tool.execute(args.clone()).await.unwrap();
            assert!(out.is_error, "{args}: {}", out.content);
        }
        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs", "old_string": "abc", "new_string": "x",
                "edits": [{"old_string": "abc", "new_string": "y"}]
            }))
            .await
            .unwrap();
        assert!(out.content.contains("not both"), "{}", out.content);
        let out = tool
            .execute(serde_json::json!({"path": "f.rs", "edits": [{"old_string": "abc"}]}))
            .await
            .unwrap();
        assert!(out.content.contains("edits[0]"), "{}", out.content);
        assert_eq!(read(&dir, "f.rs"), original);
    }

    #[tokio::test]
    async fn test_edits_array_identical_entry_names_index() {
        let (dir, tool) = setup("x = 1\n").await;
        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "edits": [
                    {"old_string": "x", "new_string": "y"},
                    {"old_string": "1", "new_string": "1"}
                ]
            }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content
                .contains("edits[1]: old_string and new_string are identical")
        );
        assert_eq!(read(&dir, "f.rs"), "x = 1\n");
    }

    // ===== 匹配失败提示 =====

    #[test]
    fn test_hint_trailing_whitespace() {
        let content = "fn a() {\n    let x = 1;   \n    let y = 2;\n}\n";
        // 模型丢了文件里的行尾空格
        let hint = mismatch_hint(content, "    let x = 1;\n    let y = 2;").unwrap();
        assert!(hint.contains("trailing whitespace"), "{hint}");
        assert!(hint.contains("at lines 2-3"), "{hint}");
        assert!(hint.contains("no fuzzy match was applied"), "{hint}");

        // 反过来：old_string 多了行尾空白
        let hint = mismatch_hint("let z = 3;\n", "let z = 3;  ").unwrap();
        assert!(
            hint.contains("trailing whitespace") && hint.contains("at line 1"),
            "{hint}"
        );
    }

    #[test]
    fn test_hint_indentation_reports_line_and_actual_indent() {
        let content = "impl A {\n    fn run(&self) {\n        go();\n    }\n}\n";
        // 模型少缩进了一层
        let hint = mismatch_hint(content, "fn run(&self) {\n    go();\n}").unwrap();
        assert!(hint.contains("indentation"), "{hint}");
        assert!(hint.contains("at lines 2-4"), "{hint}");
        assert!(
            hint.contains(
                "file line 2 is indented with 4 spaces but old_string has no indentation"
            ),
            "{hint}"
        );

        // 首行缩进一致、内部某行缩进不同：报告那一行
        let hint = mismatch_hint(content, "    fn run(&self) {\n      go();").unwrap();
        assert!(hint.contains("at lines 2-3"), "{hint}");
        assert!(
            hint.contains("file line 3 is indented with 8 spaces but old_string has 6 spaces"),
            "{hint}"
        );

        // tab 缩进的文件
        let content = "fn main() {\n\tlet v = 1;\n}\n";
        let hint = mismatch_hint(content, "  let v = 1;").unwrap();
        assert!(hint.contains("at line 2"), "{hint}");
        assert!(
            hint.contains("indented with 1 tab but old_string has 2 spaces"),
            "{hint}"
        );
    }

    #[test]
    fn test_hint_near_miss_line() {
        let content = "fn main() {\n    let total = compute(alpha, beta);\n    print(total);\n}\n";
        let hint = mismatch_hint(
            content,
            "    let total = compute(alpha, gamma);\n    print(total);",
        )
        .unwrap();
        assert!(hint.contains("old_string line 1"), "{hint}");
        assert!(hint.contains("closest line is 2"), "{hint}");
        assert!(hint.contains("let total = compute(alpha, beta);"), "{hint}");
        assert!(hint.contains("% similar"), "{hint}");

        // 完全不相干：明说不存在，建议重读
        let hint = mismatch_hint(content, "completely unrelated text here").unwrap();
        assert!(hint.contains("does not appear anywhere"), "{hint}");

        // 每行都在但顺序不对
        let hint = mismatch_hint(
            content,
            "    print(total);\n    let total = compute(alpha, beta);",
        )
        .unwrap();
        assert!(hint.contains("not consecutively"), "{hint}");
        assert!(hint.contains("line 3"), "{hint}");
    }

    #[test]
    fn test_hint_near_miss_long_text_is_clipped() {
        let long = format!("let s = \"{}\";", "x".repeat(400));
        let content = format!("{long}\n");
        let old = format!("let s = \"{}y\";", "x".repeat(399));
        let hint = mismatch_hint(&content, &old).unwrap();
        assert!(hint.contains("closest line is 1"), "{hint}");
        assert!(
            hint.len() < 600,
            "hint must be clipped: {} bytes",
            hint.len()
        );
    }

    #[tokio::test]
    async fn test_not_found_message_carries_hint() {
        let (dir, tool) = setup("fn a() {\n    body();\n}\n").await;
        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "old_string": "fn a() {\nbody();\n}",
                "new_string": "fn a() {}"
            }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.starts_with("[Error] old_string not found"),
            "{}",
            out.content
        );
        assert!(out.content.contains("\nHint: "), "{}", out.content);
        assert!(out.content.contains("at lines 1-3"), "{}", out.content);
        // 绝不静默模糊替换
        assert_eq!(read(&dir, "f.rs"), "fn a() {\n    body();\n}\n");
    }

    // ===== 结果片段 =====

    #[tokio::test]
    async fn test_snippet_line_numbers_match_read() {
        let content: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let (_dir, tool) = setup(&content).await;
        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "old_string": "line 10\n",
                "new_string": "line 10a\nline 10b\n"
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let lines: Vec<&str> = out.content.lines().collect();
        assert!(
            lines[0].ends_with("replaced 1 occurrence(s)"),
            "{}",
            out.content
        );
        // ±3 行：7..=14（新增一行后 line 11 变成第 12 行）
        assert_eq!(lines[1], "7\tline 7");
        assert!(lines.contains(&"10\tline 10a"));
        assert!(lines.contains(&"11\tline 10b"));
        assert_eq!(*lines.last().unwrap(), "14\tline 13");
        assert_eq!(lines.len(), 1 + 8);
    }

    #[tokio::test]
    async fn test_snippet_crlf_file_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("win.txt");
        let content: String = (1..=10).map(|i| format!("row {i}\r\n")).collect();
        std::fs::write(&file, content).unwrap();
        let tool = EditTool::new(Arc::new(ExecutionEnv::new(dir.path())));
        let out = tool
            .execute(serde_json::json!({
                "path": "win.txt",
                "old_string": "row 5\nrow 6",
                "new_string": "ROW 5\nROW 6"
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // 行号与 read（content.lines() 编号）一致，片段内不残留 \r
        assert!(out.content.contains("\n2\trow 2\n"), "{}", out.content);
        assert!(
            out.content.contains("\n5\tROW 5\n6\tROW 6\n"),
            "{}",
            out.content
        );
        assert!(out.content.ends_with("9\trow 9"), "{}", out.content);
        assert!(!out.content.contains('\r'));
    }

    #[tokio::test]
    async fn test_snippet_merges_windows_and_tracks_shifts() {
        let content: String = (1..=30).map(|i| format!("l{i}\n")).collect();
        let (_dir, tool) = setup(&content).await;
        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "edits": [
                    // 后改的条目把前面的行号整体下推：前一处的片段坐标必须跟着平移
                    {"old_string": "l20\n", "new_string": "L20\n"},
                    {"old_string": "l2\n", "new_string": "X\nY\nZ\n"},
                    {"old_string": "l4\n", "new_string": "L4\n"}
                ]
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let body: Vec<&str> = out.content.lines().skip(1).collect();
        // l2 → 三行（第 2-4 行），l4 现为第 6 行：两窗口重叠合并为 1..=9
        assert_eq!(body[0], "1\tl1");
        assert!(body.contains(&"2\tX") && body.contains(&"4\tZ"));
        assert!(body.contains(&"6\tL4"));
        assert!(body.contains(&"9\tl7"));
        // l20 现为第 22 行，单独窗口 19..=25，中间以 … 分隔
        let sep = body.iter().position(|l| *l == "…").unwrap();
        assert_eq!(body[sep - 1], "9\tl7");
        assert_eq!(body[sep + 1], "19\tl17");
        assert!(body.contains(&"22\tL20"));
        assert_eq!(*body.last().unwrap(), "25\tl23");
    }

    #[tokio::test]
    async fn test_snippet_caps_and_counts_elided_changes() {
        let content: String = (1..=200).map(|i| format!("v{i} = old\n")).collect();
        let (_dir, tool) = setup(&content).await;
        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "old_string": "= old",
                "new_string": "= new",
                "replace_all": true
            }))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("replaced 200 occurrence(s)"));
        let body_lines = out
            .content
            .lines()
            .skip(1)
            .filter(|l| l.contains('\t'))
            .count();
        assert!(body_lines <= MAX_SNIPPET_LINES, "{body_lines}");
        assert!(
            out.content.contains("more change(s) not shown"),
            "{}",
            out.content
        );
        // 200 处改动全部相邻 → 一个窗口；只展示了窗口头部
        assert!(
            out.content.contains("[… 160 more lines]"),
            "{}",
            out.content
        );
    }

    #[test]
    fn test_snippet_long_lines_are_clipped() {
        let content = format!("short\n{}\nend\n", "z".repeat(1000));
        let snippet = render_snippet(&content, &[(0, 5)]);
        assert!(snippet.contains("… [+760 chars]"), "{snippet}");
        assert!(snippet.len() < 400);
    }

    #[test]
    fn test_replace_at_region_remap() {
        // 先在 "aaXbb" 中把 X→XYZ（区域 2..5），再把 b→BB（两处）：旧区域不动，新区域登记
        let mut regions = Vec::new();
        let s = replace_at("aaXbb", &[2], 1, "XYZ", &mut regions);
        assert_eq!(s, "aaXYZbb");
        assert_eq!(regions, vec![(2, 5)]);
        let s = replace_at(&s, &[5, 6], 1, "BB", &mut regions);
        assert_eq!(s, "aaXYZBBBB");
        assert_eq!(regions, vec![(2, 5), (5, 7), (7, 9)]);
        // 前方替换把旧区域整体后推
        let s = replace_at(&s, &[0], 2, "", &mut regions);
        assert_eq!(s, "XYZBBBB");
        assert_eq!(regions, vec![(0, 3), (3, 5), (5, 7), (0, 0)]);
    }
}
