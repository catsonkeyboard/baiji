//! baiji-extensions — 插件层
//!
//! 插件在启动时通过 [`PluginContext`] 向运行时注入工具与 hooks，
//! 无需修改核心代码即可扩展 baiji 的能力。
//!
//! 内置：
//! - [`ClockPlugin`]：注册 `now` 工具，返回当前时间
//! - [`SafetyPlugin`]：注册 hook，拦截 `rm -rf /` 等危险 bash 命令
//! - [`mcp`]：mcporter CLI 桥（MCP 工具发现与调用）

pub mod command_hook;
pub mod mcp;

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, Hook, HookDecision, HookRegistry, ToolOutput, ToolRegistry};
use serde_json::Value;
use std::sync::Arc;

pub use command_hook::{CommandHookSpec, CommandHooks, HooksConfig};
pub use mcp::{McpTool, McporterBridge, register_mcp_tools};

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

/// 危险命令拦截 hook
struct DangerousCommandHook;

#[async_trait]
impl Hook for DangerousCommandHook {
    fn name(&self) -> &str {
        "dangerous-command-guard"
    }
    async fn on_tool_call(&self, name: &str, args: &Value) -> Result<HookDecision> {
        if name != "bash" {
            return Ok(HookDecision::Proceed);
        }
        let Some(command) = args["command"].as_str() else {
            return Ok(HookDecision::Proceed);
        };
        if rm_targets_root(command) {
            return Ok(HookDecision::Deny(
                "refusing to run rm -rf against /, ~ or $HOME".to_string(),
            ));
        }
        Ok(HookDecision::Proceed)
    }
}

/// token 级检测：处于命令位置的 `rm` 带递归旗标（-r/-R/--recursive，任意位置），
/// 且目标归一化后是根/家目录（`/`、`~`、`$HOME`、实际家目录及其 `/*` 通配）即判定危险。
///
/// 覆盖：`/bin/rm`、`\rm`、`command rm`、`sudo -n rm`、长选项、引号包裹的目标、
/// 紧贴分隔符（`cd /tmp;rm -rf /`）、`sh -c "rm -rf /"`、`--no-preserve-root`。
///
/// 这是启发式兜底，不是沙箱：变量间接、`find / -delete` 等写法无法穷举，
/// 真正的防线仍是执行前的人工确认。
fn rm_targets_root(command: &str) -> bool {
    const SEPARATORS: &[&str] = &[";", "&&", "||", "|", "&", "(", ")", "{", "}"];
    const PREFIXES: &[&str] = &[
        "sudo", "doas", "nohup", "nice", "xargs", "env", "command", "exec", "time", "then", "do",
        "else",
    ];

    // 分隔符两侧补空格，使 `/tmp;rm` 也能切开
    let mut spaced = String::with_capacity(command.len() + 16);
    let chars: Vec<char> = command.chars().collect();
    let mut k = 0;
    while k < chars.len() {
        let c = chars[k];
        let pair = matches!((c, chars.get(k + 1)), ('&', Some('&')) | ('|', Some('|')));
        if pair {
            spaced.push(' ');
            spaced.push(c);
            spaced.push(c);
            spaced.push(' ');
            k += 2;
            continue;
        }
        if matches!(c, ';' | '|' | '&' | '(' | ')' | '\n') {
            spaced.push(' ');
            spaced.push(if c == '\n' { ';' } else { c });
            spaced.push(' ');
        } else {
            spaced.push(c);
        }
        k += 1;
    }

    // 去引号与反斜杠（`"/"`、`\rm`）
    let tokens: Vec<String> = spaced
        .split_whitespace()
        .map(|t| {
            t.chars()
                .filter(|c| !matches!(c, '"' | '\'' | '\\'))
                .collect()
        })
        .collect();

    let home = std::env::var("HOME").ok();
    let is_rootish = |target: &str| -> bool {
        let mut t = target.trim_end_matches("/*").trim_end_matches("/.");
        if t.len() > 1 {
            t = t.trim_end_matches('/');
        }
        if t.is_empty() {
            return true; // 原 token 为 "/"、"//"、"/*"
        }
        matches!(t, "/" | "~" | "$HOME" | "${HOME}") || home.as_deref() == Some(t)
    };

    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        let is_rm = token == "rm" || token.ends_with("/rm");
        // 命令位置：首 token；或向前跳过旗标后，紧邻分隔符/前缀命令/`sh -c`
        let is_command_position = {
            let mut p = i;
            let mut via_dash_c = false;
            while p > 0 && tokens[p - 1].starts_with('-') {
                via_dash_c |= tokens[p - 1] == "-c";
                p -= 1;
            }
            p == 0
                || via_dash_c
                || SEPARATORS.contains(&tokens[p - 1].as_str())
                || PREFIXES.contains(&tokens[p - 1].as_str())
        };
        if !is_rm || !is_command_position {
            i += 1;
            continue;
        }

        // 本条 rm 的参数：直到下一个分隔符
        let mut j = i + 1;
        let (mut recursive, mut rootish) = (false, false);
        while j < tokens.len() && !SEPARATORS.contains(&tokens[j].as_str()) {
            let arg = tokens[j].as_str();
            if arg == "--no-preserve-root" {
                return true;
            } else if arg == "--recursive" {
                recursive = true;
            } else if arg.starts_with('-') && !arg.starts_with("--") {
                recursive |= arg.contains('r') || arg.contains('R');
            } else if !arg.starts_with('-') {
                rootish |= is_rootish(arg);
            }
            j += 1;
        }
        if recursive && rootish {
            return true;
        }
        i = j.max(i + 1);
    }
    false
}

/// 安全插件
pub struct SafetyPlugin;

impl Plugin for SafetyPlugin {
    fn id(&self) -> &str {
        "safety"
    }
    fn description(&self) -> &str {
        "Blocks destructive bash commands (rm -rf on / or ~)"
    }
    fn register(&self, ctx: &mut PluginContext) -> Result<()> {
        ctx.add_hook(Arc::new(DangerousCommandHook));
        Ok(())
    }
}

/// 默认启用的内置插件
pub fn builtin_plugins() -> Vec<Box<dyn Plugin>> {
    vec![Box::new(ClockPlugin), Box::new(SafetyPlugin)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_builtin_plugins_register() {
        let mut tools = ToolRegistry::new();
        let mut hooks = HookRegistry::new();

        let applied = PluginManager::new()
            .add(Box::new(ClockPlugin))
            .add(Box::new(SafetyPlugin))
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
            .add(Box::new(SafetyPlugin))
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
