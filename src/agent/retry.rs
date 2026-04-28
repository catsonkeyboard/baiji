//! 重试与降级引擎 — Harness Engineering 容错层
//!
//! 提供 LLM 调用和工具执行的自动重试与降级逻辑。
//! 区分"可重试"错误（网络/限流/超时）和"不可重试"错误（无效参数/授权失败）。

use std::time::Duration;
use tracing::{info, warn};

/// 重试策略配置
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// 最大重试次数
    pub max_retries: u32,
    /// 初始退避时间（毫秒）
    pub initial_backoff_ms: u64,
    /// 退避倍数
    pub backoff_multiplier: f64,
    /// 最大退避时间（毫秒）
    pub max_backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_backoff_ms: 500,
            backoff_multiplier: 2.0,
            max_backoff_ms: 5000,
        }
    }
}

/// 重试决策
#[derive(Debug, Clone, PartialEq)]
pub enum RetryDecision {
    /// 执行重试，附带等待时间
    Retry { delay: Duration, attempt: u32 },
    /// 不重试，直接失败
    Fail,
}

/// 错误分类
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorCategory {
    /// 可重试：网络错误、限流、超时、5xx
    Transient,
    /// 不可重试：参数错误、授权失败、404
    Permanent,
    /// 未知：默认不重试
    Unknown,
}

impl RetryPolicy {
    /// 判断是否应该重试
    pub fn should_retry(&self, attempt: u32, error: &str) -> RetryDecision {
        if attempt >= self.max_retries {
            return RetryDecision::Fail;
        }

        let category = Self::classify_error(error);
        match category {
            ErrorCategory::Transient => {
                let delay_ms = (self.initial_backoff_ms as f64
                    * self.backoff_multiplier.powi(attempt as i32))
                    as u64;
                let delay_ms = delay_ms.min(self.max_backoff_ms);
                RetryDecision::Retry {
                    delay: Duration::from_millis(delay_ms),
                    attempt: attempt + 1,
                }
            }
            ErrorCategory::Permanent | ErrorCategory::Unknown => RetryDecision::Fail,
        }
    }

    /// 错误分类：判断错误是否为瞬态错误
    fn classify_error(error: &str) -> ErrorCategory {
        let lower = error.to_lowercase();

        // 可重试的瞬态错误模式
        let transient_patterns = [
            "timeout",
            "timed out",
            "connection refused",
            "connection reset",
            "broken pipe",
            "rate limit",
            "too many requests",
            "429",
            "500",
            "502",
            "503",
            "504",
            "service unavailable",
            "internal server error",
            "overloaded",
            "temporarily unavailable",
            "network",
            "dns",
        ];

        if transient_patterns.iter().any(|p| lower.contains(p)) {
            return ErrorCategory::Transient;
        }

        // 明确的永久性错误
        let permanent_patterns = [
            "authentication",
            "unauthorized",
            "401",
            "403",
            "forbidden",
            "invalid api key",
            "invalid_api_key",
            "not found",
            "404",
            "invalid request",
            "invalid_request",
            "malformed",
        ];

        if permanent_patterns.iter().any(|p| lower.contains(p)) {
            return ErrorCategory::Permanent;
        }

        ErrorCategory::Unknown
    }
}

/// 带重试的异步执行器
///
/// 对任意 async 函数应用重试策略。
pub async fn execute_with_retry<F, Fut, T>(
    policy: &RetryPolicy,
    operation_name: &str,
    mut f: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut attempt = 0u32;

    loop {
        match f().await {
            Ok(result) => return Ok(result),
            Err(error) => {
                match policy.should_retry(attempt, &error) {
                    RetryDecision::Retry { delay, attempt: next } => {
                        warn!(
                            "{} failed (attempt {}/{}): {}. Retrying in {:?}...",
                            operation_name,
                            attempt + 1,
                            policy.max_retries + 1,
                            error,
                            delay
                        );
                        tokio::time::sleep(delay).await;
                        attempt = next;
                    }
                    RetryDecision::Fail => {
                        info!(
                            "{} failed permanently (attempt {}/{}): {}",
                            operation_name,
                            attempt + 1,
                            policy.max_retries + 1,
                            error
                        );
                        return Err(error);
                    }
                }
            }
        }
    }
}

/// 工具连续失败追踪器
///
/// 追踪每个工具的连续失败次数，超过阈值时标记为不可用。
pub struct ToolFailureTracker {
    failures: std::collections::HashMap<String, u32>,
    threshold: u32,
}

