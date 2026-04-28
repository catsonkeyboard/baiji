//! 工具策略引擎 — Harness Engineering 架构约束层
//!
//! 定义 Agent 的行为边界：哪些工具可用、哪些路径可访问、输出如何截断。
//! 所有工具调用在执行前都经过策略检查。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::warn;

/// 工具策略配置（从 config.json 中加载）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyConfig {
    /// 文件操作允许的根目录列表（内置工具 read/write/grep 受此约束）
    #[serde(default = "default_allowed_paths")]
    pub allowed_paths: Vec<String>,

    /// 单次工具输出最大字节数（超出则截断）
    #[serde(default = "default_max_tool_output_bytes")]
    pub max_tool_output_bytes: usize,

    /// 禁用的工具列表
    #[serde(default)]
    pub blocked_tools: Vec<String>,

    /// 需要用户确认的高危工具列表
    #[serde(default)]
    pub require_confirmation_tools: Vec<String>,

    /// grep 最大递归深度
    #[serde(default = "default_max_search_depth")]
    pub max_search_depth: usize,

    /// 跳过大于此字节数的文件（grep/read）
    #[serde(default = "default_max_file_size")]
    pub max_file_size: usize,
}

fn default_allowed_paths() -> Vec<String> {
    vec!["./".to_string()]
}

fn default_max_tool_output_bytes() -> usize {
    8192 // 8KB
}

fn default_max_search_depth() -> usize {
    10
}

fn default_max_file_size() -> usize {
    1_048_576 // 1MB
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            allowed_paths: default_allowed_paths(),
            max_tool_output_bytes: default_max_tool_output_bytes(),
            blocked_tools: Vec::new(),
            require_confirmation_tools: Vec::new(),
            max_search_depth: default_max_search_depth(),
            max_file_size: default_max_file_size(),
        }
    }
}

/// 策略决策结果
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyDecision {
    /// 允许执行
    Allow,
    /// 拒绝执行，附带原因
    Deny(String),
    /// 需要用户确认（HITL）
    RequireConfirmation,
}

/// 工具策略引擎
pub struct ToolPolicy {
    allowed_paths: Vec<PathBuf>,
    blocked_tools: HashSet<String>,
    max_output_bytes: usize,
    require_confirmation: HashSet<String>,
    max_search_depth: usize,
    max_file_size: usize,
}

impl ToolPolicy {
    /// 从配置创建策略引擎
    pub fn from_config(config: &PolicyConfig) -> Self {
        // 解析 allowed_paths 为绝对路径
        let allowed_paths: Vec<PathBuf> = config
            .allowed_paths
            .iter()
            .map(|p| {
                let path = PathBuf::from(p);
                if path.is_relative() {
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .join(&path)
                } else {
                    path
                }
            })
            .filter_map(|p| p.canonicalize().ok())
            .collect();

        Self {
            allowed_paths,
            blocked_tools: config.blocked_tools.iter().cloned().collect(),
            max_output_bytes: config.max_tool_output_bytes,
            require_confirmation: config.require_confirmation_tools.iter().cloned().collect(),
            max_search_depth: config.max_search_depth,
            max_file_size: config.max_file_size,
        }
    }

    /// 检查工具是否允许执行
    pub fn check_tool(&self, tool_name: &str) -> PolicyDecision {
        // 检查是否被禁用
        if self.blocked_tools.contains(tool_name) {
            return PolicyDecision::Deny(format!("Tool '{}' is blocked by policy", tool_name));
        }

        // 检查是否需要用户确认
        if self.require_confirmation.contains(tool_name) {
            return PolicyDecision::RequireConfirmation;
        }

        PolicyDecision::Allow
    }

    /// 检查文件路径是否在允许的目录范围内
    pub fn check_path(&self, path_str: &str) -> PolicyDecision {
        // 检查路径穿越攻击
        if path_str.contains("..") {
            // 更严格：规范化后再检查
            let path = PathBuf::from(path_str);
            match path.canonicalize() {
                Ok(canonical) => {
                    if !self.is_path_allowed(&canonical) {
                        return PolicyDecision::Deny(format!(
                            "Path '{}' resolves to '{}' which is outside allowed directories",
                            path_str,
                            canonical.display()
                        ));
                    }
                }
                Err(_) => {
                    // 路径不存在，检查父目录（用于 write 创建新文件的场景）
                    if let Some(parent) = path.parent() {
                        match parent.canonicalize() {
                            Ok(canonical_parent) => {
                                if !self.is_path_allowed(&canonical_parent) {
                                    return PolicyDecision::Deny(format!(
                                        "Path '{}' is outside allowed directories",
                                        path_str
                                    ));
                                }
                            }
                            Err(_) => {
                                return PolicyDecision::Deny(format!(
                                    "Cannot resolve path '{}' — parent directory does not exist",
                                    path_str
                                ));
                            }
                        }
                    }
                }
            }
        } else {
            // 无 .. 的路径也需要检查
            let path = PathBuf::from(path_str);
            let resolved = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(&path)
            };

            // 对于已存在的路径，用 canonicalize
            let check_path = resolved.canonicalize().unwrap_or(resolved);
            if !self.is_path_allowed(&check_path) {
                return PolicyDecision::Deny(format!(
                    "Path '{}' is outside allowed directories",
                    path_str
                ));
            }
        }

