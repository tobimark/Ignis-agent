//! 外部 CLI 兼容内核（codex / claude）共用的进程与事件基础设施。
//!
//! 两个适配器形状同构：spawn 一个自成一组的 CLI、逐行读 JSONL、把所有异常路径收束成恰好一条
//! [`RuntimeEvent::Done`]。差别只在命令行开关与事件映射，故逐字相同的部分收在这里：终局事件、
//! 退出人话、stderr 收尾、截断、整组终止、事件发送与角色名。
//!
//! **stderr 收尾必须有上限**（评审 #1）：stdout 到 EOF、直接子进程也退出了，都不代表 stderr 的
//! 写端都关了——真机的 `Bash` 工具会留下攥着继承 fd 的后台进程，MCP/helper 子进程也继承 stderr。
//! 后代比 CLI 活得久时，无界地等 stderr 读任务就会永远等不到 EOF，`Done` 也就永远发不出去：
//! 调用方为一个已经产出 `result` 的轮次无限期地等下去。丢一份 stderr 首行是便宜的，挂死不是
//! （见 [`take_stderr_head`]）。
//!
//! 收尾只做「有上限地等」，不做「子进程一退出就对进程组补一发 SIGKILL」：pid 复用之下，对已回收
//! 的 pid 发组信号有误伤无关进程组的风险，而有界的 stderr 收尾已经把挂死风险消掉。取消/超时/
//! 收尸超时三条路径照旧对整组 SIGKILL（见 [`terminate`]）。

use std::future::Future;
use std::process::ExitStatus;
use std::time::Duration;

use futures::stream;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, ChildStderr};
use tokio::sync::mpsc::{Sender, channel};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::{BoxStream, Role, RunStatus, RuntimeEvent};

/// 事件通道容量（与 native 一致）：消费者慢只会让循环在发送处等待，不会无界缓冲。
const EVENT_CHANNEL: usize = 64;
/// stderr 首行的展示上限（字符）：够定位问题，又不至于把整段日志塞进 `Done.error`。
const STDERR_HEAD_CHARS: usize = 200;
/// 收尸上限：进程组已被 SIGKILL，但管道写端可能还攥在逃逸的孙进程手里（同 `shell_run` 的教训：
/// 无上限地等会把事件流拖住）。也是 stderr 收尾的上限（见模块文档）。
pub(crate) const REAP_GRACE: Duration = Duration::from_secs(5);
/// 拿到终局事件之后的收尸上限：终局已经把答案给了，进程只剩退出收尾；给一段短限度的等待就按
/// 已得的终局判定——等满 [`REAP_GRACE`]（更别说 `task_timeout`）会把「已完成却不退出」的 CLI
/// 拖成超时，并丢掉真实的失败原因。
pub(crate) const TERMINAL_REAP_GRACE: Duration = Duration::from_secs(2);

/// 把一次 CLI 运行折成事件流：终局事件只发一次（驱动返回的 `Some` 一定是 `Done`），`None` 表示
/// 事件接收端已断开（连 `Done` 都不必发，但驱动自身照杀进程组，不留后台进程）。
pub(crate) fn event_stream<F, Fut>(drive: F) -> BoxStream<'static, RuntimeEvent>
where
    F: FnOnce(Sender<RuntimeEvent>) -> Fut + Send + 'static,
    Fut: Future<Output = Option<RuntimeEvent>> + Send + 'static,
{
    let (tx, rx) = channel(EVENT_CHANNEL);
    tokio::spawn(async move {
        if let Some(event) = drive(tx.clone()).await {
            let _ = tx.send(event).await;
        }
    });
    Box::pin(stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }))
}

/// 唯一的终局事件：所有退出路径都经这里取 `Done`，由 [`event_stream`] 的任务发一次。
pub(crate) fn done(
    status: RunStatus,
    summary: Option<String>,
    error: Option<String>,
) -> RuntimeEvent {
    RuntimeEvent::Done {
        status,
        summary,
        error,
    }
}

