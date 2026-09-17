//! read 工具：多模式读取（JIT disclosure）
//!
//! - `full`（默认）：带行号完整内容（offset/limit 分页）
//! - `signatures`：符号大纲（函数/类型/类声明 + 行号区间），LLM 先看结构
//!   再用 `lines`/offset+limit 精准展开目标区域
//! - `map`：目录紧凑树（带文件大小）
//!
//! 截断的完整内容 spill 到 ctx store，可用 expand 工具取回。

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::fs;

use crate::env::ExecutionEnv;

/// signatures 视图单文件最多输出的符号数
const MAX_SIGNATURES: usize = 200;
/// map 视图最多列出的条目数
const MAX_MAP_ENTRIES: usize = 300;
/// density 下限（防止把文件压到没法看）
const MIN_DENSITY: f32 = 0.05;

pub struct ReadTool {
    env: Arc<ExecutionEnv>,
}

impl ReadTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }

    async fn read_full(
        &self,
        resolved: &std::path::Path,
        offset: usize,
        limit: Option<usize>,
        density: Option<f32>,
    ) -> ToolOutput {
        match fs::metadata(resolved).await {
            Ok(meta) if meta.len() > self.env.max_file_size => {
                return ToolOutput::err(format!(
                    "[Error] file '{}' is too large ({} bytes, max {}). \
                     Try mode=signatures first, then read line ranges.",
                    resolved.display(),
                    meta.len(),
                    self.env.max_file_size
                ))
            }
            Err(e) => {
                return ToolOutput::err(format!("[Error] reading '{}': {e}", resolved.display()))
            }
            _ => {}
        }

        let content = match fs::read_to_string(resolved).await {
            Ok(content) => content,
            Err(e) => {
                return ToolOutput::err(format!(
                    "[Error] reading '{}' (binary or unreadable): {e}",
                    resolved.display()
                ))
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let start = (offset - 1).min(lines.len());
        let end = limit
            .map(|l| start.saturating_add(l).min(lines.len()))
            .unwrap_or(lines.len());
        let slice = &lines[start..end];
        if slice.is_empty() {
            return ToolOutput::ok(format!(
                "[Empty] '{}' has no content in the requested range",
                resolved.display()
            ));
        }

        // 显式 density 参数：按信息熵保留 ratio 比例的行
        if let Some(ratio) = density.filter(|r| *r < 1.0) {
            let (view, kept) = density_view(slice, start, ratio);
            let header = format!(
                "[density {:.0}% — kept {}/{} lines by entropy; skipped ranges marked]",
                ratio * 100.0,
                kept,
                slice.len()
            );
            let body = format!("{header}\n{view}");
            return self.deliver(body, slice);
        }

        // 预算降级：预计超限且未显式指定 density → 自动按预算比例熵选行
        let estimated: usize = slice.iter().map(|l| l.len() + 8).sum();
        if self.env.density_fallback && estimated > self.env.max_output_bytes {
            let ratio =
                (self.env.max_output_bytes as f32 / estimated as f32).clamp(MIN_DENSITY, 0.95);
            let (view, kept) = density_view(slice, start, ratio);
            let header = format!(
                "[auto-density {:.0}% — {}/{} lines kept by entropy to fit the {} byte budget; \
                 re-read with offset/limit or mode=signatures for other views]",
                ratio * 100.0,
                kept,
                slice.len(),
                self.env.max_output_bytes
            );
            let body = format!("{header}\n{view}");
            return self.deliver(body, slice);
        }

        let numbered = slice
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}\t{}", start + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        self.deliver(numbered, slice)
    }

    /// 输出落账（截断保护 + 台账）
    fn deliver(&self, text: String, slice: &[&str]) -> ToolOutput {
        let original_estimate: u64 = slice.iter().map(|l| l.len() + 8).sum::<usize>() as u64;
        let (delivered, truncated_at) = self.env.truncate_with_meta(&text);
        let original = truncated_at.unwrap_or(original_estimate);
        if (delivered.len() as u64) < original {
            ToolOutput::ok(delivered).with_original_bytes(original)
        } else {
            ToolOutput::ok(delivered)
        }
    }

    /// 符号大纲：正则匹配常见语言的声明行（Rust/TS/JS/Go/Python/Java/C 系）
    fn read_signatures(&self, resolved: &std::path::Path, content: &str) -> ToolOutput {
        // AST 优先（带精确行区间），未知语言/解析失败自动回退正则
        let symbols = crate::signatures::outline(content, crate::signatures::Lang::detect(resolved));
        let symbols: Vec<crate::signatures::Symbol> =
            symbols.into_iter().take(MAX_SIGNATURES).collect();

        if symbols.is_empty() {
            return ToolOutput::ok(format!(
                "[No symbols detected in '{}' — fall back to mode=full]",
                resolved.display()
            ));
        }

        let span_width = symbols
            .iter()
            .map(|s| format!("L{}-{}", s.line_start, s.line_end).len())
            .max()
            .unwrap_or(6);
        let mut out: Vec<String> = symbols
            .iter()
            .map(|s| {
                let span = format!("L{}-{}", s.line_start, s.line_end);
                let label = if s.name.is_empty() {
                    String::new()
                } else {
                    format!("{} {}", s.kind, s.name)
                };
                format!("{span:<width$}  {label}  {}", s.signature, width = span_width)
            })
            .collect();
        let header = format!(
            "== signatures of {} ({} lines, {} symbols; read spans with offset/limit) ==",
            resolved.display(),
            content.lines().count(),
            symbols.len()
        );
        out.insert(0, header);
        ToolOutput::ok(out.join("\n"))
    }

    /// 目录紧凑树
    fn read_map(&self, root: &std::path::Path, content: &str) -> ToolOutput {
        // map 作用对象是文件：返回单行摘要 + signatures 提示
        let lines = content.lines().count();
        ToolOutput::ok(format!(
            "'{}' is a file ({} bytes, {} lines). Use mode=signatures for its outline.",
            root.display(),
            content.len(),
            lines
        ))
    }

    fn map_dir(&self, root: &std::path::Path) -> ToolOutput {
        let mut entries = Vec::new();
        walk_map(root, "", 0, self.env.max_search_depth, &mut entries);
        if entries.is_empty() {
            return ToolOutput::ok(format!("(empty directory '{}')", root.display()));
        }
        let header = format!("== map of {} ==", root.display());
        let mut lines = vec![header];
        lines.extend(entries.iter().take(MAX_MAP_ENTRIES).cloned());
        if entries.len() > MAX_MAP_ENTRIES {
            lines.push(format!(
                "[... {} more entries omitted — use find for targeted search]",
                entries.len() - MAX_MAP_ENTRIES
            ));
        }
        ToolOutput::ok(lines.join("\n"))
    }
}

