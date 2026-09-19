//! 外部 coding agent 工具（codex / claude / pi 等 CLI 的 headless 模式）
//!
//! 配置驱动（全局 `external_agents`）：每个条目注册为一个同名工具 +
//! 同名斜杠命令。命令模板支持 `{prompt}` 占位符（自动单引号转义）；
//! 无占位符时把 prompt 追加为最后一个参数。
//!
//! - 复用 bash 的进程组击杀与有上限输出捕获（超时不残留孙进程）
//! - 输出只做 ANSI 剥离与字节截断（agent 答案是正文，不做有损压缩）
//! - 安全：外部 agent 会执行 shell / 修改工作区，等同 bash 的能力面——
//!   `external_agents` 是全局专用配置（项目级配置被白名单丢弃），
//!   需要人工把关时把名字加进 `require_confirmation_tools`
//! - 计划模式：外部 agent 不在只读白名单内，规划阶段自动被拒

use crate::env::ExecutionEnv;
use crate::tools::bash::{GroupKillGuard, PIPE_DRAIN_GRACE, finish_capture, spawn_capture};
use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput, ToolRegistry};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

/// 外部 agent 超时上限（coding 任务可能较久，但必须有界）
const MAX_TIMEOUT_SECS: u64 = 3600;
/// 默认超时
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// 一个外部 coding agent 的配置（工具名 = 斜杠命令名）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalAgentSpec {
    /// 工具/命令名（如 "codex"）。非空、无空白
    pub name: String,
    /// 命令模板：`codex exec {prompt}`。`{prompt}` 由转义后的任务描述替换；
    /// 模板不含占位符时追加为最后一个参数
    pub command: String,
    /// 工具描述里补充的用途说明（可选）
    #[serde(default)]
    pub description: String,
    /// 超时秒数（默认 300，上限 3600）
    #[serde(default)]
    pub timeout_secs: u64,
}

/// POSIX 单引号转义：任意内容安全地作为一个 shell 参数
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 展开命令模板：替换 {prompt}（无占位符则追加）
fn build_command(spec: &ExternalAgentSpec, prompt: &str) -> String {
    let quoted = shell_quote(prompt);
    if spec.command.contains("{prompt}") {
        spec.command.replacen("{prompt}", &quoted, 1)
    } else {
        format!("{} {quoted}", spec.command)
    }
}

/// 执行外部 agent 并取回全部输出（工具与 TUI 斜杠命令共用）。
/// Ok = 进程跑完（含退出码信息）；Err = 启动失败或超时（含部分输出）
pub async fn run_external(
    spec: &ExternalAgentSpec,
    prompt: &str,
    workdir: &Path,
) -> std::result::Result<String, String> {
    let command = build_command(spec, prompt);
    let secs = if spec.timeout_secs == 0 {
        DEFAULT_TIMEOUT_SECS
    } else {
        spec.timeout_secs
    };
    let timeout = Duration::from_secs(secs.clamp(1, MAX_TIMEOUT_SECS));

    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&command)
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Err(format!(
                "[Error] spawning external agent '{}': {e}",
                spec.name
            ));
        }
    };
    let mut guard = GroupKillGuard::new(child.id());
    let (out_buf, out_task) = spawn_capture(child.stdout.take());
    let (err_buf, err_task) = spawn_capture(child.stderr.take());

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(format!("[Error] waiting for external agent: {e}")),
        Err(_) => {
            drop(guard); // SIGKILL 整组
            child.wait().await.ok();
            let (stdout, _) = finish_capture(out_buf, out_task, PIPE_DRAIN_GRACE).await;
            let (stderr, _) = finish_capture(err_buf, err_task, PIPE_DRAIN_GRACE).await;
            let mut message = format!(
                "[Timeout] external agent '{}' killed after {}s",
                spec.name,
                timeout.as_secs()
            );
            for (label, text) in [("stdout", stdout), ("stderr", stderr)] {
                if !text.is_empty() {
                    message.push_str(&format!(
                        "\n\n[partial {label}]\n{}",
                        crate::compressors::strip_ansi(&text)
                    ));
                }
            }
            return Err(message);
        }
    };
    guard.disarm();

    let (stdout, _) = finish_capture(out_buf, out_task, PIPE_DRAIN_GRACE).await;
    let (stderr, _) = finish_capture(err_buf, err_task, PIPE_DRAIN_GRACE).await;
    // agent 的最终答案走 stdout；stderr 是进度/日志（非空时附上）
    let mut combined = crate::compressors::strip_ansi(&stdout).trim().to_string();
    let stderr = crate::compressors::strip_ansi(&stderr).trim().to_string();
    if !stderr.is_empty() {
        combined.push_str(&format!("\n\n[stderr]\n{stderr}"));
    }
    if !status.success() {
        combined = format!(
            "[external agent exited with {}]\n{combined}",
            status.code().unwrap_or(-1)
        );
    }
    Ok(combined)
}

