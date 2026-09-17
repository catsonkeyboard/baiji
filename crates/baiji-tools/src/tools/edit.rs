//! edit 工具：精确字符串替换（old_string 必须唯一，或显式 replace_all）

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::sync::Arc;
use tokio::fs;

use crate::env::ExecutionEnv;

pub struct EditTool {
    env: Arc<ExecutionEnv>,
}

impl EditTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact string. old_string must match exactly once \
         (include surrounding context to disambiguate), or set replace_all=true."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to edit"},
                "old_string": {"type": "string", "description": "Exact text to replace"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(path) = args["path"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'path'"));
        };
        let Some(old_string) = args["old_string"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'old_string'"));
        };
        let Some(new_string) = args["new_string"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'new_string'"));
        };
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);

        let resolved = match self.env.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        let content = match fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "[Error] reading '{}': {e}",
                    resolved.display()
                )))
            }
        };

        if old_string.is_empty() {
            return Ok(ToolOutput::err(
                "[Error] old_string must not be empty (use the write tool to create a file)",
            ));
        }
        if old_string == new_string {
            return Ok(ToolOutput::err(
                "[Error] old_string and new_string are identical; nothing to change",
            ));
        }

        // CRLF 文件：read 工具按行展示时已去掉 \r，模型给出的多行 old_string 只含 \n。
        // 直接匹配不上时，按文件的换行风格转换后重试（new_string 同步转换，保持风格一致）
        let (old_crlf, new_crlf);
        let (old_string, new_string) = if !content.contains(old_string)
            && content.contains("\r\n")
            && old_string.contains('\n')
            && !old_string.contains('\r')
        {
            old_crlf = old_string.replace('\n', "\r\n");
            new_crlf = new_string.replace("\r\n", "\n").replace('\n', "\r\n");
            (old_crlf.as_str(), new_crlf.as_str())
        } else {
            (old_string, new_string)
        };

        let count = content.matches(old_string).count();
        if count == 0 {
            return Ok(ToolOutput::err(format!(
                "[Error] old_string not found in '{}'. Verify the exact text (including whitespace).",
                resolved.display()
            )));
        }
        if count > 1 && !replace_all {
            return Ok(ToolOutput::err(format!(
                "[Error] old_string matches {} locations in '{}'. Add surrounding context to make it unique, or set replace_all=true.",
                count,
                resolved.display()
            )));
        }

        let updated = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        match crate::env::atomic_write(&resolved, updated.as_bytes()).await {
            Ok(()) => Ok(ToolOutput::ok(format!(
                "Edited {}: replaced {} occurrence(s)",
                resolved.display(),
                if replace_all { count } else { 1 }
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

    async fn setup(content: &str) -> (tempfile::TempDir, EditTool) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.rs"), content).await.unwrap();
        let tool = EditTool::new(Arc::new(ExecutionEnv::new(dir.path())));
        (dir, tool)
    }

    #[tokio::test]
    async fn test_edit_unique_replacement() {
        let (dir, tool) = setup("fn main() {\n    println!(\"hi\");\n}\n").await;
        tool.execute(serde_json::json!({
            "path": "f.rs",
            "old_string": "println!(\"hi\");",
            "new_string": "println!(\"bye\");"
        }))
        .await
        .unwrap();

        let updated = fs::read_to_string(dir.path().join("f.rs")).await.unwrap();
        assert!(updated.contains("bye"));
        assert!(!updated.contains("hi"));
    }

    #[tokio::test]
    async fn test_edit_ambiguous_requires_replace_all() {
        let (_dir, tool) = setup("let a = 1;\nlet b = 1;\n").await;

        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "old_string": "1",
                "new_string": "2"
            }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("2 locations"));

        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "old_string": "1",
                "new_string": "2",
                "replace_all": true
            }))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("2 occurrence(s)"));
    }

    #[tokio::test]
    async fn test_edit_not_found() {
        let (_dir, tool) = setup("content\n").await;
        let out = tool
            .execute(serde_json::json!({
                "path": "f.rs",
                "old_string": "nope",
                "new_string": "x"
            }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not found"));
    }

    #[tokio::test]
    async fn test_edit_crlf_file_with_lf_old_string() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("win.txt");
        std::fs::write(&file, "line one\r\nline two\r\nline three\r\n").unwrap();
        let tool = EditTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({
                "path": "win.txt",
                "old_string": "line one\nline two",
                "new_string": "LINE 1\nLINE 2"
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // 换行风格保持 CRLF
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "LINE 1\r\nLINE 2\r\nline three\r\n"
        );
    }

    #[tokio::test]
    async fn test_edit_rejects_empty_and_identical() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "abc").unwrap();
        let tool = EditTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        for (old, new) in [("", "x"), ("abc", "abc")] {
            let out = tool
                .execute(serde_json::json!({
                    "path": "a.txt", "old_string": old, "new_string": new, "replace_all": true
                }))
                .await
                .unwrap();
            assert!(out.is_error);
        }
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "abc");
        // 原子写不留临时文件
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("baiji-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
