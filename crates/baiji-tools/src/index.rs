//! 代码索引：符号表 + import 边（属性图的轻量实现）
//!
//! 按需构建（工具调用时扫描，复用 grep 的目录跳过与大小/深度限制）：
//! - 每个文件提取符号（AST 优先，见 [`crate::signatures`]）
//! - 每个文件提取 import 目标（正则，按语言）
//! - 反向引用查询：谁 import 了某文件（影响面分析的核心边）

use crate::env::ExecutionEnv;
use crate::signatures::{self, Symbol};
use std::path::{Path, PathBuf};

/// 单文件索引条目
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub symbols: Vec<Symbol>,
    /// import 声明的目标字符串（原始形式，如 `crate::agent::runtime` / `./utils` / `os`）
    pub imports: Vec<String>,
}

/// 代码索引
#[derive(Debug, Default)]
pub struct CodeIndex {
    pub entries: Vec<FileEntry>,
    /// 达到 MAX_FILES 上限，索引不完整（查询结果需向模型说明）
    pub truncated: bool,
}

/// 最多索引的文件数
const MAX_FILES: usize = 2000;

impl CodeIndex {
    /// 扫描构建索引（同步 IO，调用方放 spawn_blocking）
    pub fn build(root: &Path, env: &ExecutionEnv) -> Self {
        let mut index = Self::default();
        walk_files(root, env, &mut index);
        index
    }

    /// 反向引用：哪些文件的 import 提到了 `target`。
    ///
    /// `target` 可以是模块名（`runtime`）、模块路径（`agent::runtime`、`pkg.mod`）
    /// 或文件路径（`src/agent/runtime.rs`）。按「路径段」匹配：import 的段序列包含
    /// target 的段序列即命中；文件路径再退一步按文件名（模块名）匹配——
    /// `src/util.rs` 与 `use crate::util` 的目录前缀并不对应。
    pub fn importers_of(&self, target: &str) -> Vec<&FileEntry> {
        let needle = segments(target);
        if needle.is_empty() {
            return Vec::new();
        }
        let is_file_path = target.contains('/') || has_source_extension(target);
        let module_name = &needle[needle.len() - 1..];
        self.entries
            .iter()
            .filter(|entry| {
                entry.imports.iter().any(|imp| {
                    let imp = segments(imp);
                    contains_run(&imp, &needle)
                        || (is_file_path && contains_run(&imp, module_name))
                })
            })
            .collect()
    }

    /// 某文件直接 import 的目标列表
    pub fn imports_of(&self, file: &Path) -> &[String] {
        self.entries
            .iter()
            .find(|e| e.path == file)
            .map(|e| e.imports.as_slice())
            .unwrap_or(&[])
    }
}

const SOURCE_EXTENSIONS: &[&str] = &[".rs", ".py", ".tsx", ".ts", ".jsx", ".js", ".mjs", ".go"];

fn has_source_extension(target: &str) -> bool {
    SOURCE_EXTENSIONS.iter().any(|ext| target.ends_with(ext))
}

/// 归一化为小写路径段：去扩展名，按 `::` `/` `.` 切分，丢掉不携带信息的段
/// （`crate`/`self`/`super`、`.`/`..`、`mod`/`index`/`__init__`）
fn segments(target: &str) -> Vec<String> {
    let mut t = target.trim();
    for ext in SOURCE_EXTENSIONS {
        if let Some(stripped) = t.strip_suffix(ext) {
            t = stripped;
            break;
        }
    }
    t.split([':', '/', '.'])
        .map(|seg| seg.trim().to_ascii_lowercase())
        .filter(|seg| {
            !seg.is_empty()
                && !matches!(
                    seg.as_str(),
                    "crate" | "self" | "super" | "mod" | "index" | "__init__"
                )
        })
        .collect()
}

