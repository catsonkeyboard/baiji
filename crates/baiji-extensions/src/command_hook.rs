//! 用户可配置的命令钩子（config `hooks` 段 → shell 命令）
//!
//! 在 run / turn / 工具调用等生命周期事件上执行配置的 shell 命令：
//! 审计日志、自定义策略拦截、通知等。上下文以 JSON 写入命令 stdin，
//! 并设 `BAIJI_HOOK_EVENT` 环境变量（简单脚本不必解析 stdin）。
//!
//! 约定（参考 Claude Code hooks）：
//! - **退出码 2 = 拦截**（仅 `tool_call` 事件有意义）：stderr（缺省取 stdout）
//!   作为拒绝理由回传给 LLM；
//! - 其它非零退出 / 超时 / 启动失败 = 只记日志不拦截（fail-open——
//!   用户脚本的问题不能砖死 agent）；
//! - `tool_result` 为只读观察（不改写结果——命令 stdout 破损会污染上下文）。
//!
//! 安全：钩子执行任意 shell 命令，配置**仅认全局文件**——项目级
//! `./.baiji/config.json` 里的 `hooks` 段会被白名单过滤掉并警告。
//! `BAIJI_HOOKS=off` 为总闸。

use baiji_agent::{Hook, HookDecision, ToolOutput};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

/// 单条命令钩子配置
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct CommandHookSpec {
    /// shell 命令（sh -c 执行，工作目录 = baiji 启动目录）
    pub command: String,
    /// 超时秒数（默认 10；超时击杀且不拦截）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl CommandHookSpec {
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.unwrap_or(10).clamp(1, 300))
    }
}

/// 一个事件的执行结果
struct CommandOutcome {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

/// 全部命令钩子（实现 [`Hook`]，注册进 HookRegistry）
pub struct CommandHooks {
    run_start: Vec<CommandHookSpec>,
    run_end: Vec<CommandHookSpec>,
    turn_start: Vec<CommandHookSpec>,
    tool_call: Vec<CommandHookSpec>,
    tool_result: Vec<CommandHookSpec>,
}

/// config `hooks` 段的原样结构（bin 层的 HooksConfig 直接复用）
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub run_start: Vec<CommandHookSpec>,
    #[serde(default)]
    pub turn_start: Vec<CommandHookSpec>,
    #[serde(default)]
    pub tool_call: Vec<CommandHookSpec>,
    #[serde(default)]
    pub tool_result: Vec<CommandHookSpec>,
    #[serde(default)]
    pub run_end: Vec<CommandHookSpec>,
}

impl HooksConfig {
    /// 是否配置了任何钩子
    pub fn is_empty(&self) -> bool {
        self.run_start.is_empty()
            && self.turn_start.is_empty()
            && self.tool_call.is_empty()
            && self.tool_result.is_empty()
            && self.run_end.is_empty()
    }

    /// 钩子总条数（日志/摘要用）
    pub fn count(&self) -> usize {
        self.run_start.len()
            + self.turn_start.len()
            + self.tool_call.len()
            + self.tool_result.len()
            + self.run_end.len()
    }
}

impl CommandHooks {
    pub fn from_config(config: HooksConfig) -> Self {
        Self {
            run_start: config.run_start,
            run_end: config.run_end,
            turn_start: config.turn_start,
            tool_call: config.tool_call,
            tool_result: config.tool_result,
        }
    }

