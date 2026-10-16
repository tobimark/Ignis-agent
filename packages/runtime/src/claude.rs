//! claude 兼容内核：把本机 `claude` CLI 的 `-p --output-format stream-json` 事件流映射成 [`RuntimeEvent`]。
//!
//! 事件 schema、权限档位与命令行开关来自真机探测（本机 `claude 2.1.274`，Claude Code）与官方文档，
//! 逐条记在 `docs/spikes/2026-10-15-external-kernels-probe.md`——**那份文档是这份映射的对照
//! 表**，CLI 版本漂移时两处一起改。
//!
//! 与 native 的差别在能力声明：外部内核自带工具循环，Ignis 不注入工具（`tools: false`）、
//! 不经手审批（`approvals: false`）、也不拿它的用量记账（`usage: false`；设计文档 §6.8：
//! 外部内核降级为时间/步数上限）。因此 `tool_use` 内容块只映射成 `ToolExec`（只观察，不发
//! `ToolResult`），`result.usage` 不出 `Usage` 事件。
//!
//! 所有异常路径都收束成恰好一条 [`RuntimeEvent::Done`]：非零退出 / 流结束而未见 `result` 终局 →
//! `Failed` + 人话；取消 → `Aborted`；墙钟到点 → `LimitReached` + `task_timeout`（与 native
//! 同一枚标记字符串）。`result` 一到手就停止读流——之后只给进程 2s（`TERMINAL_REAP_GRACE`）
//! 的退出收尾时间，免得「已完成却不退出」被拖成超时、真实失败原因被丢掉。未知行静默跳过
//! （`system` 前导行、`user` 工具结果回流行、非 JSON 行都不映射），容忍 CLI 的 schema 漂移。

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use futures::stream;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::mpsc::{Sender, channel};
use tokio::time::{sleep_until, timeout};
use tokio_util::sync::CancellationToken;

use crate::native::{ERR_TASK_TIMEOUT, MAX_TASK_TIMEOUT};
use crate::{
    BoxStream, Capabilities, ChatMessage, Limits, Role, RunRequest, RunStatus, Runtime,
    RuntimeEvent,
};

/// 事件通道容量（与 native 一致）：消费者慢只会让循环在发送处等待，不会无界缓冲。
const EVENT_CHANNEL: usize = 64;
/// 权限档位（任务书钉死：取「只读/默认」档）：`--help` 的可选值是 `acceptEdits` / `auto` /
/// `bypassPermissions` / `manual` / `dontAsk` / `plan`；`manual` 就是 CLI 自己的默认档
/// （真机 `system/init` 回报 `permissionMode:"default"`）——既不像 `bypassPermissions` /
/// `acceptEdits` 那样自动放行，也不像 `plan` 那样把轮次锁成只读规划，适配器不自行放宽。
const PERMISSION_MODE: &str = "manual";
/// stderr 首行的展示上限（字符）：够定位问题，又不至于把整段日志塞进 `Done.error`。
const STDERR_HEAD_CHARS: usize = 200;
/// 失败原因（`result` 文本）的展示上限（字符）：`Done.error` 只要第一行。
const ERR_TEXT_CHARS: usize = 200;
/// `tool_use` 摘要里参数提示的字符上限。
const TOOL_ARG_CHARS: usize = 80;
/// 收尸上限：进程组已被 SIGKILL，但管道写端可能还攥在逃逸的孙进程手里（同 `shell_run` 的
/// 教训：无上限地等会把事件流拖住）。
const REAP_GRACE: Duration = Duration::from_secs(5);
/// 拿到终局事件之后的收尸上限：终局已经把答案给了，进程只剩退出收尾；给一段短限度的等待就
/// 按已得的终局判定——等满 [`REAP_GRACE`]（更别说 `task_timeout`）会把「已完成却不退出」的
/// CLI 拖成超时，并丢掉真实的失败原因。
const TERMINAL_REAP_GRACE: Duration = Duration::from_secs(2);
/// `tool_use` 块映射成 `ToolExec` 时的名字（内容块类型名，与 codex 的 `command_execution` 同构）。
const TOOL_USE_ITEM: &str = "tool_use";
/// `result.is_error == true` 但没带 `result` 文本时的兜底人话（终局确实到了，不能退回
/// 「未见终局」的文案）。
const ERR_RESULT_FAILED_NO_REASON: &str = "claude 报告失败（未提供原因）";

