//! grep 工具：递归正则搜索文件内容
//!
//! 结构化输出（默认；`structure: false` 恢复逐行全量输出）：
//! - 代码文件（signatures 支持的语言）：每文件只解析一次 tree-sitter 大纲，匹配按
//!   **最内层所属符号**分组——`fn foo @ L10-40` 头（可直接 read offset/limit）+
//!   `行号: 文本` 行；符号外的匹配归入 `(top level)`；命中 ≥3 处时附
//!   `symbols: N total, M matched` 与至多 4 个未命中的同级符号名。匹配密集的文件限制分组数，过密则回退扁平行。
//! - 非代码文件（json/yaml/toml/md/txt/lock）：每文件至多 3 条匹配 + `(+N more)`。
//! - 超长行围绕匹配截到 ~240 字符。
//!
//! 符号区间来自 tree-sitter AST 大纲（signatures.rs），不用正则猜测。

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput, estimate_text_tokens};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::compressors::search_results;
use crate::env::ExecutionEnv;
use crate::signatures::{self, Lang, Symbol};

/// 最多返回的匹配行数
const MAX_MATCHES: usize = 200;
/// 匹配数达到此值的文件视为"繁忙"：限制分组数、省略 other 列表
const BUSY_FILE_MATCHES: usize = 12;
/// 繁忙文件最多展示的分组数（含合并的溢出组）
const BUSY_MAX_GROUPS: usize = 8;
/// 匹配数超过此值的文件不做结构化（不解析，回退扁平行）
const STRUCTURE_MAX_MATCHES: usize = 24;
/// other 列表最多列出的未命中同级符号数
const MAX_OTHER_SYMBOLS: usize = 4;
/// 匹配数达到此值才输出 `symbols: … other: …` 概览行：命中 1–2 处时概览信息量低，
/// 却让输出膨胀（每文件 +100 字节左右），符号头 `fn x @ Lm-n` 已足够定位
const SUMMARY_MIN_MATCHES: usize = 3;
/// 非代码文件每文件最多展示的匹配行
const NON_CODE_MAX_MATCHES: usize = 3;
/// 匹配行最大展示字符数（超长行围绕匹配截取）
const MAX_LINE_CHARS: usize = 240;
/// 截取时匹配前保留的字符数
const LINE_PREFIX_CONTEXT_CHARS: usize = 80;
/// 非代码文件扩展名（匹配多为数据/文档噪声，封顶展示）
const NON_CODE_EXTENSIONS: &[&str] = &[
    "json", "jsonl", "yaml", "yml", "toml", "md", "markdown", "txt", "lock",
];

/// other 列表的字符预算（名字很长时少列几个）
const OTHER_SYMBOLS_MAX_CHARS: usize = 60;
/// 文档注释/属性行向下寻找所属符号的最大距离
const DECORATION_MAX_LINES: usize = 30;

/// 一个文件的匹配
struct FileHits {
    path: PathBuf,
    /// (1-based 行号, 行文本)
    hits: Vec<(usize, String)>,
    /// 结构信息：仅结构化模式、受支持语言、匹配数不超过阈值且有具名符号时才有
    structure: Option<Structure>,
}

/// 代码文件的结构（search_file 中内容尚在内存时一次算好）
struct Structure {
    symbols: Vec<Symbol>,
    /// 每个符号的父符号下标
    parents: Vec<Option<usize>>,
    /// 每条匹配的归属符号（None = top level）
    owners: Vec<Option<usize>>,
}

/// 一次搜索的结果与"结果不完整"的原因（必须如实告诉模型，否则它会把部分结果当全部）
#[derive(Default)]
struct SearchOutcome {
    files: Vec<FileHits>,
    /// 已收集的匹配总数
    total: usize,
    /// 达到 MAX_MATCHES 上限后提前停止
    capped: bool,
    /// 因超过 max_file_size 被跳过的文件数
    skipped_large: usize,
}

/// 渲染结果
struct Rendered {
    text: String,
    /// 有展示省略（非代码封顶/长行截取）时：完整扁平输出的 (字节, token)，供台账
    full: Option<(u64, u64)>,
}

pub struct GrepTool {
    env: Arc<ExecutionEnv>,
}