/// density 视图：按行信息熵选行，保留原顺序与行号，跳过的连续段
/// 以 `[La-Lb skipped]` 标记（行号锚点不丢，可继续 offset/limit 精读）。
/// 返回（渲染文本, 保留行数）。确定性：熵相同时取更早的行。
pub fn density_view(lines: &[&str], start_index: usize, keep_ratio: f32) -> (String, usize) {
    let total = lines.len();
    let keep = (((total as f32) * keep_ratio.clamp(MIN_DENSITY, 1.0)).ceil() as usize)
        .min(total)
        .max(1);
    if keep >= total {
        return (
            lines
                .iter()
                .enumerate()
                .map(|(i, l)| format!("{}\t{}", start_index + i + 1, l))
                .collect::<Vec<_>>()
                .join("\n"),
            total,
        );
    }

    // 按熵降序选前 keep 行（total_cmp 全序 + 索引决胜，保证确定性与排序一致性）
    let entropies: Vec<f64> = lines.iter().map(|l| line_entropy(l)).collect();
    let mut ranked: Vec<usize> = (0..total).collect();
    ranked.sort_by(|&a, &b| {
        entropies[b]
            .total_cmp(&entropies[a])
            .then(a.cmp(&b))
    });
    ranked.truncate(keep);
    ranked.sort_unstable();

    let mut out = Vec::new();
    let mut prev: Option<usize> = None;
    for &i in &ranked {
        match prev {
            Some(p) if i > p + 1 => out.push(format!("[L{}-L{} skipped]", start_index + p + 2, start_index + i)),
            None if i > 0 => out.push(format!("[L{}-L{} skipped]", start_index + 1, start_index + i)),
            _ => {}
        }
        out.push(format!("{}\t{}", start_index + i + 1, lines[i]));
        prev = Some(i);
    }
    if let Some(p) = prev {
        if p + 1 < total {
            out.push(format!("[L{}-L{} skipped]", start_index + p + 2, start_index + total));
        }
    }
    (out.join("\n"), keep)
}

