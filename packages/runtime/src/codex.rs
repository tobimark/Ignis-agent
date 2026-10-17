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
//! 同一枚标记字符串）。`turn.completed` / `turn.failed` 一到手就停止读流——之后只给进程 2s
//! （[`cli_process::TERMINAL_REAP_GRACE`]）的退出收尾时间，免得「已完成却不退出」被拖成超时、
//! 真实失败原因被丢掉。未知行静默跳过，容忍 CLI 的 schema 漂移。
//!
//! 与 claude 逐字相同的进程/事件基础设施在 [`cli_process`] 里（含 stderr 收尾的上限，评审 #1）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::Sender;
use tokio::time::{sleep_until, timeout};
use tokio_util::sync::CancellationToken;

use crate::cli_process::{
    REAP_GRACE, TERMINAL_REAP_GRACE, done, emit, event_stream, exit_text, kill_process_group,
    spawn_stderr_drain, take_stderr_head, terminate,
};
use crate::native::{ERR_TASK_TIMEOUT, MAX_TASK_TIMEOUT};
use crate::render::render_external_prompt;
use crate::{BoxStream, Capabilities, Limits, RunRequest, RunStatus, Runtime, RuntimeEvent};

/// 沙箱档位（任务书钉死）：`--help` 的可选值只有 `read-only` / `workspace-write` /
/// `danger-full-access`，这里取「能干活、又不出工作区」的那档，适配器不自行放宽。
const SANDBOX_MODE: &str = "workspace-write";
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
        let program = self.program.clone();
        let workspace = self.workspace.clone();
        let model = self.model.clone();
        let prompt = render_external_prompt(&req.system_prompt, &req.messages);
        let limits = req.limits;
        // 事件管道是共享的：终局事件只发一次（驱动返回的 `Some` 一定是 `Done`）
        event_stream(move |tx| async move {
            drive(
                &program,
                &workspace,
                model.as_deref(),
                prompt,
                limits,
                cancel,
                &tx,
            )
            .await
        })
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
                Some(spawn_error_text(program, &err)),
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
    let stderr_task = spawn_stderr_drain(stderr);

    // 墙钟上限：与 native 同样的溢出兜底（`Instant + Duration` 溢出会 panic）
    let deadline =
        Instant::now() + Duration::from_secs(limits.task_timeout_secs).min(MAX_TASK_TIMEOUT);
    let mut lines = BufReader::new(stdout).lines();
    // 本轮终局：`turn.completed` 成功；`turn.failed` 失败（原因可能缺失）
    let mut turn_end: Option<TurnEnd> = None;
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
                            // Task 3 改填真实 tool id；本任务先占位空串（旧日志兼容语义）
                            call_id: String::new(),
                        },
                    )
                    .await
                    {
                        terminate(&mut child, pid).await;
                        return None;
                    }
                }
                // 终局事件到手即停止读流：CLI 可能在终局行之后赖着不退出，继续读到 EOF 会把
                // 已完成的轮次（或已给出的失败原因）拖成 `task_timeout`
                Some(CodexEvent::TurnCompleted) => {
                    turn_end = Some(TurnEnd::Completed);
                    break;
                }
                Some(CodexEvent::TurnFailed { message }) => {
                    turn_end = Some(TurnEnd::Failed(message));
                    break;
                }
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

    // 收尾：等子进程退出，带上限（stdout 关了不代表进程退了，fd 也可能还攥在孙进程手里）。
    // 已经拿到终局事件时只等一小段：终局已给出答案，别为了等一个赖着不走的进程丢掉它。
    let grace = if turn_end.is_some() {
        TERMINAL_REAP_GRACE
    } else {
        REAP_GRACE
    };
    let status = match timeout(grace, child.wait()).await {
        Ok(result) => result.ok(),
        Err(_) => None,
    };
    if status.is_none() {
        // 没能在收尾上限内退出：兜底杀整组（`kill_on_drop` 只管直接子进程）
        kill_process_group(pid);
    }
    // stderr 收尾带上限（评审 #1）：后代还攥着继承来的写端时，无界地等会把已完成的一轮挂死
    let stderr_head = take_stderr_head(stderr_task).await;

    match (turn_end, status) {
        // 终局失败事件：CLI 自己给了原因，优先用它；连原因都没给（schema 漂移）也照样是失败，
        // 只换成通用人话——终局确实到了，不能退回「流结束但未见终局」那套说辞
        (Some(TurnEnd::Failed(message)), _) => Some(done(
            RunStatus::Failed,
            None,
            Some(match message {
                Some(message) => format!("codex 报告失败：{message}"),
                None => ERR_TURN_FAILED_NO_REASON.into(),
            }),
        )),
        // 见过 `turn.completed` 但退出码非零：进程自己都说不正常，按失败收
        (Some(TurnEnd::Completed), Some(status)) if !status.success() => Some(done(
            RunStatus::Failed,
            None,
            Some(exit_text("codex", &status, stderr_head.as_deref())),
        )),
        // 见过 `turn.completed` 即成功；收尸没跟上（`status` 为 `None`）也不改变这个结论
        (Some(TurnEnd::Completed), _) => Some(done(RunStatus::Success, last_text, None)),
        // 流结束却没见到成功终局（非零退出、被信号终止、或干脆没来得及收尸）
        (None, Some(status)) if status.success() => Some(done(
            RunStatus::Failed,
            None,
            Some(read_error.unwrap_or_else(|| {
                "codex 事件流结束但未见 turn.completed（事件 schema 可能已变更）".into()
            })),
        )),
        (None, Some(status)) => Some(done(
            RunStatus::Failed,
            None,
            Some(exit_text("codex", &status, stderr_head.as_deref())),
        )),
        (None, None) => Some(done(
            RunStatus::Failed,
            None,
            Some("codex 子进程未在收尾上限内退出（已终止整个进程组）".into()),
        )),
    }
}

