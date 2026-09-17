//! ExecutionEnv — 工具执行环境
//!
//! 所有内置工具共享的约束集合：
//! - 工作目录与路径白名单（read/write/edit/grep/find/ls 只能访问白名单内的路径）
//! - 输出截断（防止工具结果撑爆上下文）
//! - 文件大小 / 搜索深度 / 命令超时限制

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// 执行环境
#[derive(Debug, Clone)]
pub struct ExecutionEnv {
    /// 工作目录（相对路径基准；bash 的 cwd）
    pub workdir: PathBuf,
    /// 允许访问的根目录白名单（词法归一化后）
    pub allowed_roots: Vec<PathBuf>,
    /// 单次工具输出最大字节数（超出截断并追加标记）
    pub max_output_bytes: usize,
    /// 跳过大于此字节数的文件（read/grep）
    pub max_file_size: u64,
    /// 目录递归搜索最大深度（grep/find）
    pub max_search_depth: usize,
    /// bash 命令默认超时
    pub command_timeout: Duration,
    /// 内容寻址存储目录（CCR）：截断的完整输出 spill 至此，
    /// `expand` 工具凭句柄取回。None = 截断即丢弃（旧行为）。
    ctx_store: Option<PathBuf>,
    /// 预算降级：read 输出将超限时自动按信息熵 density 选行，
    /// 而不是硬截断（true 默认；false 恢复纯截断行为）
    pub density_fallback: bool,
}

impl ExecutionEnv {
    pub fn new(workdir: impl AsRef<Path>) -> Self {
        Self {
            workdir: workdir.as_ref().to_path_buf(),
            allowed_roots: vec![workdir.as_ref().to_path_buf()],
            max_output_bytes: 32 * 1024,
            max_file_size: 1024 * 1024,
            max_search_depth: 10,
            command_timeout: Duration::from_secs(30),
            ctx_store: None,
            density_fallback: true,
        }
    }

    /// 关闭预算降级（read 超限回到硬截断行为）
    pub fn without_density_fallback(mut self) -> Self {
        self.density_fallback = false;
        self
    }

    /// 启用内容寻址存储（截断可逆）。目录自动创建。
    pub fn with_ctx_store(mut self, dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).ok();
        self.ctx_store = Some(dir);
        self
    }

    /// 存储目录（expand 工具使用）
    pub fn ctx_store(&self) -> Option<&Path> {
        self.ctx_store.as_deref()
    }

    /// 把完整内容 spill 到存储，返回短句柄（sha256 前 16 位十六进制）。
    /// 同内容只写一次（内容寻址天然去重）。
    pub fn spill(&self, content: &str) -> Option<String> {
        let store = self.ctx_store.as_ref()?;
        let digest = Sha256::digest(content.as_bytes());
        let handle = digest.iter().take(8).map(|b| format!("{b:02x}")).collect::<String>();
        let file = store.join(&handle);
        // 已存在则跳过写入（去重）
        if !file.exists() {
            std::fs::write(&file, content).ok()?;
        }
        Some(handle)
    }

    /// 按句柄取回 spill 的内容
    pub fn retrieve(&self, handle: &str) -> Result<String> {
        let store = self
            .ctx_store
            .as_deref()
            .ok_or_else(|| anyhow!("ctx store not configured"))?;
        if !is_valid_handle(handle) {
            return Err(anyhow!("invalid handle '{handle}' (expected 16-char hex like ctx:0123abcd…)"));
        }
        let file = store.join(handle);
        std::fs::read_to_string(&file)
            .with_context(|| format!("no stored content for handle '{handle}'"))
    }

    /// 追加路径白名单（相对路径基于 workdir 解析）
    pub fn with_allowed_root(mut self, root: impl AsRef<Path>) -> Self {
        let root = if root.as_ref().is_absolute() {
            root.as_ref().to_path_buf()
        } else {
            self.workdir.join(root)
        };
        self.allowed_roots.push(lexical_normalize(&root));
        self
    }

    pub fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// 解析并校验工具访问的路径：
    /// 相对路径基于 workdir；结果必须在白名单内，否则拒绝。
    pub fn resolve_path(&self, input: &str) -> Result<PathBuf> {
        let expanded = expand_tilde(input);
        let raw = Path::new(&expanded);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.workdir.join(raw)
        };
        let normalized = lexical_normalize(&joined);

        let allowed = self.allowed_roots.iter().any(|root| {
            normalized.starts_with(root)
                && (normalized == *root || is_child_of(&normalized, root))
        });
        if !allowed {
            return Err(anyhow!(
                "path '{}' is outside allowed roots",
                normalized.display()
            ));
        }
        // 词法检查挡不住符号链接：再按真实路径校验一次
        let real = real_path(&normalized);
        if !self.real_path_allowed(&real) {
            return Err(anyhow!(
                "path '{}' resolves via symlink to '{}', which is outside allowed roots",
                normalized.display(),
                real.display()
            ));
        }
        Ok(normalized)
    }

    /// 目录遍历（grep/find/index/map）用：条目是指向白名单外的符号链接时返回 false
    pub fn allows_entry(&self, path: &Path) -> bool {
        let is_symlink = std::fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        !is_symlink || self.real_path_allowed(&real_path(path))
    }

    fn real_path_allowed(&self, real: &Path) -> bool {
        self.allowed_roots
            .iter()
            .any(|root| real.starts_with(real_path(root)))
    }

    /// 输出截断：按字节上限在字符边界截断。
    /// 启用 ctx store 时完整内容先 spill（可经 expand 取回），标记携带句柄。
    pub fn truncate_output(&self, output: &str) -> String {
        if output.len() <= self.max_output_bytes {
            return output.to_string();
        }
        let mut cut = self.max_output_bytes;
        while cut > 0 && !output.is_char_boundary(cut) {
            cut -= 1;
        }
        let marker = match self.spill(output) {
            Some(handle) => format!(
                "\n[truncated at {} bytes; full content handle: ctx:{handle} — call the expand tool with this handle to retrieve it]",
                output.len()
            ),
            None => format!(
                "\n[truncated at {} bytes of {}]",
                cut,
                output.len()
            ),
        };
        format!("{}{}", &output[..cut], marker)
    }

    /// 截断并返回压缩元信息：(交付文本, 原始字节数)。
    /// 发生截断时返回 Some(原始字节)，供台账统计。
    pub fn truncate_with_meta(&self, output: &str) -> (String, Option<u64>) {
        if output.len() <= self.max_output_bytes {
            return (output.to_string(), None);
        }
        (
            self.truncate_output(output),
            Some(output.len() as u64),
        )
    }
}

