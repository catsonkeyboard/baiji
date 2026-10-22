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
    // 全局目录归用户自己所有：不限制链接去向
    if let Some(dir) = global_dir
        && let Some(file) = first_existing(dir, None)
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
    // 项目根（git 根，找不到则 cwd）的真实路径：仓库内的说明文件解析符号链接后
    // 必须仍在根内，否则 `AGENTS.md -> ~/.aws/credentials` 会把密钥送进系统提示
    let project_root = chain.last().and_then(|root| root.canonicalize().ok());
    let Some(project_root) = project_root else {
        return files;
    };
    for dir in chain.into_iter().rev() {
        if let Some(file) = first_existing(dir, Some(&project_root))
            && !files.contains(&file)
        {
            files.push(file);
        }
    }
    files
}

/// 目录下第一个可用的说明文件。`confine_to` 给定时，文件（解析链接后）必须是
/// 该目录内的普通文件；越界的跳过并尝试下一个文件名
fn first_existing(dir: &Path, confine_to: Option<&Path>) -> Option<PathBuf> {
    FILE_NAMES.iter().map(|name| dir.join(name)).find(|p| {
        if !p.is_file() {
            return false;
        }
        let Some(root) = confine_to else {
            return true;
        };
        let inside = p.canonicalize().is_ok_and(|real| real.starts_with(root));
        if !inside {
            tracing::warn!(
                "skipping {}: it resolves outside the project root",
                p.display()
            );
        }
        inside
    })
}

/// 最多读 `cap + 1` 字节（多出的 1 字节让调用方知道需要截断）。截在多字节字符
/// 中间时保留完整前缀；真正的非 UTF-8 文件返回 None
fn read_capped(path: &Path, cap: usize) -> Option<String> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(cap as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    match String::from_utf8(buf) {
        Ok(text) => Some(text),
        // error_len() == None：只是结尾不完整（被上限截断），保留有效前缀
        Err(e) if e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).ok()
        }
        Err(_) => None,
    }
}

/// 读取并拼装说明段（无文件或全为空时 None）
pub fn load_project_instructions(cwd: &Path, global_dir: Option<&Path>) -> Option<String> {
    if std::env::var("BAIJI_AGENTS_MD").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
        return None;
    }
    let mut body = String::new();
    for file in instruction_files(cwd, global_dir) {
        let Some(content) = read_capped(&file, MAX_INSTRUCTIONS_BYTES) else {
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

    /// 仓库里的说明文件可以是指向仓库外的符号链接（`AGENTS.md -> ~/.aws/credentials`），
    /// 内容会进系统提示、发给模型厂商：项目级文件的真实路径必须留在项目根内
    #[cfg(unix)]
    #[test]
    fn test_symlink_escaping_the_project_is_not_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let secret = tmp.path().join("credentials");
        std::fs::write(&secret, "aws_secret_access_key = SECRET").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("AGENTS.md")).unwrap();
        std::fs::write(root.join("CLAUDE.md"), "repo rules").unwrap();

        let files = instruction_files(&root, None);
        assert_eq!(files, vec![root.join("CLAUDE.md")], "escaping link skipped");
        let section = load_project_instructions(&root, None).unwrap();
        assert!(!section.contains("SECRET"));
        assert!(section.contains("repo rules"));

        // 相对路径的链接同样不行
        std::fs::remove_file(root.join("AGENTS.md")).unwrap();
        std::os::unix::fs::symlink("../credentials", root.join("AGENTS.md")).unwrap();
        assert_eq!(instruction_files(&root, None), vec![root.join("CLAUDE.md")]);
    }

    /// 常见写法 `CLAUDE.md -> AGENTS.md`（链接留在仓库内）照常加载；
    /// 全局目录归用户自己所有，不受限制
    #[cfg(unix)]
    #[test]
    fn test_symlink_inside_the_project_and_global_links_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let sub = root.join("app");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("docs").join("rules.md"), "shared rules").unwrap();
        std::os::unix::fs::symlink("docs/rules.md", root.join("AGENTS.md")).unwrap();
        std::os::unix::fs::symlink("../AGENTS.md", sub.join("CLAUDE.md")).unwrap();
        let dotfiles = tmp.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(dotfiles.join("AGENTS.md"), "my global rules").unwrap();
        let global = tmp.path().join("global");
        std::fs::create_dir_all(&global).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("AGENTS.md"), global.join("AGENTS.md")).unwrap();

        assert_eq!(
            instruction_files(&sub, Some(&global)),
            vec![
                global.join("AGENTS.md"),
                root.join("AGENTS.md"),
                sub.join("CLAUDE.md"),
            ]
        );
        let section = load_project_instructions(&sub, Some(&global)).unwrap();
        assert!(section.contains("my global rules"));
        assert!(section.contains("shared rules"));
    }

    /// 只读上限 + 1 字节：仓库里放一个几 GB 的 AGENTS.md 不会被整个读进内存
    #[test]
    fn test_read_is_capped_before_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("AGENTS.md");
        std::fs::write(&file, "x".repeat(MAX_INSTRUCTIONS_BYTES * 4)).unwrap();
        let content = read_capped(&file, MAX_INSTRUCTIONS_BYTES).unwrap();
        assert_eq!(content.len(), MAX_INSTRUCTIONS_BYTES + 1);

        // 截在多字节字符中间：保留完整字符前缀，而不是整个文件读失败
        std::fs::write(&file, "规".repeat(MAX_INSTRUCTIONS_BYTES)).unwrap();
        let content = read_capped(&file, MAX_INSTRUCTIONS_BYTES).unwrap();
        assert!(content.len() <= MAX_INSTRUCTIONS_BYTES + 1);
        assert!(content.chars().all(|c| c == '规'));

        // 真正的非 UTF-8 文件仍然跳过
        std::fs::write(&file, b"ok \xff\xfe bad").unwrap();
        assert!(read_capped(&file, MAX_INSTRUCTIONS_BYTES).is_none());
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
