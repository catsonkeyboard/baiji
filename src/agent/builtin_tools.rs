//! 内置工具：grep、read、write
//!
//! 无需 MCP 服务器，Agent 可直接操作本地文件系统。
//! 所有文件操作均受 ToolPolicy 策略约束。

use crate::agent::tool_policy::ToolPolicy;
use crate::llm::ToolDefinition;
use regex::Regex;
use serde_json::{json, Value};
use std::path::Path;
use tokio::fs;
use tracing::warn;

/// 返回所有内置工具的定义
pub fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "builtin__grep".to_string(),
            description: "Search for a regex pattern in files. Returns matching lines with file path and line number.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory path to search in"
                    },
                    "glob": {
                        "type": "string",
                        "description": "Optional filename glob filter (e.g. *.rs)"
                    }
                },
                "required": ["pattern", "path"]
            }),
        },
        ToolDefinition {
            name: "builtin__read".to_string(),
            description: "Read the contents of a file, optionally with line offset and limit.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Starting line number (1-based, optional)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return (optional)"
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "builtin__write".to_string(),
            description: "Write content to a file, creating it if it does not exist.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path to write to"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        },
    ]
}

/// 分发内置工具调用（带策略检查）
pub async fn execute_builtin_tool(name: &str, args: &Value, policy: &ToolPolicy) -> String {
    match name {
        "builtin__grep" => grep_tool(args, policy).await,
        "builtin__read" => read_tool(args, policy).await,
        "builtin__write" => write_tool(args, policy).await,
        _ => format!("[Unknown builtin tool: {}]", name),
    }
}

async fn grep_tool(args: &Value, policy: &ToolPolicy) -> String {
    let pattern = match args["pattern"].as_str() {
        Some(p) => p,
        None => return "[Error: missing required argument 'pattern']".to_string(),
    };
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return "[Error: missing required argument 'path']".to_string(),
    };
    let glob_filter = args["glob"].as_str();

    // 策略检查：路径是否允许
    if let crate::agent::tool_policy::PolicyDecision::Deny(reason) = policy.check_path(path) {
        return format!("[Policy denied] {}", reason);
    }

    let regex = match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return format!("[Error: invalid regex '{}': {}]", pattern, e),
    };

    let path_owned = path.to_string();
    let glob_filter = glob_filter.map(|s| s.to_string());
    let regex_pattern = pattern.to_string();
    let max_depth = policy.max_search_depth();
    let max_file_size = policy.max_file_size();

    let result = tokio::task::spawn_blocking(move || {
        search_path(
            &path_owned,
            &Regex::new(&regex_pattern).unwrap(),
            glob_filter.as_deref(),
            max_depth,
            max_file_size,
        )
    })
    .await;

    match result {
        Ok(matches) => {
            if matches.is_empty() {
                format!("No matches found for pattern '{}' in '{}'", regex, path)
            } else {
                let output = matches.join("\n");
                policy.truncate_output(&output)
            }
        }
        Err(e) => format!("[Error running grep: {}]", e),
    }
}

fn search_path(
    path: &str,
    regex: &Regex,
    glob_filter: Option<&str>,
    max_depth: usize,
    max_file_size: usize,
) -> Vec<String> {
    let mut results = Vec::new();
    let p = Path::new(path);

    if p.is_file() {
        search_file(p, regex, &mut results, max_file_size);
    } else if p.is_dir() {
        search_dir(p, regex, glob_filter, &mut results, 0, max_depth, max_file_size);
    }

    results
}

fn search_dir(
    dir: &Path,
    regex: &Regex,
    glob_filter: Option<&str>,
    results: &mut Vec<String>,
    current_depth: usize,
    max_depth: usize,
    max_file_size: usize,
) {
    // 深度限制
    if current_depth >= max_depth {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!("Cannot read directory {:?}: {}", dir, e);
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip hidden directories
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.'))
                .unwrap_or(false)
            {
                continue;
            }
            search_dir(
                &path,
                regex,
                glob_filter,
                results,
                current_depth + 1,
                max_depth,
                max_file_size,
            );
        } else if path.is_file() {
            if let Some(filter) = glob_filter {
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if !matches_glob(filename, filter) {
                    continue;
                }
            }
            search_file(&path, regex, results, max_file_size);
        }

        if results.len() >= 200 {
            return;
        }
    }
}

fn matches_glob(filename: &str, pattern: &str) -> bool {
    // Simple glob: support leading * and suffix matching (e.g. *.rs)
    if let Some(suffix) = pattern.strip_prefix('*') {
        filename.ends_with(suffix)
    } else {
        filename == pattern
    }
}

fn search_file(path: &Path, regex: &Regex, results: &mut Vec<String>, max_file_size: usize) {
    // 检查文件大小，跳过过大的文件
    if let Ok(metadata) = path.metadata() {
        if metadata.len() as usize > max_file_size {
            return; // 静默跳过大文件
        }
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return, // Skip binary or unreadable files
    };

    for (i, line) in content.lines().enumerate() {
        if regex.is_match(line) {
            results.push(format!("{}:{}: {}", path.display(), i + 1, line));
            if results.len() >= 200 {
                return;
            }
        }
    }
}

async fn read_tool(args: &Value, policy: &ToolPolicy) -> String {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return "[Error: missing required argument 'path']".to_string(),
    };
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().map(|l| l as usize);

    // 策略检查：路径是否允许
    if let crate::agent::tool_policy::PolicyDecision::Deny(reason) = policy.check_path(path) {
        return format!("[Policy denied] {}", reason);
    }

    // 检查文件大小
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() as usize > policy.max_file_size() => {
            return format!(
                "[Error: file '{}' is too large ({} bytes, max {} bytes)]",
                path,
                meta.len(),
                policy.max_file_size()
            );
        }
        Err(e) => return format!("[Error reading '{}': {}]", path, e),
        _ => {}
    }

    match fs::read_to_string(path).await {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = (offset - 1).min(lines.len());
            let end = match limit {
                Some(l) => (start + l).min(lines.len()),
                None => lines.len(),
            };
            let output = lines[start..end]
                .iter()
                .enumerate()
                .map(|(i, line)| format!("{}\t{}", start + i + 1, line))
                .collect::<Vec<_>>()
                .join("\n");
            policy.truncate_output(&output)
        }
        Err(e) => format!("[Error reading '{}': {}]", path, e),
    }
}

async fn write_tool(args: &Value, policy: &ToolPolicy) -> String {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return "[Error: missing required argument 'path']".to_string(),
    };
    let content = match args["content"].as_str() {
        Some(c) => c,
        None => return "[Error: missing required argument 'content']".to_string(),
    };

    // 策略检查：路径是否允许
    if let crate::agent::tool_policy::PolicyDecision::Deny(reason) = policy.check_path(path) {
        return format!("[Policy denied] {}", reason);
    }

    // Create parent directories if needed
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = fs::create_dir_all(parent).await {
                return format!("[Error creating directories for '{}': {}]", path, e);
            }
        }
    }

    let bytes = content.len();
    match fs::write(path, content).await {
        Ok(()) => format!("Successfully wrote {} bytes to {}", bytes, path),
        Err(e) => format!("[Error writing '{}': {}]", path, e),
    }
}
