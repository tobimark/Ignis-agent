//! codex 兼容内核：把本机 `codex` CLI 的 `exec --json` 事件流映射成 [`RuntimeEvent`]。
//!
//! 事件 schema、沙箱档位与命令行开关来自真机探测（本机 `codex-cli 0.162.1`）与上游文档，
//! 逐条记在 `docs/spikes/2026-10-15-external-kernels-probe.md`——**那份文档是这份映射的对照
//! 表**，CLI 版本漂移时两处一起改。
//!
//! 与 native 的差别在能力声明：外部内核自带工具循环，Ignis 不注入工具（`tools: false`）、
//! 不经手审批（`approvals: false`）、也不拿它的用量记账（`usage: false`；设计文档 §6.8：
//! 外部内核降级为时间/步数上限）。因此命令条目只映射成 `ToolExec`（只观察，不发
//! `ToolResult`），`turn.completed.usage` 不出 `Usage` 事件。
//!
//! 所有异常路径都收束成恰好一条 [`RuntimeEvent::Done`]：非零退出 / 流结束而未见成功终局 →
//! `Failed` + 人话；取消 → `Aborted`；墙钟到点 → `LimitReached` + `task_timeout`（与 native
//! 同一枚标记字符串）。未知行静默跳过，容忍 CLI 的 schema 漂移。

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use futures::stream;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::mpsc::{Sender, channel};
use tokio::time::{sleep_until, timeout};
use tokio_util::sync::CancellationToken;

use crate::native::MAX_TASK_TIMEOUT;
use crate::{
    BoxStream, Capabilities, ChatMessage, Limits, Role, RunRequest, RunStatus, Runtime,
    RuntimeEvent,
};

/// 事件通道容量（与 native 一致）：消费者慢只会让循环在发送处等待，不会无界缓冲。
const EVENT_CHANNEL: usize = 64;
/// 墙钟上限标记：与 native 的 `ERR_TASK_TIMEOUT` 逐字相同（跨内核共用的上限标记，勿改）。
const ERR_TASK_TIMEOUT: &str = "task_timeout";
/// 沙箱档位（任务书钉死）：`--help` 的可选值只有 `read-only` / `workspace-write` /
/// `danger-full-access`，这里取「能干活、又不出工作区」的那档，适配器不自行放宽。
const SANDBOX_MODE: &str = "workspace-write";
/// stderr 首行的展示上限（字符）：够定位问题，又不至于把整段日志塞进 `Done.error`。
const STDERR_HEAD_CHARS: usize = 200;
/// 收尸上限：进程组已被 SIGKILL，但管道写端可能还攥在逃逸的孙进程手里（同 `shell_run` 的
/// 教训：无上限地等会把事件流拖住）。
const REAP_GRACE: Duration = Duration::from_secs(5);
/// `command_execution` 条目映射成 `ToolExec` 时的名字（探测记录里的条目类型名）。
const COMMAND_ITEM: &str = "command_execution";

/// codex 兼容内核：工作区、可选模型与可执行文件路径随构造固定。
pub struct CodexRuntime {
    /// 每次运行的工作目录（同时经 `-C` 告知 codex 自己的工作根）。
    workspace: PathBuf,
    /// `-m/--model` 的值；`None` 时不带该开关，用 CLI 自己的默认模型。
    model: Option<String>,
    /// codex 可执行文件；默认是 PATH 上的 `codex`。
    program: PathBuf,
}

impl CodexRuntime {
    pub fn new(workspace: PathBuf, model: Option<String>) -> Self {
        Self {
            workspace,
            model,
            program: PathBuf::from("codex"),
        }
    }

    /// 测试接缝：把 `program` 换成夹具脚本（`tests/fixtures/fake-codex.sh`），不碰 PATH。
    pub fn with_command(program: PathBuf, workspace: PathBuf, model: Option<String>) -> Self {
        Self {
            workspace,
            model,
            program,
        }
    }
}

#[async_trait::async_trait]
impl Runtime for CodexRuntime {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: false,
            approvals: false,
            usage: false,
        }
    }

    async fn run(
        &self,
        req: RunRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, RuntimeEvent> {
        let (tx, rx) = channel(EVENT_CHANNEL);
        let program = self.program.clone();
        let workspace = self.workspace.clone();
        let model = self.model.clone();
        let prompt = render_prompt(&req.system_prompt, &req.messages);
        let limits = req.limits;
        tokio::spawn(async move {
            // 终局事件只发一次：`drive` 返回的 `Some` 一定是 `Done`，`None` 表示接收端已断开
            if let Some(event) = drive(
                &program,
                &workspace,
                model.as_deref(),
                prompt,
                limits,
                cancel,
                &tx,
            )
            .await
            {
                let _ = tx.send(event).await;
            }
        });
        Box::pin(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        }))
    }
}

