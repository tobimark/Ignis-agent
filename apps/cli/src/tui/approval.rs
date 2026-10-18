//! 通道式审批处理器：提示画在 TUI 的滚动区/输入区，按键由 UI 循环收。
//!
//! 分工与经典的 [`crate::approval::InteractiveApproval`] 对应：本模块只负责「把请求发给 UI
//! 循环并等回信」，提示文案与按键语义分别复用 [`crate::approval::prompt_lines`] 与
//! [`crate::approval::parse_choice`]——两个界面因此是同一份文案、同一套归一。
//!
//! fail-safe 与 [`crate::approval::PolicyOnlyApproval`] 同向：发送失败（UI 已退出）或回信端掉落
//! （UI 走了）= [`ApprovalDecision::Reject`]——审批面不可用时绝不能静默放行。
//!
//! 接线：`TuiApproval::channel()` 的两端分别交给 `Session::new_with_handler`（handler）与
//! `TuiApp::attach_approvals`（receiver，由 `drive` 的审批分支收下并画进滚动区）。

use async_trait::async_trait;
use ignis_runtime::{ApprovalDecision, ApprovalHandler, ApprovalRequest};
use tokio::sync::{mpsc, oneshot};

/// 一次审批请求 + 回信口：`TuiApproval::decide` 把它发给 UI 循环并等回信。
pub struct ApprovalJob {
    /// 待展示的审批请求（工具名、摘要、参数、风险）。
    pub request: ApprovalRequest,
    /// 回信口：UI 按键给出决定后回信（掉落 = UI 走了）。
    pub reply: oneshot::Sender<ApprovalDecision>,
}

/// 通道式审批：提示画在 TUI 的滚动区/输入区，按键由 UI 循环收。
pub struct TuiApproval {
    /// 请求交给 UI 循环的那一端（接收端由 `TuiApp::attach_approvals` 接进驱动循环）。
    tx: mpsc::UnboundedSender<ApprovalJob>,
}

impl TuiApproval {
    /// 接上 UI 循环的发送端（另一端交给应用状态）。
    pub fn new(tx: mpsc::UnboundedSender<ApprovalJob>) -> Self {
        Self { tx }
    }

    /// 建一对：`(handler, receiver)`（handler 交给 `Session::new_with_handler`）。
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<ApprovalJob>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self::new(tx), rx)
    }
}

#[async_trait]
impl ApprovalHandler for TuiApproval {
    /// 把请求交给 UI 循环并等它回信；两处失败都 fail-safe 拒绝。
    async fn decide(&self, req: ApprovalRequest) -> ApprovalDecision {
        let (reply, rx) = oneshot::channel();
        // UI 循环已经退出（接收端掉了）：没有人能回答
        if self
            .tx
            .send(ApprovalJob {
                request: req,
                reply,
            })
            .is_err()
        {
            return ApprovalDecision::Reject;
        }
        // 回信端随 UI 走掉（请求被丢掉、回信前 UI 退出）：同样拒绝——绝不静默放行
        rx.await.unwrap_or(ApprovalDecision::Reject)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ignis_runtime::{ApprovalDecision, ApprovalHandler, ApprovalRequest, RiskLevel};
    use serde_json::json;

    use super::TuiApproval;

    /// 一次审批请求（与经典审批用例同形）。
    fn request() -> ApprovalRequest {
        ApprovalRequest {
            tool: "fs_write".to_string(),
            summary: "Writing a.txt".to_string(),
            args: json!({ "path": "a.txt", "recursive": true }),
            risk: RiskLevel::Elevated,
        }
    }

    /// 一次请求的往返：`decide` 把请求交给 UI 循环并等回信，回信就是它的返回值。
    #[tokio::test]
    async fn tui_approval_sends_the_request_and_awaits_the_reply() {
        let (handler, mut rx) = TuiApproval::channel();
        // 交给 `Session::new_with_handler` 的就是这个形状
        let handler: Arc<dyn ApprovalHandler> = Arc::new(handler);
        let deciding = tokio::spawn(async move { handler.decide(request()).await });

        let job = rx.recv().await.expect("审批请求必须到达 UI 循环");
        assert_eq!(job.request.tool, "fs_write");
        assert_eq!(job.request.summary, "Writing a.txt");
        assert_eq!(job.request.risk, RiskLevel::Elevated);
        assert_eq!(job.request.args["path"], "a.txt");

        job.reply
            .send(ApprovalDecision::AllowOnce)
            .expect("回信端还在");
        assert_eq!(
            deciding.await.expect("审批任务应结束"),
            ApprovalDecision::AllowOnce
        );
    }

    /// fail-safe（与 `PolicyOnlyApproval` 同向）：UI 已经走了（接收端掉落）或回信端掉落
    /// （请求被丢掉、回信前 UI 退出）一律 Reject——审批面不可用时绝不能静默放行。
    #[tokio::test]
    async fn tui_approval_rejects_when_the_ui_is_gone() {
        // 接收端已经掉落：请求根本发不出去
        let (handler, rx) = TuiApproval::channel();
        drop(rx);
        assert_eq!(handler.decide(request()).await, ApprovalDecision::Reject);

        // 接收端还在但请求被丢掉（回信端随请求一起消失）
        let (handler, mut rx) = TuiApproval::channel();
        let deciding = tokio::spawn(async move { handler.decide(request()).await });
        let job = rx.recv().await.expect("审批请求必须到达 UI 循环");
        drop(job);
        assert_eq!(
            deciding.await.expect("审批任务应结束"),
            ApprovalDecision::Reject
        );
    }
}
