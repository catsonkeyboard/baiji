//! 原生 MCP stdio 客户端：常驻进程 + JSON-RPC。
//!
//! 每个 MCP server 一个 [`McpClient`]：spawn 后保持存活，
//! `initialize` 握手一次，后续 `tools/list` / `tools/call` 复用同一进程
//! （毫秒级往返，替代旧 mcporter CLI 桥每次调用 1-3s 的冷启动）。
//!
//! 设计要点：
//! - reader 任务独占 stdout：逐行解析 JSON-RPC 响应，按 `id` 分发给
//!   pending 等待者；通知（无 `id`）忽略；非 JSON 行（server 日志噪声）跳过
//! - 超时只放弃本次等待，**不杀进程**（进程是共享资源，下次调用还要用）；
//!   超时后迟到的响应因 pending 项已移除而被安全丢弃
//! - 进程退出：唤醒所有 pending（以错误失败），下次调用惰性重启 + 重新握手
//! - 请求 id 单调递增，进程重启后归零（新进程没有旧 id 的语义负担）

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// 单次 JSON-RPC 请求的默认超时（server 内工具可能很慢，但协议往返不该挂死）
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// spawn + initialize 握手的超时（冷启动最贵的一段）
const INIT_TIMEOUT: Duration = Duration::from_secs(30);

/// server 进程规格（来自 mcporter.json 的单个条目）
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// 常驻 MCP server 客户端
pub struct McpClient {
    spec: ServerSpec,
    inner: tokio::sync::Mutex<Option<RunningServer>>,
}

/// 活跃的 server 进程与其 IO 通道
struct RunningServer {
    child: Child,
    stdin: tokio::process::ChildStdin,
    /// reader 任务与 rpc_over 共享的等待者表（随进程生命周期存亡）
    pending: Arc<Mutex<Pending>>,
}

/// reader 任务与 pending 等待者的共享状态
#[derive(Default)]
struct Pending {
    /// 请求 id → 响应投递通道
    map: HashMap<u64, oneshot::Sender<Value>>,
    /// 进程已死：拒绝新请求直到重启完成
    dead: bool,
}

impl McpClient {
    pub fn new(spec: ServerSpec) -> Self {
        Self {
            spec,
            inner: tokio::sync::Mutex::new(None),
        }
    }

