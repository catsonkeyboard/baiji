//! expand 工具：凭句柄取回被截断的完整工具输出（CCR 可逆压缩）
//!
//! 配合 ExecutionEnv 的内容寻址存储：read/grep/bash 等输出超限截断时，
//! 完整内容 spill 到 `~/.baiji/ctx-store/<sha256-16>`，标记携带
//! `ctx:<handle>`；LLM 调用本工具即可按需取回（支持 offset/limit 分页）。

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::sync::Arc;

use crate::env::ExecutionEnv;

pub struct ExpandTool {
    env: Arc<ExecutionEnv>,
}

impl ExpandTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }

    /// 规范化句柄：接受 `ctx:xxxx` 或裸 16 位十六进制
    fn normalize_handle(raw: &str) -> String {
        raw.trim()
            .strip_prefix("ctx:")
            .unwrap_or(raw.trim())
            .to_string()
    }
}

#[async_trait]
impl AgentTool for ExpandTool {
    fn name(&self) -> &str {
        "expand"
    }

    fn description(&self) -> &str {
        "Retrieve the FULL content of a previously truncated tool output by its handle \
         (the `ctx:...` value from a [truncated] marker). Supports offset/limit paging."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string", "description": "Handle from a truncation marker, e.g. 'ctx:0123abcdef012345'"},
                "offset": {"type": "integer", "description": "Starting line number (1-based, optional)"},
                "limit": {"type": "integer", "description": "Maximum number of lines to return (optional)"}
            },
            "required": ["handle"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(raw) = args["handle"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'handle'"));
        };
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().map(|l| l as usize);

        let content = match self.env.retrieve(&Self::normalize_handle(raw)) {
            Ok(content) => content,
            Err(e) => return Ok(ToolOutput::err(format!("[Error] {e}"))),
        };

        let lines: Vec<&str> = content.lines().collect();
        let start = (offset - 1).min(lines.len());
        let end = limit.map(|l| start.saturating_add(l).min(lines.len())).unwrap_or(lines.len());
        let numbered = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}\t{}", start + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        let (delivered, original) = self.env.truncate_with_meta(&numbered);
        let mut output = ToolOutput::ok(delivered);
        if let Some(bytes) = original {
            output = output.with_original_bytes(bytes);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_expand_roundtrip_with_paging() {
        let dir = tempfile::tempdir().unwrap();
        let env = Arc::new(ExecutionEnv::new(".").with_ctx_store(dir.path()));
        let content = (1..=20)
            .map(|i| format!("line-{i}\n"))
            .collect::<String>();
        let handle = env.spill(&content).unwrap();

        let tool = ExpandTool::new(env);

        // ctx: 前缀或裸句柄均可
        let out = tool
            .execute(serde_json::json!({"handle": format!("ctx:{handle}")}))
            .await
            .unwrap();
        assert!(out.content.contains("1\tline-1"));
        assert!(out.content.contains("20\tline-20"));

        let out = tool
            .execute(serde_json::json!({"handle": handle, "offset": 5, "limit": 2}))
            .await
            .unwrap();
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines[0], "5\tline-5");
        assert_eq!(lines[1], "6\tline-6");
    }

    #[tokio::test]
    async fn test_expand_bad_handle() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ExpandTool::new(Arc::new(ExecutionEnv::new(".").with_ctx_store(dir.path())));

        let out = tool
            .execute(serde_json::json!({"handle": "../etc/passwd"}))
            .await
            .unwrap();
        assert!(out.is_error);

        let out = tool
            .execute(serde_json::json!({"handle": "ffffffffffffffff"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no stored content"));

        let out = tool
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn test_expand_without_store() {
        let tool = ExpandTool::new(Arc::new(ExecutionEnv::new(".")));
        let out = tool
            .execute(serde_json::json!({"handle": "0123456789abcdef"}))
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