impl GrepTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regex, recursively from a path (default '.'). \
         Respects .gitignore. Optional glob filter (e.g. '*.rs', '*.{ts,tsx}', 'src/**/*.rs'). \
         Rows are 'path:line: text' (or 'line: text' under a 'File: path' header). In code \
         files (rs/py/js/ts/go) matches are grouped under their innermost enclosing symbol, \
         e.g. 'fn foo @ L10-40' — read that span with offset/limit — plus a 'symbols:' summary \
         line; non-code files (json/yaml/toml/md/txt/lock) show at most 3 matches each and \
         long lines are cut around the match. structure=false returns every match as full \
         plain rows."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for"},
                "path": {"type": "string", "description": "File or directory to search (default '.')"},
                "glob": {"type": "string", "description": "Glob filter, e.g. '*.rs', '*.{ts,tsx}', 'src/**/*.rs' (optional)"},
                "structure": {"type": "boolean", "description": "Group code matches by enclosing symbol, cap non-code files at 3 matches, cut long lines (default true). false = every match as a full plain row"}
            },
            "required": ["pattern"]
        })
    }

    /// 只读、无共享可变状态：可与其它只读工具同轮并发
    fn parallel(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(pattern) = args["pattern"].as_str() else {
            return Ok(ToolOutput::err(
                "[Error] missing required argument 'pattern'",
            ));
        };
        let path = args["path"].as_str().unwrap_or(".");
        let glob = args["glob"].as_str().map(str::to_string);
        let structure = args["structure"].as_bool().unwrap_or(true);

        let regex = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "[Error] invalid regex '{pattern}': {e}"
                )));
            }
        };

        let root: PathBuf = match self.env.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        let env = self.env.clone();
        let search_root = root.clone();
        let search_regex = regex.clone();
        let result = tokio::task::spawn_blocking(move || {
            let outcome = search(
                &search_root,
                &search_regex,
                glob.as_deref(),
                &env,
                structure,
            )?;
            let rendered = render(&outcome, &search_regex, structure, search_root.is_file());
            Ok::<_, String>((outcome, rendered))
        })
        .await
        .map_err(|e| anyhow::anyhow!("grep task failed: {e}"))?;
        let (outcome, rendered) = match result {
            Ok(result) => result,
            Err(e) => return Ok(ToolOutput::err(format!("[Error] {e}"))),
        };

        let mut notes = Vec::new();
        if outcome.capped {
            notes.push(format!(
                "[Results capped at {MAX_MATCHES} matches — there are more. Narrow the pattern, path or glob.]"
            ));
        }
        if outcome.skipped_large > 0 {
            notes.push(format!(
                "[{} file(s) larger than {} bytes were NOT searched]",
                outcome.skipped_large, self.env.max_file_size
            ));
        }

        if outcome.files.is_empty() {
            let mut message = format!(
                "No matches found for pattern '{regex}' in '{}'",
                root.display()
            );
            for note in &notes {
                message.push('\n');
                message.push_str(note);
            }
            return Ok(ToolOutput::ok(message));
        }
        let (mut delivered, mut original, mut original_tokens) =
            self.env.truncate_with_meta(&rendered.text);
        // 展示省略：台账按完整扁平输出计原始量
        if let Some((bytes, tokens)) = rendered.full
            && bytes > original.unwrap_or(delivered.len() as u64)
        {
            original = Some(bytes);
            original_tokens = Some(tokens);
        }
        // 提示放在截断之后追加，保证一定可见
        for note in &notes {
            delivered.push('\n');
            delivered.push_str(note);
        }
        let mut out = ToolOutput::ok(delivered);
        if let Some(bytes) = original {
            out = out.with_original_bytes(bytes);
        }
        if let Some(tokens) = original_tokens {
            out = out.with_original_tokens(tokens);
        }
        Ok(out)
    }
}

fn search(
    root: &Path,
    regex: &Regex,
    glob: Option<&str>,
    env: &ExecutionEnv,
    structure: bool,
) -> std::result::Result<SearchOutcome, String> {
    let mut outcome = SearchOutcome::default();

    // 单文件：直接搜（不受 glob / gitignore 影响——用户点名要搜它）
    if root.is_file() {
        search_file(root, regex, env, structure, &mut outcome);
        return Ok(outcome);
    }

    for entry in crate::walk::walk(root, env, glob)? {
        if outcome.capped {
            break;
        }
        if entry.path().is_file() {
            search_file(entry.path(), regex, env, structure, &mut outcome);
        }
    }
    Ok(outcome)
}

