//! MCP 工具桥（mcporter CLI）
//!
//! 通过 `mcporter.json`（项目根）发现并调用 MCP 服务器工具。
//! 工具名格式为 `server.tool_name`（点号分隔，与 mcporter call 一致）。
//! 依赖本机可用 `npx -y mcporter`；发现失败的服务器仅告警跳过。

use anyhow::{Context, Result};
use baiji_agent::{AgentTool, ToolOutput, ToolRegistry};
use baiji_ai::ToolDefinition;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tracing::{info, warn};

/// mcporter CLI 单次调用超时
const MCP_TIMEOUT_SECS: u64 = 30;

/// mcporter 桥
pub struct McporterBridge {
    config_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct McporterListOutput {
    status: String,
    tools: Option<Vec<McporterTool>>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McporterTool {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

#[derive(Debug, Deserialize)]
struct McporterConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: std::collections::HashMap<String, Value>,
}

impl McporterBridge {
    pub fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }

    /// 配置文件中的服务器名列表
    pub fn server_names(&self) -> Result<Vec<String>> {
        let content = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("Cannot read {}", self.config_path.display()))?;
        let config: McporterConfig =
            serde_json::from_str(&content).context("Failed to parse mcporter.json")?;
        Ok(config.mcp_servers.keys().cloned().collect())
    }

    /// 发现全部服务器的工具（单个服务器失败只告警不中断）
    pub async fn discover_tools(&self) -> Result<Vec<ToolDefinition>> {
        let server_names = self.server_names()?;
        let mut all_tools = Vec::new();
        // 并发发现：串行时每个慢 server 都会给启动加最多 MCP_TIMEOUT_SECS 秒
        let results = futures::future::join_all(
            server_names.iter().map(|name| self.discover_server_tools(name)),
        )
        .await;
        for (name, result) in server_names.iter().zip(results) {
            match result {
                Ok(tools) => {
                    info!("MCP server '{}': {} tools", name, tools.len());
                    all_tools.extend(tools);
                }
                Err(e) => warn!("MCP server '{}' discovery failed: {}", name, e),
            }
        }
        info!("Total MCP tools: {}", all_tools.len());
        Ok(all_tools)
    }

    pub async fn discover_server_tools(&self, server_name: &str) -> Result<Vec<ToolDefinition>> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(MCP_TIMEOUT_SECS),
            Command::new("npx")
                .kill_on_drop(true)
                .stdin(std::process::Stdio::null())
                .args([
                "-y",
                "mcporter",
                "list",
                server_name,
                "--json",
                "--schema",
                "--config",
                &self.config_path.to_string_lossy(),
            ]).output(),
        )
        .await
        .with_context(|| format!("Timeout discovering tools for '{}'", server_name))?
        .with_context(|| format!("Failed to run npx mcporter list {}", server_name))?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "mcporter list {} exit {}: {}",
                server_name,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let parsed: McporterListOutput = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("Failed to parse mcporter output for '{}'", server_name))?;
        if parsed.status != "ok" {
            return Err(anyhow::anyhow!(
                "Server '{}' error: {}",
                server_name,
                parsed.error.unwrap_or(parsed.status)
            ));
        }

        Ok(parsed
            .tools
            .unwrap_or_default()
            .into_iter()
            .map(|t| ToolDefinition {
                name: format!("{}.{}", server_name, t.name),
                description: t.description,
                parameters: t.input_schema,
            })
            .collect())
    }

    /// 调用工具（tool_name 格式 "server.tool_name"）
    pub async fn execute_tool(&self, tool_name: &str, arguments: Value) -> Result<String> {
        let args_json = serde_json::to_string(&arguments)?;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(MCP_TIMEOUT_SECS),
            Command::new("npx")
                .kill_on_drop(true)
                .stdin(std::process::Stdio::null())
                .args([
                "-y",
                "mcporter",
                "call",
                tool_name,
                "--args",
                &args_json,
                "--output",
                "json",
                "--config",
                &self.config_path.to_string_lossy(),
            ])
            .output(),
        )
        .await
        .with_context(|| format!("Timeout calling tool '{}'", tool_name))?
        .with_context(|| format!("Failed to run npx mcporter call {}", tool_name))?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "mcporter call {} exit {}: {}",
                tool_name,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// LLM 侧的工具名：`mcp__server__tool`。
/// Anthropic / OpenAI 都要求工具名匹配 `^[a-zA-Z0-9_-]{1,64}$`，mcporter 的
/// `server.tool` 含点号，直接暴露会让注册了 MCP 工具后的每个请求都被拒绝。
pub fn llm_tool_name(call_name: &str) -> String {
    let sanitized: String = call_name
        .split('.')
        .map(|part| {
            part.chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
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
    bridge: Arc<McporterBridge>,
    /// `definition.name` 保持 mcporter 的调用名（`server.tool`）
    definition: ToolDefinition,
    /// 暴露给 LLM 的合法工具名
    llm_name: String,
}

impl McpTool {
    pub fn new(bridge: Arc<McporterBridge>, definition: ToolDefinition) -> Self {
        let llm_name = llm_tool_name(&definition.name);
        Self {
            bridge,
            definition,
            llm_name,
        }
    }
}

#[async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.llm_name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn parameters(&self) -> Value {
        self.definition.parameters.clone()
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        match self.bridge.execute_tool(&self.definition.name, args).await {
            Ok(output) => Ok(ToolOutput::ok(output)),
            Err(e) => Ok(ToolOutput::err(format!("[MCP error] {e}"))),
        }
    }
}

/// 发现 mcporter.json 中的全部 MCP 工具并注册进工具表。
/// 返回注册的工具数；文件不存在时返回 Ok(0)。
pub async fn register_mcp_tools(
    tools: &mut ToolRegistry,
    config_path: PathBuf,
) -> Result<usize> {
    if !config_path.exists() {
        return Ok(0);
    }
    let bridge = Arc::new(McporterBridge::new(config_path));
    let definitions = bridge.discover_tools().await?;
    let count = definitions.len();
    for definition in definitions {
        tools.register(Arc::new(McpTool::new(Arc::clone(&bridge), definition)));
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_names_missing_file() {
        let bridge = McporterBridge::new(PathBuf::from("/nonexistent/mcporter.json"));
        assert!(bridge.server_names().is_err());
    }

    #[test]
    fn test_server_names_valid() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("mcporter.json");
        std::fs::write(
            &config,
            r#"{"mcpServers": {"my-server": {}, "other": {}}}"#,
        )
        .unwrap();
        let bridge = McporterBridge::new(config);
        let mut names = bridge.server_names().unwrap();
        names.sort();
        assert_eq!(names, vec!["my-server".to_string(), "other".to_string()]);
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
    fn test_mcp_tool_definition_shapes() {
        let bridge = Arc::new(McporterBridge::new(PathBuf::from("mcporter.json")));
        let tool = McpTool::new(
            bridge,
            ToolDefinition {
                name: "server.tool".to_string(),
                description: "does things".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
        );
        // LLM 侧名字必须匹配 ^[a-zA-Z0-9_-]{1,64}$
        assert_eq!(tool.name(), "mcp__server__tool");
        assert_eq!(tool.description(), "does things");
        assert_eq!(tool.definition().name, "mcp__server__tool");
        // 调用 mcporter 仍用原名
        assert_eq!(tool.definition.name, "server.tool");

        let weird = llm_tool_name("my server.get/thing v2");
        assert_eq!(weird, "mcp__my_server__get_thing_v2");
        assert!(llm_tool_name(&"x".repeat(200)).len() <= 64);
    }
}