/// 外部 agent 工具：把任务委托给配置好的 CLI coding agent
pub struct ExternalAgentTool {
    env: Arc<ExecutionEnv>,
    spec: ExternalAgentSpec,
    /// 描述在构造时预构建（description() 每次请求都会被调用）
    desc: String,
}

impl ExternalAgentTool {
    pub fn new(env: Arc<ExecutionEnv>, spec: ExternalAgentSpec) -> Self {
        let extra = if spec.description.is_empty() {
            String::new()
        } else {
            format!(" {}", spec.description)
        };
        let desc = format!(
            "Delegate a self-contained coding task to the external '{}' agent CLI. It runs in \
             this workspace with its own tools/model — it can run shell commands and modify \
             files (NOT read-only), so prefer it for whole subtasks rather than single lookups. \
             Returns the agent's final output only.{extra}",
            spec.name
        );
        Self { env, spec, desc }
    }
}

#[async_trait]
impl AgentTool for ExternalAgentTool {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> &str {
        &self.desc
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Self-contained task description — the external agent sees nothing of this conversation"}
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let Some(prompt) = args["prompt"]
            .as_str()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            return Ok(ToolOutput::err("[Error] requires a non-empty 'prompt'"));
        };

        let output = match run_external(&self.spec, prompt, &self.env.workdir).await {
            Ok(text) => text,
            Err(text) => {
                return Ok(ToolOutput::err(self.env.truncate_output(&text)));
            }
        };
        // 正文不做有损压缩：只截断（超限时 spill 由 truncate 管线负责可逆）
        let (delivered, truncated_at, truncated_tokens) = self.env.truncate_with_meta(&output);
        let original = truncated_at.unwrap_or(0);
        let original_tokens =
            truncated_tokens.unwrap_or(baiji_agent::estimate_text_tokens(&output) as u64);
        let mut result = if (delivered.len() as u64) < original {
            ToolOutput::ok(delivered)
                .with_original_bytes(original)
                .with_original_tokens(original_tokens)
        } else {
            ToolOutput::ok(delivered)
        };
        if output.starts_with("[external agent exited with") || output.starts_with("[Timeout]") {
            result.is_error = true;
        }
        Ok(result)
    }
}

/// 名称合法性：非空、无空白（作为工具名与 shell 单词）
fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && !name.chars().any(|c| c.is_whitespace()) && !name.starts_with('/')
}