fn search_file(
    path: &Path,
    regex: &Regex,
    env: &ExecutionEnv,
    structure: bool,
    outcome: &mut SearchOutcome,
) {
    if let Ok(meta) = path.metadata()
        && meta.len() > env.max_file_size
    {
        outcome.skipped_large += 1;
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return; // 二进制或不可读
    };
    let mut hits = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if regex.is_match(line) {
            if outcome.total >= MAX_MATCHES {
                outcome.capped = true;
                break;
            }
            outcome.total += 1;
            hits.push((i + 1, line.to_string()));
        }
    }
    if hits.is_empty() {
        return;
    }
    // 大纲每文件只算一次（内容已在内存）；过密文件不解析
    let structure = if structure && hits.len() <= STRUCTURE_MAX_MATCHES {
        Lang::detect(path).and_then(|lang| build_structure(&content, lang, &hits))
    } else {
        None
    };
    outcome.files.push(FileHits {
        path: path.to_path_buf(),
        hits,
        structure,
    });
}

fn build_structure(content: &str, lang: Lang, hits: &[(usize, String)]) -> Option<Structure> {
    let symbols: Vec<Symbol> = signatures::outline(content, Some(lang))
        .into_iter()
        // 正则回退的 decl 无名字、无区间，无法归组
        .filter(|s| s.kind != "decl" && !s.name.is_empty())
        .collect();
    if symbols.is_empty() {
        return None;
    }
    let parents = parent_indices(&symbols);
    let lines: Vec<&str> = content.lines().collect();
    let owners = hits
        .iter()
        .map(|(line, _)| owner_of(&symbols, &parents, &lines, *line))
        .collect();
    Some(Structure {
        symbols,
        parents,
        owners,
    })
}

/// 匹配行的归属符号：最内层包含者；文档注释/属性/装饰器行（AST 里是兄弟节点）
/// 归给紧随其后、同一容器内的符号
fn owner_of(
    symbols: &[Symbol],
    parents: &[Option<usize>],
    lines: &[&str],
    line: usize,
) -> Option<usize> {
    let inner = innermost(symbols, line);
    if !lines.get(line - 1).is_some_and(|l| is_decoration(l)) {
        return inner;
    }
    let next = symbols.iter().position(|s| s.line_start > line)?;
    let start = symbols[next].line_start;
    let attached = parents[next] == inner
        && start - line <= DECORATION_MAX_LINES
        && lines
            .get(line..start - 1)
            .is_some_and(|between| between.iter().all(|l| is_decoration(l)));
    if attached { Some(next) } else { inner }
}

/// 注释/属性/装饰器行（`//!` 与 `#![` 是所在模块的内部文档/属性，不向下归属）
fn is_decoration(line: &str) -> bool {
    let t = line.trim_start();
    !t.is_empty()
        && !t.starts_with("//!")
        && !t.starts_with("#!")
        && ["//", "/*", "*", "#", "@"].iter().any(|p| t.starts_with(p))
}

// ========== 渲染 ==========

fn flat_rows(file: &FileHits) -> impl Iterator<Item = String> + '_ {
    let display = file.path.display();
    file.hits
        .iter()
        .map(move |(n, text)| format!("{display}:{n}: {text}"))
}

/// 扁平行按连续段交给 search_results 归组（与旧行为一致）
fn flush_plain(plain: &mut Vec<String>, blocks: &mut Vec<String>) {
    if !plain.is_empty() {
        blocks.push(search_results::render(plain));
        plain.clear();
    }
}

fn render(outcome: &SearchOutcome, regex: &Regex, structure: bool, single_file: bool) -> Rendered {
    if !structure {
        let rows: Vec<String> = outcome.files.iter().flat_map(flat_rows).collect();
        return Rendered {
            text: search_results::render(&rows),
            full: None,
        };
    }

    let mut lossy = false;
    let mut blocks: Vec<String> = Vec::new();
    let mut plain: Vec<String> = Vec::new();
    for file in &outcome.files {
        let display = file.path.display().to_string();
        if let Some(structure) = &file.structure {
            flush_plain(&mut plain, &mut blocks);
            blocks.push(render_structured(
                &display, &file.hits, structure, regex, &mut lossy,
            ));
        } else if !single_file && is_non_code(&file.path) && file.hits.len() > NON_CODE_MAX_MATCHES
        {
            // 用户点名的单个文件不封顶
            flush_plain(&mut plain, &mut blocks);
            lossy = true;
            let mut block = format!("File: {display}");
            for (n, text) in file.hits.iter().take(NON_CODE_MAX_MATCHES) {
                block.push_str(&format!("\n{n}: {}", compact_line(text, regex, &mut lossy)));
            }
            block.push_str(&format!(
                "\n(+{} more matches; structure:false lists all)",
                file.hits.len() - NON_CODE_MAX_MATCHES
            ));
            blocks.push(block);
        } else {
            for (n, text) in &file.hits {
                plain.push(format!(
                    "{display}:{n}: {}",
                    compact_line(text, regex, &mut lossy)
                ));
            }
        }
    }
    flush_plain(&mut plain, &mut blocks);

    let full = lossy.then(|| {
        let rows: Vec<String> = outcome.files.iter().flat_map(flat_rows).collect();
        let flat = search_results::render(&rows);
        (flat.len() as u64, estimate_text_tokens(&flat) as u64)
    });
    Rendered {
        text: blocks.join("\n"),
        full,
    }
}