/// claude 兼容内核：工作区、可选模型与可执行文件路径随构造固定。
pub struct ClaudeRuntime {
    /// 每次运行的工作目录。
    workspace: PathBuf,
    /// `--model` 的值；`None` 时不带该开关，用 CLI 自己的默认模型。
    model: Option<String>,
    /// claude 可执行文件；默认是 PATH 上的 `claude`。
    program: PathBuf,
}

impl ClaudeRuntime {
    pub fn new(workspace: PathBuf, model: Option<String>) -> Self {
        Self {
            workspace,
            model,
            program: PathBuf::from("claude"),
        }
    }

    /// 测试接缝：把 `program` 换成夹具脚本（`tests/fixtures/fake-claude.sh`），不碰 PATH。
    pub fn with_command(program: PathBuf, workspace: PathBuf, model: Option<String>) -> Self {
        Self {
            workspace,
            model,
            program,
        }
    }
}

#[async_trait::async_trait]
impl Runtime for ClaudeRuntime {
    fn id(&self) -> &'static str {
        "claude"
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
    // `--verbose` 是 `-p` + `--output-format stream-json` 的硬要求（探测记录：不带它 CLI 直接
    // 报错退出），不是可选的调试开关。
    cmd.arg("-p")
        .arg("--verbose")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--permission-mode")
        .arg(PERMISSION_MODE);
    if let Some(model) = model {
        cmd.arg("--model").arg(model);
    }
    cmd.arg(prompt)
        .current_dir(workspace)
        // 绝不读父进程的 stdin：`-p` 会把管道内容当成 prompt 输入
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
                    "无法启动 claude 内核（{}）：{err}；请确认已安装 claude CLI 并在 PATH 中",
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
                "claude 子进程已退出，无法追踪：{}",
                program.display()
            )),
        ));
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        terminate(&mut child, pid).await;
        return Some(done(
            RunStatus::Failed,
            None,
            Some("claude 子进程的 stdout/stderr 不可读".into()),
        ));
    };
    // stderr 必须有人一直读：CLI 会往这里写告警（如 `[claude-code:unrecognized_model]`），
    // 管道写满会把子进程卡死
    let stderr_task = tokio::spawn(drain_stderr(stderr));

    // 墙钟上限：与 native 同样的溢出兜底（`Instant + Duration` 溢出会 panic）
    let deadline =
        Instant::now() + Duration::from_secs(limits.task_timeout_secs).min(MAX_TASK_TIMEOUT);
    let mut lines = BufReader::new(stdout).lines();
    // 本轮终局：`result` 行（`is_error` 决定成败）
    let mut turn_end: Option<TurnEnd> = None;
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
            Ok(Some(text)) => {
                // 一行可以带多个内容块（真机常见 `thinking` + `text`，或 `thinking` + `tool_use`）
                let mut terminal = None;
                for event in parse_line(&text) {
                    match event {
                        ClaudeEvent::Text { text } => {
                            if !emit(tx, RuntimeEvent::TextDelta { text }).await {
                                terminate(&mut child, pid).await;
                                return None;
                            }
                        }
                        ClaudeEvent::ToolUse { summary, args } => {
                            if !emit(
                                tx,
                                RuntimeEvent::ToolExec {
                                    name: TOOL_USE_ITEM.into(),
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
                        ClaudeEvent::Result { is_error, text } => {
                            terminal = Some(if is_error {
                                TurnEnd::Failed(text)
                            } else {
                                TurnEnd::Completed { summary: text }
                            });
                        }
                    }
                }
                // 终局行到手即停止读流：CLI 可能在终局行之后赖着不退出，继续读到 EOF 会把
                // 已完成的轮次（或已给出的失败原因）拖成 `task_timeout`
                if let Some(end) = terminal {
                    turn_end = Some(end);
                    break;
                }
            }
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
    let stderr_head = stderr_task.await.ok().flatten();

    match (turn_end, status) {
        // 终局失败：CLI 自己给了原因，优先用它（只取首行）；连原因都没给（schema 漂移）也照样是
        // 失败，只换成通用人话——终局确实到了，不能退回「流结束但未见终局」那套说辞
        (Some(TurnEnd::Failed(message)), _) => Some(done(
            RunStatus::Failed,
            None,
            Some(match message {
                Some(message) => {
                    format!("claude 报告失败：{}", first_line(&message, ERR_TEXT_CHARS))
                }
                None => ERR_RESULT_FAILED_NO_REASON.into(),
            }),
        )),
        // 见过成功终局但退出码非零：进程自己都说不正常，按失败收
        (Some(TurnEnd::Completed { .. }), Some(status)) if !status.success() => Some(done(
            RunStatus::Failed,
            None,
            Some(exit_text(&status, stderr_head.as_deref())),
        )),
        // 见过成功终局即成功；收尸没跟上（`status` 为 `None`）也不改变这个结论
        (Some(TurnEnd::Completed { summary }), _) => Some(done(RunStatus::Success, summary, None)),
        // 流结束却没见到终局（非零退出、被信号终止、或干脆没来得及收尸）
        (None, Some(status)) if status.success() => Some(done(
            RunStatus::Failed,
            None,
            Some(read_error.unwrap_or_else(|| {
                "claude 事件流结束但未见 result 终局（事件 schema 可能已变更）".into()
            })),
        )),
        (None, Some(status)) => Some(done(
            RunStatus::Failed,
            None,
            Some(exit_text(&status, stderr_head.as_deref())),
        )),
        (None, None) => Some(done(
            RunStatus::Failed,
            None,
            Some("claude 子进程未在收尾上限内退出（已终止整个进程组）".into()),
        )),
    }
}

/// 已识别的 claude 事件：只留本适配器用得上的形状，其余字段不解析。
#[derive(Debug, PartialEq)]
enum ClaudeEvent {
    /// `assistant.message.content[].type == "text"`：助手文本（块里的 `text`）。
    Text { text: String },
    /// `assistant.message.content[].type == "tool_use"`：工具调用（只观察）。摘要取
    /// 「工具名 + 一行参数提示」，`args` 留原始块（`name` / `input` / `id` 都在里面，
    /// 供任务日志还原现场）。
    ToolUse {
        summary: String,
        args: serde_json::Value,
    },
    /// `result`：本轮终局。`is_error` 决定成败（判别终局只看顶层 `type`，不因字段缺失而漏掉
    /// 终局）；`text` 即 `result` 字段——成功时是最终答复，失败时是原因（schema 漂移下可能没有）。
    Result {
        is_error: bool,
        text: Option<String>,
    },
}

/// 本轮终局事件（一到手就读循环收摊，判定交给 [`drive`] 末尾的决策表）。
#[derive(Debug, PartialEq)]
enum TurnEnd {
    /// `result` 且 `is_error == false`：成功收束，`result` 文本即 summary。
    Completed { summary: Option<String> },
    /// `result` 且 `is_error == true`：失败收束，`result` 文本是原因（可能没有）。
    Failed(Option<String>),
}

/// 按行映射：一行可以带多个事件（`assistant` 的多个内容块），认不出的行返回空表。
///
/// 两个映射只按顶层 `type` 判别：`assistant` 取 `message.content` 里的 `text` / `tool_use`
/// 块；`result` 一律认作终局。静默跳过的四类（容忍 schema 漂移）：`system` 前导行
/// （`init` / `thinking_tokens` 等）、`user`（工具结果回流）行、未知顶层类型、非 JSON 行；
/// 已知的 `thinking` 内容块也不映射（对任务日志没有执行语义，T3 之后按需再收）。
fn parse_line(line: &str) -> Vec<ClaudeEvent> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    match value.get("type").and_then(|kind| kind.as_str()) {
        Some("assistant") => value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_array())
            .map(|blocks| blocks.iter().filter_map(parse_block).collect())
            .unwrap_or_default(),
        Some("result") => vec![ClaudeEvent::Result {
            // 缺失 `is_error` 时按失败收：schema 漂移下宁可误报失败，也不把漂移静默当成功
            is_error: value
                .get("is_error")
                .and_then(|is_error| is_error.as_bool())
                .unwrap_or(true),
            text: value
                .get("result")
                .and_then(|text| text.as_str())
                .map(str::to_string),
        }],
        _ => Vec::new(),
    }
}