/// 失败人话：`<内核名> 退出码 N`（被信号终止时没有退出码）+ stderr 首行（有则带上）。
pub(crate) fn exit_text(kernel: &str, status: &ExitStatus, stderr_head: Option<&str>) -> String {
    let code = match status.code() {
        Some(code) => format!("退出码 {code}"),
        None => "被信号终止".to_string(),
    };
    match stderr_head {
        Some(head) => format!("{kernel} {code}：{head}"),
        None => format!("{kernel} {code}"),
    }
}

/// 读完 stderr 并留首行（截断）：一直读到 EOF，避免写端因管道满而卡死。
async fn drain_stderr(stderr: ChildStderr) -> Option<String> {
    let mut lines = BufReader::new(stderr).lines();
    let mut head = None;
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if head.is_none() && !line.is_empty() {
            head = Some(truncate(line, STDERR_HEAD_CHARS));
        }
    }
    head
}

/// 起一个常驻的 stderr 读任务：CLI 会一直往这里写告警/重连日志，管道写满会把子进程卡死。
pub(crate) fn spawn_stderr_drain(stderr: ChildStderr) -> JoinHandle<Option<String>> {
    tokio::spawn(drain_stderr(stderr))
}

/// 收 stderr 读任务的产物，带上限（模块文档里的「stderr 收尾必须有上限」）。
///
/// 上限内没等到 EOF 就放弃这份首行：丢一行日志是便宜的，把已经产出终局的一轮挂死不是。
pub(crate) async fn take_stderr_head(task: JoinHandle<Option<String>>) -> Option<String> {
    timeout(REAP_GRACE, task)
        .await
        .ok()
        .and_then(|joined| joined.ok())
        .flatten()
}

/// 按字符截断，附省略号（`char_indices` 保证切点落在字符边界上）。
pub(crate) fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// 终止整组并收尸：直接子进程被 SIGKILL 后立刻可收，上限只为防异常情况下无限地等。
///
/// 收尸（`wait`）别省：只杀不等会留下僵尸进程，等 tokio 的孤儿回收队列兜底不及时。
pub(crate) async fn terminate(child: &mut Child, pid: u32) {
    kill_process_group(pid);
    let _ = timeout(REAP_GRACE, child.wait()).await;
}

/// 对子进程所在进程组发 SIGKILL（负 pid 即「该组全部进程」）；进程已退出时失败无副作用。
pub(crate) fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    // SAFETY: pid 取自本进程刚 spawn 的子进程，`kill` 只发信号、不触碰内存，
    // 目标不存在或已退出时也只是返回错误码。
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid; // 非 Unix 没有进程组语义，仅由 `kill_on_drop(true)` 兜底
}

/// 发一个事件；`false` 表示接收端已断开（循环随即收摊）。
///
/// 发送后让出一次执行权，事件消费者（以及按下取消的人）才有机会插进来。
pub(crate) async fn emit(tx: &Sender<RuntimeEvent>, event: RuntimeEvent) -> bool {
    if tx.send(event).await.is_err() {
        return false;
    }
    tokio::task::yield_now().await;
    true
}

/// 角色名：与 [`Role`] 的 serde 名逐字一致，外置内核读得懂。
pub(crate) fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按字符截断（多字节安全），短文本不加工（原 codex 的同名单元测试，随函数搬到这里；
    /// claude 侧另有 `truncates_on_char_boundaries_and_keeps_the_first_error_line` 覆盖首行提取）。
    #[test]
    fn truncates_text_on_char_boundaries() {
        assert_eq!(truncate("短", 200), "短");
        assert_eq!(truncate("中文一行", 2), "中文…");
    }

    /// 失败人话的内核名前缀（codex / claude 各自逐字保留）。
    #[test]
    fn exit_text_prefixes_the_kernel_name() {
        assert_eq!(
            exit_text(
                "codex",
                &std::process::Command::new("false").status().unwrap(),
                None
            ),
            "codex 退出码 1"
        );
        assert_eq!(
            exit_text(
                "claude",
                &std::process::Command::new("false").status().unwrap(),
                Some("告警首行")
            ),
            "claude 退出码 1：告警首行"
        );
    }
}
