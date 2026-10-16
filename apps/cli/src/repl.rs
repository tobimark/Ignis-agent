//! 无子命令时的交互式 REPL：banner、提示词、斜杠命令与逐行任务。
//!
//! 输入读取走 [`LineReader`] 这一层薄接口：[`StdinReader`] 只做「写提示词 + 读一行」，
//! 将来换成 rustyline（历史与行编辑）只需要在这里加一个实现，REPL 主循环不用动。

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
use crate::commands::{Session, kernel_note, spawn_ctrl_c_watcher};

/// 提示词（固定文案）。
const PROMPT: &str = "❯ ";

/// `/help` 列出的命令：退出、帮助与内核切换。
const HELP: [&str; 3] = [
    "/exit  退出 REPL",
    "/help  显示可用命令",
    "/kernel [name]  显示或切换执行内核（native|codex|claude）",
];

/// 一行输入的含义。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Input {
    /// 空行/纯空白：跳过，不作为任务。
    Empty,
    /// `/exit`：退出 REPL。
    Exit,
    /// `/help`：打印可用命令。
    Help,
    /// `/kernel`（无参显示当前内核）或 `/kernel <name>`（切换后续任务的内核）。
    Kernel(Option<String>),
    /// 其余输入：一次任务的内容。
    Task(String),
}

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

/// REPL banner：固定 4 行（版本号是 P1 的固定文案，不跟 `CARGO_PKG_VERSION` 联动），有未读告警时
/// 追加第 5 行。
///
/// 第 4 行是运行时就绪度：技能目录条数 + active 事实数；一条事实都没有（`memory == 0`，
/// 也含计数读失败时调用方退回的 0）打 `Memory: off`。
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
        "IGNIS v0.1".to_string(),
        "● Ready".to_string(),
        format!("Workspace: {}", workspace.display()),
        format!(
            "Runtime: {kernel} · Skills: {skills} · {}",
            memory_segment(memory)
        ),
    ];
    if let Some(count) = unread_alerts.filter(|count| *count > 0) {
        lines.push(format!("⚠ {count} unread alerts — run `ignis alerts`"));
    }
    lines
}

/// banner 的记忆段：有 active 事实给 `Memory: {n} facts`，没有给 `Memory: off`。
fn memory_segment(facts: usize) -> String {
    if facts == 0 {
        "Memory: off".to_string()
    } else {
        format!("Memory: {facts} facts")
    }
}

/// 分类一行输入：空行跳过，三个斜杠命令之外都当作任务内容（去掉行尾换行与首尾空白）。
///
/// `/kernel` 只认「单独一条命令」或「命令 + 一个名字」；多出的词与其它未知斜杠输入一样当任务，
/// 与 `/exit` / `/help` 的既有口径一致。
fn classify(line: &str) -> Input {
    let trimmed = line.trim();
    match trimmed.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] => Input::Empty,
        ["/exit"] => Input::Exit,
        ["/help"] => Input::Help,
        ["/kernel"] => Input::Kernel(None),
        ["/kernel", name] => Input::Kernel(Some((*name).to_string())),
        _ => Input::Task(trimmed.to_string()),
    }
}

/// `/kernel` 的输出：当前生效的内核名，非 native 时补上那行能力提示。
///
/// 无参显示与切换成功共用同一份文案（提示来自 [`kernel_note`]）：两处措辞分家的话，用户会以为
/// 切换没生效。
fn kernel_lines(kernel: &str) -> Vec<String> {
    let mut lines = vec![format!("kernel: {kernel}")];
    if let Some(note) = kernel_note(kernel) {
        lines.push(note);
    }
    lines
}

/// 逐行打印一个命令的输出。
fn print_lines(lines: &[String]) {
    for line in lines {
        println!("{line}");
    }
}

