//! HITL（human-in-the-loop）确认
//!
//! 高危工具执行前先经 [`Approver`] 审批：
//! - 默认 [`AutoApprover`] 全部放行（无 UI 依赖时保持自动行为）
//! - 交互式实现（如 TUI 的对话框）在 `baiji-tui` 侧提供
//! - [`ConfirmationDecision::AllowAll`] 对本次运行内同名工具持续放行
//! - 可注入两个判定闭包（main.rs 用 baiji-command-risk 实现，agent 不依赖它）：
//!   `auto_approve`（只读 bash 等免弹窗）与 `escalate`（不可逆命令：无视 AllowAll
//!   必弹，并把原因展示给用户）

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// 一次确认请求
#[derive(Debug, Clone, Default)]
pub struct ConfirmationRequest {
    pub tool_name: String,
    pub args: Value,
    /// 升级原因（不可逆/高危命令为何必须人工确认）；None = 普通确认
    pub reason: Option<String>,
}

impl ConfirmationRequest {
    pub fn new(tool_name: impl Into<String>, args: Value) -> Self {
        Self {
            tool_name: tool_name.into(),
            args,
            reason: None,
        }
    }
}

/// 免确认判定：返回 true 时跳过弹窗（如只读 bash 命令）
pub type AutoApproveFn = Arc<dyn Fn(&str, &Value) -> bool + Send + Sync>;
/// 升级判定：返回 Some(原因) 时必须弹窗，且 AllowAll 记忆不生效
pub type EscalateFn = Arc<dyn Fn(&str, &Value) -> Option<String> + Send + Sync>;

/// 确定裁决
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmationDecision {
    /// 允许本次执行
    Allow,
    /// 允许本次，且本次运行内同名工具不再询问
    AllowAll,
    /// 拒绝执行（理由回传给 LLM）
    Deny(String),
}

/// 审批人契约
#[async_trait]
pub trait Approver: Send + Sync {
    /// `cancel` 触发（用户取消整个运行）时应立即返回 Deny，避免悬挂
    async fn confirm(
        &self,
        request: ConfirmationRequest,
        cancel: &CancellationToken,
    ) -> ConfirmationDecision;
}

/// 自动放行（默认）
pub struct AutoApprover;

#[async_trait]
impl Approver for AutoApprover {
    async fn confirm(
        &self,
        _request: ConfirmationRequest,
        _cancel: &CancellationToken,
    ) -> ConfirmationDecision {
        ConfirmationDecision::Allow
    }
}

/// 全部拒绝（headless 模式的安全默认：无人值守时高危工具直接拒绝）
pub struct DenyAllApprover;

#[async_trait]
impl Approver for DenyAllApprover {
    async fn confirm(
        &self,
        _request: ConfirmationRequest,
        _cancel: &CancellationToken,
    ) -> ConfirmationDecision {
        ConfirmationDecision::Deny(
            "unattended mode: no user to confirm (run with --yes to auto-approve)".to_string(),
        )
    }
}

/// 确认门控：命中名单的工具需要审批；AllowAll 结果在运行期内记忆。
///
/// 判定顺序（仅对名单内工具）：escalate 命中 → 必弹（带原因，AllowAll 只算本次）；
/// 否则 auto_approve 命中 → 静默放行；否则已 AllowAll → 静默放行；否则弹窗。
#[derive(Clone)]
pub struct ConfirmationGate {
    required: HashSet<String>,
    approver: Arc<dyn Approver>,
    /// 本次运行内已 AllowAll 的工具名
    granted_all: Arc<Mutex<HashSet<String>>>,
    auto_approve: Option<AutoApproveFn>,
    escalate: Option<EscalateFn>,
}

impl Default for ConfirmationGate {
    fn default() -> Self {
        Self::new(Vec::new(), Arc::new(AutoApprover))
    }
}

impl ConfirmationGate {
    pub fn new(required: Vec<String>, approver: Arc<dyn Approver>) -> Self {
        Self {
            required: required.into_iter().collect(),
            approver,
            granted_all: Arc::new(Mutex::new(HashSet::new())),
            auto_approve: None,
            escalate: None,
        }
    }

