//! bash 命令安全：风险分级 hook + HITL 判定闭包
//!
//! 两层防线（分级来自 [`baiji_command_risk::assess`]，按"会毁掉什么"判定而非命令名）：
//! 1. [`SafetyPlugin`] 的 hook（在 HITL 之前执行）：
//!    - Catastrophic（家目录/系统目录/凭据库/设备节点）→ 直接拒绝，任何理由都解锁不了
//!    - Confirm（不可逆且目标无法静态确定、`git reset --hard`、`curl | sh` 等）→
//!      首次拒绝并要求模型对照用户原始请求反思；带上充分的 `justification` 参数重发才放行
//! 2. [`readonly_auto_approve`] / [`irreversible_escalation`]：注入 `ConfirmationGate`——
//!    只读命令免弹窗；Confirm 级命令无视 AllowAll 必弹，并把原因展示给用户

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AutoApproveFn, EscalateFn, Hook, HookDecision};
use baiji_command_risk::{
    GateOutcome, Justification, RiskContext, RiskLevel, assess, gate,
    readonly::{ReadOnlyPolicy, is_read_only},
};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

use crate::{Plugin, PluginContext};

/// bash 工具名（风险分级只作用于它）
const BASH: &str = "bash";

fn command_of(args: &Value) -> Option<&str> {
    args["command"].as_str()
}

fn risk_context(working_dir: Option<PathBuf>) -> RiskContext {
    RiskContext::from_env(working_dir.or_else(|| std::env::current_dir().ok()))
}

/// 危险命令拦截 hook
struct DangerousCommandHook {
    working_dir: Option<PathBuf>,
}

#[async_trait]
impl Hook for DangerousCommandHook {
    fn name(&self) -> &str {
        "dangerous-command-guard"
    }

    async fn on_tool_call(&self, name: &str, args: &Value) -> Result<HookDecision> {
        if name != BASH {
            return Ok(HookDecision::Proceed);
        }
        let Some(command) = command_of(args) else {
            return Ok(HookDecision::Proceed);
        };
        let assessment = assess(command, &risk_context(self.working_dir.clone()));
        if assessment.level == RiskLevel::Low {
            info!("low-risk destructive command allowed: {command}");
        }
        let justification = Justification {
            text: args["justification"].as_str().map(str::to_string),
        };
        Ok(match gate(&assessment, &justification) {
            GateOutcome::Allow => HookDecision::Proceed,
            GateOutcome::Reflect { prompt } => HookDecision::Deny(prompt),
            GateOutcome::Deny { reason } => HookDecision::Deny(reason),
        })
    }
}

/// 安全插件：bash 命令按破坏半径分级拦截
#[derive(Default)]
pub struct SafetyPlugin {
    /// 判定"工作区内"的基准目录（None = 进程当前目录）
    working_dir: Option<PathBuf>,
}

impl SafetyPlugin {
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: Some(working_dir.into()),
        }
    }
}

impl Plugin for SafetyPlugin {
    fn id(&self) -> &str {
        "safety"
    }
    fn description(&self) -> &str {
        "Blocks catastrophic bash commands and makes irreversible ones justify themselves"
    }
    fn register(&self, ctx: &mut PluginContext) -> Result<()> {
        ctx.add_hook(Arc::new(DangerousCommandHook {
            working_dir: self.working_dir.clone(),
        }));
        Ok(())
    }
}

/// 只读 bash 命令免确认：所有命令段都是已知检查程序、路径都在 `roots` 内，
/// 或命令段以用户信任的前缀开头（`trusted` 仅来自全局配置）
pub fn readonly_auto_approve(roots: Vec<PathBuf>, trusted: &[String]) -> AutoApproveFn {
    let policy = ReadOnlyPolicy::new(roots).with_trusted_prefixes(trusted);
    Arc::new(move |tool, args| {
        tool == BASH
            && !args["run_in_background"].as_bool().unwrap_or(false)
            && command_of(args).is_some_and(|cmd| is_read_only(cmd, &policy))
    })
}