/// 单行香农熵（字符分布多样性）：空行 0，重复字符行低，代码行高
fn line_entropy(line: &str) -> f64 {
    let n = line.chars().count();
    if n == 0 {
        return 0.0;
    }
    // BTreeMap：固定的遍历（=浮点求和）顺序。HashMap 的随机迭代顺序会让相同的行
    // 算出末位不同的熵，破坏 density_view 的确定性
    let mut freq: BTreeMap<char, u32> = BTreeMap::new();
    for c in line.chars() {
        *freq.entry(c).or_insert(0) += 1;
    }
    freq.values()
        .map(|&k| {
            let p = k as f64 / n as f64;
            -p * p.log2()
        })
        .sum()
}

/// 声明行判定（保守：宁可多报一行也不漏符号）
pub(crate) fn is_signature_line(line: &str) -> bool {
    let t = line.trim_start();
    let indent_depth = line.len() - t.len();

    // Rust
    if t.starts_with("pub ") || t.starts_with("fn ") || t.starts_with("async fn ") {
        return t.contains("fn ")
            || t.starts_with("pub struct")
            || t.starts_with("pub enum")
            || t.starts_with("pub trait")
            || t.starts_with("pub mod")
            || t.starts_with("pub const")
            || t.starts_with("pub static");
    }
    if t.starts_with("struct ") || t.starts_with("enum ") || t.starts_with("trait ")
        || t.starts_with("impl ") || t.starts_with("mod ") || t.starts_with("macro_rules!")
    {
        return true;
    }
    // TS/JS
    if t.starts_with("export ") || t.starts_with("function ") || t.starts_with("class ")
        || t.starts_with("interface ") || t.starts_with("type ")
    {
        return t.contains("function")
            || t.contains("class")
            || t.contains("interface")
            || t.contains("=>")
            || t.contains("type ")
            || t.ends_with('{');
    }
    // Go
    if t.starts_with("func ") || t.starts_with("type ") && t.contains(" struct")
        || t.starts_with("type ") && t.contains(" interface")
    {
        return true;
    }
    // Python / Java / C 系
    if t.starts_with("def ") || t.starts_with("class ") || t.starts_with("async def ") {
        return true;
    }
    // 缩进的 impl 块内方法（Rust/Java/C++）：8 空格内的 fn/def/pubic 等
    if indent_depth > 0 && indent_depth <= 8 {
        let short = t.trim();
        if short.starts_with("pub fn ") || short.starts_with("fn ") || short.starts_with("def ") {
            return true;
        }
    }
    false
}

