//! OpenAI Provider
//!
//! 同时支持两种 OpenAI 接口协议，通过 `ApiType` 选择：
//! - `ChatCompletions`：Chat Completions API，`POST {base_url}/v1/chat/completions`
//! - `Responses`：Responses API，`POST {base_url}/v1/responses`

pub mod chat_completions;
pub mod responses;

use crate::provider::{Protocol, Provider};
use crate::types::{ChatRequest, ChatResponse, StreamChunk};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chat_completions::ChatStreamState;
use eventsource_stream::Eventsource;
use futures::future::ready;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use responses::ResponsesStreamState;
use serde::Serialize;
use std::sync::Arc;

/// OpenAI 接口协议类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiType {
    /// Chat Completions API
    ChatCompletions,
    /// Responses API
    Responses,
}

impl ApiType {
    /// 解析配置值：`"chat"`（默认）/ `"responses"`，另接受别名 `"chat_completions"`
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "chat" | "chat_completions" => Some(Self::ChatCompletions),
            "responses" => Some(Self::Responses),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat",
            Self::Responses => "responses",
        }
    }
}

/// OpenAI Provider
pub struct OpenAIProvider {
    client: Arc<Client>,
    base_url: String,
    api_key: String,
    model: String,
    api_type: ApiType,
}

