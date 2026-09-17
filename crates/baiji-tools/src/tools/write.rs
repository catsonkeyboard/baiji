//! write 工具：写入文件（自动创建父目录）

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::sync::Arc;
use tokio::fs;

use crate::env::ExecutionEnv;

pub struct WriteTool {
    env: Arc<ExecutionEnv>,
}

impl WriteTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }
}

#[async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating it (and parent directories) if needed. \
         Overwrites existing content."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to write"},
                "content": {"type": "string", "description": "Full content to write"}
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(path) = args["path"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'path'"));
        };
        let Some(content) = args["content"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'content'"));
        };

        let resolved = match self.env.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        if let Some(parent) = resolved.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = fs::create_dir_all(parent).await {
                    return Ok(ToolOutput::err(format!(
                        "[Error] creating directories for '{}': {e}",
                        resolved.display()
                    )));
                }
            }
        }

        let bytes = content.len();
        match crate::env::atomic_write(&resolved, content.as_bytes()).await {
            Ok(()) => Ok(ToolOutput::ok(format!(
                "Wrote {} bytes to {}",
                bytes,
                resolved.display()
            ))),
            Err(e) => Ok(ToolOutput::err(format!(
                "[Error] writing '{}': {e}",
                resolved.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_write_creates_nested_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"path": "src/deep/mod.rs", "content": "fn a() {}"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(dir.path().join("src/deep/mod.rs"))
                .await
                .unwrap(),
            "fn a() {}"
        );

        // 覆盖
        tool.execute(serde_json::json!({"path": "src/deep/mod.rs", "content": "fn b() {}"}))
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("src/deep/mod.rs"))
                .await
                .unwrap(),
            "fn b() {}"
        );
    }

    #[tokio::test]
    async fn test_write_policy_denied() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"path": "/etc/cron.d/evil", "content": "x"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("[Policy denied]"));
    }
}
