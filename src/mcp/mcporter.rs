use crate::llm::ToolDefinition;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use tokio::process::Command;
use tracing::{debug, info, warn};

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

    pub fn server_names(&self) -> Result<Vec<String>> {
        let content = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("Cannot read {}", self.config_path.display()))?;
        let config: McporterConfig = serde_json::from_str(&content)
            .context("Failed to parse mcporter.json")?;
        Ok(config.mcp_servers.keys().cloned().collect())
    }

    pub async fn discover_tools(&self) -> Result<Vec<ToolDefinition>> {
        let server_names = self.server_names()?;
        let mut all_tools = Vec::new();
        for name in &server_names {
            match self.discover_server_tools(name).await {
                Ok(tools) => {
                    info!("Server '{}': {} tools", name, tools.len());
                    all_tools.extend(tools);
                }
                Err(e) => warn!("Server '{}' discovery failed: {}", name, e),
            }
        }
        info!("Total MCP tools: {}", all_tools.len());
        Ok(all_tools)
    }

    pub async fn discover_server_tools(&self, server_name: &str) -> Result<Vec<ToolDefinition>> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            Command::new("npx")
                .args([
                    "-y",
                    "mcporter",
                    "list",
                    server_name,
                    "--json",
                    "--schema",
                    "--config",
                    &self.config_path.to_string_lossy(),
                ])
                .output(),
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
            .with_context(|| {
                format!("Failed to parse mcporter output for '{}'", server_name)
            })?;
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

    /// tool_name 格式为 "server.tool_name"（点号分隔）
    pub async fn execute_tool(&self, tool_name: &str, arguments: Value) -> Result<String> {
        let args_json = serde_json::to_string(&arguments)?;
        debug!("mcporter call {} args={}", tool_name, args_json);
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            Command::new("npx")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let b = McporterBridge::new(PathBuf::from("mcporter.json"));
        assert_eq!(b.config_path, PathBuf::from("mcporter.json"));
    }

    #[test]
    fn test_server_names_missing_file() {
        let b = McporterBridge::new(PathBuf::from("/nonexistent/mcporter.json"));
        assert!(b.server_names().is_err());
    }

    #[test]
    fn test_server_names_valid() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, r#"{{"mcpServers": {{"my-server": {{}}}}}}"#).unwrap();
        let b = McporterBridge::new(f.path().to_path_buf());
        assert_eq!(b.server_names().unwrap(), vec!["my-server"]);
    }
}