impl OpenAIProvider {
    pub fn new(api_type: ApiType, base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Arc::new(crate::error::http_client()),
            base_url,
            api_key,
            model,
            api_type,
        }
    }

    async fn post(
        &self,
        versionless_path: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::Response> {
        let url = endpoint(&self.base_url, versionless_path);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .with_context(|| format!("Failed to send request to {}", url))?;

        if !response.status().is_success() {
            return Err(crate::error::ApiError::from_response("OpenAI-compatible", response)
                .await
                .into());
        }

        Ok(response)
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        match self.api_type {
            ApiType::ChatCompletions => {
                let body = chat_completions::build_request(&self.model, &request, false);
                let response = self.post("/chat/completions", &body).await?;
                let parsed: chat_completions::ChatCompletionsResponse = response
                    .json()
                    .await
                    .context("Failed to parse OpenAI response")?;
                Ok(chat_completions::parse_response(parsed))
            }
            ApiType::Responses => {
                let body = responses::build_request(&self.model, &request, false);
                let response = self.post("/responses", &body).await?;
                let parsed: responses::ResponsesApiResponse = response
                    .json()
                    .await
                    .context("Failed to parse OpenAI response")?;
                Ok(responses::parse_response(parsed))
            }
        }
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<StreamChunk>>> {
        let client = self.client.clone();
        let api_key = self.api_key.clone();

        let (url, body, state) = match self.api_type {
            ApiType::ChatCompletions => (
                endpoint(&self.base_url, "/chat/completions"),
                serde_json::to_value(chat_completions::build_request(&self.model, &request, true))?,
                StreamState::Chat(ChatStreamState::default()),
            ),
            ApiType::Responses => (
                endpoint(&self.base_url, "/responses"),
                serde_json::to_value(responses::build_request(&self.model, &request, true))?,
                StreamState::Responses(ResponsesStreamState::default()),
            ),
        };

        let response = client
            .post(&url)
            .bearer_auth(api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to send streaming request to {}", url))?;

        if !response.status().is_success() {
            return Err(crate::error::ApiError::from_response("OpenAI-compatible", response)
                .await
                .into());
        }

        // SSE 事件流 + 终止哨兵：即使服务端没有发 [DONE] / response.completed，
        // 流自然结束时也会触发一次 flush，避免缓冲中的工具调用丢失
        let stream = response
            .bytes_stream()
            .eventsource()
            .map(|ev| ev.map(|e| Some(e.data)).map_err(|e| e.to_string()))
            .chain(futures::stream::once(ready(Ok(None))))
            .scan(state, |state, data| {
                let result: Result<Vec<StreamChunk>> = match data {
                    Ok(Some(data)) => match state {
                        StreamState::Chat(s) => Ok(s.process_data(&data)),
                        StreamState::Responses(s) => Ok(s.process_data(&data)),
                    },
                    Ok(None) => match state {
                        StreamState::Chat(s) => Ok(s.process_end()),
                        StreamState::Responses(s) => Ok(s.process_end()),
                    },
                    Err(e) => Err(anyhow::anyhow!("SSE error: {}", e)),
                };
                ready(Some(result))
            })
            .flat_map(|result| {
                futures::stream::iter(match result {
                    Ok(chunks) => chunks.into_iter().map(Ok).collect::<Vec<_>>(),
                    Err(e) => vec![Err(e)],
                })
            })
            .boxed();

        Ok(stream)
    }

    fn protocol(&self) -> Protocol {
        match self.api_type {
            ApiType::ChatCompletions => Protocol::OpenAIChat,
            ApiType::Responses => Protocol::OpenAIResponses,
        }
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "openai"
    }
}

/// 流状态：None 是追加在事件流末尾的终止哨兵
enum StreamState {
    Chat(ChatStreamState),
    Responses(ResponsesStreamState),
}

/// 拼接最终请求端点。
/// `versionless_path` 形如 `"/chat/completions"` / `"/responses"`，规则：
/// - base_url 已以完整路径结尾（如 `.../v4/chat/completions`）→ 原样使用
/// - base_url 以版本段结尾（`/v1`、`/v4`、`/plan/v3` 等）→ 仅追加路径
/// - 其余（裸主机根）→ 追加 `/v1<path>`
pub fn endpoint(base_url: &str, versionless_path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with(versionless_path) {
        base.to_string()
    } else if ends_with_version_segment(base) {
        format!("{}{}", base, versionless_path)
    } else {
        format!("{}/v1{}", base, versionless_path)
    }
}

/// 判断 URL 最后一段是否是版本段（v + 数字，如 /v1、/v4）
fn ends_with_version_segment(base: &str) -> bool {
    base.rsplit('/')
        .next()
        .map(|last| {
            let digits = last.strip_prefix('v').unwrap_or("");
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    #[test]
    fn test_api_type_parse() {
        assert_eq!(ApiType::parse("chat"), Some(ApiType::ChatCompletions));
        assert_eq!(
            ApiType::parse("chat_completions"),
            Some(ApiType::ChatCompletions)
        );
        assert_eq!(ApiType::parse("responses"), Some(ApiType::Responses));
        assert_eq!(ApiType::parse("other"), None);
    }

    #[test]
    fn test_endpoint() {
        // 裸根地址：追加 /v1
        assert_eq!(
            endpoint("https://api.openai.com", "/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint("https://api.deepseek.com", "/chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );

        // 以 /v1 结尾：直接追加
        assert_eq!(
            endpoint("https://api.openai.com/v1", "/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint("https://api.moonshot.cn/v1/", "/models"),
            "https://api.moonshot.cn/v1/models"
        );

        // 其他版本段（/v4、/plan/v3）：直接追加，不重复加 /v1
        assert_eq!(
            endpoint("https://open.bigmodel.cn/api/paas/v4", "/chat/completions"),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
        assert_eq!(
            endpoint("https://api.lkeap.cloud.tencent.com/plan/v3", "/chat/completions"),
            "https://api.lkeap.cloud.tencent.com/plan/v3/chat/completions"
        );

        // 完整路径：原样使用
        assert_eq!(
            endpoint(
                "https://open.bigmodel.cn/api/paas/v4/chat/completions",
                "/chat/completions"
            ),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
    }

    #[test]
    fn test_provider_creation() {
        let provider = OpenAIProvider::new(
            ApiType::Responses,
            "https://api.openai.com".to_string(),
            "sk-test".to_string(),
            "gpt-5".to_string(),
        );
        assert_eq!(provider.provider_name(), "openai");
        assert_eq!(provider.protocol(), Protocol::OpenAIResponses);
        assert_eq!(provider.model(), "gpt-5");
    }

    // ========== 本地 SSE 服务器端到端测试 ==========

    /// 读取一个完整 HTTP 请求（请求头 + Content-Length 定界的请求体）
    fn read_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream.read(&mut tmp).expect("read request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let s = String::from_utf8_lossy(&buf);
            if let Some(header_end) = s.find("\r\n\r\n") {
                let content_length = s[..header_end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if buf.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// 起一个只服务一次请求的 SSE 服务器，返回 (地址, 处理线程 JoinHandle)
    fn spawn_sse_server(
        respond: fn(&str) -> String,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let request = read_request(&mut socket);
            use std::io::Write;
            socket.write_all(respond(&request).as_bytes()).unwrap();
            request
        });
        (addr, handle)
    }

    fn sse_response(events: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            events.len(),
            events
        )
    }

    #[tokio::test]
    async fn test_chat_stream_end_to_end() {
        let (addr, server) = spawn_sse_server(|request| {
            assert!(request.starts_with("POST /v1/chat/completions"));
            assert!(request.to_lowercase().contains("authorization: bearer sk-test"));
            assert!(request.contains(r#""stream":true"#));
            sse_response(concat!(
                r#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#,
                "\n\n",
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"grep","arguments":""}}]}}]}"#,
                "\n\n",
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#,
                "\n\n",
                "data: [DONE]\n\n",
            ))
        });

        let provider = OpenAIProvider::new(
            ApiType::ChatCompletions,
            format!("http://{}", addr),
            "sk-test".to_string(),
            "gpt-4o".to_string(),
        );
        let stream = provider
            .chat_stream(ChatRequest::new(vec![Message::user("hi")]))
            .await
            .unwrap();
        let chunks: Vec<_> = futures::StreamExt::collect::<Vec<_>>(stream).await;
        server.join().unwrap();

        let chunks: Vec<StreamChunk> = chunks.into_iter().map(|c| c.unwrap()).collect();
        assert_eq!(
            chunks,
            vec![
                StreamChunk::Content("Hi".to_string()),
                StreamChunk::ToolCallStart {
                    id: "call_a".to_string(),
                    name: "grep".to_string(),
                },
                StreamChunk::ToolCallArguments {
                    id: "call_a".to_string(),
                    arguments: "{}".to_string(),
                },
                StreamChunk::Done,
            ]
        );
    }

    #[tokio::test]
    async fn test_chat_stream_without_done_marker_flushes_on_end() {
        // 服务端未发 [DONE] 直接断流：依赖终止哨兵兜底 flush
        let (addr, server) = spawn_sse_server(|request| {
            assert!(request.starts_with("POST /v1/responses"));
            sse_response(concat!(
                r#"data: {"type":"response.output_text.delta","delta":"Hi"}"#,
                "\n\n",
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"grep","arguments":""}}"#,
                "\n\n",
                r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{}"}"#,
                "\n\n",
            ))
        });

        let provider = OpenAIProvider::new(
            ApiType::Responses,
            format!("http://{}", addr),
            "sk-test".to_string(),
            "gpt-5".to_string(),
        );
        let stream = provider
            .chat_stream(ChatRequest::new(vec![Message::user("hi")]))
            .await
            .unwrap();
        let chunks: Vec<_> = futures::StreamExt::collect::<Vec<_>>(stream).await;
        server.join().unwrap();

        let chunks: Vec<StreamChunk> = chunks.into_iter().map(|c| c.unwrap()).collect();
        assert_eq!(
            chunks,
            vec![
                StreamChunk::Content("Hi".to_string()),
                StreamChunk::ToolCallStart {
                    id: "call_1".to_string(),
                    name: "grep".to_string(),
                },
                StreamChunk::ToolCallArguments {
                    id: "call_1".to_string(),
                    arguments: "{}".to_string(),
                },
                StreamChunk::Done,
            ]
        );
    }
}