/// 原子写：同目录临时文件 → fsync → rename。崩溃/磁盘满时原文件保持完整，
/// 不会留下被截断的半截文件。保留原文件权限；目标是符号链接时写入其指向的文件。
pub async fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    // 符号链接：替换链接指向的真实文件，而不是把链接本身换成普通文件
    let target = tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.baiji-tmp-{}", std::process::id()));

    let result = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(content).await?;
        file.sync_all().await?;
        drop(file);
        if let Ok(meta) = tokio::fs::metadata(&target).await {
            tokio::fs::set_permissions(&tmp, meta.permissions()).await?;
        }
        tokio::fs::rename(&tmp, &target).await
    }
    .await;
    if result.is_err() {
        tokio::fs::remove_file(&tmp).await.ok();
    }
    result
}

/// 合法句柄：16 位小写十六进制
fn is_valid_handle(handle: &str) -> bool {
    handle.len() == 16
        && handle
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 词法归一化：消除 `.` 与 `..`，不触碰文件系统
/// （`..` 越过根时按根截断，行为等价于 canonicalize 的路径部分）
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                match parts.last() {
                    Some(Component::Normal(_)) => {
                        parts.pop();
                    }
                    // 根目录或空前遇到 .. ：绝对路径截断在根，相对路径保留 ..
                    Some(Component::RootDir) | None => {
                        if !matches!(parts.last(), Some(Component::RootDir)) {
                            parts.push(comp);
                        }
                    }
                    _ => parts.push(comp),
                }
            }
            other => parts.push(other),
        }
    }
    parts.iter().collect()
}

/// 真实路径：canonicalize 最深的已存在祖先（解析其中的符号链接），
/// 再拼回尚不存在的尾部（write 新建文件时目标本身不存在）。
fn real_path(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        // symlink_metadata：悬空符号链接也算"存在"，交给 canonicalize 判定
        if std::fs::symlink_metadata(&existing).is_ok() {
            match existing.canonicalize() {
                Ok(real) => {
                    let mut real = real;
                    real.extend(tail.iter().rev());
                    return real;
                }
                // 悬空链接：无法确认去向，按链接目标的字面路径处理
                Err(_) => {
                    if let Ok(target) = std::fs::read_link(&existing) {
                        let base = existing.parent().unwrap_or(Path::new("/"));
                        let mut real = real_path(&lexical_normalize(&base.join(target)));
                        real.extend(tail.iter().rev());
                        return real;
                    }
                }
            }
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => {
                let mut real = existing;
                real.extend(tail.iter().rev());
                return real;
            }
        }
    }
}

fn is_child_of(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok()
}

