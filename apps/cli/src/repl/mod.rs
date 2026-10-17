//! 无子命令时的交互式 REPL：banner、提示词、斜杠命令（[`crate::commands::slash`]）与逐行任务。
//!
//! 输入读取走 [`LineReader`] 这一层薄接口：[`StdinReader`] 只做「写提示词 + 读一行」，
//! 将来换成 rustyline（历史与行编辑）只需要在这里加一个实现，REPL 主循环不用动。
//!
//! 分类只走 `slash::parse` 一条路：空行跳过、`/…` 是本地命令（未知命令只在本地报错，**不送模型**）、
//! 其余是任务。

use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use ignis_core::task::TaskKind;
use ignis_runtime::ChatMessage;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::commands::slash::{self, Dispatch, Parsed};
use crate::commands::{Session, kernel_note, spawn_ctrl_c_watcher};

/// 提示词（固定文案）。
const PROMPT: &str = "❯ ";

/// 读一行输入的最小接口；REPL 只依赖它，实现可以换（rustyline / 脚本化输入）。
pub trait LineReader: Send + Sync {
    /// 打印提示词并读一行；EOF 返回 `Ok(None)`，读取失败返回 `Err`。
    fn read_line(&self, prompt: &str) -> io::Result<Option<String>>;
}

/// 兜底实现：`std::io::stdin()` 逐行读取，没有历史与行编辑。
pub struct StdinReader;

impl LineReader for StdinReader {
    fn read_line(&self, prompt: &str) -> io::Result<Option<String>> {
        let mut out = io::stdout();
        write!(out, "{prompt}")?;
        out.flush()?;

        // 终端 / 输入法 / 粘贴都可能送进非法 UTF-8 字节；std 的 `read_line` 对此会硬失败，
        // 一个坏字节就足以带走整个会话（历史一起丢）。这里按字节读了宽松解码（见 `input`）。
        let Some((line, valid)) = crate::input::read_line_lossy(&mut io::stdin().lock())? else {
            return Ok(None); // EOF（Ctrl-D）时读到 0 字节
        };
        if !valid {
            eprintln!("warning: 输入含非法 UTF-8 字节，已按替换字符处理（检查终端编码或输入法）");
        }
        Ok(Some(line))
    }
}

/// REPL banner：固定 4 行（版本号跟 `CARGO_PKG_VERSION` 联动，不写死字面量），有未读告警时追加第 5 行。
///
/// 第 4 行是运行时就绪度：技能目录条数 + active 事实数（0 也照实显示 `Memory: 0 facts`——
/// 记忆是「有就注入」的可选段，不是开关，不该在 banner 上显示成 `off`）。
///
/// 第 5 行只在有未读告警时出现（spec §6.1：启动时若有未读告警，顶部横幅提示）：`unread` 为
/// `Some(n)` 且 `n > 0` 时给 `⚠ {n} unread alerts — run \`ignis alerts\``；`None`（计数读失败，
/// 调用方已打警告）与 `Some(0)` 都保持 4 行。
pub fn banner_lines(
    workspace: &Path,
    kernel: &str,
    skills: usize,
    memory: usize,
    unread_alerts: Option<usize>,
) -> Vec<String> {
    let mut lines = vec![
        format!("IGNIS v{}", env!("CARGO_PKG_VERSION")),
        "● Ready".to_string(),
        format!("Workspace: {}", workspace.display()),
        format!("Runtime: {kernel} · Skills: {skills} · Memory: {memory} facts"),
    ];
    if let Some(count) = unread_alerts.filter(|count| *count > 0) {
        lines.push(format!("⚠ {count} unread alerts — run `ignis alerts`"));
    }
    lines
}