    /// 执行一条钩子命令：stdin = JSON 上下文，env = BAIJI_HOOK_EVENT
    async fn run(spec: &CommandHookSpec, event: &str, context: Value) -> CommandOutcome {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&spec.command)
            .env("BAIJI_HOOK_EVENT", event)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true); // 超时路径整进程击杀

        let payload = serde_json::to_string(&context).unwrap_or_default();
        let timeout = spec.timeout();

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!("hook '{event}' failed to spawn '{}': {e}", spec.command);
                return CommandOutcome {
                    code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    timed_out: false,
                };
            }
        };
        let mut child = child;

        // stdin 写入上下文后关闭（cat 才会结束）
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.as_bytes()).await;
        }
        drop(child.stdin.take());

        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => CommandOutcome {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                timed_out: false,
            },
            Ok(Err(e)) => {
                tracing::warn!("hook '{event}' wait failed: {e}");
                CommandOutcome {
                    code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    timed_out: false,
                }
            }
            Err(_) => {
                // kill_on_drop 已击杀；这里只记录
                tracing::warn!(
                    "hook '{event}' timed out after {timeout:?}: {}",
                    spec.command
                );
                CommandOutcome {
                    code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    timed_out: true,
                }
            }
        }
    }

    /// 执行一组观察型钩子（失败只记日志）
    async fn run_observers(specs: &[CommandHookSpec], event: &str, context: Value) {
        for spec in specs {
            let out = Self::run(spec, event, context.clone()).await;
            if out.timed_out || out.code.is_some_and(|c| c != 0) {
                tracing::warn!(
                    "hook '{event}' command '{}' exited with {:?} (non-blocking)",
                    spec.command,
                    out.code
                );
            }
        }
    }

    /// 拒绝理由：stderr 优先，空则 stdout，再空则通用文案
    fn deny_reason(out: &CommandOutcome, command: &str) -> String {
        let detail = if !out.stderr.trim().is_empty() {
            out.stderr.trim()
        } else if !out.stdout.trim().is_empty() {
            out.stdout.trim()
        } else {
            "blocked by a tool_call hook"
        };
        format!("[Hook denied] {command}: {detail}")
    }
}

#[async_trait::async_trait]
impl Hook for CommandHooks {
    fn name(&self) -> &str {
        "command-hooks"
    }

    async fn on_run_start(&self, user_input: &str) -> anyhow::Result<()> {
        Self::run_observers(
            &self.run_start,
            "run_start",
            serde_json::json!({
                "event": "run_start",
                "input": user_input,
            }),
        )
        .await;
        Ok(())
    }

    async fn on_turn_start(&self, turn: u32) -> anyhow::Result<()> {
        Self::run_observers(
            &self.turn_start,
            "turn_start",
            serde_json::json!({
                "event": "turn_start",
                "turn": turn,
            }),
        )
        .await;
        Ok(())
    }

    async fn on_tool_call(&self, name: &str, args: &Value) -> anyhow::Result<HookDecision> {
        for spec in &self.tool_call {
            let out = Self::run(
                spec,
                "tool_call",
                serde_json::json!({
                    "event": "tool_call",
                    "tool": name,
                    "args": args,
                }),
            )
            .await;
            // 退出码 2 = 拦截；其它非零/超时 fail-open
            if out.code == Some(2) {
                return Ok(HookDecision::Deny(Self::deny_reason(&out, &spec.command)));
            }
        }
        Ok(HookDecision::Proceed)
    }

    async fn on_tool_result(&self, name: &str, output: &ToolOutput) -> anyhow::Result<()> {
        // 只读观察：输出截到 4KB（stdin 管道不该被巨量结果撑爆）
        let preview: String = output.content.chars().take(4 * 1024).collect();
        Self::run_observers(
            &self.tool_result,
            "tool_result",
            serde_json::json!({
                "event": "tool_result",
                "tool": name,
                "is_error": output.is_error,
                "output": preview,
            }),
        )
        .await;
        Ok(())
    }

    async fn on_run_end(&self, answer: &str) -> anyhow::Result<()> {
        let preview: String = answer.chars().take(4 * 1024).collect();
        Self::run_observers(
            &self.run_end,
            "run_end",
            serde_json::json!({
                "event": "run_end",
                "answer": preview,
            }),
        )
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks(event: &str, command: &str) -> CommandHooks {
        let spec = CommandHookSpec {
            command: command.to_string(),
            timeout_secs: Some(5),
        };
        let mut config = HooksConfig::default();
        match event {
            "run_start" => config.run_start.push(spec),
            "run_end" => config.run_end.push(spec),
            "turn_start" => config.turn_start.push(spec),
            "tool_call" => config.tool_call.push(spec),
            "tool_result" => config.tool_result.push(spec),
            _ => panic!("unknown event {event}"),
        }
        CommandHooks::from_config(config)
    }

    #[tokio::test]
    async fn test_observer_hook_writes_marker_and_receives_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("events.log");
        let hook = hooks(
            "run_start",
            &format!(
                "cat > {}; echo done >> {}",
                marker.display(),
                marker.display()
            ),
        );

        hook.on_run_start("hello hooks").await.unwrap();
        let content = std::fs::read_to_string(&marker).unwrap();
        // stdin 收到 JSON 上下文
        assert!(
            content.contains("\"event\": \"run_start\"")
                || content.contains("\"event\":\"run_start\""),
            "{content}"
        );
        assert!(content.contains("hello hooks"), "{content}");
        assert!(content.contains("done"), "{content}");
    }