/// `~` / `~/x` 展开为 HOME
fn expand_tilde(input: &str) -> String {
    if input == "~" || input.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return input.replacen("~", &home.to_string_lossy(), 1);
        }
    }
    input.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_path_within_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(dir.path());

        // 相对路径 → 基于 workdir
        let p = env.resolve_path("src/main.rs").unwrap();
        assert_eq!(p, dir.path().join("src/main.rs"));

        // ./ 前缀与 .. 回溯仍落在白名单内
        assert!(env.resolve_path("./x/../y").is_ok());

        // 绝对路径 = workdir 本身 OK
        assert!(env.resolve_path(dir.path().to_str().unwrap()).is_ok());
    }

    #[test]
    fn test_resolve_path_denies_escape() {
        let dir = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(dir.path());

        // 逃逸出 workdir
        assert!(env.resolve_path("../../etc/passwd").is_err());
        // 绝对路径指向白名单外
        assert!(env.resolve_path("/etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_path_denies_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s").unwrap();
        let env = ExecutionEnv::new(dir.path());

        // 指向白名单外的目录链接 / 文件链接 / 悬空链接
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("f")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("new.txt"), dir.path().join("dangling"))
            .unwrap();
        assert!(env.resolve_path("out/secret.txt").is_err());
        assert!(env.resolve_path("out/not-yet-created.txt").is_err());
        assert!(env.resolve_path("f").is_err());
        assert!(env.resolve_path("dangling").is_err());
        assert!(!env.allows_entry(&dir.path().join("out")));

        // 白名单内部的链接仍可用
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();
        assert!(env.resolve_path("alias/x.txt").is_ok());
        assert!(env.allows_entry(&dir.path().join("alias")));
        assert!(env.allows_entry(&dir.path().join("real")));
    }

    #[test]
    fn test_extra_allowed_root() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(dir.path()).with_allowed_root(other.path());

        assert!(env.resolve_path(other.path().join("a.txt").to_str().unwrap()).is_ok());
        assert!(env.resolve_path("/etc/hosts").is_err());
    }

    #[test]
    fn test_truncate_output() {
        let env = ExecutionEnv::new(".").with_max_output_bytes(10);
        assert_eq!(env.truncate_output("short"), "short");

        let truncated = env.truncate_output("a very long output line");
        assert!(truncated.starts_with("a very lon"));
        assert!(truncated.contains("[truncated at"));

        // 中文按字符边界截断，不 panic
        let cjk = env.truncate_output("你好世界你好世界");
        assert!(cjk.contains("[truncated"));
    }

    #[test]
    fn test_spill_and_retrieve_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(".").with_ctx_store(dir.path());
        assert!(env.ctx_store().is_some());

        let content = "full content that would be truncated\n".repeat(10);
        let handle = env.spill(&content).expect("spill works");
        assert_eq!(handle.len(), 16);
        // 内容寻址去重：同内容同句柄
        assert_eq!(env.spill(&content).as_deref(), Some(handle.as_str()));
        // 文件存在
        assert!(dir.path().join(&handle).exists());

        assert_eq!(env.retrieve(&handle).unwrap(), content);

        // 截断标记携带句柄
        let small = ExecutionEnv::new(".").with_ctx_store(dir.path()).with_max_output_bytes(10);
        let truncated = small.truncate_output(&content);
        assert!(truncated.contains(&format!("ctx:{handle}")));

        // 截断 + 元信息
        let (delivered, original) = small.truncate_with_meta(&content);
        assert!(delivered.contains("ctx:"));
        assert_eq!(original, Some(content.len() as u64));
        // 未超限时无元信息
        let (delivered, original) = small.truncate_with_meta("tiny");
        assert_eq!(delivered, "tiny");
        assert_eq!(original, None);
    }

    #[test]
    fn test_truncate_without_store_falls_back() {
        let env = ExecutionEnv::new(".").with_max_output_bytes(10);
        let truncated = env.truncate_output("a very long output line");
        // 无存储：丢弃式截断（不含句柄）
        assert!(!truncated.contains("ctx:"));
        assert!(truncated.contains("[truncated"));

        assert!(env.ctx_store().is_none());
        assert!(env.retrieve("0123456789abcdef").is_err());
    }

    #[test]
    fn test_retrieve_rejects_bad_handles() {
        let dir = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(".").with_ctx_store(dir.path());

        assert!(env.retrieve("../etc/passwd").is_err());
        assert!(env.retrieve("short").is_err());
        assert!(env.retrieve("GGGGGGGGGGGGGGGG").is_err()); // 非十六进制
        assert!(env.retrieve("0123456789ABCDEF").is_err()); // 大写
        assert!(env.retrieve("0123456789abcdef").is_err()); // 合法但不存在
    }

    #[test]
    fn test_lexical_normalize() {
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(lexical_normalize(Path::new("a/./b")), PathBuf::from("a/b"));
        // 根之上的 .. 被截断
        assert_eq!(lexical_normalize(Path::new("/..")), PathBuf::from("/"));
    }
}