/// 注册全部外部 agent 工具；名称非法或与已注册工具冲突的条目跳过并告警。
/// 返回实际注册数
pub fn register_external_agents(
    registry: &mut ToolRegistry,
    env: &Arc<ExecutionEnv>,
    specs: &[ExternalAgentSpec],
) -> usize {
    let mut registered = 0;
    for spec in specs {
        if !valid_name(&spec.name) {
            tracing::warn!("external agent '{}' skipped: invalid name", spec.name);
            continue;
        }
        if registry.get(&spec.name).is_some() {
            tracing::warn!(
                "external agent '{}' skipped: a tool with this name is already registered",
                spec.name
            );
            continue;
        }
        registry.register(Arc::new(ExternalAgentTool::new(env.clone(), spec.clone())));
        registered += 1;
    }
    registered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(dir: &std::path::Path) -> Arc<ExecutionEnv> {
        Arc::new(ExecutionEnv::new(dir))
    }

    #[test]
    fn test_build_command_placeholder_and_append() {
        let spec = ExternalAgentSpec {
            name: "codex".into(),
            command: "codex exec {prompt}".into(),
            description: String::new(),
            timeout_secs: 0,
        };
        assert_eq!(build_command(&spec, "hi there"), "codex exec 'hi there'");
        // 单引号在 prompt 内被转义
        assert_eq!(build_command(&spec, "it's"), "codex exec 'it'\\''s'");
        // 无占位符：追加为最后一个参数
        let spec = ExternalAgentSpec {
            command: "claude -p".into(),
            name: "claude".into(),
            description: String::new(),
            timeout_secs: 0,
        };
        assert_eq!(build_command(&spec, "do it"), "claude -p 'do it'");
    }

    #[tokio::test]
    async fn test_run_external_captures_output_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ExternalAgentSpec {
            name: "fake".into(),
            command: "printf 'ans:%s' {prompt}".into(),
            description: String::new(),
            timeout_secs: 10,
        };
        let out = run_external(&spec, "hello", dir.path()).await.unwrap();
        assert!(out.contains("ans:hello"), "{out}");

        // 非零退出：输出保留并带退出码标注
        let spec = ExternalAgentSpec {
            command: "sh -c 'echo boom >&2; exit 3'".into(),
            name: "fail".into(),
            description: String::new(),
            timeout_secs: 10,
        };
        let out = run_external(&spec, "x", dir.path()).await.unwrap();
        assert!(out.contains("exited with 3"), "{out}");
        assert!(out.contains("boom"), "{out}");
    }

    #[tokio::test]
    async fn test_run_external_timeout_kills() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ExternalAgentSpec {
            name: "slow".into(),
            // 尾注释吞掉追加的 prompt 参数（模拟不接收任务参数的长命令）
            command: "sleep 30 #".into(),
            description: String::new(),
            timeout_secs: 1,
        };
        let start = std::time::Instant::now();
        let err = run_external(&spec, "x", dir.path()).await.unwrap_err();
        assert!(err.contains("[Timeout]"), "{err}");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_tool_truncates_and_marks_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = ExecutionEnv::new(dir.path());
        e.max_output_bytes = 64;
        let tool = ExternalAgentTool::new(
            Arc::new(e),
            ExternalAgentSpec {
                name: "big".into(),
                command: "printf %s {prompt}".into(),
                description: String::new(),
                timeout_secs: 10,
            },
        );
        let out = tool
            .execute(serde_json::json!({"prompt": "x".repeat(4096)}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.len() < 200, "truncated: {}", out.content.len());
        assert!(
            out.original_bytes.is_some(),
            "ledger records the original size"
        );
    }

    #[test]
    fn test_register_skips_invalid_and_conflicting_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(crate::tools::BashTool::new(env(dir.path()))));
        let specs = vec![
            ExternalAgentSpec {
                name: "codex".into(),
                command: "codex exec".into(),
                description: String::new(),
                timeout_secs: 0,
            },
            ExternalAgentSpec {
                name: "bash".into(),
                command: "evil".into(),
                description: String::new(),
                timeout_secs: 0,
            },
            ExternalAgentSpec {
                name: "bad name".into(),
                command: "x".into(),
                description: String::new(),
                timeout_secs: 0,
            },
        ];
        let n = register_external_agents(&mut registry, &env(dir.path()), &specs);
        assert_eq!(n, 1, "only codex registered");
        assert!(registry.get("codex").is_some());
    }
}
