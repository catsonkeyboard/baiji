//! Provider 错误分类与共享 HTTP 客户端
//!
//! 重试决策必须基于 HTTP 状态码 / 传输层错误类型，而不是对错误文本做子串匹配：
//! 429 的响应体里未必有 "429" 字样，而一条含 "rate" 的普通报错也不该被重试。

use std::time::Duration;

/// 厂商 API 返回的非 2xx 响应
#[derive(Debug, Clone)]
pub struct ApiError {
    pub provider: &'static str,
    pub status: u16,
    pub body: String,
    /// `Retry-After` 头（秒）
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} API error (HTTP {}): {}", self.provider, self.status, self.body)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// 从失败的响应构造（读取 Retry-After 与响应体）
    pub async fn from_response(provider: &'static str, response: reqwest::Response) -> Self {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        Self {
            provider,
            status,
            body,
            retry_after,
        }
    }

    /// 408 超时 / 409 冲突 / 425 / 429 限流 / 5xx（含 Anthropic 529 overloaded）
    pub fn is_transient(&self) -> bool {
        matches!(self.status, 408 | 409 | 425 | 429) || self.status >= 500
    }
}

/// 该错误是否值得重试
pub fn is_transient_error(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        if let Some(api) = cause.downcast_ref::<ApiError>() {
            return api.is_transient();
        }
        if let Some(http) = cause.downcast_ref::<reqwest::Error>() {
            // 连接失败 / 超时 / 响应体中途断开可重试；请求构造、重定向、解码错误不可
            return http.is_timeout() || http.is_connect() || http.is_body() || http.is_request();
        }
    }
    // 流内错误事件只有文本（StreamChunk::Error）：仅认厂商明确的过载/限流类型
    let text = error.to_string().to_lowercase();
    ["overloaded", "rate_limit", "rate limit", "server_error", "timeout", "timed out"]
        .iter()
        .any(|kw| text.contains(kw))
}

/// 服务端建议的重试等待（`Retry-After`）
pub fn retry_after(error: &anyhow::Error) -> Option<Duration> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ApiError>())
        .and_then(|api| api.retry_after)
}

/// 共享的 HTTP 客户端配置。
/// - `connect_timeout`：连不上尽快失败
/// - `read_timeout`：**两次读取之间**的空闲超时，而非总时长——SSE 长回答不会被误杀，
///   但卡死的连接会在超时后报错（此前会一直挂到用户手动取消）
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(180))
        .build()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16) -> anyhow::Error {
        anyhow::Error::new(ApiError {
            provider: "test",
            status,
            body: "{}".to_string(),
            retry_after: Some(Duration::from_secs(7)),
        })
    }

    #[test]
    fn test_transient_by_status_not_by_text() {
        for status in [408, 429, 500, 502, 503, 529] {
            assert!(is_transient_error(&api(status)), "{status}");
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(!is_transient_error(&api(status)), "{status}");
        }
        // 带上下文包装后仍能识别
        let wrapped = api(429).context("streaming request failed");
        assert!(is_transient_error(&wrapped));
        assert_eq!(retry_after(&wrapped), Some(Duration::from_secs(7)));

        // 文本里碰巧有 "500"/"connection" 的普通错误不再被重试
        assert!(!is_transient_error(&anyhow::anyhow!("file has 500 lines")));
        assert!(!is_transient_error(&anyhow::anyhow!("invalid connection string in args")));
        // 流内过载事件可重试
        assert!(is_transient_error(&anyhow::anyhow!("Stream error: overloaded_error")));
    }
}
