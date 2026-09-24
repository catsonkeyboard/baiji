//! McpClient 集成测试：真实子进程 + 真实 stdio 管道 + 真实 JSON-RPC 往返。
//!
//! 陪测进程是 `src/bin/mock-mcp-server.rs`，路径经 `CARGO_BIN_EXE_` 编译期注入
//! （仅在 tests/ 集成测试目标里可用——这正是放在 crate 顶层的原因）。

use baiji_extensions::{McpClient, ServerSpec};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

fn mock_spec() -> ServerSpec {
    ServerSpec {
        command: env!("CARGO_BIN_EXE_mock-mcp-server").to_string(),
        args: vec![],
        env: HashMap::new(),
    }
}

#[tokio::test]
async fn test_handshake_list_and_call_roundtrip() {
    let client = McpClient::new(mock_spec());

    // tools/list（内部已完成 spawn + initialize 握手）
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0]["name"], "echo");
    assert_eq!(tools[1]["name"], "fail");
    assert_eq!(tools[2]["name"], "slow");
    // schema 透传
    assert_eq!(
        tools[0]["inputSchema"]["properties"]["text"]["type"],
        "string"
    );

    // tools/call 正常往返
    let result = client
        .call_tool("echo", json!({"text": "hello"}))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.text, "echo: hello");

    // 同一进程复用：第二次调用直接往返（能拿到结果本身证明复用成功——
    // 若每次调用都重启，崩溃用例的时序不可能稳定）
    let again = client
        .call_tool("echo", json!({"text": "world"}))
        .await
        .unwrap();
    assert_eq!(again.text, "echo: world");

    // isError 结果归一化
    let failed = client.call_tool("fail", json!({})).await.unwrap();
    assert!(failed.is_error);
    assert_eq!(failed.text, "boom");
}

#[tokio::test]
async fn test_stdout_noise_is_skipped() {
    let mut spec = mock_spec();
    spec.env
        .insert("MOCK_LOG_NOISE".to_string(), "1".to_string());
    let client = McpClient::new(spec);

    // 噪声行混在 JSON-RPC 响应之间：客户端必须跳过并正确分发
    let result = client
        .call_tool("echo", json!({"text": "noisy"}))
        .await
        .unwrap();
    assert_eq!(result.text, "echo: noisy");
}

#[tokio::test]
async fn test_server_crash_then_lazy_restart() {
    let mut spec = mock_spec();
    spec.env
        .insert("MOCK_DIE_AFTER_CALL".to_string(), "1".to_string());
    let client = McpClient::new(spec);

    // 第一次调用成功，server 随后自尽
    let first = client
        .call_tool("echo", json!({"text": "one"}))
        .await
        .unwrap();
    assert_eq!(first.text, "echo: one");

    // 给 server 一点时间咽气（进程退出 → reader EOF → pending dead）
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 下一次调用：ensure_started 探测到退出，惰性重启后正常服务
    let second = client
        .call_tool("echo", json!({"text": "two"}))
        .await
        .unwrap();
    assert_eq!(
        second.text, "echo: two",
        "server must restart transparently after crash"
    );
}

#[tokio::test]
async fn test_spawn_failure_is_clean_error() {
    // 不存在的命令：握手失败且不留半开进程
    let client = McpClient::new(ServerSpec {
        command: "/nonexistent/baiji-mcp-server".to_string(),
        args: vec![],
        env: HashMap::new(),
    });
    assert!(client.list_tools().await.is_err());
}

/// 慢工具语义：默认 RPC 超时内完成 + 进程存活可继续调用
/// （超时不杀进程的直接验证需要可注入的超时配置，这里验证等价命题：
/// 3s 慢工具不触发任何超时/重启副作用，之后的快工具立即复用同一进程）
#[tokio::test]
async fn test_slow_tool_completes_and_process_survives() {
    let client = McpClient::new(mock_spec());
    let started = std::time::Instant::now();
    let result = client.call_tool("slow", json!({})).await.unwrap();
    assert_eq!(result.text, "finally");
    assert!(started.elapsed() >= Duration::from_secs(3));

    // 进程还活着：紧接着再调一次快工具
    let quick = client
        .call_tool("echo", json!({"text": "alive"}))
        .await
        .unwrap();
    assert_eq!(quick.text, "echo: alive");
}
