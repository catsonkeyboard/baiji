//! 项目说明文件（AGENTS.md / CLAUDE.md）加载
//!
//! 按 AGENTS.md 约定把仓库里的协作说明注入系统提示（`## Project instructions`）：
//! - 全局：`~/.baiji/AGENTS.md`
//! - 项目：从 git 根目录到当前目录逐级，每级取 `AGENTS.md`（没有则 `CLAUDE.md`）
//!
//! 越靠近当前目录越具体，排在越后面。总量封顶 [`MAX_INSTRUCTIONS_BYTES`]，超出部分
//! 截断并注明。`BAIJI_AGENTS_MD=off` 关闭。

use std::path::{Path, PathBuf};

/// 注入系统提示的说明文件总字节上限
pub const MAX_INSTRUCTIONS_BYTES: usize = 32 * 1024;

const FILE_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// 找到要加载的说明文件（全局在前，项目由外到内）
pub fn instruction_files(cwd: &Path, global_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(dir) = global_dir
        && let Some(file) = first_existing(dir)
    {
        files.push(file);
    }
    // git 根目录（找不到则只看当前目录）到 cwd 的各级目录
    let mut chain: Vec<&Path> = Vec::new();
    let mut found_root = false;
    for dir in cwd.ancestors() {
        chain.push(dir);
        if dir.join(".git").exists() {
            found_root = true;
            break;
        }
    }
    if !found_root {
        chain.truncate(1);
    }
    for dir in chain.into_iter().rev() {
        if let Some(file) = first_existing(dir)
            && !files.contains(&file)
        {
            files.push(file);
        }
    }
    files
}

fn first_existing(dir: &Path) -> Option<PathBuf> {
    FILE_NAMES
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

/// 读取并拼装说明段（无文件或全为空时 None）
pub fn load_project_instructions(cwd: &Path, global_dir: Option<&Path>) -> Option<String> {
    if std::env::var("BAIJI_AGENTS_MD").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
        return None;
    }
    let mut body = String::new();
    for file in instruction_files(cwd, global_dir) {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        let shown = file
            .strip_prefix(cwd)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| file.display().to_string());
        body.push_str(&format!("\n\n### {shown}\n{content}"));
    }
    if body.is_empty() {
        return None;
    }
    if body.len() > MAX_INSTRUCTIONS_BYTES {
        let mut cut = MAX_INSTRUCTIONS_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
        body.push_str(&format!(
            "\n\n[instructions truncated at {} KB — read the files directly for the rest]",
            MAX_INSTRUCTIONS_BYTES / 1024
        ));
    }
    Some(format!(
        "## Project instructions\nThese files were written by the user/repository maintainers. \
         Follow them unless they conflict with an explicit user request.{body}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loads_git_root_to_cwd_with_fallback_to_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let sub = root.join("crates").join("app");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "ignored: AGENTS.md wins").unwrap();
        std::fs::write(sub.join("CLAUDE.md"), "app rules").unwrap();
        let global = tmp.path().join("global");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(global.join("AGENTS.md"), "global rules").unwrap();

        let files = instruction_files(&sub, Some(&global));
        assert_eq!(
            files,
            vec![
                global.join("AGENTS.md"),
                root.join("AGENTS.md"),
                sub.join("CLAUDE.md"),
            ]
        );
        let section = load_project_instructions(&sub, Some(&global)).unwrap();
        let (g, r, a) = (
            section.find("global rules").unwrap(),
            section.find("root rules").unwrap(),
            section.find("app rules").unwrap(),
        );
        assert!(g < r && r < a, "outer → inner order");
        assert!(!section.contains("ignored"));
        assert!(section.starts_with("## Project instructions"));
    }

    #[test]
    fn test_without_git_root_only_cwd_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        let cwd = parent.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(parent.join("AGENTS.md"), "parent").unwrap();
        assert!(instruction_files(&cwd, None).is_empty());
        std::fs::write(cwd.join("AGENTS.md"), "  ").unwrap();
        assert_eq!(instruction_files(&cwd, None), vec![cwd.join("AGENTS.md")]);
        // 空文件不产生段落
        assert!(load_project_instructions(&cwd, None).is_none());
    }

    #[test]
    fn test_oversized_instructions_are_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "规".repeat(MAX_INSTRUCTIONS_BYTES)).unwrap();
        let section = load_project_instructions(tmp.path(), None).unwrap();
        assert!(section.contains("instructions truncated"));
        assert!(section.len() < MAX_INSTRUCTIONS_BYTES + 512);
    }
}
