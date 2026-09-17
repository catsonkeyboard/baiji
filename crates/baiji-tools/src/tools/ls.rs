//! ls 工具：列出目录内容

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::sync::Arc;

use crate::env::ExecutionEnv;

pub struct LsTool {
    env: Arc<ExecutionEnv>,
}

impl LsTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }
}

#[async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List entries of a directory (default '.'). Directories are prefixed with 'd/'."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list (default '.')"}
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let path = args["path"].as_str().unwrap_or(".");

        let dir = match self.env.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "[Error] reading dir '{}': {e}",
                    dir.display()
                )))
            }
        };

        let mut dirs: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                dirs.push(format!("d/ {name}"));
            } else {
                files.push(name);
            }
        }

        dirs.sort();
        files.sort();

        let mut lines = dirs;
        lines.extend(files);

        if lines.is_empty() {
            return Ok(ToolOutput::ok(format!("(empty directory '{}')", dir.display())));
        }
        let joined = lines.join("\n");
        let (delivered, original) = self.env.truncate_with_meta(&joined);
        let mut out = ToolOutput::ok(delivered);
        if let Some(bytes) = original {
            out = out.with_original_bytes(bytes);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ls_groups_dirs_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("zz_dir")).unwrap();
        std::fs::write(dir.path().join("a_file.txt"), "").unwrap();
        std::fs::write(dir.path().join("m_file.txt"), "").unwrap();

        let tool = LsTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool.execute(serde_json::json!({})).await.unwrap();
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines[0], "d/ zz_dir"); // 目录在前
        assert_eq!(lines[1], "a_file.txt");
        assert_eq!(lines[2], "m_file.txt");
    }

    #[tokio::test]
    async fn test_ls_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tool = LsTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"path": "not_exist"}))
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
