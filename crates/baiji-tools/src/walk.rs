//! 统一的目录遍历（grep / find / 代码索引共用）
//!
//! 基于 ripgrep 的 `ignore` 库：
//! - 遵守 `.gitignore` / `.ignore`（不要求目录是 git 仓库）
//! - 跳过隐藏文件与常见构建产物目录
//! - 不跟随符号链接目录；指向白名单外的符号链接文件被过滤
//! - 按文件名排序，结果确定（上限截断时每次截到同一批）

use crate::env::ExecutionEnv;
use ignore::overrides::OverrideBuilder;
use ignore::{DirEntry, WalkBuilder};
use std::path::Path;

/// 无论是否被 gitignore 都跳过的目录
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "dist",
    "build",
];

/// 遍历 `root` 下的条目（含目录；root 本身除外）。
/// `glob`：可选的文件名/路径过滤，完整 glob 语法（`*.rs`、`*.{ts,tsx}`、`src/**/*.rs`）。
pub fn walk(
    root: &Path,
    env: &ExecutionEnv,
    glob: Option<&str>,
) -> Result<impl Iterator<Item = DirEntry>, String> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(env.max_search_depth))
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| {
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            !(is_dir && entry.file_name().to_str().is_some_and(|n| SKIP_DIRS.contains(&n)))
        });

    if let Some(glob) = glob.filter(|g| !g.trim().is_empty()) {
        let mut overrides = OverrideBuilder::new(root);
        overrides
            .add(glob)
            .map_err(|e| format!("invalid glob '{glob}': {e}"))?;
        builder.overrides(
            overrides
                .build()
                .map_err(|e| format!("invalid glob '{glob}': {e}"))?,
        );
    }

    let env = env.clone();
    Ok(builder
        .build()
        .flatten()
        .filter(|entry| entry.depth() > 0)
        .filter(move |entry| env.allows_entry(entry.path())))
}
