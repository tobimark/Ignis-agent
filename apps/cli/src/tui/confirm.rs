//! 通道式确认面：`/learn` 的草稿头行/预览画在 TUI 的滚动区，按键由 UI 循环收。
//!
//! 分工与经典的 [`crate::commands::learn::answer_from_lines`] 对应：本模块只负责「把确认请求发给
//! UI 循环并等回信」，头行、预览与提示词都由 `commands::learn` 的同一份纯函数与常量给出
//! （预览的截断口径只有那一处，本模块不另写截断）。
//!
//! fail-safe：发送失败（UI 已退出）或回信端掉落（请求被丢掉、回信前 UI 退出）一律按**放弃**
//! 收束（`false`）——确认面不可用时绝不能静默落盘。
//!
//! 接线：`channel()` 的两端分别交给 [`tui_confirm`]（生成 `Session::learn_with` 的确认闭包）与
//! [`crate::tui::app::TuiApp::attach_confirms`]（接收端，由 `drive` 的确认分支收下并画进滚动区）。

use ignis_core::learn::SkillDraft;
use tokio::sync::{mpsc, oneshot};

use crate::commands::ConfirmLearn;

/// 一次 `/learn` 确认请求：头行/预览的正文与回信口。
pub struct ConfirmJob {
    /// 草稿名（`draft_header(name)` 的入参）。
    pub name: String,
    /// 草稿正文（`preview_lines(skill_md)` 的入参）。
    pub skill_md: String,
    /// 回信口：UI 按键给出结论后回信（`true` = 保存，`false` = 放弃）。
    pub reply: oneshot::Sender<bool>,
}

/// 建一对：`(tx, rx)`；`tx` 交给 [`tui_confirm`]，`rx` 交给 [`crate::tui::app::TuiApp`]。
pub fn channel() -> (
    mpsc::UnboundedSender<ConfirmJob>,
    mpsc::UnboundedReceiver<ConfirmJob>,
) {
    mpsc::unbounded_channel()
}

/// 生成交给 [`crate::commands::Session::learn_with`] 的确认闭包：把请求发给 UI 循环并等回信；
/// 回信端掉了按放弃（fail-safe）。
pub fn tui_confirm(
    tx: mpsc::UnboundedSender<ConfirmJob>,
) -> impl FnOnce(&SkillDraft, String) -> ConfirmLearn + Send {
    move |draft, skill_md| {
        // 闭包要 `'static`：名字先从草稿里取出来（正文本来就是 `to_skill_md()` 的新字符串）
        let name = draft.name.clone();
        let sender = tx.clone();
        let (reply, rx) = oneshot::channel();
        let sent = sender.send(ConfirmJob {
            name,
            skill_md,
            reply,
        });
        Box::pin(async move {
            // UI 循环已经退出（接收端掉了）：没有人能回答，按放弃收束
            if sent.is_err() {
                return false;
            }
            // 回信端随 UI 走掉（请求被丢掉、回信前 UI 退出）：同样放弃——绝不静默落盘
            rx.await.unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use ignis_core::learn::SkillDraft;

    use super::{channel, tui_confirm};

    /// 草稿夹具：确认面只读 `name`，正文原样带走。
    fn draft() -> SkillDraft {
        SkillDraft {
            name: "demo-skill".to_string(),
            description: "Demo draft.".to_string(),
            body: "# 步骤\n1. 做事".to_string(),
        }
    }

    /// 往返：确认闭包把请求交给 UI 循环并等回信，回信就是它的结论；名字与正文逐字带上。
    #[tokio::test]
    async fn tui_confirm_sends_the_request_and_awaits_the_reply() {
        let (tx, mut rx) = channel();
        let skill_md = "---\nname: demo-skill\n---\n".to_string();
        let confirming = tokio::spawn(async move { tui_confirm(tx)(&draft(), skill_md).await });

        let job = rx.recv().await.expect("确认请求必须到达 UI 循环");
        assert_eq!(job.name, "demo-skill");
        assert_eq!(job.skill_md, "---\nname: demo-skill\n---\n");
        job.reply.send(true).expect("回信端还在");
        assert!(
            confirming.await.expect("确认任务应结束"),
            "回信 true 即保存"
        );
    }

    /// fail-safe：UI 走了（发送失败 / 请求被丢掉使回信端掉落）一律按放弃收束——确认面不可用时
    /// 绝不当成保存。
    #[tokio::test]
    async fn tui_confirm_gives_up_when_the_ui_is_gone() {
        // 接收端已经掉落：请求根本发不出去
        let (tx, rx) = channel();
        drop(rx);
        assert!(!tui_confirm(tx)(&draft(), String::new()).await);

        // 接收端还在但请求被丢掉（回信端随请求一起消失）
        let (tx, mut rx) = channel();
        let confirming =
            tokio::spawn(async move { tui_confirm(tx)(&draft(), String::new()).await });
        let job = rx.recv().await.expect("确认请求必须到达 UI 循环");
        drop(job);
        assert!(!confirming.await.expect("确认任务应结束"));
    }
}