/// 一次运行的全部动作：spawn CLI → 逐行读 JSONL → 返回唯一的终局 `Done`；`None` 表示事件
/// 接收端已断开（连 `Done` 都不必发，但进程组照杀，不留后台进程）。
async fn drive(
    program: &Path,
    workspace: &Path,
    model: Option<&str>,
    prompt: String,
    limits: Limits,
    cancel: CancellationToken,
    tx: &Sender<RuntimeEvent>,
) -> Option<RuntimeEvent> {
    let mut cmd = Command::new(program);
    cmd.arg("exec")
        .arg("--json")
        .arg("-C")
        .arg(workspace)
        .arg("--sandbox")
        .arg(SANDBOX_MODE)
        .arg("--skip-git-repo-check");
    if let Some(model) = model {
        cmd.arg("-m").arg(model);
    }
    cmd.arg(prompt)
        .current_dir(workspace)
        // 绝不读父进程的 stdin：codex 会把管道内容当 `<stdin>` 块追加进 prompt
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // 子进程自成进程组：组号 == 子进程 pid，取消/超时时对整组 SIGKILL
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Some(done(
                RunStatus::Failed,
                None,
                Some(format!(
                    "无法启动 codex 内核（{}）：{err}；请确认已安装 codex CLI 并在 PATH 中",
                    program.display()
                )),
            ));
        }
    };
    let Some(pid) = child.id() else {
        // 进程已经退出（拿不到 pid 就没有进程组可杀），`kill_on_drop` 兜底
        return Some(done(
            RunStatus::Failed,
            None,
            Some(format!(
                "codex 子进程已退出，无法追踪：{}",
                program.display()
            )),
        ));
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        terminate(&mut child, pid).await;
        return Some(done(
            RunStatus::Failed,
            None,
            Some("codex 子进程的 stdout/stderr 不可读".into()),
        ));
    };
    // stderr 必须有人一直读：CLI 会往这里写重连/告警日志，管道写满会把子进程卡死
    let stderr_task = tokio::spawn(drain_stderr(stderr));

    // 墙钟上限：与 native 同样的溢出兜底（`Instant + Duration` 溢出会 panic）
    let deadline =
        Instant::now() + Duration::from_secs(limits.task_timeout_secs).min(MAX_TASK_TIMEOUT);
    let mut lines = BufReader::new(stdout).lines();
    // 终局标记：`turn.completed` 到达即成功；`turn.failed` 带失败原因
    let mut completed = false;
    let mut failure: Option<String> = None;
    // 最后一段助手文本：成功终局的 summary（与 native 的 summary 语义一致）
    let mut last_text: Option<String> = None;
    let mut read_error: Option<String> = None;

    loop {
        let line = tokio::select! {
            () = cancel.cancelled() => {
                terminate(&mut child, pid).await;
                return Some(done(RunStatus::Aborted, None, None));
            }
            () = sleep_until(deadline.into()) => {
                terminate(&mut child, pid).await;
                return Some(done(
                    RunStatus::LimitReached,
                    None,
                    Some(ERR_TASK_TIMEOUT.into()),
                ));
            }
            line = lines.next_line() => line,
        };
        match line {
            Ok(Some(text)) => match parse_line(&text) {
                Some(CodexEvent::Text { text }) => {
                    last_text = Some(text.clone());
                    if !emit(tx, RuntimeEvent::TextDelta { text }).await {
                        terminate(&mut child, pid).await;
                        return None;
                    }
                }
                Some(CodexEvent::Command { summary, args }) => {
                    if !emit(
                        tx,
                        RuntimeEvent::ToolExec {
                            name: COMMAND_ITEM.into(),
                            summary,
                            args,
                        },
                    )
                    .await
                    {
                        terminate(&mut child, pid).await;
                        return None;
                    }
                }
                Some(CodexEvent::TurnCompleted) => completed = true,
                Some(CodexEvent::TurnFailed { message }) => failure = Some(message),
                // 未知事件类型 / 未知条目类型 / 非 JSON 行：静默跳过（容忍 schema 漂移）
                None => {}
            },
            // 流结束（`None`）或读管道出错：交给下面的退出码 + 终局标记判定
            Ok(None) => break,
            Err(err) => {
                read_error = Some(err.to_string());
                break;
            }
        }
    }

    // 收尾：等子进程退出，带上限（stdout 关了不代表进程退了，fd 也可能还攥在孙进程手里）
    let status = match timeout(REAP_GRACE, child.wait()).await {
        Ok(result) => result.ok(),
        Err(_) => None,
    };
    if status.is_none() {
        // 没能在收尾上限内退出：兜底杀整组（`kill_on_drop` 只管直接子进程）
        kill_process_group(pid);
    }
    let stderr_head = stderr_task.await.ok().flatten();

    match (completed, status, failure) {
        // 终局失败事件：CLI 自己给了原因，优先用它
        (_, _, Some(message)) => Some(done(
            RunStatus::Failed,
            None,
            Some(format!("codex 报告失败：{message}")),
        )),
        // 见过 `turn.completed` 但退出码非零：进程自己都说不正常，按失败收
        (true, Some(status), None) if !status.success() => Some(done(
            RunStatus::Failed,
            None,
            Some(exit_text(&status, stderr_head.as_deref())),
        )),
        // 见过 `turn.completed` 即成功；收尸没跟上（`status` 为 `None`）也不改变这个结论
        (true, _, None) => Some(done(RunStatus::Success, last_text, None)),
        // 流结束却没见到成功终局（非零退出、被信号终止、或干脆没来得及收尸）
        (false, Some(status), None) if status.success() => Some(done(
            RunStatus::Failed,
            None,
            Some(read_error.unwrap_or_else(|| {
                "codex 事件流结束但未见 turn.completed（事件 schema 可能已变更）".into()
            })),
        )),
        (false, Some(status), None) => Some(done(
            RunStatus::Failed,
            None,
            Some(exit_text(&status, stderr_head.as_deref())),
        )),
        (false, None, None) => Some(done(
            RunStatus::Failed,
            None,
            Some("codex 子进程未在收尾上限内退出（已终止整个进程组）".into()),
        )),
    }
}

