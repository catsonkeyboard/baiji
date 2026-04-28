//! 工具结果验证器 — Harness Engineering 反馈循环层
//!
//! 验证工具执行结果的质量，检测错误和异常，
//! 生成观察提示 (Observation) 辅助 LLM 自我纠正。

/// 验证结果
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationResult {
    /// 结果有效
    Valid,
    /// 空结果
    Empty {
        /// 给 LLM 的建议
        suggestion: String,
    },
    /// 检测到错误
    Error {
        /// 错误摘要
        summary: String,
        /// 建议的纠正方向
        suggestion: String,
    },
    /// 可疑输出（不一定错，但需要 LLM 注意）
    Suspicious {
        /// 警告信息
        warning: String,
    },
}

/// 工具结果验证器
pub struct ToolResultValidator;

impl ToolResultValidator {
    /// 验证工具输出
    pub fn validate(tool_name: &str, result: &str, is_error: bool) -> ValidationResult {
        // 如果执行本身报错，直接归类
        if is_error {
            return Self::validate_error_result(tool_name, result);
        }

        // 空结果检查
        if result.trim().is_empty() {
            return Self::validate_empty_result(tool_name);
        }

        // 按工具类型做特定校验
        match tool_name {
            "builtin__grep" => Self::validate_grep_result(result),
            "builtin__read" => Self::validate_read_result(result),
            "builtin__write" => Self::validate_write_result(result),
            _ => Self::validate_generic_result(result),
        }
    }

    /// 将验证结果转换为注入 LLM 上下文的观察标签
    ///
    /// 返回 None 表示无需注入
    pub fn to_observation(validation: &ValidationResult) -> Option<String> {
        match validation {
            ValidationResult::Valid => None,
            ValidationResult::Empty { suggestion } => {
                Some(format!("[Observation: empty result. {}]", suggestion))
            }
            ValidationResult::Error {
                summary,
                suggestion,
            } => Some(format!(
                "[Observation: error detected — {}. {}]",
                summary, suggestion
            )),
            ValidationResult::Suspicious { warning } => {
                Some(format!("[Observation: {}]", warning))
            }
        }
    }

    // ===== 具体验证逻辑 =====

    fn validate_error_result(tool_name: &str, result: &str) -> ValidationResult {
        let lower = result.to_lowercase();

        if lower.contains("permission denied") || lower.contains("access denied") {
            return ValidationResult::Error {
                summary: "permission denied".to_string(),
                suggestion: format!(
                    "The path may not be accessible. Try a different path or check permissions for '{}'.",
                    tool_name
                ),
            };
        }

        if lower.contains("not found") || lower.contains("no such file") {
            return ValidationResult::Error {
                summary: "file/path not found".to_string(),
                suggestion: "Verify the path exists. Use grep or list the directory first.".to_string(),
            };
        }

        if lower.contains("timeout") || lower.contains("timed out") {
            return ValidationResult::Error {
                summary: "operation timed out".to_string(),
                suggestion: "The operation took too long. Try with a smaller scope or different parameters.".to_string(),
            };
        }

        if lower.contains("policy denied") || lower.contains("blocked") {
            return ValidationResult::Error {
                summary: "blocked by security policy".to_string(),
                suggestion: "This operation is restricted. Try a different approach within allowed directories.".to_string(),
            };
        }

        // 通用错误
        ValidationResult::Error {
            summary: "tool execution failed".to_string(),
            suggestion: "Review the error message and adjust parameters.".to_string(),
        }
    }

    fn validate_empty_result(tool_name: &str) -> ValidationResult {
        let suggestion = match tool_name {
            "builtin__grep" => {
                "No matches found. Consider broadening the search pattern, using a different regex, or checking the search path."
            }
            "builtin__read" => {
                "The file appears to be empty. Verify the file path is correct."
            }
            _ => "The tool returned no output. This may indicate the operation had no results or failed silently.",
        };

        ValidationResult::Empty {
            suggestion: suggestion.to_string(),
        }
    }

    fn validate_grep_result(result: &str) -> ValidationResult {
        // 检查 "No matches" 的特殊情况
        if result.starts_with("No matches found") {
            return ValidationResult::Empty {
                suggestion: "Consider broadening the search pattern, using case-insensitive matching, or checking a different directory.".to_string(),
            };
        }

        // 检查是否被截断
        if result.contains("[truncated:") {
            return ValidationResult::Suspicious {
                warning: "Search results were truncated. The full result set may contain additional relevant matches. Consider narrowing the search.".to_string(),
            };
        }

        ValidationResult::Valid
    }

