//! baiji-extensions — 插件层
//!
//! 插件在启动时通过 [`PluginContext`] 向运行时注入工具与 hooks，
//! 无需修改核心代码即可扩展 baiji 的能力。
//!
//! 内置：
//! - [`ClockPlugin`]：注册 `now` 工具，返回当前时间
//! - [`SafetyPlugin`]：bash 命令风险分级 hook（baiji-command-risk：灾难级拒绝、
//!   不可逆命令要求 justification），另有 HITL 判定闭包 [`readonly_auto_approve`] /
//!   [`irreversible_escalation`]
//! - [`mcp`]：MCP 工具桥（原生 stdio 客户端，常驻进程）

pub mod command_hook;
pub mod mcp;
pub mod safety;

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, Hook, HookRegistry, ToolOutput, ToolRegistry};
use serde_json::Value;
use std::sync::Arc;

pub use command_hook::{CommandHookSpec, CommandHooks, HooksConfig};
pub use mcp::{McpClient, McpTool, ServerSpec, register_mcp_tools};
pub use safety::{SafetyPlugin, irreversible_escalation, readonly_auto_approve};

/// 插件注册上下文：插件把工具/hooks 塞进来
#[derive(Default)]
pub struct PluginContext {
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub hooks: Vec<Arc<dyn Hook>>,
}

impl PluginContext {
    pub fn add_tool(&mut self, tool: Arc<dyn AgentTool>) {
        self.tools.push(tool);
    }

    pub fn add_hook(&mut self, hook: Arc<dyn Hook>) {
        self.hooks.push(hook);
    }
}

/// 插件契约
pub trait Plugin: Send + Sync {
    /// 唯一 ID
    fn id(&self) -> &str;
    fn description(&self) -> &str;
    /// 注册扩展点
    fn register(&self, ctx: &mut PluginContext) -> Result<()>;
}

/// 插件管理器
#[derive(Default)]
pub struct PluginManager {
    plugins: Vec<Box<dyn Plugin>>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 链式添加插件
    pub fn add(mut self, plugin: Box<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    pub fn ids(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.id()).collect()
    }

    /// 执行所有插件的注册，合并进工具/hook 注册表
    pub fn apply(self, tools: &mut ToolRegistry, hooks: &mut HookRegistry) -> Result<Vec<String>> {
        let mut applied = Vec::new();
        let mut ctx = PluginContext::default();
        for plugin in self.plugins {
            plugin.register(&mut ctx)?;
            applied.push(plugin.id().to_string());
        }
        for tool in ctx.tools {
            tools.register(tool);
        }
        for hook in ctx.hooks {
            hooks.register(hook);
        }
        Ok(applied)
    }
}

// ========== 内置示例插件 ==========

/// `now` 工具：返回当前本地时间
struct NowTool;

#[async_trait]
impl AgentTool for NowTool {
    fn name(&self) -> &str {
        "now"
    }
    fn description(&self) -> &str {
        "Get the current local date and time (RFC 3339)."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: Value) -> Result<ToolOutput> {
        Ok(ToolOutput::ok(
            chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        ))
    }
}

/// 时钟插件
pub struct ClockPlugin;

impl Plugin for ClockPlugin {
    fn id(&self) -> &str {
        "clock"
    }
    fn description(&self) -> &str {
        "Adds a 'now' tool that reports the current time"
    }
    fn register(&self, ctx: &mut PluginContext) -> Result<()> {
        ctx.add_tool(Arc::new(NowTool));
        Ok(())
    }
}

/// 默认启用的内置插件
pub fn builtin_plugins() -> Vec<Box<dyn Plugin>> {
    vec![Box::new(ClockPlugin), Box::new(SafetyPlugin::default())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use baiji_agent::HookDecision;

    #[tokio::test]
    async fn test_builtin_plugins_register() {
        let mut tools = ToolRegistry::new();
        let mut hooks = HookRegistry::new();

        let applied = PluginManager::new()
            .add(Box::new(ClockPlugin))
            .add(Box::new(SafetyPlugin::default()))
            .apply(&mut tools, &mut hooks)
            .unwrap();

        assert_eq!(applied, vec!["clock".to_string(), "safety".to_string()]);
        assert!(tools.get("now").is_some());
        assert_eq!(hooks.len(), 1);

        // now 工具可用
        let out = tools
            .get("now")
            .unwrap()
            .execute(Value::Null)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains('T')); // RFC3339
    }

    #[tokio::test]
    async fn test_safety_hook_blocks_dangerous_commands() {
        let mut tools = ToolRegistry::new();
        let mut hooks = HookRegistry::new();
        PluginManager::new()
            .add(Box::new(SafetyPlugin::default()))
            .apply(&mut tools, &mut hooks)
            .unwrap();

        for cmd in [
            "rm -rf /",
            "rm -rf ~",
            "rm -fr /",
            "sudo rm -rf /",
            "cd /tmp && rm -rf $HOME",
            // 以下写法曾可绕过
            "cd /tmp; rm -rf /",
            "cd /tmp;rm -rf /",
            "/bin/rm -rf /",
            "\\rm -rf /",
            "command rm -rf /",
            "sudo -n rm -rf /",
            "rm --recursive --force /",
            "rm -r /",
            "rm -rf ~/",
            "rm -rf \"/\"",
            "rm -rf /*",
            "rm -rf ${HOME}/",
            "rm / -rf",
            "rm -rf --no-preserve-root /x",
            "sh -c 'rm -rf /'",
            "ls\nrm -rf /",
        ] {
            let decision = hooks
                .tool_call("bash", &serde_json::json!({"command": cmd}))
                .await
                .unwrap();
            assert!(
                matches!(decision, HookDecision::Deny(_)),
                "should deny: {cmd}"
            );
        }

        // 正常命令放行
        for cmd in [
            "rm -rf ./build",
            "rm -rf /tmp/cache",
            "ls /",
            "echo rm -rf /",
            "rm -f /tmp/x",
            "rm -rf build; ls /",
            "rm -rf ~/project/target",
        ] {
            let decision = hooks
                .tool_call("bash", &serde_json::json!({"command": cmd}))
                .await
                .unwrap();
            assert_eq!(decision, HookDecision::Proceed, "should allow: {cmd}");
        }

        // 非 bash 工具不干预
        let decision = hooks
            .tool_call("read", &serde_json::json!({"path": "x"}))
            .await
            .unwrap();
        assert_eq!(decision, HookDecision::Proceed);
    }

    #[test]
    fn test_builtin_plugins_list() {
        let plugins = builtin_plugins();
        let ids: Vec<&str> = plugins.iter().map(|p| p.id()).collect();
        assert!(ids.contains(&"clock"));
        assert!(ids.contains(&"safety"));
    }
}
