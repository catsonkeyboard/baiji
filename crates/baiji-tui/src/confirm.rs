//! HITL 确认的 UI 桥接
//!
//! [`InteractiveApprover`] 实现 baiji-agent 的 [`Approver`]：
//! 把确认请求推给 TUI 事件循环（弹对话框），等待用户按键回复。
//! 运行被取消时立即返回 Deny，不悬挂。

use async_trait::async_trait;
use baiji_agent::{Approver, ConfirmationDecision, ConfirmationRequest};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tokio_util::sync::CancellationToken;

/// 一次待用户裁决的确认（请求 + 回复通道）
pub struct ConfirmDialog {
    pub request: ConfirmationRequest,
    pub reply: oneshot::Sender<ConfirmationDecision>,
}

/// 交互式审批人：桥接 runtime → TUI 对话框
pub struct InteractiveApprover {
    tx: UnboundedSender<ConfirmDialog>,
}

impl InteractiveApprover {
    pub fn new(tx: UnboundedSender<ConfirmDialog>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl Approver for InteractiveApprover {
    async fn confirm(
        &self,
        request: ConfirmationRequest,
        cancel: &CancellationToken,
    ) -> ConfirmationDecision {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(ConfirmDialog {
                request,
                reply: reply_tx,
            })
            .is_err()
        {
            // UI 已退出：拒绝而非悬挂
            return ConfirmationDecision::Deny("UI unavailable".to_string());
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => ConfirmationDecision::Deny("cancelled".to_string()),
            reply = reply_rx => reply.unwrap_or_else(|_| {
                ConfirmationDecision::Deny("no reply from UI".to_string())
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_interactive_approver_roundtrip() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let approver = InteractiveApprover::new(tx);

        let handle = tokio::spawn(async move {
            approver
                .confirm(
                    ConfirmationRequest {
                        tool_name: "bash".to_string(),
                        args: serde_json::json!({"command": "rm -rf ./build"}),
                    },
                    &CancellationToken::new(),
                )
                .await
        });

        let dialog = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("dialog arrives")
            .expect("channel open");
        assert_eq!(dialog.request.tool_name, "bash");
        dialog
            .reply
            .send(ConfirmationDecision::Allow)
            .expect("reply accepted");

        assert_eq!(handle.await.unwrap(), ConfirmationDecision::Allow);
    }

    #[tokio::test]
    async fn test_interactive_approver_cancel_returns_deny() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let approver = InteractiveApprover::new(tx);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                approver
                    .confirm(
                        ConfirmationRequest {
                            tool_name: "bash".to_string(),
                            args: serde_json::json!({}),
                        },
                        &cancel,
                    )
                    .await
            }
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();

        let decision = handle.await.unwrap();
        assert!(matches!(decision, ConfirmationDecision::Deny(_)));
    }

    #[tokio::test]
    async fn test_interactive_approver_ui_gone_denies() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx); // UI 退出
        let approver = InteractiveApprover::new(tx);
        let decision = approver
            .confirm(
                ConfirmationRequest {
                    tool_name: "bash".to_string(),
                    args: serde_json::json!({}),
                },
                &CancellationToken::new(),
            )
            .await;
        assert!(matches!(decision, ConfirmationDecision::Deny(_)));
    }
}