/// spawn 失败的人话：只有 `NotFound`（没装 / 不在 PATH）给安装提示；其余失败保留底层错误，
/// 附一条通用排查提示——渲染后的 prompt 是单个 argv 元素，超过 Linux 的 `MAX_ARG_STRLEN`
/// （128 KiB）时 spawn 会以 `E2BIG` 失败，把已装好的 CLI 说成「请确认已安装」是误导。
fn spawn_error_text(program: &Path, err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => format!(
            "无法启动 codex 内核（{}）：{err}；请确认已安装 codex CLI 并在 PATH 中",
            program.display()
        ),
        _ => format!(
            "无法启动 codex 内核（{}）：{err}；若提示参数过长，请缩短任务或历史，或改用 native 内核",
            program.display()
        ),
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
    /// `turn.failed`：本轮失败收束。`error.message` 是原因，但 schema 漂移下可能缺失或形状
    /// 不对，故为 `Option`——判别终局只看顶层 `type`，不因原因缺失而漏掉终局。
    TurnFailed { message: Option<String> },
}

/// 本轮终局事件（一到手就读循环收摊，判定交给 [`drive`] 末尾的决策表）。
#[derive(Debug, PartialEq)]
enum TurnEnd {
    /// `turn.completed`：成功收束。
    Completed,
    /// `turn.failed`：失败收束，带上 CLI 给的原因（可能没有）。
    Failed(Option<String>),
}

/// `turn.failed` 没带 `error.message` 时的兜底人话（终局确实到了，不能退回「未见终局」的文案）。
const ERR_TURN_FAILED_NO_REASON: &str = "codex 报告失败（未提供原因）";

/// 按行映射：认不出的行一律 `None`。
///
/// 两个终局只按顶层 `type` 判别：`turn.failed` 的原因缺失（或 `error` 不是对象）时仍认作终局，
/// 只是原因为 `None`——把整行当成「未知行」会让一次真实的失败终局丢掉。
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
            message: value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .map(str::to_string),
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
                message: Some("stream ended".into())
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
        assert_eq!(parse_line(r#"{"type":"item.completed"}"#), None);
        // 终局失败只看顶层 `type`：`error.message` 缺失或形状不对也认得出终局（原因是 `None`）
        assert_eq!(
            parse_line(r#"{"type":"turn.failed"}"#),
            Some(CodexEvent::TurnFailed { message: None })
        );
        assert_eq!(
            parse_line(r#"{"type":"turn.failed","error":"stream ended"}"#),
            Some(CodexEvent::TurnFailed { message: None })
        );
    }

    /// spawn 失败文案：只有 `NotFound` 才提示「安装 CLI」；其余失败（典型是 prompt 超
    /// `MAX_ARG_STRLEN` 的 `E2BIG`）保留底层错误并给缩短任务/历史的提示。
    #[test]
    fn spawn_error_text_keeps_the_install_hint_only_for_not_found() {
        let program = Path::new("/usr/bin/codex");
        let missing = std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        );
        let text = spawn_error_text(program, &missing);
        assert!(text.contains("os error 2"), "{text}");
        assert!(
            text.contains("请确认已安装 codex CLI 并在 PATH 中"),
            "{text}"
        );

        let too_long = std::io::Error::new(
            std::io::ErrorKind::ArgumentListTooLong,
            "Argument list too long (os error 7)",
        );
        let text = spawn_error_text(program, &too_long);
        assert!(text.contains("os error 7"), "应保留底层错误：{text}");
        assert!(
            !text.contains("请确认已安装"),
            "非 ENOENT 不得再提示安装：{text}"
        );
        assert!(
            text.contains("缩短任务或历史"),
            "应给出真正的排查提示：{text}"
        );
    }
}