/// 目录树遍历（跳过隐藏目录与构建产物）
fn walk_map(
    dir: &std::path::Path,
    prefix: &str,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<String>,
) {
    if depth >= max_depth || out.len() >= MAX_MAP_ENTRIES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| (e.file_type().map(|t| t.is_file()).unwrap_or(true), e.file_name()));
    for entry in items {
        if out.len() >= MAX_MAP_ENTRIES {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        // file_type 不跟随符号链接：链接目录不下钻（可能指向白名单外）
        let file_type = entry.file_type().ok();
        if file_type.is_some_and(|t| t.is_symlink()) {
            out.push(format!("{prefix}{name} -> (symlink)"));
            continue;
        }
        if path.is_dir() {
            if name.starts_with('.')
                || matches!(name.as_str(), "target" | "node_modules" | "__pycache__" | ".venv")
            {
                continue;
            }
            out.push(format!("{prefix}{name}/"));
            walk_map(&path, &format!("{prefix}  "), depth + 1, max_depth, out);
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(format!("{prefix}{name} ({size}B)"));
        }
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read files or directory structure. Modes: 'signatures' (symbol outline with line \
         numbers — best first look), 'map' (compact directory tree), 'full' (numbered lines, \
         default). 'density' (0.05-1.0) keeps the highest-entropy lines in full mode. \
         Workflow: signatures first, then read the exact line range with offset/limit. \
         Truncated output can be recovered via the expand tool."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path (or directory for mode=map)"},
                "mode": {"type": "string", "enum": ["full", "signatures", "map"], "description": "Read mode (default full)"},
                "offset": {"type": "integer", "description": "Starting line number (1-based, full mode)"},
                "limit": {"type": "integer", "description": "Maximum lines to return (full mode)"},
                "density": {"type": "number", "description": "Fraction of lines to keep, selected by information entropy (full mode, e.g. 0.4)"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(path) = args["path"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'path'"));
        };
        let mode = args["mode"].as_str().unwrap_or("full");
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().map(|l| l as usize);
        let density = args["density"].as_f64().map(|d| d.clamp(0.0, 1.0) as f32);

        let resolved = match self.env.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        let is_dir = resolved.is_dir();

        match mode {
            "map" => {
                if is_dir {
                    Ok(self.map_dir(&resolved))
                } else {
                    match fs::read_to_string(&resolved).await {
                        Ok(content) => Ok(self.read_map(&resolved, &content)),
                        Err(e) => Ok(ToolOutput::err(format!(
                            "[Error] reading '{}': {e}",
                            resolved.display()
                        ))),
                    }
                }
            }
            "signatures" => {
                if is_dir {
                    return Ok(ToolOutput::err(format!(
                        "[Error] '{}' is a directory; use mode=map for directories",
                        resolved.display()
                    )));
                }
                match fs::metadata(&resolved).await {
                    Ok(meta) if meta.len() > self.env.max_file_size => {
                        return Ok(ToolOutput::err(format!(
                            "[Error] file too large ({} bytes) — use grep to locate regions",
                            meta.len()
                        )));
                    }
                    Err(e) => {
                        return Ok(ToolOutput::err(format!(
                            "[Error] reading '{}': {e}",
                            resolved.display()
                        )))
                    }
                    _ => {}
                }
                match fs::read_to_string(&resolved).await {
                    Ok(content) => Ok(self.read_signatures(&resolved, &content)),
                    Err(e) => Ok(ToolOutput::err(format!(
                        "[Error] reading '{}': {e}",
                        resolved.display()
                    ))),
                }
            }
            _ => {
                if is_dir {
                    return Ok(ToolOutput::err(format!(
                        "[Error] '{}' is a directory; use mode=map",
                        resolved.display()
                    )));
                }
                Ok(self.read_full(&resolved, offset, limit, density).await)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> (tempfile::TempDir, ReadTool) {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadTool::new(Arc::new(ExecutionEnv::new(dir.path())));
        (dir, tool)
    }

    #[tokio::test]
    async fn test_read_full_with_line_numbers_and_range() {
        let (dir, tool) = setup().await;
        let file = dir.path().join("a.txt");
        fs::write(&file, "one\ntwo\nthree\n").await.unwrap();

        let out = tool
            .execute(serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert!(out.content.contains("1\tone"));
        assert!(out.content.contains("3\tthree"));

        let out = tool
            .execute(serde_json::json!({"path": "a.txt", "offset": 2, "limit": 1}))
            .await
            .unwrap();
        assert_eq!(out.content, "2\ttwo");
    }

    #[tokio::test]
    async fn test_read_signatures_mode() {
        let (dir, tool) = setup().await;
        let file = dir.path().join("lib.rs");
        let code = "// header comment\nfn helper() {}\n\npub struct Config {\n    x: u32,\n}\n\nimpl Config {\n    pub fn new() -> Self { Self { x: 1 } }\n}\n\nasync fn main() {}\n";
        fs::write(&file, code).await.unwrap();

        let out = tool
            .execute(serde_json::json!({"path": "lib.rs", "mode": "signatures"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        // 每个符号带行区间锚点 + kind + 名称
        assert!(out.content.contains("L2-2"), "{}", out.content);
        assert!(out.content.contains("fn helper"));
        assert!(out.content.contains("struct Config"));
        assert!(out.content.contains("impl Config"));
        assert!(out.content.contains("fn new"));
        // 行区间完整（impl 块区间）
        assert!(out.content.contains("L8-10"), "impl spans to its closing brace: {}", out.content);
        // 注释行不是符号
        assert!(!out.content.contains("header comment"));
    }

    #[tokio::test]
    async fn test_read_map_mode() {
        let (dir, tool) = setup().await;
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/nested/util.rs"), "fn x() {}").unwrap();
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap(); // 构建产物跳过

        let out = tool
            .execute(serde_json::json!({"path": ".", "mode": "map"}))
            .await
            .unwrap();
        assert!(out.content.contains("src/"));
        assert!(out.content.contains("nested/"));
        assert!(out.content.contains("util.rs"));
        assert!(out.content.contains("README.md"));
        assert!(!out.content.contains("target/"));
    }

    #[tokio::test]
    async fn test_read_mode_mismatch_errors() {
        let (dir, tool) = setup().await;
        std::fs::create_dir_all(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();

        // signatures 用于目录 → 提示用 map
        let out = tool
            .execute(serde_json::json!({"path": "d", "mode": "signatures"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("mode=map"));

        // full 用于目录 → 提示用 map
        let out = tool
            .execute(serde_json::json!({"path": "d"}))
            .await
            .unwrap();
        assert!(out.is_error);

        // map 用于文件 → 返回摘要提示
        let out = tool
            .execute(serde_json::json!({"path": "f.txt", "mode": "map"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("is a file"));
    }

    #[tokio::test]
    async fn test_read_over_budget_reports_original_bytes() {
        let (dir, tool) = setup().await;
        let big: String = "0123456789\n".repeat(5000); // 55KB
        let file = dir.path().join("big.txt");
        fs::write(&file, &big).await.unwrap();
        let out = tool
            .execute(serde_json::json!({"path": "big.txt"}))
            .await
            .unwrap();
        // 默认 density_fallback 开启：超预算走自动熵降级（不再硬截断）
        assert!(
            out.content.contains("[auto-density"),
            "got: {}",
            out.content.lines().next().unwrap()
        );
        assert!(out.original_bytes.is_some());
        assert!(out.bytes_saved() > 0);
    }

    #[tokio::test]
    async fn test_read_missing_and_policy() {
        let (_dir, tool) = setup().await;

        let out = tool
            .execute(serde_json::json!({"path": "missing.txt"}))
            .await
            .unwrap();
        assert!(out.is_error);

        let out = tool
            .execute(serde_json::json!({"path": "../../etc/passwd"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("[Policy denied]"));
    }

    #[test]
    fn test_signature_detection_languages() {
        assert!(is_signature_line("fn a() {}"));
        assert!(is_signature_line("    pub fn inner() {}"));
        assert!(is_signature_line("export function handler(req: Request) {"));
        assert!(is_signature_line("export interface User {"));
        assert!(is_signature_line("func main() {"));
        assert!(is_signature_line("def load(path):"));
        assert!(is_signature_line("class Service:"));
        assert!(!is_signature_line("let x = 1;"));
        assert!(!is_signature_line("// fn fake() {}"));
        assert!(!is_signature_line("println!(\"fn not a decl\")"));
    }

    #[test]
    fn test_line_entropy_ranking() {
        assert_eq!(line_entropy(""), 0.0);
        assert_eq!(line_entropy("          "), 0.0); // 单一字符
        // 多样字符的代码行熵远高于重复行
        assert!(line_entropy("fn main() { println!(x); }") > line_entropy("aaaaaaaa"));
        assert!(line_entropy("use std::collections::HashMap;") > line_entropy("aaaaaaa"));
    }

    #[test]
    fn test_density_view_selects_and_marks_gaps() {
        // 10 行：交替空行与代码行
        let lines: Vec<&str> = vec![
            "",                          // L1 熵 0 → 被丢
            "fn alpha() -> Config {",   // L2 保留
            "",                          // L3 丢
            "let x = compute(a, b, c)?;", // L4 保留
            "",                          // L5 丢
            "}",                         // L6 短但多样 → 可能保留
            "// note",                   // L7 中等
            "",                          // L8 丢
            "pub async fn beta() {}",    // L9 保留
            "",                          // L10 丢
        ];

        let (view, kept) = density_view(&lines, 0, 0.4);
        assert_eq!(kept, 4, "40% of 10 lines");
        // 行号保持原始编号
        assert!(view.contains("2\tfn alpha()"));
        assert!(view.contains("4\tlet x = compute"));
        // 跳过段有标记（行号锚点可继续精读）
        assert!(view.contains("[L1-L1 skipped]"), "{view}");
        assert!(view.contains("[L10-L10 skipped]"), "{view}");
        // 输出 = 保留行 + 至多 kept+1 个跳段标记
        assert!(view.lines().count() <= 2 * kept + 1);
    }

    #[test]
    fn test_density_view_deterministic() {
        let lines: Vec<&str> = (0..20).map(|i| {
            if i % 2 == 0 { "let v = value_function(x)?" } else { "" }
        }).collect();
        let (a, _) = density_view(&lines, 0, 0.3);
        let (b, _) = density_view(&lines, 0, 0.3);
        assert_eq!(a, b, "same input must produce same output");
    }

    #[tokio::test]
    async fn test_read_explicit_density_and_auto_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(dir.path())
            .with_ctx_store(dir.path().join("ctx"))
            .with_max_output_bytes(8 * 1024); // 显式小预算，确定触发降级
        let tool = ReadTool::new(Arc::new(env));

        // 显式 density：无论预算都按比例熵选行
        let content = (0..100)
            .map(|i| format!("line {i:03}: let value_{i} = compute(input_{i}, factor)?;\n"))
            .collect::<String>();
        let file = dir.path().join("mid.rs");
        fs::write(&file, &content).await.unwrap();

        let out = tool
            .execute(serde_json::json!({"path": "mid.rs", "density": 0.1}))
            .await
            .unwrap();
        assert!(
            out.content.contains("[density 10%"),
            "{}",
            out.content.lines().next().unwrap()
        );
        assert!(out.original_bytes.is_some());
        assert!(out.bytes_saved() > 0);

        // 超预算未指定 density → 自动降级（不硬截断）
        let content = (0..200)
            .map(|i| format!("line {i:03}: let value_{i} = compute(input_{i}, factor, modifier_{i}, extra_padding)?;\n"))
            .collect::<String>();
        let file = dir.path().join("big.rs");
        fs::write(&file, &content).await.unwrap();

        let out = tool
            .execute(serde_json::json!({"path": "big.rs"}))
            .await
            .unwrap();
        assert!(
            out.content.contains("[auto-density"),
            "should auto-degrade, got: {}",
            out.content.lines().next().unwrap()
        );
        // 密度选行优先；若仍略超预算，CCR 截断兜底（两者可共存）
        assert!(out.original_bytes.is_some());
    }

    #[tokio::test]
    async fn test_read_auto_fallback_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(dir.path())
            .without_density_fallback()
            .with_ctx_store(dir.path().join("ctx"))
            .with_max_output_bytes(8 * 1024);
        let tool = ReadTool::new(Arc::new(env));

        let content = (0..200)
            .map(|i| format!("line {i:03}: let value_{i} = compute(input_{i}, factor, modifier_{i}, extra_padding)?;\n"))
            .collect::<String>();
        let file = dir.path().join("big.rs");
        fs::write(&file, &content).await.unwrap();

        let out = tool
            .execute(serde_json::json!({"path": "big.rs"}))
            .await
            .unwrap();
        // 关闭降级 → 回到 CCR 硬截断（带句柄）
        assert!(!out.content.contains("[auto-density"));
        assert!(
            out.content.contains("full content handle"),
            "expected CCR handle, got: {}",
            out.content.lines().last().unwrap()
        );
    }
}