impl ToolFailureTracker {
    pub fn new(threshold: u32) -> Self {
        Self {
            failures: std::collections::HashMap::new(),
            threshold,
        }
    }

    /// 记录工具执行成功，重置计数
    pub fn record_success(&mut self, tool_name: &str) {
        self.failures.remove(tool_name);
    }

    /// 记录工具执行失败，返回是否应该禁用该工具
    pub fn record_failure(&mut self, tool_name: &str) -> bool {
        let count = self.failures.entry(tool_name.to_string()).or_insert(0);
        *count += 1;
        *count >= self.threshold
    }

    /// 检查工具是否可用
    pub fn is_available(&self, tool_name: &str) -> bool {
        self.failures
            .get(tool_name)
            .map(|c| *c < self.threshold)
            .unwrap_or(true)
    }

    /// 获取工具的连续失败次数
    pub fn failure_count(&self, tool_name: &str) -> u32 {
        *self.failures.get(tool_name).unwrap_or(&0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_policy_default() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 2);
        assert_eq!(policy.initial_backoff_ms, 500);
    }

    #[test]
    fn test_classify_transient_errors() {
        assert_eq!(
            RetryPolicy::classify_error("connection timeout"),
            ErrorCategory::Transient
        );
        assert_eq!(
            RetryPolicy::classify_error("rate limit exceeded (429)"),
            ErrorCategory::Transient
        );
        assert_eq!(
            RetryPolicy::classify_error("HTTP 503 service unavailable"),
            ErrorCategory::Transient
        );
    }

    #[test]
    fn test_classify_permanent_errors() {
        assert_eq!(
            RetryPolicy::classify_error("authentication_error: invalid api key"),
            ErrorCategory::Permanent
        );
        assert_eq!(
            RetryPolicy::classify_error("HTTP 401 Unauthorized"),
            ErrorCategory::Permanent
        );
    }

    #[test]
    fn test_classify_unknown_errors() {
        assert_eq!(
            RetryPolicy::classify_error("something went wrong"),
            ErrorCategory::Unknown
        );
    }

    #[test]
    fn test_should_retry_transient() {
        let policy = RetryPolicy::default();
        let decision = policy.should_retry(0, "connection timeout");
        match decision {
            RetryDecision::Retry { delay, attempt } => {
                assert_eq!(attempt, 1);
                assert!(delay.as_millis() >= 500);
            }
            _ => panic!("Expected Retry decision"),
        }
    }

    #[test]
    fn test_should_retry_max_reached() {
        let policy = RetryPolicy::default();
        let decision = policy.should_retry(2, "connection timeout");
        assert_eq!(decision, RetryDecision::Fail);
    }

    #[test]
    fn test_should_retry_permanent() {
        let policy = RetryPolicy::default();
        let decision = policy.should_retry(0, "401 Unauthorized");
        assert_eq!(decision, RetryDecision::Fail);
    }

    #[test]
    fn test_backoff_increase() {
        let policy = RetryPolicy::default();

        let d1 = match policy.should_retry(0, "timeout") {
            RetryDecision::Retry { delay, .. } => delay,
            _ => panic!("Expected retry"),
        };

        let d2 = match policy.should_retry(1, "timeout") {
            RetryDecision::Retry { delay, .. } => delay,
            _ => panic!("Expected retry"),
        };

        assert!(d2 > d1, "Backoff should increase: {:?} vs {:?}", d1, d2);
    }

    #[test]
    fn test_failure_tracker() {
        let mut tracker = ToolFailureTracker::new(3);

        assert!(tracker.is_available("my_tool"));
        assert!(!tracker.record_failure("my_tool"));
        assert!(!tracker.record_failure("my_tool"));
        assert!(tracker.record_failure("my_tool")); // 3rd failure = disabled
        assert!(!tracker.is_available("my_tool"));

        // Success resets
        tracker.record_success("my_tool");
        assert!(tracker.is_available("my_tool"));
        assert_eq!(tracker.failure_count("my_tool"), 0);
    }

    #[tokio::test]
    async fn test_execute_with_retry_success() {
        let policy = RetryPolicy::default();
        let result = execute_with_retry(&policy, "test_op", || async { Ok::<_, String>(42) }).await;
        assert_eq!(result, Ok(42));
    }

    #[tokio::test]
    async fn test_execute_with_retry_permanent_fail() {
        let policy = RetryPolicy::default();
        let result = execute_with_retry(&policy, "test_op", || async {
            Err::<i32, _>("401 Unauthorized".to_string())
        })
        .await;
        assert!(result.is_err());
    }
}