        PolicyDecision::Allow
    }

    /// 截断工具输出到允许的最大字节数
    pub fn truncate_output(&self, output: &str) -> String {
        if output.len() <= self.max_output_bytes {
            return output.to_string();
        }

        warn!(
            "Tool output truncated: {} -> {} bytes",
            output.len(),
            self.max_output_bytes
        );

        // 找到不超过 max_output_bytes 的最近 char 边界
        let end = (0..=self.max_output_bytes)
            .rev()
            .find(|&i| output.is_char_boundary(i))
            .unwrap_or(0);

        format!(
            "{}...\n[truncated: showing {}/{} bytes]",
            &output[..end],
            end,
            output.len()
        )
    }

    /// 获取最大搜索深度
    pub fn max_search_depth(&self) -> usize {
        self.max_search_depth
    }

    /// 获取最大文件大小
    pub fn max_file_size(&self) -> usize {
        self.max_file_size
    }

    /// 检查路径是否在允许列表中
    fn is_path_allowed(&self, path: &Path) -> bool {
        if self.allowed_paths.is_empty() {
            // 如果没配置白名单，允许所有路径
            return true;
        }
        self.allowed_paths
            .iter()
            .any(|allowed| path.starts_with(allowed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_default_policy() -> ToolPolicy {
        ToolPolicy::from_config(&PolicyConfig::default())
    }

    #[test]
    fn test_blocked_tool() {
        let config = PolicyConfig {
            blocked_tools: vec!["dangerous_tool".to_string()],
            ..Default::default()
        };
        let policy = ToolPolicy::from_config(&config);

        assert_eq!(
            policy.check_tool("dangerous_tool"),
            PolicyDecision::Deny("Tool 'dangerous_tool' is blocked by policy".to_string())
        );
        assert_eq!(policy.check_tool("safe_tool"), PolicyDecision::Allow);
    }

    #[test]
    fn test_require_confirmation() {
        let config = PolicyConfig {
            require_confirmation_tools: vec!["builtin__write".to_string()],
            ..Default::default()
        };
        let policy = ToolPolicy::from_config(&config);

        assert_eq!(
            policy.check_tool("builtin__write"),
            PolicyDecision::RequireConfirmation
        );
        assert_eq!(policy.check_tool("builtin__read"), PolicyDecision::Allow);
    }

    #[test]
    fn test_truncate_output_short() {
        let policy = make_default_policy();
        let short = "Hello, world!";
        assert_eq!(policy.truncate_output(short), short);
    }

    #[test]
    fn test_truncate_output_long() {
        let config = PolicyConfig {
            max_tool_output_bytes: 20,
            ..Default::default()
        };
        let policy = ToolPolicy::from_config(&config);

        let long_output = "a".repeat(100);
        let truncated = policy.truncate_output(&long_output);
        assert!(truncated.contains("[truncated:"));
        assert!(truncated.contains("20/100 bytes"));
    }

    #[test]
    fn test_truncate_output_multibyte() {
        let config = PolicyConfig {
            max_tool_output_bytes: 10,
            ..Default::default()
        };
        let policy = ToolPolicy::from_config(&config);

        // 每个中文字符 3 字节, 10字节 = 3个完整中文字符 + 1字节不够
        let chinese = "你好世界测试";
        let truncated = policy.truncate_output(chinese);
        assert!(truncated.contains("[truncated:"));
        // 确保没有切断多字节字符
        assert!(!truncated.is_empty());
    }

    #[test]
    fn test_path_check_system_dir() {
        let policy = make_default_policy();
        // /etc/ 通常不在 allowed_paths 中
        let result = policy.check_path("/etc/passwd");
        assert!(matches!(result, PolicyDecision::Deny(_)));
    }

    #[test]
    fn test_default_config() {
        let config = PolicyConfig::default();
        assert_eq!(config.max_tool_output_bytes, 8192);
        assert_eq!(config.max_search_depth, 10);
        assert_eq!(config.max_file_size, 1_048_576);
        assert!(config.blocked_tools.is_empty());
    }
}