/// Confirm 级 bash 命令升级为必弹确认（无视 AllowAll），原因附模型的 justification
pub fn irreversible_escalation(working_dir: PathBuf) -> EscalateFn {
    Arc::new(move |tool, args| {
        if tool != BASH {
            return None;
        }
        let assessment = assess(command_of(args)?, &risk_context(Some(working_dir.clone())));
        if assessment.level != RiskLevel::Confirm {
            return None;
        }
        let mut reason = assessment.explanation().trim_end().to_string();
        if let Some(why) = args["justification"].as_str().filter(|s| !s.trim().is_empty()) {
            reason.push_str("\njustification: ");
            reason.push_str(why.trim());
        }
        Some(reason)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use baiji_agent::{HookRegistry, ToolRegistry};

    async fn hooks() -> HookRegistry {
        let mut tools = ToolRegistry::new();
        let mut hooks = HookRegistry::new();
        crate::PluginManager::new()
            .add(Box::new(SafetyPlugin::new("/home/u/proj")))
            .apply(&mut tools, &mut hooks)
            .unwrap();
        hooks
    }

    async fn decide(hooks: &HookRegistry, args: Value) -> HookDecision {
        hooks.tool_call("bash", &args).await.unwrap()
    }

    #[tokio::test]
    async fn test_confirm_level_requires_justification() {
        let hooks = hooks().await;
        let cmd = "git reset --hard HEAD~2";
        match decide(&hooks, serde_json::json!({ "command": cmd })).await {
            HookDecision::Deny(prompt) => {
                assert!(prompt.contains("justification"), "{prompt}");
                assert!(prompt.contains("reset --hard"), "{prompt}");
            }
            other => panic!("expected reflection, got {other:?}"),
        }
        // 敷衍的理由不算
        let lazy = serde_json::json!({ "command": cmd, "justification": "yes" });
        assert!(matches!(decide(&hooks, lazy).await, HookDecision::Deny(_)));
        // 充分的理由放行
        let ok = serde_json::json!({
            "command": cmd,
            "justification": "the user asked me to drop the last two local commits"
        });
        assert_eq!(decide(&hooks, ok).await, HookDecision::Proceed);
    }

    #[tokio::test]
    async fn test_catastrophic_cannot_be_justified() {
        let hooks = hooks().await;
        let args = serde_json::json!({
            "command": "rm -rf ~",
            "justification": "the user explicitly asked to wipe the home directory"
        });
        match decide(&hooks, args).await {
            HookDecision::Deny(reason) => assert!(reason.contains("cannot be confirmed")),
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[test]
    fn test_readonly_auto_approve_closure() {
        let f = readonly_auto_approve(vec![PathBuf::from("/home/u/proj")], &["cargo test".into()]);
        let bash = |cmd: &str| serde_json::json!({ "command": cmd });
        assert!(f("bash", &bash("git status")));
        assert!(f("bash", &bash("cargo test -p x")));
        assert!(!f("bash", &bash("cargo build")));
        assert!(!f("bash", &bash("cat /etc/passwd")));
        // 后台任务永不免确认；非 bash 工具不归它管
        assert!(!f(
            "bash",
            &serde_json::json!({ "command": "ls", "run_in_background": true })
        ));
        assert!(!f("write", &bash("ls")));
    }

    #[test]
    fn test_escalation_closure_includes_justification() {
        let f = irreversible_escalation(PathBuf::from("/home/u/proj"));
        assert!(f("bash", &serde_json::json!({ "command": "ls" })).is_none());
        assert!(f("bash", &serde_json::json!({ "command": "rm -rf build" })).is_none());
        let reason = f(
            "bash",
            &serde_json::json!({
                "command": "git push --force",
                "justification": "user asked to overwrite the remote branch"
            }),
        )
        .expect("escalated");
        assert!(reason.contains("rewrites shared history"), "{reason}");
        assert!(reason.contains("justification: user asked"), "{reason}");
    }
}