    #[tokio::test]
    async fn test_tool_call_exit_2_denies_with_stderr_reason() {
        let hook = hooks("tool_call", "echo blocked-by-policy >&2; exit 2");

        let decision = hook
            .on_tool_call("bash", &serde_json::json!({"command": "rm -rf /"}))
            .await
            .unwrap();
        match decision {
            HookDecision::Deny(reason) => {
                assert!(reason.contains("blocked-by-policy"), "{reason}");
                assert!(reason.contains("[Hook denied]"), "{reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_tool_call_other_failures_fail_open() {
        // 退出码 1：不拦截
        let hook = hooks("tool_call", "exit 1");
        let decision = hook
            .on_tool_call("bash", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(decision, HookDecision::Proceed);

        // 超时：不拦截且快速返回
        let hook = hooks("tool_call", "sleep 30");
        let spec_timeout = Duration::from_secs(1);
        let started = std::time::Instant::now();
        let hook = CommandHooks::from_config(HooksConfig {
            tool_call: vec![CommandHookSpec {
                command: "sleep 30".into(),
                timeout_secs: Some(1),
            }],
            ..Default::default()
        });
        let _ = spec_timeout;
        let decision = hook
            .on_tool_call("bash", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(decision, HookDecision::Proceed);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must kill promptly"
        );

        // 正常退出 0：放行
        let hook = hooks("tool_call", "exit 0");
        let decision = hook
            .on_tool_call("bash", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(decision, HookDecision::Proceed);
    }

    #[tokio::test]
    async fn test_tool_result_observer_gets_context() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("result.json");
        let hook = hooks("tool_result", &format!("cat > {}", marker.display()));

        hook.on_tool_result("grep", &ToolOutput::ok("matched: 3 lines"))
            .await
            .unwrap();
        let content = std::fs::read_to_string(&marker).unwrap();
        assert!(
            content.contains("\"tool\": \"grep\"") || content.contains("\"tool\":\"grep\""),
            "{content}"
        );
        assert!(content.contains("matched: 3 lines"), "{content}");
    }

    #[tokio::test]
    async fn test_registry_integration_denies_tool() {
        use baiji_agent::HookRegistry;
        let mut registry = HookRegistry::new();
        registry.register(std::sync::Arc::new(hooks("tool_call", "exit 2")));

        let decision = registry
            .tool_call("bash", &serde_json::json!({}))
            .await
            .unwrap();
        assert!(matches!(decision, HookDecision::Deny(_)));
        // 非 bash 照样拦（钩子不区分工具——脚本里自行判断）
        let decision = registry
            .tool_call("read", &serde_json::json!({}))
            .await
            .unwrap();
        assert!(matches!(decision, HookDecision::Deny(_)));
    }

    #[test]
    fn test_hooks_config_parse_and_count() {
        let config: HooksConfig = serde_json::from_value(serde_json::json!({
            "run_start": [{ "command": "echo hi" }],
            "tool_call": [
                { "command": "./guard.sh", "timeout_secs": 5 },
                { "command": "./audit.sh" }
            ]
        }))
        .unwrap();
        assert_eq!(config.count(), 3);
        assert!(!config.is_empty());
        assert_eq!(config.tool_call[0].timeout_secs, Some(5));
        assert_eq!(config.tool_call[1].timeout_secs, None);

        // 空段
        let empty: HooksConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(empty.is_empty());
    }
}
