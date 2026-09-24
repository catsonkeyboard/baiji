//! Mock MCP stdio server — McpClient 集成测试的陪测进程。
//!
//! 标准输入逐行读 JSON-RPC，标准输出回 JSON-RPC 响应。
//! 行为由环境变量控制（见 mcp/tests.rs）：
//! - `MOCK_LOG_NOISE=1`：在响应前输出一行非 JSON 噪声（测容错）
//! - `MOCK_DIE_AFTER_CALL=1`：第一次 tools/call 响应后进程退出（测惰性重启）

use std::io::{BufRead, Write};

fn main() {
    let log_noise = std::env::var("MOCK_LOG_NOISE").is_ok();
    let die_after_call = std::env::var("MOCK_DIE_AFTER_CALL").is_ok();

    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return; // EOF：客户端关闭
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let Some(method) = value
            .get("method")
            .and_then(|m| m.as_str())
            .map(str::to_string)
        else {
            continue; // 响应（不该到 server 手里）或畸形帧
        };
        let id = value.get("id").cloned();
        let response = match method.as_str() {
            "initialize" => Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock", "version": "0.1"}
                }
            })),
            "tools/list" => Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"tools": [
                    {
                        "name": "echo",
                        "description": "Echo the text argument back",
                        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
                    },
                    {
                        "name": "fail",
                        "description": "Always returns an isError result",
                        "inputSchema": {"type": "object"}
                    },
                    {
                        "name": "slow",
                        "description": "Sleeps 3s before answering (timeout tests)",
                        "inputSchema": {"type": "object"}
                    }
                ]}
            })),
            "tools/call" => {
                let params = value.get("params").cloned().unwrap_or(serde_json::Value::Null);
                let tool = params.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                let text = params
                    .pointer("/arguments/text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                let result = match tool {
                    "fail" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"content": [{"type": "text", "text": "boom"}], "isError": true}
                    }),
                    "slow" => {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"content": [{"type": "text", "text": "finally"}], "isError": false}
                        })
                    }
                    _ => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"content": [{"type": "text", "text": format!("echo: {text}")}], "isError": false}
                    }),
                };
                // 崩溃模式：响应落地后立刻退出（先 flush 保证客户端读到）
                if die_after_call {
                    let _ = writeln!(stdout, "{result}");
                    let _ = stdout.flush();
                    std::process::exit(1);
                }
                Some(result)
            }
            _ => None, // 通知（notifications/initialized 等）：无需响应
        };
        if let Some(response) = response {
            if log_noise {
                // 模拟 server 把日志打到 stdout：客户端必须跳过非 JSON 行
                let _ = writeln!(stdout, "[mock] handling {method}");
                let _ = stdout.flush();
            }
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
}
