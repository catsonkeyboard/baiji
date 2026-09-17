//! find 工具：按名称模式查找文件/目录

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::env::ExecutionEnv;

const MAX_RESULTS: usize = 200;

pub struct FindTool {
    env: Arc<ExecutionEnv>,
}

impl FindTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }
}

#[async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files or directories whose name contains a pattern (case-insensitive), \
         starting from a path (default '.'). Optional kind filter: 'file' or 'dir'."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Substring of the name to match (case-insensitive)"},
                "path": {"type": "string", "description": "Directory to search in (default '.')"},
                "kind": {"type": "string", "enum": ["file", "dir"], "description": "Restrict to files or directories (optional)"}
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(pattern) = args["pattern"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'pattern'"));
        };
        let path = args["path"].as_str().unwrap_or(".");
        let kind = args["kind"].as_str();

        let root: PathBuf = match self.env.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        let needle = pattern.to_lowercase();
        let env = self.env.clone();
        let search_root = root.clone();
        let kind = kind.map(str::to_string);
        // 阻塞式目录遍历放到 blocking 线程池
        let (results, capped) = tokio::task::spawn_blocking(move || {
            walk(&search_root, &needle, kind.as_deref(), &env)
        })
        .await
        .map_err(|e| anyhow::anyhow!("find task failed: {e}"))?;

        if results.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "No matches found for '{pattern}' under '{}'",
                root.display()
            )));
        }
        let joined = results.join("\n");
        let (mut delivered, original) = self.env.truncate_with_meta(&joined);
        if capped {
            delivered.push_str(&format!(
                "\n[Results capped at {MAX_RESULTS} — there are more. Use a more specific pattern or path.]"
            ));
        }
        let mut out = ToolOutput::ok(delivered);
        if let Some(bytes) = original {
            out = out.with_original_bytes(bytes);
        }
        Ok(out)
    }
}

/// 返回（匹配路径, 是否因上限提前停止）
fn walk(root: &Path, needle: &str, kind: Option<&str>, env: &ExecutionEnv) -> (Vec<String>, bool) {
    let mut results = Vec::new();
    let Ok(entries) = crate::walk::walk(root, env, None) else {
        return (results, false);
    };
    for entry in entries {
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if !name.to_lowercase().contains(needle) {
            continue;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let kind_ok = match kind {
            Some("file") => !is_dir,
            Some("dir") => is_dir,
            _ => true,
        };
        if !kind_ok {
            continue;
        }
        if results.len() >= MAX_RESULTS {
            return (results, true);
        }
        results.push(entry.path().display().to_string());
    }
    (results, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_find_by_name_and_kind() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/nested/util.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();

        let tool = FindTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        // 大小写不敏感子串匹配
        let out = tool
            .execute(serde_json::json!({"pattern": "UTIL"}))
            .await
            .unwrap();
        assert!(out.content.contains("util.rs"));

        // kind 过滤
        let out = tool
            .execute(serde_json::json!({"pattern": "nested", "kind": "dir"}))
            .await
            .unwrap();
        assert!(out.content.contains("nested"));

        let out = tool
            .execute(serde_json::json!({"pattern": "nothing_matches"}))
            .await
            .unwrap();
        assert!(out.content.contains("No matches"));
    }
}