    fn validate_read_result(result: &str) -> ValidationResult {
        // 检查是否被截断
        if result.contains("[truncated:") {
            return ValidationResult::Suspicious {
                warning: "File content was truncated due to size. Use offset/limit parameters to read specific sections.".to_string(),
            };
        }

        ValidationResult::Valid
    }

    fn validate_write_result(result: &str) -> ValidationResult {
        if result.starts_with("Successfully wrote") {
            return ValidationResult::Valid;
        }

        ValidationResult::Suspicious {
            warning: "Write operation returned unexpected output.".to_string(),
        }
    }

    fn validate_generic_result(result: &str) -> ValidationResult {
        let lower = result.to_lowercase();

        // 检测常见错误模式
        let error_patterns = [
            "error",
            "failed",
            "exception",
            "traceback",
            "panic",
        ];

        // 只有当错误模式出现在开头或独立行时才报警（避免误报）
        let first_line = result.lines().next().unwrap_or("");
        if error_patterns
            .iter()
            .any(|p| first_line.to_lowercase().starts_with(p))
        {
            return ValidationResult::Suspicious {
                warning: format!(
                    "Tool output starts with an error-like pattern: '{}'",
                    if first_line.len() > 80 {
                        format!("{}...", &first_line[..80])
                    } else {
                        first_line.to_string()
                    }
                ),
            };
        }

        // 检查被截断
        if result.contains("[truncated:") {
            return ValidationResult::Suspicious {
                warning: "Tool output was truncated. Consider narrowing the request scope."
                    .to_string(),
            };
        }

        ValidationResult::Valid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_grep_result() {
        let result = "src/main.rs:10: use tokio;";
        let v = ToolResultValidator::validate("builtin__grep", result, false);
        assert_eq!(v, ValidationResult::Valid);
    }

    #[test]
    fn test_empty_grep_result() {
        let result = "No matches found for pattern 'xyz' in 'src/'";
        let v = ToolResultValidator::validate("builtin__grep", result, false);
        assert!(matches!(v, ValidationResult::Empty { .. }));
    }

    #[test]
    fn test_truncated_result() {
        let result = "line1\nline2\n[truncated: showing 100/5000 bytes]";
        let v = ToolResultValidator::validate("builtin__grep", result, false);
        assert!(matches!(v, ValidationResult::Suspicious { .. }));
    }

    #[test]
    fn test_error_permission_denied() {
        let result = "[Error: Permission denied for /etc/shadow]";
        let v = ToolResultValidator::validate("builtin__read", result, true);
        match v {
            ValidationResult::Error { summary, .. } => {
                assert!(summary.contains("permission denied"));
            }
            _ => panic!("Expected Error, got {:?}", v),
        }
    }

    #[test]
    fn test_error_not_found() {
        let result = "[Error reading '/nonexistent': No such file or directory]";
        let v = ToolResultValidator::validate("builtin__read", result, true);
        match v {
            ValidationResult::Error { summary, .. } => {
                assert!(summary.contains("not found"));
            }
            _ => panic!("Expected Error, got {:?}", v),
        }
    }

    #[test]
    fn test_write_success() {
        let result = "Successfully wrote 42 bytes to output.txt";
        let v = ToolResultValidator::validate("builtin__write", result, false);
        assert_eq!(v, ValidationResult::Valid);
    }

    #[test]
    fn test_empty_result() {
        let v = ToolResultValidator::validate("some_tool", "  ", false);
        assert!(matches!(v, ValidationResult::Empty { .. }));
    }

    #[test]
    fn test_observation_none_for_valid() {
        let obs = ToolResultValidator::to_observation(&ValidationResult::Valid);
        assert!(obs.is_none());
    }

    #[test]
    fn test_observation_for_empty() {
        let v = ValidationResult::Empty {
            suggestion: "try something else".to_string(),
        };
        let obs = ToolResultValidator::to_observation(&v).unwrap();
        assert!(obs.contains("empty result"));
        assert!(obs.contains("try something else"));
    }

    #[test]
    fn test_policy_denied_error() {
        let result = "[Policy denied] Path '/etc/passwd' is outside allowed directories";
        let v = ToolResultValidator::validate("builtin__read", result, true);
        match v {
            ValidationResult::Error { summary, .. } => {
                assert!(summary.contains("blocked by security policy"));
            }
            _ => panic!("Expected Error, got {:?}", v),
        }
    }

    #[test]
    fn test_generic_error_pattern() {
        let result = "Error: connection refused\nsome more details";
        let v = ToolResultValidator::validate("tavily.search", result, false);
        assert!(matches!(v, ValidationResult::Suspicious { .. }));
    }
}