/// 启动 REPL；stdin 不是 tty 时拒绝启动（脚本/管道请用 `ignis run`）。
pub async fn run(cli: &Cli) -> Result<ExitCode> {
    if !io::stdin().is_terminal() {
        bail!("交互模式需要 TTY；请用 ignis run");
    }

    let mut session = Session::new(cli)?;
    // 计数来自会话里那份存储/技能目录（与运行器同一份），读库失败只退回 `Memory: off`
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

        match classify(&line) {
            Input::Empty => {}
            Input::Exit => return Ok(ExitCode::SUCCESS),
            Input::Help => {
                for command in HELP {
                    println!("{command}");
                }
            }
            // 无参：显示当前内核，非 native 时补一行能力提示
            Input::Kernel(None) => print_lines(&kernel_lines(session.kernel())),
            // 有参：校验可用后切换后续任务的内核；不可用就只打错误行，不切换
            Input::Kernel(Some(name)) => match session.set_kernel(&name) {
                Ok(kernel) => print_lines(&kernel_lines(kernel)),
                Err(error) => eprintln!("error: {error}"),
            },
            Input::Task(prompt) => {
                let outcome = session
                    .run_task(prompt, history.clone(), TaskKind::Repl)
                    .await?;
                history = outcome.messages;
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
                "IGNIS v0.1".to_string(),
                "● Ready".to_string(),
                "Workspace: /tmp/ws".to_string(),
                "Runtime: native · Skills: 3 · Memory: 2 facts".to_string(),
            ]
        );
        assert!(lines.contains(&"IGNIS v0.1".to_string()), "{lines:?}");
        assert!(lines.contains(&"● Ready".to_string()), "{lines:?}");
    }

    #[test]
    fn banner_marks_memory_off_when_there_are_no_facts() {
        let lines = banner_lines(Path::new("/tmp/ws"), "native", 5, 0, None);

        assert_eq!(lines[3], "Runtime: native · Skills: 5 · Memory: off");
        assert!(!lines[3].contains("0 facts"), "{lines:?}");
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

    #[test]
    fn classify_separates_commands_tasks_and_blank_lines() {
        // stdin 给的行带换行符；空行跳过
        assert_eq!(classify("\n"), Input::Empty);
        assert_eq!(classify("   \n"), Input::Empty);
        assert_eq!(classify("/exit\n"), Input::Exit);
        assert_eq!(classify("/help\n"), Input::Help);
        // `/kernel` 无参与带参两种形状；多余空白归一
        assert_eq!(classify("/kernel\n"), Input::Kernel(None));
        assert_eq!(
            classify("/kernel codex\n"),
            Input::Kernel(Some("codex".to_string()))
        );
        assert_eq!(
            classify("  /kernel   claude  \n"),
            Input::Kernel(Some("claude".to_string()))
        );
        // 多余的词不当命令：与其它未知斜杠输入一样当任务
        assert_eq!(
            classify("/kernel codex now\n"),
            Input::Task("/kernel codex now".to_string())
        );
        // 任务内容保留原文（只去掉行尾换行与首尾空白）
        assert_eq!(
            classify("把 README 翻译成中文\n"),
            Input::Task("把 README 翻译成中文".to_string())
        );
        assert_eq!(classify("/unknown\n"), Input::Task("/unknown".to_string()));
    }

    #[test]
    fn help_lists_the_three_commands() {
        let help = HELP.join("\n");
        assert!(help.contains("/exit"), "{help}");
        assert!(help.contains("/help"), "{help}");
        // 内核切换必须可发现：无参形状与三个取值都写在帮助里
        assert!(help.contains("/kernel [name]"), "{help}");
        assert!(help.contains("native|codex|claude"), "{help}");
    }

    /// `/kernel` 的两行输出：当前内核名 + 非 native 的那行能力提示（与 `--kernel` 路径同一份文案）。
    #[test]
    fn kernel_lines_pair_the_current_kernel_with_its_capability_note() {
        assert_eq!(kernel_lines("native"), ["kernel: native"]);

        assert_eq!(kernel_lines("codex").len(), 2);
        assert_eq!(kernel_lines("codex")[0], "kernel: codex");
        // 提示行与 `commands::kernel_note` 是同一条（两处措辞分家 = 用户以为切换没生效）
        assert_eq!(kernel_lines("codex")[1], kernel_note("codex").unwrap());
        assert_eq!(kernel_lines("claude")[1], kernel_note("claude").unwrap());
    }
}
