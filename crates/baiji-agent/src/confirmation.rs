//! HITL（human-in-the-loop）确认
//!
//! 高危工具执行前先经 [`Approver`] 审批：
//! - 默认 [`AutoApprover`] 全部放行（无 UI 依赖时保持自动行为）
//! - 交互式实现（如 TUI 的对话框）在 `baiji-tui` 侧提供
//! - [`ConfirmationDecision::AllowAll`] 对本次运行内同名工具持续放行

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// 一次确认请求
#[derive(Debug, Clone)]
pub struct ConfirmationRequest {
    pub tool_name: String,
    pub args: Value,
}

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
#[derive(Clone)]
pub struct ConfirmationGate {
    required: HashSet<String>,
    approver: Arc<dyn Approver>,
    /// 本次运行内已 AllowAll 的工具名
    granted_all: Arc<Mutex<HashSet<String>>>,
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
        }
    }

    /// 该工具是否需要确认
    pub fn needs(&self, tool_name: &str) -> bool {
        self.required.contains(tool_name)
    }

    /// 是否需要弹窗（已 AllowAll 的工具静默放行）
    fn needs_prompt(&self, tool_name: &str) -> bool {
        self.needs(tool_name)
            && !self.granted_all.lock().unwrap().contains(tool_name)
    }

    /// 审批：命中名单且未放行时询问 Approver；AllowAll 记入运行期放行集
    pub async fn confirm(
        &self,
        request: ConfirmationRequest,
        cancel: &CancellationToken,
    ) -> ConfirmationDecision {
        if !self.needs_prompt(&request.tool_name) {
            return ConfirmationDecision::Allow;
        }
        let tool_name = request.tool_name.clone();
        match self.approver.confirm(request, cancel).await {
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
        ConfirmationRequest {
            tool_name: name.to_string(),
            args: serde_json::json!({}),
        }
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
