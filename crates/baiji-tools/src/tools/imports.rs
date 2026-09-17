//! imports 工具：import 依赖边查询（影响面分析）
//!
//! 基于代码索引的属性图边：
//! - `outgoing`：某文件直接 import 了什么
//! - `incoming`：谁 import 了某文件（改动它会影响谁）

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::sync::Arc;

use crate::env::ExecutionEnv;
use crate::index::CodeIndex;

pub struct ImportsTool {
    env: Arc<ExecutionEnv>,
}

impl ImportsTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }
}

#[async_trait]
impl AgentTool for ImportsTool {
    fn name(&self) -> &str {
        "imports"
    }

    fn description(&self) -> &str {
        "Query import dependency edges of the codebase (impact analysis). \
         direction='outgoing': what a file imports; \
         direction='incoming': which files import the target (who is impacted by changing it). \
         Matching is by module/file name segments."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {"type": "string", "description": "File path or module name to analyze"},
                "direction": {"type": "string", "enum": ["outgoing", "incoming"], "description": "Edge direction (default incoming)"},
                "path": {"type": "string", "description": "Project root to index (default '.')"}
            },
            "required": ["target"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(target) = args["target"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'target'"));
        };
        let direction = args["direction"].as_str().unwrap_or("incoming");
        let root_path = args["path"].as_str().unwrap_or(".");

        let root = match self.env.resolve_path(root_path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        let env = self.env.clone();
        let target = target.to_string();
        let direction = direction.to_string();
        let lines = tokio::task::spawn_blocking(move || -> Vec<String> {
            let index = CodeIndex::build(&root, &env);
            match direction.as_str() {
                "outgoing" => {
                    // 解析目标为已索引文件（路径后缀匹配）
                    let entry = index
                        .entries
                        .iter()
                        .find(|e| path_matches(&e.path, &target));
                    match entry {
                        Some(entry) => {
                            let mut lines = vec![format!(
                                "== {} imports {} module(s) ==",
                                entry.path.display(),
                                entry.imports.len()
                            )];
                            lines.extend(entry.imports.iter().map(|i| format!("  → {i}")));
                            lines
                        }
                        None => vec![format!(
                            "'{target}' is not an indexed file (supported: rs/py/js/ts/go)"
                        )],
                    }
                }
                _ => {
                    // incoming：谁 import 了 target（按名字/路径段匹配）
                    let importers = index.importers_of(&target);
                    let mut lines = vec![format!(
                        "== {} file(s) import '{}'; changing it impacts them ==",
                        importers.len(),
                        target
                    )];
                    lines.extend(
                        importers
                            .iter()
                            .map(|e| format!("  ← {} ({} symbols)", e.path.display(), e.symbols.len())),
                    );
                    lines
                }
            }
            .into_iter()
            .chain(index.truncated.then(|| {
                "[INDEX INCOMPLETE: file limit reached — this list may be missing importers; \
                 narrow 'path' or confirm with grep]"
                    .to_string()
            }))
            .collect()
        })
        .await
        .map_err(|e| anyhow::anyhow!("imports task failed: {e}"))?;

        Ok(ToolOutput::ok(self.env.truncate_output(&lines.join("\n"))))
    }
}

/// 路径后缀匹配（target 可为相对路径或文件名）
fn path_matches(path: &std::path::Path, target: &str) -> bool {
    let target = target.trim_start_matches("./");
    path.display().to_string().ends_with(target)
        || path
            .file_name()
            .map(|n| n.to_string_lossy() == target)
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_imports_incoming_and_outgoing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "mod util;\nuse crate::util::helper;\nfn main() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/util.rs"),
            "pub fn helper() {}\nuse std::fmt;\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/other.rs"),
            "use crate::util::helper;\nfn x() {}\n",
        )
        .unwrap();

        let tool = ImportsTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        // incoming：util 被两个文件引用
        let out = tool
            .execute(serde_json::json!({"target": "util"}))
            .await
            .unwrap();
        assert!(out.content.contains("2 file(s)"), "{}", out.content);
        assert!(out.content.contains("src/main.rs"));
        assert!(out.content.contains("src/other.rs"));

        // outgoing：util.rs 自己 import 了什么
        let out = tool
            .execute(serde_json::json!({"target": "src/util.rs", "direction": "outgoing"}))
            .await
            .unwrap();
        assert!(out.content.contains("std::fmt"), "{}", out.content);

        // 未索引文件
        let out = tool
            .execute(serde_json::json!({"target": "nope.rs", "direction": "outgoing"}))
            .await
            .unwrap();
        assert!(out.content.contains("not an indexed file"));
    }

    #[test]
    fn test_path_matches() {
        assert!(path_matches(std::path::Path::new("/a/b/src/util.rs"), "src/util.rs"));
        assert!(path_matches(std::path::Path::new("/a/b/src/util.rs"), "util.rs"));
        assert!(path_matches(std::path::Path::new("/a/b/src/util.rs"), "./util.rs"));
        assert!(!path_matches(std::path::Path::new("/a/b/util.rs"), "src/util.rs"));
    }
}
