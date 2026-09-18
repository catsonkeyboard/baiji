//! grep 工具：递归正则搜索文件内容

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::compressors::search_results;
use crate::env::ExecutionEnv;

/// 最多返回的匹配行数
const MAX_MATCHES: usize = 200;

/// 一次搜索的结果与"结果不完整"的原因（必须如实告诉模型，否则它会把部分结果当全部）
#[derive(Default)]
struct SearchOutcome {
    matches: Vec<String>,
    /// 达到 MAX_MATCHES 上限后提前停止
    capped: bool,
    /// 因超过 max_file_size 被跳过的文件数
    skipped_large: usize,
}

pub struct GrepTool {
    env: Arc<ExecutionEnv>,
}

impl GrepTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regex, recursively from a path (default '.'). \
         Returns 'path:line: text' matches (consecutive matches in the same file are \
         grouped under a 'File: path' header when that is shorter). Respects .gitignore. \
         Optional glob filter (e.g. '*.rs', '*.{ts,tsx}', 'src/**/*.rs')."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for"},
                "path": {"type": "string", "description": "File or directory to search (default '.')"},
                "glob": {"type": "string", "description": "Glob filter, e.g. '*.rs', '*.{ts,tsx}', 'src/**/*.rs' (optional)"}
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(pattern) = args["pattern"].as_str() else {
            return Ok(ToolOutput::err(
                "[Error] missing required argument 'pattern'",
            ));
        };
        let path = args["path"].as_str().unwrap_or(".");
        let glob = args["glob"].as_str().map(str::to_string);

        let regex = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "[Error] invalid regex '{pattern}': {e}"
                )));
            }
        };

        let root: PathBuf = match self.env.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        let env = self.env.clone();
        let pattern = pattern.to_string();
        let search_root = root.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let regex = Regex::new(&pattern).expect("validated above");
            search(&search_root, &regex, glob.as_deref(), &env)
        })
        .await
        .map_err(|e| anyhow::anyhow!("grep task failed: {e}"))?;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => return Ok(ToolOutput::err(format!("[Error] {e}"))),
        };

        let mut notes = Vec::new();
        if outcome.capped {
            notes.push(format!(
                "[Results capped at {MAX_MATCHES} matches — there are more. Narrow the pattern, path or glob.]"
            ));
        }
        if outcome.skipped_large > 0 {
            notes.push(format!(
                "[{} file(s) larger than {} bytes were NOT searched]",
                outcome.skipped_large, self.env.max_file_size
            ));
        }

        let matches = outcome.matches;
        if matches.is_empty() {
            let mut message = format!(
                "No matches found for pattern '{regex}' in '{}'",
                root.display()
            );
            for note in &notes {
                message.push('\n');
                message.push_str(note);
            }
            return Ok(ToolOutput::ok(message));
        }
        let (mut delivered, original) = self
            .env
            .truncate_with_meta(&search_results::render(&matches));
        // 提示放在截断之后追加，保证一定可见
        for note in &notes {
            delivered.push('\n');
            delivered.push_str(note);
        }
        let mut out = ToolOutput::ok(delivered);
        if let Some(bytes) = original {
            out = out.with_original_bytes(bytes);
        }
        Ok(out)
    }
}

fn search(
    root: &Path,
    regex: &Regex,
    glob: Option<&str>,
    env: &ExecutionEnv,
) -> std::result::Result<SearchOutcome, String> {
    let mut outcome = SearchOutcome::default();

    // 单文件：直接搜（不受 glob / gitignore 影响——用户点名要搜它）
    if root.is_file() {
        search_file(root, regex, env, &mut outcome);
        return Ok(outcome);
    }

    for entry in crate::walk::walk(root, env, glob)? {
        if outcome.capped {
            break;
        }
        if entry.path().is_file() {
            search_file(entry.path(), regex, env, &mut outcome);
        }
    }
    Ok(outcome)
}