/// 启动 REPL；stdin 不是 tty 时拒绝启动（脚本/管道请用 `ignis run`）。
pub async fn run(cli: &Cli) -> Result<ExitCode> {
    if !io::stdin().is_terminal() {
        bail!("交互模式需要 TTY；请用 ignis run");
    }

    let mut session = Session::new(cli)?;
    // 计数来自会话里那份存储/技能目录（与运行器同一份），读库失败只退回 0 facts
    // 与「不打未读行」（各自已打一行警告）
    let (skills, memory) = session.banner_counts();
    let unread = session.unread_alerts();
    for line in banner_lines(
        session.workspace(),
        session.kernel(),
        skills,
        memory,
        unread,
    ) {
        println!("{line}");
    }
    // 选定非 native 内核时能力提示行跟着 banner 一起给出（`/kernel` 切换时是同一份文案）；
    // native 不加任何行——启动屏的形状保持 4 行（有未读告警时 5 行）
    if let Some(note) = kernel_note(session.kernel()) {
        println!("{note}");
    }

    let reader: Arc<dyn LineReader> = Arc::new(StdinReader);
    // 跨轮上下文：每次任务结束后换成 `outcome.messages`，内核只带回了最终答复
    let mut history: Vec<ChatMessage> = Vec::new();

    loop {
        // 空闲期：Ctrl-C 退出。任务运行中的 Ctrl-C 由 `run_task` 自己的监听任务处理
        let idle = CancellationToken::new();
        let watcher = spawn_ctrl_c_watcher(idle.clone());
        let line = tokio::select! {
            line = next_line(Arc::clone(&reader)) => line?,
            _ = idle.cancelled() => {
                // 终端里 Ctrl-C 不留换行，补一个让下一行提示符另起一行
                println!();
                return Ok(ExitCode::SUCCESS);
            }
        };
        watcher.abort();

        let Some(line) = line else {
            // EOF（Ctrl-D）同样退出：终端里它也不留换行
            println!();
            return Ok(ExitCode::SUCCESS);
        };

        // 本地分类：空行跳过；未知命令只在本地报错（v0.1 里它们会被当成任务送给模型）
        match slash::parse(&line) {
            Parsed::Empty => {}
            Parsed::Unknown(name) => eprintln!("{}", slash::unknown_command_line(&name)),
            Parsed::Command(command, args) => match slash::dispatch(&mut session, command, &args) {
                Ok(Dispatch::Exit) => return Ok(ExitCode::SUCCESS),
                Ok(Dispatch::Lines(lines)) => {
                    for line in lines {
                        println!("{line}");
                    }
                }
                // 命令自己的错误（内核名非法、取值不对、尚未实现的命令）打到 stderr，
                // 与 `main` 的错误行同形
                Err(error) => eprintln!("error: {error}"),
            },
            Parsed::Task(prompt) => {
                let outcome = session
                    .run_task(prompt, history.clone(), TaskKind::Repl)
                    .await?;
                history = outcome.messages;
                // 轮次之间空一行：上一轮的结束行与下一轮的提示符不至于粘在一起
                // （`ignis run` 是一锤子买卖，不补这一行）
                println!();
            }
        }
    }
}

/// 在独立线程里读一行，结果以 oneshot 交回 async 侧。
///
/// 这里不能用 `tokio::task::spawn_blocking`：运行时收尾会等阻塞任务跑完，而停在提示符上的终端读
/// 永远不会返回——空闲时按 Ctrl-C 退出正好卡在这一步。脱离的 std 线程在进程退出时随之消失。
async fn next_line(reader: Arc<dyn LineReader>) -> Result<Option<String>> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        // 接收端已经走了（Ctrl-C 退出）时结果直接丢掉
        let _ = tx.send(reader.read_line(PROMPT));
    });

    Ok(rx.await.map_err(|_| anyhow!("读取输入的线程提前退出"))??)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn banner_is_the_four_locked_lines() {
        let lines = banner_lines(Path::new("/tmp/ws"), "native", 3, 2, None);

        assert_eq!(
            lines,
            [
                // 版本号跟 `CARGO_PKG_VERSION` 联动（钉死字面量会让升级后 banner 说谎）
                format!("IGNIS v{}", env!("CARGO_PKG_VERSION")),
                "● Ready".to_string(),
                "Workspace: /tmp/ws".to_string(),
                "Runtime: native · Skills: 3 · Memory: 2 facts".to_string(),
            ]
        );
        assert!(lines.contains(&"● Ready".to_string()), "{lines:?}");
    }

    #[test]
    fn banner_counts_zero_facts_instead_of_turning_memory_off() {
        let lines = banner_lines(Path::new("/tmp/ws"), "native", 5, 0, None);

        assert_eq!(lines[3], "Runtime: native · Skills: 5 · Memory: 0 facts");
        assert!(!lines[3].contains("off"), "{lines:?}");
    }

    /// 未读告警：有就追加第 5 行（点名条数与下一步命令），没有（0 或计数读失败）保持 4 行。
    #[test]
    fn banner_appends_an_unread_alerts_line_only_when_there_are_some() {
        let lines = banner_lines(Path::new("/tmp/ws"), "native", 3, 2, Some(4));
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert_eq!(lines[4], "⚠ 4 unread alerts — run `ignis alerts`");
        // 前四行不受影响
        assert_eq!(lines[3], "Runtime: native · Skills: 3 · Memory: 2 facts");

        let quiet = banner_lines(Path::new("/tmp/ws"), "native", 3, 2, Some(0));
        assert_eq!(quiet.len(), 4, "{quiet:?}");
        assert!(
            !quiet.iter().any(|line| line.contains("unread")),
            "{quiet:?}"
        );

        // 计数读失败（`None`，调用方已打警告）：跳过那一行，banner 形状不变
        let failed = banner_lines(Path::new("/tmp/ws"), "native", 3, 2, None);
        assert_eq!(failed, quiet, "读失败与「没有未读」对用户是同一屏");
    }
}
