//! 目录树遍历（跳过隐藏目录与构建产物）

/// map 视图最多列出的条目数
pub(crate) const MAX_MAP_ENTRIES: usize = 300;

pub(crate) fn walk_map(
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
    items.sort_by_key(|e| {
        (
            e.file_type().map(|t| t.is_file()).unwrap_or(true),
            e.file_name(),
        )
    });
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
                || matches!(
                    name.as_str(),
                    "target" | "node_modules" | "__pycache__" | ".venv"
                )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_walk_map_skips_build_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::create_dir(root.join(".hidden")).unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}").unwrap();
        std::fs::write(root.join("root.txt"), "hi").unwrap();

        let mut out = Vec::new();
        walk_map(root, "", 0, 10, &mut out);
        let joined = out.join("\n");
        assert!(joined.contains("root.txt"));
        assert!(joined.contains("src/"));
        assert!(joined.contains("a.rs"));
        assert!(!joined.contains("target"));
        assert!(!joined.contains(".hidden"));
    }
}