/// `message.content` 里的单个块：只认 `text` 与 `tool_use`，其余（`thinking` 等）跳过。
fn parse_block(block: &serde_json::Value) -> Option<ClaudeEvent> {
    match block.get("type")?.as_str()? {
        "text" => Some(ClaudeEvent::Text {
            text: block.get("text")?.as_str()?.to_string(),
        }),
        "tool_use" => {
            let name = block.get("name")?.as_str()?;
            Some(ClaudeEvent::ToolUse {
                summary: tool_summary(name, block.get("input")),
                args: block.clone(),
            })
        }
        _ => None,
    }
}

/// `tool_use` 的摘要：工具名 + 一行参数提示；`input` 取不到提示时只剩工具名。
fn tool_summary(name: &str, input: Option<&serde_json::Value>) -> String {
    match input.and_then(arg_hint) {
        Some(hint) => format!("{name} {hint}"),
        None => name.to_string(),
    }
}

/// 参数提示：取 `input` 里第一个字符串字段（真机 `Bash` 是 `command`、`Read` 是 `file_path`），
/// 压成一行并按字符截断；一个字符串字段都没有时退成整份紧凑 JSON。
fn arg_hint(input: &serde_json::Value) -> Option<String> {
    let hint = input
        .as_object()?
        .values()
        .find_map(|value| value.as_str())
        .map(str::to_string)
        .or_else(|| serde_json::to_string(input).ok())?;
    Some(truncate(&one_line(&hint), TOOL_ARG_CHARS))
}