/// 代码文件：匹配按最内层所属符号分组
fn render_structured(
    display: &str,
    hits: &[(usize, String)],
    structure: &Structure,
    regex: &Regex,
    lossy: &mut bool,
) -> String {
    let Structure {
        symbols,
        parents,
        owners,
    } = structure;
    // 分组：(top level) 在前，其余按首个匹配出现的顺序
    let mut groups: Vec<(Option<usize>, Vec<usize>)> = Vec::new();
    for (i, owner) in owners.iter().enumerate() {
        match groups.iter_mut().find(|(g, _)| g == owner) {
            Some((_, members)) => members.push(i),
            None => groups.push((*owner, vec![i])),
        }
    }
    groups.sort_by_key(|(owner, _)| owner.is_some()); // 稳定排序：None 在前
    let matched: Vec<usize> = groups.iter().filter_map(|(owner, _)| *owner).collect();
    let busy = hits.len() >= BUSY_FILE_MATCHES;

    let mut out = vec![format!("File: {display}")];
    if hits.len() >= SUMMARY_MIN_MATCHES {
    let mut summary = format!(
        "symbols: {} total, {} matched",
        symbols.len(),
        matched.len()
    );
    if !busy {
        let has_top_level = groups.first().is_some_and(|(owner, _)| owner.is_none());
        if let Some(others) = sibling_names(symbols, parents, &matched, has_top_level) {
            summary.push_str("; other: ");
            summary.push_str(&others);
        }
    }
    out.push(summary);
    }

    let row = |i: usize, lossy: &mut bool| {
        let (line, text) = &hits[i];
        format!("{line}: {}", compact_line(text, regex, lossy))
    };
    // 繁忙文件：至多 BUSY_MAX_GROUPS 个组头，其余组合并为一个溢出组（行不丢）
    let keep = if busy && groups.len() > BUSY_MAX_GROUPS {
        BUSY_MAX_GROUPS - 1
    } else {
        groups.len()
    };
    for (owner, members) in &groups[..keep] {
        out.push(match owner {
            Some(idx) => symbol_header(symbols, parents, *idx),
            None => "(top level)".to_string(),
        });
        for &i in members {
            out.push(row(i, lossy));
        }
    }
    if keep < groups.len() {
        let mut rest: Vec<usize> = groups[keep..]
            .iter()
            .flat_map(|(_, members)| members.iter().copied())
            .collect();
        rest.sort_unstable();
        out.push(format!("(in {} more symbols)", groups.len() - keep));
        for i in rest {
            out.push(row(i, lossy));
        }
    }
    out.join("\n")
}

/// 组头：`fn new @ L9-11 in impl Config`（父符号只取一层）
fn symbol_header(symbols: &[Symbol], parents: &[Option<usize>], idx: usize) -> String {
    let s = &symbols[idx];
    let mut header = format!("{} {} @ L{}-{}", s.kind, s.name, s.line_start, s.line_end);
    if let Some(parent) = parents[idx] {
        let p = &symbols[parent];
        header.push_str(&format!(" in {} {}", p.kind, p.name));
    }
    header
}

/// 父符号下标：大纲是 AST 先序（祖先在前、按起始行有序），用栈求最近的包含者
fn parent_indices(symbols: &[Symbol]) -> Vec<Option<usize>> {
    let mut parents = vec![None; symbols.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, s) in symbols.iter().enumerate() {
        while let Some(&top) = stack.last() {
            let t = &symbols[top];
            if t.line_start <= s.line_start && s.line_end <= t.line_end {
                break;
            }
            stack.pop();
        }
        parents[i] = stack.last().copied();
        stack.push(i);
    }
    parents
}

/// 包含该行的最内层符号：先序中最后一个包含者即最深者
fn innermost(symbols: &[Symbol], line: usize) -> Option<usize> {
    symbols
        .iter()
        .rposition(|s| s.line_start <= line && line <= s.line_end)
}