fn search_file(path: &Path, regex: &Regex, env: &ExecutionEnv, outcome: &mut SearchOutcome) {
    if let Ok(meta) = path.metadata()
        && meta.len() > env.max_file_size
    {
        outcome.skipped_large += 1;
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return; // 二进制或不可读
    };
    for (i, line) in content.lines().enumerate() {
        if regex.is_match(line) {
            if outcome.matches.len() >= MAX_MATCHES {
                outcome.capped = true;
                return;
            }
            outcome
                .matches
                .push(format!("{}:{}: {}", path.display(), i + 1, line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "alpha mentioned in text\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "alpha should be skipped\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn test_grep_recursive_and_skips_hidden() {
        let dir = setup().await;
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"pattern": "alpha"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("src/a.rs:1: fn alpha() {}"));
        assert!(out.content.contains("b.txt:1: alpha mentioned in text"));
        assert!(!out.content.contains(".git")); // 隐藏目录被跳过
    }

    #[tokio::test]
    async fn test_grep_glob_filter() {
        let dir = setup().await;
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"pattern": "alpha", "glob": "*.rs"}))
            .await
            .unwrap();
        assert!(out.content.contains("a.rs"));
        assert!(!out.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn test_grep_invalid_regex_and_no_match() {
        let dir = setup().await;
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"pattern": "(unclosed"}))
            .await
            .unwrap();
        assert!(out.is_error);

        let out = tool
            .execute(serde_json::json!({"pattern": "zzz_not_there"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("No matches"));
    }

    #[tokio::test]
    async fn test_grep_gitignore_full_glob_and_cap_notice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.path().join("generated")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(dir.path().join("generated/out.ts"), "needle\n").unwrap();
        std::fs::write(dir.path().join("src/a.ts"), "needle\n").unwrap();
        std::fs::write(dir.path().join("src/deep/b.tsx"), "needle\n").unwrap();
        std::fs::write(dir.path().join("src/c.rs"), "needle\n").unwrap();
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        // .gitignore 生效（不要求是 git 仓库）
        let out = tool
            .execute(serde_json::json!({"pattern": "needle"}))
            .await
            .unwrap();
        assert!(!out.content.contains("generated"), "{}", out.content);
        assert!(out.content.contains("c.rs"));

        // 花括号与 ** glob
        let out = tool
            .execute(serde_json::json!({"pattern": "needle", "glob": "*.{ts,tsx}"}))
            .await
            .unwrap();
        assert!(out.content.contains("a.ts") && out.content.contains("b.tsx"));
        assert!(!out.content.contains("c.rs"));
        let out = tool
            .execute(serde_json::json!({"pattern": "needle", "glob": "src/deep/**"}))
            .await
            .unwrap();
        assert!(out.content.contains("b.tsx") && !out.content.contains("a.ts"));

        // 达到上限必须明示
        let many = "needle\n".repeat(MAX_MATCHES + 50);
        std::fs::write(dir.path().join("src/many.rs"), many).unwrap();
        let out = tool
            .execute(serde_json::json!({"pattern": "needle", "glob": "many.rs"}))
            .await
            .unwrap();
        assert!(out.content.contains("Results capped"), "{}", out.content);
    }

    #[tokio::test]
    async fn test_grep_groups_same_file_matches() {
        let dir = tempfile::tempdir().unwrap();
        let content = (1..=12)
            .map(|i| format!("needle line {i}\n"))
            .collect::<String>();
        std::fs::write(dir.path().join("src.txt"), content).unwrap();
        let tool = GrepTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"pattern": "needle", "glob": "src.txt"}))
            .await
            .unwrap();
        // ≥8 条同文件匹配：归组为 File: 头 + 行号行（路径只出现一次）
        assert!(out.content.starts_with("File: "), "{}", out.content);
        assert!(
            out.content.contains("src.txt\n1: needle line 1"),
            "{}",
            out.content
        );
        assert!(out.content.contains("3: needle line 3"), "{}", out.content);
        assert!(
            out.content.contains("12: needle line 12"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("src.txt:3:"), "{}", out.content);
        assert_eq!(out.content.matches("src.txt").count(), 1);
    }

    #[tokio::test]
    async fn test_grep_reports_skipped_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = ExecutionEnv::new(dir.path());
        env.max_file_size = 10;
        std::fs::write(dir.path().join("big.txt"), "needle in a big file\n").unwrap();
        let tool = GrepTool::new(Arc::new(env));

        let out = tool
            .execute(serde_json::json!({"pattern": "needle"}))
            .await
            .unwrap();
        assert!(out.content.contains("No matches"));
        assert!(out.content.contains("NOT searched"), "{}", out.content);
    }
}