/// 压成一行：切分空白再以单空格拼接（换行、缩进、连续空白都归一）。
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
        Some(head) => format!("claude {code}：{head}"),
        None => format!("claude {code}"),
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

/// 取首行并按字符截断（`Done.error` 是单行人话，失败原因可能是多行长文本）。
fn first_line(text: &str, limit: usize) -> String {
    truncate(text.lines().next().unwrap_or("").trim(), limit)
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

    /// 逐行映射：探测记录里的已知行各归其位，未知/不映射的行一律空表。
    #[test]
    fn maps_known_lines_and_skips_the_rest() {
        // 助手文本；同一行的 `thinking` 块被跳过
        let assistant = parse_line(
            r#"{"type":"assistant","parent_tool_use_id":null,"session_id":"s","message":{"id":"m","type":"message","role":"assistant","content":[{"type":"thinking","thinking":"先想一下"},{"type":"text","text":"OK"}],"model":"MiniMax-M2"}}"#,
        );
        assert_eq!(assistant, vec![ClaudeEvent::Text { text: "OK".into() }]);
        // 工具调用块：摘要 = 工具名 + 一行参数提示，args 留原始块
        let blocks = parse_line(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"call_function_1","name":"Bash","input":{"command":"ls -la","description":"List files"}}]}}"#,
        );
        let Some(ClaudeEvent::ToolUse { summary, args }) = blocks.into_iter().next() else {
            panic!("tool_use 应映射成 ToolUse");
        };
        assert_eq!(summary, "Bash ls -la");
        assert_eq!(args["name"], "Bash");
        assert_eq!(args["input"]["command"], "ls -la");
        assert_eq!(args["id"], "call_function_1");
        // `result` 终局：`is_error` 决定成败，`result` 字段是文本
        assert_eq!(
            parse_line(
                r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"OK"}"#
            ),
            vec![ClaudeEvent::Result {
                is_error: false,
                text: Some("OK".into())
            }]
        );
        assert_eq!(
            parse_line(
                r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":429,"result":"API Error: Request rejected (429)"}"#
            ),
            vec![ClaudeEvent::Result {
                is_error: true,
                text: Some("API Error: Request rejected (429)".into())
            }]
        );
        // 过程性行：`system/init`、`system/thinking_tokens`、`user`（工具结果回流）、未知顶层类型
        assert_eq!(
            parse_line(
                r#"{"type":"system","subtype":"init","model":"MiniMax-M2","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}"#
            ),
            Vec::new()
        );
        assert_eq!(
            parse_line(
                r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":2,"uuid":"x"}"#
            ),
            Vec::new()
        );
        assert_eq!(
            parse_line(
                r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t","type":"tool_result","content":"a.txt","is_error":false}]}}"#
            ),
            Vec::new()
        );
        assert_eq!(
            parse_line(r#"{"type":"future.event","payload":{"n":1}}"#),
            Vec::new()
        );
        // 未知内容块 / 未知块形状 / 非 JSON / 缺字段：跳过而不报错
        assert_eq!(
            parse_line(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"future_block","text":"x"}]}}"#
            ),
            Vec::new()
        );
        assert_eq!(
            parse_line(r#"{"type":"assistant","message":{"role":"assistant"}}"#),
            Vec::new()
        );
        assert_eq!(parse_line(r#"{"type":"assistant"}"#), Vec::new());
        assert_eq!(parse_line("not json at all"), Vec::new());
        // 终局只看顶层 `type`：`is_error` 缺失按失败的安全默认收，`result` 缺失则无原因
        assert_eq!(
            parse_line(r#"{"type":"result","subtype":"success","num_turns":1}"#),
            vec![ClaudeEvent::Result {
                is_error: true,
                text: None
            }]
        );
    }

    /// 同一行里的多个可映射块按顺序展开（`thinking` 不占位）。
    #[test]
    fn expands_multiple_blocks_in_one_line_in_order() {
        assert_eq!(
            parse_line(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"先说一句"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.txt"}}]}}"#
            ),
            vec![
                ClaudeEvent::Text {
                    text: "先说一句".into()
                },
                ClaudeEvent::ToolUse {
                    summary: "Read a.txt".into(),
                    args: serde_json::json!({"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.txt"}}),
                },
            ]
        );
    }

    /// 参数提示：第一个字符串字段、压成一行、按字符截断；没有字符串字段时退成紧凑 JSON。
    #[test]
    fn tool_summary_prefers_a_one_line_arg_hint() {
        assert_eq!(
            tool_summary(
                "Bash",
                Some(&serde_json::json!({"command": "ls -la\n   -h"}))
            ),
            "Bash ls -la -h"
        );
        assert_eq!(
            tool_summary("Read", Some(&serde_json::json!({"file_path": "a.txt"}))),
            "Read a.txt"
        );
        // 非字符串字段：退成整份紧凑 JSON
        assert_eq!(
            tool_summary("Task", Some(&serde_json::json!({"count": 2}))),
            r#"Task {"count":2}"#
        );
        // 没有 input / input 不是对象：只剩工具名
        assert_eq!(tool_summary("Bash", None), "Bash");
        assert_eq!(tool_summary("Bash", Some(&serde_json::json!("ls"))), "Bash");
        // 超长提示按字符截断（多字节安全）
        let long = "字".repeat(TOOL_ARG_CHARS + 5);
        assert_eq!(
            tool_summary("Bash", Some(&serde_json::json!({ "command": long }))),
            format!("Bash {}…", "字".repeat(TOOL_ARG_CHARS))
        );
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

    /// stderr 首行与失败原因都是按字符截断（多字节安全），短行不加工；失败原因只取首行。
    #[test]
    fn truncates_on_char_boundaries_and_keeps_the_first_error_line() {
        assert_eq!(truncate("短", 200), "短");
        assert_eq!(truncate("中文一行", 2), "中文…");
        assert_eq!(first_line("第一行\n第二行", ERR_TEXT_CHARS), "第一行");
        assert_eq!(
            first_line("  首尾留白  \n下一行", ERR_TEXT_CHARS),
            "首尾留白"
        );
        assert_eq!(first_line("", ERR_TEXT_CHARS), "");
        assert_eq!(first_line("中文一行", 2), "中文…");
    }
}
