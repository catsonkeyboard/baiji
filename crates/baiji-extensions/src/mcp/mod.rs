//! MCP 工具桥（原生 stdio 客户端）
//!
//! 通过项目根的 `mcporter.json` 发现并调用 MCP 服务器工具。
//! 配置格式与 Claude Code 的 `.mcp.json` 同构（`mcpServers: {command, args, env}`），
//! 旧 mcporter CLI 桥的用户无感迁移。env 值支持 `$VAR` / `${VAR}` 展开。
//!
//! 与旧桥（每次调用 `npx mcporter` 冷启动 1-3s）不同：每个 server spawn 一次
//! 常驻进程，initialize 握手后 tools/list / tools/call 复用（毫秒级往返），
//! server 端状态（连接/会话/缓存）跨调用保留。见 [`client::McpClient`]。

mod client;

pub use client::{CallToolResult, McpClient, ServerSpec};

use anyhow::{Context, Result};
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput, ToolRegistry};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Debug, Deserialize)]
struct McpConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, ServerEntry>,
}

/// mcporter.json 里单个 server 的条目（与 Claude Code .mcp.json 同构）
#[derive(Debug, Deserialize)]
struct ServerEntry {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

/// 解析配置文件为 (server 名, 规格) 列表；文件不存在返回 Ok(None)
fn parse_config(config_path: &Path) -> Result<Option<Vec<(String, ServerSpec)>>> {
    if !config_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Cannot read {}", config_path.display()))?;
    let config: McpConfig =
        serde_json::from_str(&content).context("Failed to parse mcporter.json")?;
    let servers = config
        .mcp_servers
        .into_iter()
        .map(|(name, entry)| {
            let spec = ServerSpec {
                command: entry.command,
                args: entry.args,
                env: entry
                    .env
                    .into_iter()
                    .map(|(k, v)| (k, baiji_ai::expand_env_vars(&v)))
                    .collect(),
            };
            (name, spec)
        })
        .collect();
    Ok(Some(servers))
}

/// LLM 侧的工具名：`mcp__server__tool`。
/// Anthropic / OpenAI 都要求工具名匹配 `^[a-zA-Z0-9_-]{1,64}$`，内部调用名
/// `server.tool` 含点号，直接暴露会让注册了 MCP 工具后的每个请求都被拒绝。
pub fn llm_tool_name(call_name: &str) -> String {
    let sanitized: String = call_name
        .split('.')
        .map(|part| {
            part.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("__");
    let mut name = format!("mcp__{sanitized}");
    name.truncate(64); // 全 ASCII，按字节截断安全
    name
}

/// 包装单个 MCP 工具为 AgentTool
pub struct McpTool {
    client: Arc<McpClient>,
    /// 内部调用名（裸工具名，不带 server 前缀）
    tool_name: String,
    /// 暴露给 LLM 的合法工具名（`mcp__server__tool`）
    llm_name: String,
    description: String,
    parameters: Value,
}

impl McpTool {
    fn new(client: Arc<McpClient>, server_name: &str, tool: &Value) -> Self {
        let tool_name = tool
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let call_name = format!("{server_name}.{tool_name}");
        Self {
            llm_name: llm_tool_name(&call_name),
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            parameters: tool
                .get("inputSchema")
                .cloned()
                .unwrap_or(serde_json::json!({"type": "object"})),
            client,
            tool_name,
        }
    }
}

#[async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.llm_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        match self.client.call_tool(&self.tool_name, args).await {
            Ok(result) => {
                if result.is_error {
                    Ok(ToolOutput::err(format!(
                        "[MCP tool error] {}",
                        result.text
                    )))
                } else {
                    Ok(ToolOutput::ok(result.text))
                }
            }
            Err(e) => Ok(ToolOutput::err(format!("[MCP error] {e}"))),
        }
    }
}

/// 发现 mcporter.json 中的全部 MCP 工具并注册进工具表。
/// 返回注册的工具数；文件不存在时返回 Ok(0)。
/// 单个 server 启动/握手失败只告警跳过（fail-open），不中断其余 server。
pub async fn register_mcp_tools(tools: &mut ToolRegistry, config_path: PathBuf) -> Result<usize> {
    let Some(servers) = parse_config(&config_path)? else {
        return Ok(0);
    };

    // 并发启动 + 握手：串行时每个慢 server 都给启动加最多 INIT 超时
    let results = futures::future::join_all(servers.iter().map(|(name, spec)| {
        let name = name.clone();
        let client = Arc::new(McpClient::new(spec.clone()));
        async move {
            let list = client.list_tools().await;
            (name, client, list)
        }
    }))
    .await;

    let mut count = 0;
    for (name, client, list) in results {
        match list {
            Ok(tool_defs) => {
                info!("MCP server '{name}': {} tools", tool_defs.len());
                for tool in &tool_defs {
                    let mcp_tool = McpTool::new(Arc::clone(&client), &name, tool);
                    tools.register(Arc::new(mcp_tool));
                    count += 1;
                }
            }
            Err(e) => warn!("MCP server '{name}' unavailable, skipped: {e}"),
        }
    }
    if count > 0 {
        info!("registered {count} MCP tool(s) via resident stdio processes");
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config_missing_file() {
        assert!(parse_config(Path::new("/nonexistent/mcporter.json"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_parse_config_valid_with_env_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("mcporter.json");
        std::fs::write(
            &config,
            r#"{"mcpServers": {"fetch": {"command": "node", "args": ["server.js"], "env": {"KEY": "$HOME"}}}}"#,
        )
        .unwrap();
        let servers = parse_config(&config).unwrap().unwrap();
        assert_eq!(servers.len(), 1);
        let (name, spec) = &servers[0];
        assert_eq!(name, "fetch");
        assert_eq!(spec.command, "node");
        assert_eq!(spec.args, vec!["server.js".to_string()]);
        // $HOME 已展开（不再是字面量）
        assert_ne!(spec.env.get("KEY").unwrap(), "$HOME");
        assert!(spec.env.get("KEY").unwrap().len() > 1);
    }

    #[tokio::test]
    async fn test_register_mcp_tools_missing_config_is_noop() {
        let mut tools = ToolRegistry::new();
        let count = register_mcp_tools(&mut tools, PathBuf::from("/nonexistent/mcporter.json"))
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert!(tools.is_empty());
    }

    #[test]
    fn test_llm_tool_name_shapes() {
        assert_eq!(llm_tool_name("server.tool"), "mcp__server__tool");
        assert_eq!(
            llm_tool_name("my server.get/thing v2"),
            "mcp__my_server__get_thing_v2"
        );
        assert!(llm_tool_name(&"x".repeat(200)).len() <= 64);
    }
}