    /// 确保 server 存活且已握手（惰性启动/重启）
    async fn ensure_started(&self) -> Result<()> {
        let mut guard = self.inner.lock().await;
        match guard.as_mut() {
            Some(running) => {
                // 尝试探测：进程已退出则回收并重启
                if let Some(status) = running.child.try_wait()? {
                    warn!(
                        "MCP server '{}' exited ({status}); restarting",
                        self.spec.command
                    );
                    *guard = None;
                } else {
                    return Ok(());
                }
            }
            None => {
                debug!("starting MCP server '{}'", self.spec.command);
            }
        }

        // 启动 + 握手（失败则不留半开进程）
        let started = Self::spawn_and_init(&self.spec).await;
        match started {
            Ok(running) => {
                *guard = Some(running);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// spawn 进程并完成 initialize 握手。失败时击杀子进程（不留孤儿）。
    async fn spawn_and_init(spec: &ServerSpec) -> Result<RunningServer> {
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &spec.env {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server '{}'", spec.command))?;
        let stdin = child
            .stdin
            .take()
            .context("MCP server stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("MCP server stdout unavailable")?;

        let pending = Arc::new(Mutex::new(Pending::default()));
        // reader 任务：stdout 逐行分发（guard 持有期间存活）
        tokio::spawn(reader_loop(BufReader::new(stdout), Arc::clone(&pending)));

        let mut running = RunningServer {
            child,
            stdin,
            pending: Arc::clone(&pending),
        };

        // initialize 握手（协议规定的开场白）
        let result = tokio::time::timeout(
            INIT_TIMEOUT,
            Self::rpc_over(&mut running.stdin, &running.pending, 1, initialize_request()),
        )
        .await
        .map_err(|_| anyhow::anyhow!("MCP initialize timed out after {}s", INIT_TIMEOUT.as_secs()))?
        .with_context(|| format!("MCP initialize failed for '{}'", spec.command))?;

        // 协议规定的就绪通知（无 id，不需要响应）
        if let Err(e) = running
            .stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
        {
            warn!("failed to send initialized notification: {e}");
        }
        let _ = result;
        Ok(running)
    }

    /// 就地 RPC：对已运行的进程发起一次请求并等待响应
    async fn rpc_over(
        stdin: &mut tokio::process::ChildStdin,
        pending: &Arc<Mutex<Pending>>,
        id: u64,
        request: Value,
    ) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        {
            let mut map = pending.lock().unwrap();
            if map.dead {
                bail!("MCP server process exited");
            }
            map.map.insert(id, tx);
        }

        let payload = format!("{}\n", serde_json::to_string(&request)?);
        if let Err(e) = stdin.write_all(payload.as_bytes()).await {
            // 写失败：注册的等待者必须摘除，否则泄漏
            pending.lock().unwrap().map.remove(&id);
            bail!("failed writing to MCP server stdin: {e}");
        }

        match rx.await {
            Ok(response) => {
                if let Some(error) = response.get("error") {
                    bail!("MCP server error: {error}")
                }
                Ok(response)
            }
            Err(_) => bail!("MCP response channel dropped (server exited?)"),
        }
    }

    /// 对外 RPC 入口：确保存活 → 取号 → 请求 → 超时只放弃等待。
    /// 进程死亡在 ensure_started 或响应通道关闭处被发现，下次调用自动重启。
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        self.ensure_started().await?;
        let mut guard = self.inner.lock().await;
        let running = guard
            .as_mut()
            .context("MCP server not running (start failed)")?;

        // id 从 2 起：1 被 initialize 用掉；重启后新进程从 2 重新计数无碍
        static NEXT_ID: AtomicU64 = AtomicU64::new(2);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        // 超时语义：放弃等待但不杀进程（进程供后续调用复用）。
        // 超时后 future drop → rx drop → reader 侧 send 失败 → map 条目清除，无泄漏
        let fut = Self::rpc_over(&mut running.stdin, &running.pending, id, request);
        match tokio::time::timeout(DEFAULT_RPC_TIMEOUT, fut).await {
            Ok(result) => result,
            Err(_) => {
                bail!(
                    "MCP '{}' timed out after {}s (process kept alive)",
                    method,
                    DEFAULT_RPC_TIMEOUT.as_secs()
                )
            }
        }
    }

    /// `tools/list`：返回 server 的工具定义数组
    pub async fn list_tools(&self) -> Result<Vec<Value>> {
        let response = self
            .rpc("tools/list", json!({}))
            .await
            .context("tools/list failed")?;
        let tools = response
            .pointer("/result/tools")
            .and_then(|t| t.as_array().cloned())
            .unwrap_or_default();
        Ok(tools)
    }

    /// `tools/call`：返回 MCP CallToolResult（含 content 数组与 isError 标志）
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult> {
        let response = self
            .rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await
            .with_context(|| format!("tools/call '{name}' failed"))?;
        let result = response.get("result").cloned().unwrap_or(Value::Null);
        Ok(CallToolResult::from_json(&result))
    }
}

/// tools/call 的结果（MCP CallToolResult 的归一化）
pub struct CallToolResult {
    /// content 数组里的文本段拼接
    pub text: String,
    /// server 标注的 isError
    pub is_error: bool,
}

impl CallToolResult {
    fn from_json(result: &Value) -> Self {
        let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let mut text = String::new();
        if let Some(contents) = result.get("content").and_then(Value::as_array) {
            for block in contents {
                // text 块直接取；其他类型块（image/resource）以类型注记占位
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some(other) => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&format!("[{other} content block]"));
                    }
                    None => {}
                }
            }
        }
        Self { text, is_error }
    }
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "baiji", "version": env!("CARGO_PKG_VERSION")}
        }
    })
}

/// reader 循环：逐行读 stdout，按 id 分发；进程退出时 fail 所有 pending。
/// 泛型于 `AsyncBufRead`：生产是 ChildStdout，测试用 duplex 管道。
async fn reader_loop<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    pending: Arc<Mutex<Pending>>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break, // EOF（进程退出）或 IO 错误
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                    // server 把日志打到 stdout 的情形：跳过非 JSON 噪声行
                    debug!("MCP stdout noise: {}", trimmed.chars().take(120).collect::<String>());
                    continue;
                };
                let Some(id) = value.get("id").and_then(Value::as_u64) else {
                    continue; // 通知（无 id）：忽略
                };
                let waiter = pending.lock().unwrap().map.remove(&id);
                if let Some(tx) = waiter {
                    let _ = tx.send(value); // 接收端已超时放弃：send 失败即丢弃
                }
            }
        }
    }
    // 通道关闭：叫醒所有等待者（以错误失败）
    let mut map = pending.lock().unwrap();
    map.dead = true;
    map.map.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// reader 分发逻辑的纯管道测试：噪声跳过、通知忽略、EOF 后 dead 置位
    #[tokio::test]
    async fn test_reader_dispatch_noise_notification_and_dead() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, rx) = tokio::io::duplex(4096);
        let reader_task = tokio::spawn(reader_loop(BufReader::new(rx), Arc::clone(&pending)));

        // 注册 id=7 的等待者
        let (waiter_tx, waiter_rx) = oneshot::channel();
        pending.lock().unwrap().map.insert(7, waiter_tx);

        let mut tx = tx;
        tx.write_all(b"this is a log line, not JSON\n").await.unwrap();
        tx.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"some/notification\"}\n")
            .await
            .unwrap();
        tx.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n")
            .await
            .unwrap();

        let response = tokio::time::timeout(Duration::from_secs(2), waiter_rx)
            .await
            .expect("waiter must be served")
            .expect("channel must not close before response");
        assert_eq!(response["result"]["ok"], true);

        // 通道关闭后 dead 标志置位、pending 清空
        drop(tx);
        let _ = reader_task.await;
        assert!(pending.lock().unwrap().dead);
        assert!(pending.lock().unwrap().map.is_empty());
    }
}