/// 未命中的同级符号名（与命中符号同父；排除命中符号的祖先），至多 MAX_OTHER_SYMBOLS 个
fn sibling_names(
    symbols: &[Symbol],
    parents: &[Option<usize>],
    matched: &[usize],
    has_top_level: bool,
) -> Option<String> {
    let mut levels: Vec<Option<usize>> = matched.iter().map(|&m| parents[m]).collect();
    if has_top_level {
        levels.push(None);
    }
    let mut ancestors = Vec::new();
    for &m in matched {
        let mut cur = parents[m];
        while let Some(p) = cur {
            ancestors.push(p);
            cur = parents[p];
        }
    }
    let matched_names: Vec<&str> = matched.iter().map(|&m| symbols[m].name.as_str()).collect();
    // 同名去重（struct X 与 impl X），且不重复已命中的名字
    let mut others: Vec<&str> = Vec::new();
    for (i, s) in symbols.iter().enumerate() {
        let name = s.name.as_str();
        if !matched.contains(&i)
            && !ancestors.contains(&i)
            && levels.contains(&parents[i])
            && !matched_names.contains(&name)
            && !others.contains(&name)
        {
            others.push(name);
        }
    }
    if others.is_empty() {
        return None;
    }
    // 至多 MAX_OTHER_SYMBOLS 个、且受字符预算约束（至少列一个）
    let mut list = String::new();
    let mut shown = 0;
    for name in &others {
        if shown == MAX_OTHER_SYMBOLS
            || (shown > 0 && list.len() + 2 + name.len() > OTHER_SYMBOLS_MAX_CHARS)
        {
            break;
        }
        if shown > 0 {
            list.push_str(", ");
        }
        list.push_str(name);
        shown += 1;
    }
    if shown < others.len() {
        list.push_str(", …");
    }
    Some(list)
}