    /// 注入免确认判定（如只读 bash 命令跳过弹窗）
    pub fn with_auto_approve(mut self, f: AutoApproveFn) -> Self {
        self.auto_approve = Some(f);
        self
    }

    /// 注入升级判定（不可逆命令无视 AllowAll 必弹）
    pub fn with_escalation(mut self, f: EscalateFn) -> Self {
        self.escalate = Some(f);
        self
    }

    /// 该工具是否需要确认
    pub fn needs(&self, tool_name: &str) -> bool {
        self.required.contains(tool_name)
    }

    /// 审批：命中名单时按 escalate → auto_approve → AllowAll 记忆 → 询问 的顺序裁决
    pub async fn confirm(
        &self,
        mut request: ConfirmationRequest,
        cancel: &CancellationToken,
    ) -> ConfirmationDecision {
        let tool_name = request.tool_name.clone();
        if !self.needs(&tool_name) {
            return ConfirmationDecision::Allow;
        }
        let escalation = self
            .escalate
            .as_ref()
            .and_then(|f| f(&tool_name, &request.args));
        if escalation.is_none() {
            if self
                .auto_approve
                .as_ref()
                .is_some_and(|f| f(&tool_name, &request.args))
            {
                return ConfirmationDecision::Allow;
            }
            if self.granted_all.lock().unwrap().contains(&tool_name) {
                return ConfirmationDecision::Allow;
            }
        }
        let escalated = escalation.is_some();
        if escalated {
            request.reason = escalation;
        }
        match self.approver.confirm(request, cancel).await {
            // 升级请求的 AllowAll 只放行本次：不让一次 "a" 静默放过之后的不可逆命令
            ConfirmationDecision::AllowAll if escalated => ConfirmationDecision::Allow,
            ConfirmationDecision::AllowAll => {
                self.granted_all.lock().unwrap().insert(tool_name);
                ConfirmationDecision::AllowAll
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingApprover {
        calls: AtomicUsize,
        verdict: ConfirmationDecision,
    }

    #[async_trait]
    impl Approver for CountingApprover {
        async fn confirm(
            &self,
            _request: ConfirmationRequest,
            _cancel: &CancellationToken,
        ) -> ConfirmationDecision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.verdict.clone()
        }
    }

    fn request(name: &str) -> ConfirmationRequest {
        ConfirmationRequest::new(name, serde_json::json!({}))
    }

    fn bash(cmd: &str) -> ConfirmationRequest {
        ConfirmationRequest::new("bash", serde_json::json!({ "command": cmd }))
    }

    /// 记录收到的请求（验证 reason 透传）
    struct RecordingApprover {
        seen: Mutex<Vec<ConfirmationRequest>>,
        verdict: ConfirmationDecision,
    }

    #[async_trait]
    impl Approver for RecordingApprover {
        async fn confirm(
            &self,
            request: ConfirmationRequest,
            _cancel: &CancellationToken,
        ) -> ConfirmationDecision {
            self.seen.lock().unwrap().push(request);
            self.verdict.clone()
        }
    }

    fn command_of(args: &Value) -> &str {
        args["command"].as_str().unwrap_or("")
    }

    #[tokio::test]
    async fn test_auto_approve_skips_prompt() {
        let approver = Arc::new(RecordingApprover {
            seen: Mutex::new(Vec::new()),
            verdict: ConfirmationDecision::Deny("no".into()),
        });
        let gate = ConfirmationGate::new(vec!["bash".into()], approver.clone())
            .with_auto_approve(Arc::new(|tool, args| {
                tool == "bash" && command_of(args) == "ls"
            }));
        let cancel = CancellationToken::new();
        assert_eq!(gate.confirm(bash("ls"), &cancel).await, ConfirmationDecision::Allow);
        assert!(matches!(
            gate.confirm(bash("make"), &cancel).await,
            ConfirmationDecision::Deny(_)
        ));
        assert_eq!(approver.seen.lock().unwrap().len(), 1, "only `make` prompted");
    }

    #[tokio::test]
    async fn test_escalation_bypasses_allow_all_and_carries_reason() {
        let approver = Arc::new(RecordingApprover {
            seen: Mutex::new(Vec::new()),
            verdict: ConfirmationDecision::AllowAll,
        });
        let gate = ConfirmationGate::new(vec!["bash".into()], approver.clone())
            .with_auto_approve(Arc::new(|_, _| true))
            .with_escalation(Arc::new(|_, args| {
                command_of(args)
                    .contains("reset --hard")
                    .then(|| "discards uncommitted changes".to_string())
            }));
        let cancel = CancellationToken::new();

        // 升级命令：即使 auto_approve 也要弹窗；AllowAll 只算本次
        assert_eq!(
            gate.confirm(bash("git reset --hard"), &cancel).await,
            ConfirmationDecision::Allow
        );
        assert_eq!(
            gate.confirm(bash("git reset --hard"), &cancel).await,
            ConfirmationDecision::Allow
        );
        {
            let seen = approver.seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "escalated commands prompt every time");
            assert_eq!(seen[0].reason.as_deref(), Some("discards uncommitted changes"));
        }
        // 普通命令走 auto_approve，不弹窗
        assert_eq!(gate.confirm(bash("ls"), &cancel).await, ConfirmationDecision::Allow);
        assert_eq!(approver.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_escalation_only_applies_to_required_tools() {
        let approver = Arc::new(CountingApprover {
            calls: AtomicUsize::new(0),
            verdict: ConfirmationDecision::Deny("no".into()),
        });
        // bash 不在名单内：用户关闭了 HITL，升级判定也不弹窗（反思门在 hook 层兜底）
        let gate = ConfirmationGate::new(vec!["write".into()], approver.clone())
            .with_escalation(Arc::new(|_, _| Some("risky".into())));
        assert_eq!(
            gate.confirm(bash("git reset --hard"), &CancellationToken::new()).await,
            ConfirmationDecision::Allow
        );
        assert_eq!(approver.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_gate_default_allows_everything() {
        let gate = ConfirmationGate::default();
        assert!(!gate.needs("bash"));
        assert_eq!(
            gate.confirm(request("bash"), &CancellationToken::new()).await,
            ConfirmationDecision::Allow
        );
    }

    #[tokio::test]
    async fn test_gate_deny_stops_tool() {
        let approver = Arc::new(CountingApprover {
            calls: AtomicUsize::new(0),
            verdict: ConfirmationDecision::Deny("too risky".to_string()),
        });
        let gate = ConfirmationGate::new(vec!["bash".to_string()], approver.clone());

        assert!(gate.needs("bash"));
        assert_eq!(
            gate.confirm(request("bash"), &CancellationToken::new()).await,
            ConfirmationDecision::Deny("too risky".to_string())
        );
        assert!(!gate.needs("read"));
        assert_eq!(approver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_deny_all_approver() {
        let approver = DenyAllApprover;
        let decision = approver
            .confirm(request("bash"), &CancellationToken::new())
            .await;
        match decision {
            ConfirmationDecision::Deny(reason) => assert!(reason.contains("--yes")),
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_allow_all_remembers_for_run() {
        let approver = Arc::new(CountingApprover {
            calls: AtomicUsize::new(0),
            verdict: ConfirmationDecision::AllowAll,
        });
        let gate = ConfirmationGate::new(vec!["bash".to_string()], approver.clone());

        // 第一次询问 → AllowAll
        assert_eq!(
            gate.confirm(request("bash"), &CancellationToken::new()).await,
            ConfirmationDecision::AllowAll
        );
        // 后续同名工具静默放行
        assert_eq!(
            gate.confirm(request("bash"), &CancellationToken::new()).await,
            ConfirmationDecision::Allow
        );
        assert_eq!(approver.calls.load(Ordering::SeqCst), 1, "no re-prompt");
    }
}