/// 已识别的 codex 事件：只留本适配器用得上的形状，其余字段不解析。
#[derive(Debug, PartialEq)]
enum CodexEvent {
    /// `item.completed` + `item.type == "agent_message"`：助手文本（`item.text`）。
    Text { text: String },
    /// `command_execution` 的任一生命周期行：摘要取 `item.command`，`args` 留原始条目
    /// （`status` / `exit_code` / `aggregated_output` 都在里面，供任务日志还原现场）。
    Command {
        summary: String,
        args: serde_json::Value,
    },
    /// `turn.completed`：本轮成功收束。
    TurnCompleted,
    /// `turn.failed`：本轮失败收束，`error.message` 是原因。
    TurnFailed { message: String },
}

/// 按行映射：认不出的行一律 `None`。
///
/// 静默跳过的三类（容忍 schema 漂移）：未知顶层事件类型（如 `thread.started`、重连用的
/// `error`）、未知条目类型、非 JSON 行。本轮不映射的已知条目也只有跳过一种处理：
/// `reasoning` / `file_change` / `mcp_tool_call` / `web_search` / `todo_list` / `error` 条目
/// 对任务日志没有执行语义，T3 之后按需再收。
fn parse_line(line: &str) -> Option<CodexEvent> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    match value.get("type")?.as_str()? {
        "turn.completed" => Some(CodexEvent::TurnCompleted),
        "turn.failed" => Some(CodexEvent::TurnFailed {
            message: value.get("error")?.get("message")?.as_str()?.to_string(),
        }),
        "item.started" | "item.updated" | "item.completed" => {
            let item = value.get("item")?;
            match item.get("type")?.as_str()? {
                "agent_message" => Some(CodexEvent::Text {
                    text: item.get("text")?.as_str()?.to_string(),
                }),
                "command_execution" => Some(CodexEvent::Command {
                    summary: item.get("command")?.as_str()?.to_string(),
                    args: item.clone(),
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// T1 的过渡渲染：`system 提示 + 逐条消息`直拼。
///
/// TODO(T3): T3 落地 `render_external_prompt` 后删掉本函数、改调共享函数（任务书放行的过渡
/// 实现，代价是 codex/claude 两个适配器之间重复一处渲染）。
fn render_prompt(system_prompt: &str, messages: &[ChatMessage]) -> String {
    let rendered = messages
        .iter()
        .map(|message| format!("{}: {}", role_label(message.role), message.content))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{system_prompt}\n\n{rendered}")
}

/// 角色名：与 [`Role`] 的 serde 名逐字一致，外置内核读得懂。
fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// 唯一的终局事件：所有退出路径都经这里取 `Done`，由 [`Runtime::run`] 的任务发一次。
fn done(status: RunStatus, summary: Option<String>, error: Option<String>) -> RuntimeEvent {
    RuntimeEvent::Done {
        status,
        summary,
        error,
    }
}

/// 失败人话：退出码（被信号终止时没有退出码）+ stderr 首行（有则带上）。
fn exit_text(status: &ExitStatus, stderr_head: Option<&str>) -> String {
    let code = match status.code() {
        Some(code) => format!("退出码 {code}"),
        None => "被信号终止".to_string(),
    };
    match stderr_head {
        Some(head) => format!("codex {code}：{head}"),
        None => format!("codex {code}"),
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

/// 按字符截断，附省略号（`char_indices` 保证切点落在字符边界上）。
fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// 终止整组并收尸：直接子进程被 SIGKILL 后立刻可收，上限只为防异常情况下无限地等。
///
/// 收尸（`wait`）别省：只杀不等会留下僵尸进程，等 tokio 的孤儿回收队列兜底不及时。
async fn terminate(child: &mut Child, pid: u32) {
    kill_process_group(pid);
    let _ = timeout(REAP_GRACE, child.wait()).await;
}

/// 对子进程所在进程组发 SIGKILL（负 pid 即「该组全部进程」）；进程已退出时失败无副作用。
fn kill_process_group(pid: u32) {
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
async fn emit(tx: &Sender<RuntimeEvent>, event: RuntimeEvent) -> bool {
    if tx.send(event).await.is_err() {
        return false;
    }
    tokio::task::yield_now().await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逐行映射：探测记录里的已知行各归其位，未知/不映射的行一律 `None`。
    #[test]
    fn maps_known_lines_and_skips_the_rest() {
        // 助手文本（`item.completed` + `agent_message`）
        assert_eq!(
            parse_line(
                r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"OK."}}"#
            ),
            Some(CodexEvent::Text { text: "OK.".into() })
        );
        // 命令事件：摘要取 `command`，原始条目整个留作 args
        let Some(CodexEvent::Command { summary, args }) = parse_line(
            r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"bash -lc ls","aggregated_output":"docs\n","exit_code":0,"status":"completed"}}"#,
        ) else {
            panic!("command_execution 应映射成 Command");
        };
        assert_eq!(summary, "bash -lc ls");
        assert_eq!(args["exit_code"], 0);
        assert_eq!(args["aggregated_output"], "docs\n");
        // 两个终局事件
        assert_eq!(
            parse_line(r#"{"type":"turn.completed","usage":{"input_tokens":21}}"#),
            Some(CodexEvent::TurnCompleted)
        );
        assert_eq!(
            parse_line(r#"{"type":"turn.failed","error":{"message":"stream ended"}}"#),
            Some(CodexEvent::TurnFailed {
                message: "stream ended".into()
            })
        );
        // 过程性事件与不映射的条目：跳过
        assert_eq!(
            parse_line(r#"{"type":"thread.started","thread_id":"x"}"#),
            None
        );
        assert_eq!(
            parse_line(r#"{"type":"error","message":"Reconnecting... 2/5"}"#),
            None
        );
        assert_eq!(
            parse_line(
                r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"**x**"}}"#
            ),
            None
        );
        assert_eq!(
            parse_line(r#"{"type":"item.completed","item":{"id":"i","type":"web_search"}}"#),
            None
        );
        // 未知条目类型 / 非 JSON / 缺字段：跳过而不报错
        assert_eq!(
            parse_line(r#"{"type":"item.completed","item":{"id":"i","type":"future_item"}}"#),
            None
        );
        assert_eq!(parse_line("not json at all"), None);
        assert_eq!(parse_line(r#"{"type":"turn.failed"}"#), None);
        assert_eq!(parse_line(r#"{"type":"item.completed"}"#), None);
    }

    /// 过渡渲染：system 提示在前，随后逐条消息（角色名 + 正文），段落之间空行。
    #[test]
    fn render_prompt_puts_system_then_messages() {
        let messages = vec![
            ChatMessage {
                role: Role::User,
                content: "说一句 ok".into(),
                tool_call_id: None,
                tool_calls: vec![],
            },
            ChatMessage {
                role: Role::Tool,
                content: "a.txt".into(),
                tool_call_id: Some("call_1".into()),
                tool_calls: vec![],
            },
        ];
        assert_eq!(
            render_prompt("你是 Ignis", &messages),
            "你是 Ignis\n\nuser: 说一句 ok\n\ntool: a.txt"
        );
    }

    /// stderr 首行按字符截断（多字节安全），短行不加工。
    #[test]
    fn truncates_stderr_head_on_char_boundaries() {
        assert_eq!(truncate("短", 200), "短");
        assert_eq!(truncate("中文一行", 2), "中文…");
    }
}