fn is_non_code(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| NON_CODE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// 超长行围绕首个匹配截取 MAX_LINE_CHARS 个字符（匹配前保留 ~80 个），标注原长度
fn compact_line(line: &str, regex: &Regex, lossy: &mut bool) -> String {
    let total = line.chars().count();
    if total <= MAX_LINE_CHARS {
        return line.to_string();
    }
    *lossy = true;
    let match_start = regex
        .find(line)
        .map(|m| line[..m.start()].chars().count())
        .unwrap_or(0);
    // 匹配靠近行尾时整体左移，始终展示满 MAX_LINE_CHARS 个字符
    let start = match_start
        .saturating_sub(LINE_PREFIX_CONTEXT_CHARS)
        .min(total - MAX_LINE_CHARS);
    let end = start + MAX_LINE_CHARS;
    let snippet: String = line.chars().skip(start).take(MAX_LINE_CHARS).collect();
    format!(
        "{}{snippet}{} [line cut: {total} chars]",
        if start > 0 { "…" } else { "" },
        if end < total { "…" } else { "" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "alpha mentioned in text\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "alpha should be skipped\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn test_grep_recursive_and_skips_hidden() {
        let dir = setup().await;
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"pattern": "alpha"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        // 代码文件：按所属符号归组；只命中 1 处时不输出 symbols 概览行
        assert!(
            out.content.contains("src/a.rs\nfn alpha @ L1-1\n1: fn alpha() {}"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("symbols:"), "{}", out.content);
        assert!(out.content.contains("b.txt:1: alpha mentioned in text"));
        assert!(!out.content.contains(".git")); // 隐藏目录被跳过

        // structure:false：旧的逐行格式
        let out = tool
            .execute(serde_json::json!({"pattern": "alpha", "structure": false}))
            .await
            .unwrap();
        assert!(
            out.content.contains("src/a.rs:1: fn alpha() {}"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("symbols:"), "{}", out.content);
    }

    #[tokio::test]
    async fn test_grep_glob_filter() {
        let dir = setup().await;
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"pattern": "alpha", "glob": "*.rs"}))
            .await
            .unwrap();
        assert!(out.content.contains("a.rs"));
        assert!(!out.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn test_grep_invalid_regex_and_no_match() {
        let dir = setup().await;
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"pattern": "(unclosed"}))
            .await
            .unwrap();
        assert!(out.is_error);

        let out = tool
            .execute(serde_json::json!({"pattern": "zzz_not_there"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("No matches"));
    }

    #[tokio::test]
    async fn test_grep_gitignore_full_glob_and_cap_notice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.path().join("generated")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(dir.path().join("generated/out.ts"), "needle\n").unwrap();
        std::fs::write(dir.path().join("src/a.ts"), "needle\n").unwrap();
        std::fs::write(dir.path().join("src/deep/b.tsx"), "needle\n").unwrap();
        std::fs::write(dir.path().join("src/c.rs"), "needle\n").unwrap();
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        // .gitignore 生效（不要求是 git 仓库）
        let out = tool
            .execute(serde_json::json!({"pattern": "needle"}))
            .await
            .unwrap();
        assert!(!out.content.contains("generated"), "{}", out.content);
        assert!(out.content.contains("c.rs"));

        // 花括号与 ** glob
        let out = tool
            .execute(serde_json::json!({"pattern": "needle", "glob": "*.{ts,tsx}"}))
            .await
            .unwrap();
        assert!(out.content.contains("a.ts") && out.content.contains("b.tsx"));
        assert!(!out.content.contains("c.rs"));
        let out = tool
            .execute(serde_json::json!({"pattern": "needle", "glob": "src/deep/**"}))
            .await
            .unwrap();
        assert!(out.content.contains("b.tsx") && !out.content.contains("a.ts"));

        // 达到上限必须明示
        let many = "needle\n".repeat(MAX_MATCHES + 50);
        std::fs::write(dir.path().join("src/many.rs"), many).unwrap();
        let out = tool
            .execute(serde_json::json!({"pattern": "needle", "glob": "many.rs"}))
            .await
            .unwrap();
        assert!(out.content.contains("Results capped"), "{}", out.content);
    }

    #[tokio::test]
    async fn test_grep_groups_same_file_matches() {
        let dir = tempfile::tempdir().unwrap();
        let content = (1..=12)
            .map(|i| format!("needle line {i}\n"))
            .collect::<String>();
        std::fs::write(dir.path().join("src.txt"), content).unwrap();
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        // 逐行全量模式（结构化模式下 .txt 会被封顶，见 test_grep_non_code_cap）
        let out = tool
            .execute(
                serde_json::json!({"pattern": "needle", "glob": "src.txt", "structure": false}),
            )
            .await
            .unwrap();
        // ≥8 条同文件匹配：归组为 File: 头 + 行号行（路径只出现一次）
        assert!(out.content.starts_with("File: "), "{}", out.content);
        assert!(
            out.content.contains("src.txt\n1: needle line 1"),
            "{}",
            out.content
        );
        assert!(out.content.contains("3: needle line 3"), "{}", out.content);
        assert!(
            out.content.contains("12: needle line 12"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("src.txt:3:"), "{}", out.content);
        assert_eq!(out.content.matches("src.txt").count(), 1);
    }

    #[tokio::test]
    async fn test_grep_reports_skipped_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = ExecutionEnv::new(dir.path());
        env.max_file_size = 10;
        std::fs::write(dir.path().join("big.txt"), "needle in a big file\n").unwrap();
        let tool = GrepTool::new(Arc::new(env));

        let out = tool
            .execute(serde_json::json!({"pattern": "needle"}))
            .await
            .unwrap();
        assert!(out.content.contains("No matches"));
        assert!(out.content.contains("NOT searched"), "{}", out.content);
    }

    // ===== 结构化输出 =====

    const RUST_SAMPLE: &str = "\
use crate::needle_mod;

pub struct Config {
    needle: u32,
}

impl Config {
    pub fn new() -> Self {
        Self { needle: 1 }
    }
    pub fn sibling() {}
}

fn helper() {}

fn uses_needle() {
    let x = needle();
}

fn unrelated_a() {}
fn unrelated_b() {}
";

    fn tool_with(files: &[(&str, &str)]) -> (tempfile::TempDir, GrepTool) {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));
        (dir, tool)
    }

    async fn grep(tool: &GrepTool, args: Value) -> String {
        let out = tool.execute(args).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        out.content
    }

    #[tokio::test]
    async fn test_structure_groups_by_innermost_symbol() {
        let (_dir, tool) = tool_with(&[("src/lib.rs", RUST_SAMPLE)]);
        let out = grep(&tool, serde_json::json!({"pattern": "needle"})).await;
        let body = out.split_once('\n').unwrap().1;
        assert!(out.starts_with("File: ") && out.lines().next().unwrap().ends_with("src/lib.rs"));
        assert_eq!(
            body,
            "\
symbols: 8 total, 3 matched; other: sibling, helper, unrelated_a, unrelated_b
(top level)
1: use crate::needle_mod;
struct Config @ L3-5
4:     needle: u32,
fn new @ L8-10 in impl Config
9:         Self { needle: 1 }
fn uses_needle @ L16-18
16: fn uses_needle() {
17:     let x = needle();"
        );
    }

    #[tokio::test]
    async fn test_structure_other_list_is_capped() {
        // ≥3 处命中才输出概览行
        let code = "fn target() {\n    needle();\n    needle();\n    needle();\n}\n\
                    fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n";
        let (_dir, tool) = tool_with(&[("m.rs", code)]);
        let out = grep(&tool, serde_json::json!({"pattern": "needle"})).await;
        assert!(
            out.contains(
                "symbols: 6 total, 1 matched; other: a, b, c, d, …\nfn target @ L1-5\n2: "
            ),
            "{out}"
        );
    }

    #[tokio::test]
    async fn test_structure_top_level_only_and_python() {
        let py = "import needle\n\nclass Svc:\n    def run(self):\n        return 1\n";
        let (_dir, tool) = tool_with(&[("svc.py", py)]);
        let out = grep(&tool, serde_json::json!({"pattern": "needle"})).await;
        assert!(out.contains("svc.py\n(top level)\n1: import needle"), "{out}");
        assert!(!out.contains("symbols:"), "{out}");
    }

    #[tokio::test]
    async fn test_structure_doc_comments_attach_to_next_symbol() {
        let code = "\
//! module docs mention needle
pub struct Thing;

impl Thing {
    /// needle constructor
    #[inline]
    pub fn make() -> Self {
        Self
    }

    // needle: separated from the next item by a blank line

    fn helper() {}
}
";
        let (_dir, tool) = tool_with(&[("m.rs", code)]);
        let out = grep(&tool, serde_json::json!({"pattern": "needle"})).await;
        let body = out.split_once('\n').unwrap().1;
        // //! 是模块内部文档 → top level；/// + #[inline] 归下一个符号；隔空行的注释留在容器里。
        // other 去重：struct Thing 与已命中的 impl Thing 同名，不再列出
        assert_eq!(
            body,
            "\
symbols: 4 total, 2 matched; other: helper
(top level)
1: //! module docs mention needle
fn make @ L7-9 in impl Thing
5:     /// needle constructor
impl Thing @ L4-14
11:     // needle: separated from the next item by a blank line"
        );
    }

    #[tokio::test]
    async fn test_structure_non_code_cap() {
        let notes: String = (1..=10).map(|i| format!("needle note {i}\n")).collect();
        let (dir, tool) =
            tool_with(&[("docs/notes.md", &notes), ("cfg.json", "{\"needle\": 1}\n")]);
        let out = tool
            .execute(serde_json::json!({"pattern": "needle"}))
            .await
            .unwrap();
        assert!(out.content.contains("notes.md\n1: needle note 1\n2: needle note 2\n3: needle note 3\n(+7 more matches; structure:false lists all)"), "{}", out.content);
        assert!(!out.content.contains("needle note 4"), "{}", out.content);
        // 未超上限的非代码文件照常展示
        assert!(
            out.content.contains("cfg.json:1: {\"needle\": 1}"),
            "{}",
            out.content
        );
        // 台账记录完整扁平输出的原始量
        assert!(out.original_bytes.unwrap() > out.content.len() as u64);

        // structure:false 列出全部
        let out = grep(
            &tool,
            serde_json::json!({"pattern": "needle", "structure": false}),
        )
        .await;
        assert!(out.contains("needle note 10"), "{out}");
        // 点名的单个文件不封顶
        let path = dir.path().join("docs/notes.md");
        let out = grep(
            &tool,
            serde_json::json!({"pattern": "needle", "path": path.to_str().unwrap()}),
        )
        .await;
        assert!(out.contains("needle note 10"), "{out}");
    }

    #[tokio::test]
    async fn test_structure_busy_file_limits_groups() {
        // 14 个函数各一处匹配：≥12 → 最多 8 个组头（7 组 + 1 个溢出组），不列 other
        let code: String = (0..14)
            .map(|i| format!("fn f{i}() {{\n    needle();\n}}\n"))
            .collect::<String>()
            + "fn quiet() {}\n";
        let (_dir, tool) = tool_with(&[("busy.rs", &code)]);
        let out = grep(&tool, serde_json::json!({"pattern": "needle"})).await;
        assert!(out.contains("symbols: 15 total, 14 matched\n"), "{out}");
        assert!(!out.contains("other:"), "{out}");
        let headers = out.lines().filter(|l| l.starts_with("fn f")).count();
        assert_eq!(headers, 7, "{out}");
        assert!(out.contains("(in 7 more symbols)"), "{out}");
        // 所有匹配行都在
        assert_eq!(out.matches("    needle();").count(), 14, "{out}");
        assert!(out.contains("\n41:     needle();"), "{out}");
    }

    #[tokio::test]
    async fn test_structure_dense_file_falls_back_to_flat_rows() {
        let code: String = (0..30)
            .map(|i| format!("fn f{i}() {{ needle(); }}\n"))
            .collect();
        let (_dir, tool) = tool_with(&[("dense.rs", &code)]);
        let out = grep(&tool, serde_json::json!({"pattern": "needle"})).await;
        assert!(!out.contains("symbols:"), "{out}");
        assert!(!out.contains(" @ L"), "{out}");
        assert!(out.starts_with("File: "), "{out}");
        assert!(out.contains("\n30: fn f29() { needle(); }"), "{out}");
    }

    #[tokio::test]
    async fn test_structure_cuts_long_lines_around_match() {
        let line = format!("{}NEEDLE{}", "a".repeat(500), "b".repeat(500));
        let (_dir, tool) = tool_with(&[("min.js", &format!("{line}\n"))]);
        let out = grep(&tool, serde_json::json!({"pattern": "NEEDLE"})).await;
        let row = out.lines().next().unwrap();
        assert!(row.contains(&format!("…{}NEEDLE", "a".repeat(80))), "{row}");
        assert!(!row.contains(&"a".repeat(81)), "{row}");
        assert!(row.ends_with("… [line cut: 1006 chars]"), "{row}");
        let text = row.split_once(":1: ").unwrap().1;
        assert!(
            text.chars().count() <= MAX_LINE_CHARS + 30,
            "{}",
            text.chars().count()
        );

        // 匹配靠近行尾：窗口左移，仍展示满 240 字符
        let tail = format!("{}NEEDLE", "c".repeat(600));
        let (_dir, tool) = tool_with(&[("tail.txt", &format!("{tail}\n"))]);
        let out = grep(&tool, serde_json::json!({"pattern": "NEEDLE"})).await;
        assert!(
            out.contains(&format!("…{}NEEDLE [line cut: 606 chars]", "c".repeat(234))),
            "{out}"
        );

        // structure:false：整行原样
        let (_dir, tool) = tool_with(&[("min.js", &format!("{line}\n"))]);
        let out = grep(
            &tool,
            serde_json::json!({"pattern": "NEEDLE", "structure": false}),
        )
        .await;
        assert!(out.contains(&line), "{out}");
    }

    #[tokio::test]
    async fn test_structure_output_stays_compact() {
        // 结构化输出的匹配行不比扁平行长：增量只来自头部行
        let (_dir, tool) = tool_with(&[("src/lib.rs", RUST_SAMPLE), ("src/other.rs", RUST_SAMPLE)]);
        let structured = grep(&tool, serde_json::json!({"pattern": "needle"})).await;
        let flat = grep(
            &tool,
            serde_json::json!({"pattern": "needle", "structure": false}),
        )
        .await;
        let is_header = |l: &str| {
            l.starts_with("File: ")
                || l.starts_with("symbols: ")
                || l.starts_with('(')
                || l.contains(" @ L")
        };
        let header_bytes: usize = structured
            .lines()
            .filter(|l| is_header(l))
            .map(|l| l.len() + 1)
            .sum();
        assert!(
            structured.len() <= flat.len() + header_bytes,
            "structured {} > flat {} + headers {header_bytes}",
            structured.len(),
            flat.len()
        );
        // 两个文件 × 5 处匹配，行一条不少
        assert_eq!(structured.lines().filter(|l| !is_header(l)).count(), 10);
    }

    #[test]
    fn test_parent_and_innermost() {
        let symbols = signatures::outline(RUST_SAMPLE, Some(Lang::Rust));
        let parents = parent_indices(&symbols);
        let new = symbols.iter().position(|s| s.name == "new").unwrap();
        let imp = symbols.iter().position(|s| s.kind == "impl").unwrap();
        assert_eq!(parents[new], Some(imp));
        assert_eq!(parents[imp], None);
        assert_eq!(innermost(&symbols, 9), Some(new));
        assert_eq!(
            innermost(&symbols, 11).map(|i| symbols[i].name.as_str()),
            Some("sibling")
        );
        assert_eq!(innermost(&symbols, 12), Some(imp));
        assert_eq!(innermost(&symbols, 1), None);
    }
}