/// `haystack` 是否包含连续的 `needle` 段序列
fn contains_run(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn walk_files(root: &Path, env: &ExecutionEnv, index: &mut CodeIndex) {
    let Ok(entries) = crate::walk::walk(root, env, None) else {
        return;
    };
    for entry in entries {
        if index.entries.len() >= MAX_FILES {
            index.truncated = true;
            return;
        }
        if entry.path().is_file()
            && let Some(file_entry) = index_file(entry.path(), env)
        {
            index.entries.push(file_entry);
        }
    }
}

/// 索引单个文件（受支持语言 + 大小限制；读取失败静默跳过）
/// 单文件解析缓存：path → (mtime, len, 解析结果)。
/// 每次 search / imports 调用都会重建索引；没有缓存时那是最多 2000 次 tree-sitter 解析。
/// 以 mtime+len 判断失效，文件被 edit/bash 修改后下次自动重解析。
type ParseCache = std::collections::HashMap<PathBuf, (std::time::SystemTime, u64, Option<FileEntry>)>;
static PARSE_CACHE: std::sync::OnceLock<std::sync::Mutex<ParseCache>> = std::sync::OnceLock::new();
const PARSE_CACHE_MAX: usize = 20_000;

fn index_file(path: &Path, env: &ExecutionEnv) -> Option<FileEntry> {
    let meta = path.metadata().ok()?;
    if meta.len() > env.max_file_size {
        return None;
    }
    let lang = signatures::Lang::detect(path)?;

    let stamp = meta.modified().ok().map(|mtime| (mtime, meta.len()));
    let cache = PARSE_CACHE.get_or_init(Default::default);
    if let Some((mtime, len)) = stamp
        && let Some((m, l, cached)) = cache.lock().unwrap().get(path)
        && *m == mtime
        && *l == len
    {
        return cached.clone();
    }

    let parsed = std::fs::read_to_string(path).ok().map(|content| FileEntry {
        path: path.to_path_buf(),
        symbols: signatures::outline(&content, Some(lang)),
        imports: extract_imports(&content, Some(lang)),
    });
    if let Some((mtime, len)) = stamp {
        let mut cache = cache.lock().unwrap();
        if cache.len() >= PARSE_CACHE_MAX {
            cache.clear();
        }
        cache.insert(path.to_path_buf(), (mtime, len, parsed.clone()));
    }
    parsed
}

/// 从文件内容提取 import 目标（按语言正则匹配）
pub fn extract_imports(content: &str, lang: Option<signatures::Lang>) -> Vec<String> {
    let mut imports = Vec::new();
    let mut in_go_block = false;
    for line in content.lines() {
        let t = line.trim();
        let target = match lang {
            Some(signatures::Lang::Rust) => {
                if !t.starts_with("use ") {
                    continue;
                }
                // `use a::b::{c, d};` → a::b
                let body = t
                    .trim_start_matches("use ")
                    .trim_end_matches(';')
                    .split('{')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches(':')
                    .trim();
                body.to_string()
            }
            Some(signatures::Lang::Python) => {
                if let Some(rest) = t.strip_prefix("from ") {
                    rest.split(" import").next().unwrap_or("").trim().to_string()
                } else if let Some(rest) = t.strip_prefix("import ") {
                    rest.split(',').next().unwrap_or("").trim().to_string()
                } else {
                    continue;
                }
            }
            Some(signatures::Lang::JavaScript)
            | Some(signatures::Lang::TypeScript)
            | Some(signatures::Lang::TypeScriptTsx) => {
                if let Some(rest) = t.strip_prefix("} from ") {
                    // 多行 import { a,\n b\n} from './y' 的收尾行
                    rest.trim_matches(['\'', '"', ';', ' ']).to_string()
                } else if t.starts_with("import ") {
                    // import x from './y' | import './y'
                    if let Some(pos) = t.find(" from ") {
                        t[pos + 6..].trim_matches(['\'', '"', ';', ' ']).to_string()
                    } else if !t.contains(['\'', '"']) {
                        // 多行 import 的起始行（`import {`）：目标在收尾行
                        continue;
                    } else {
                        t.trim_start_matches("import ")
                            .trim_matches(['\'', '"', ';', ' '])
                            .to_string()
                    }
                } else if let Some(rest) = t.strip_prefix("export * from ") {
                    rest.trim_matches(['\'', '"', ';', ' ']).to_string()
                } else {
                    continue;
                }
            }
            Some(signatures::Lang::Go) => {
                // import 块：`import (` … `)`，块内每行是 [别名] "路径"
                if in_go_block {
                    if t.starts_with(')') {
                        in_go_block = false;
                        continue;
                    }
                    go_import_path(t)
                } else if t == "import (" || t.starts_with("import (") {
                    in_go_block = true;
                    continue;
                } else if let Some(rest) = t.strip_prefix("import ") {
                    go_import_path(rest)
                } else {
                    continue;
                }
            }
            None => continue,
        };
        if !target.is_empty() && imports.len() < 100 {
            imports.push(target);
        }
    }
    imports
}

/// Go import 行里的路径：取引号内的内容（忽略别名与行尾注释）
fn go_import_path(line: &str) -> String {
    line.split('"').nth(1).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, imports: &[&str]) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            symbols: Vec::new(),
            imports: imports.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn test_importers_of_matches_paths_modules_and_files() {
        let index = CodeIndex {
            entries: vec![
                entry("a.rs", &["crate::agent::runtime"]),
                entry("b.py", &["pkg.utils.strings"]),
                entry("c.ts", &["./lib/format"]),
                entry("d.rs", &["std::fmt"]),
            ],
            truncated: false,
        };
        let hits = |t: &str| -> Vec<String> {
            index
                .importers_of(t)
                .iter()
                .map(|e| e.path.display().to_string())
                .collect()
        };
        assert_eq!(hits("runtime"), vec!["a.rs"]);
        assert_eq!(hits("agent::runtime"), vec!["a.rs"]);
        // 文件路径：目录前缀对不上时按模块名匹配
        assert_eq!(hits("src/agent/runtime.rs"), vec!["a.rs"]);
        // Python 点分路径
        assert_eq!(hits("utils.strings"), vec!["b.py"]);
        assert_eq!(hits("pkg/utils/strings.py"), vec!["b.py"]);
        assert_eq!(hits("lib/format.ts"), vec!["c.ts"]);
        // 段匹配而非子串：`fmt` 不会命中 `format`
        assert_eq!(hits("fmt"), vec!["d.rs"]);
        assert!(hits("").is_empty());
    }

    #[test]
    fn test_extract_imports_go_block_and_multiline_ts() {
        let go = "package main\n\nimport (\n\t\"fmt\"\n\tlog \"github.com/x/log\" // alias\n)\n\nimport \"os\"\n";
        assert_eq!(
            extract_imports(go, Some(signatures::Lang::Go)),
            vec!["fmt", "github.com/x/log", "os"]
        );

        let ts = "import {\n  a,\n  b,\n} from './multi';\nimport x from \"./single\";\nimport './side-effect';\n";
        assert_eq!(
            extract_imports(ts, Some(signatures::Lang::TypeScript)),
            vec!["./multi", "./single", "./side-effect"]
        );
    }

    #[test]
    fn test_extract_imports_rust() {
        let code = "use std::collections::HashMap;\nuse crate::agent::runtime::{AgentRuntime, AgentEvent};\nfn x() {}\n";
        let imports = extract_imports(code, Some(signatures::Lang::Rust));
        assert_eq!(imports[0], "std::collections::HashMap");
        // 组导入取路径部分
        assert_eq!(imports[1], "crate::agent::runtime");
    }

    #[test]
    fn test_extract_imports_python_js_go() {
        let py = "from .utils import helper\nimport os, sys\n";
        let imports = extract_imports(py, Some(signatures::Lang::Python));
        assert_eq!(imports, vec![".utils".to_string(), "os".to_string()]);

        let js = "import React from 'react';\nimport './styles.css';\nexport * from './lib';\n";
        let imports = extract_imports(js, Some(signatures::Lang::JavaScript));
        assert_eq!(
            imports,
            vec![
                "react".to_string(),
                "./styles.css".to_string(),
                "./lib".to_string()
            ]
        );

        let go = "import \"fmt\"\nfunc x() {}\n";
        let imports = extract_imports(go, Some(signatures::Lang::Go));
        assert_eq!(imports, vec!["fmt".to_string()]);
    }

    #[test]
    fn test_reverse_imports() {
        let dir = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(dir.path());
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "mod util;\nuse crate::util::helper;\nfn main() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/util.rs"),
            "pub fn helper() {}\nuse std::fmt;\n",
        )
        .unwrap();

        let index = CodeIndex::build(dir.path(), &env);
        assert_eq!(index.entries.len(), 2);

        // 谁 import 了 util？
        let importers = index.importers_of("util");
        let paths: Vec<&Path> = importers.iter().map(|e| e.path.as_path()).collect();
        assert_eq!(paths.len(), 1, "{:?}", paths);
        assert!(paths[0].ends_with("src/main.rs"));

        // main.rs 的直接 import
        let main = index
            .entries
            .iter()
            .find(|e| e.path.ends_with("src/main.rs"))
            .unwrap();
        assert!(main.imports.iter().any(|i| i.contains("util")));
        // 符号表来自 AST
        assert!(main.symbols.iter().any(|s| s.name == "main"));
    }
}
