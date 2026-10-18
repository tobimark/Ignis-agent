//! 无子命令时的交互式 REPL：banner、提示词、斜杠命令（[`crate::commands::slash`]）与逐行任务。
//!
//! 输入读取走 [`LineReader`] 这一层薄接口：终端能承载转义重绘时用行编辑器
//! （[`crate::editor::reader::EditorLineReader`]：多行、历史、Tab 补全），否则回退 [`StdinReader`]
//! ——v0.1 的按行输入。REPL 主循环只依赖这一层，不关心背后是谁。
//!
//! 分类只走 `slash::parse` 一条路：空行跳过、`/…` 是本地命令（未知命令只在本地报错，**不送模型**）、
//! 其余是任务。

use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use ignis_core::config::ignis_home;
use ignis_core::task::TaskKind;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::commands::slash::{self, Dispatch, Parsed};
use crate::commands::{Session, install_ctrl_c_handler, kernel_note, spawn_ctrl_c_watcher};
use crate::editor::raw::{self, RawMode};
use crate::editor::reader::{EditorLineReader, INVALID_WARNING};

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
            // 与编辑器那条路同一份英文文案（界面 chrome；行为不变，只是语言收口）
            eprintln!("{INVALID_WARNING}");
        }
        Ok(Some(line))
    }
}

/// REPL banner：固定 5 行（版本号跟 `CARGO_PKG_VERSION` 联动，不写死字面量），有未读告警时追加第 6 行。
///
/// 第 4 行是运行时就绪度：技能目录条数 + active 事实数（0 也照实显示 `Memory: 0 facts`——
/// 记忆是「有就注入」的可选段，不是开关，不该在 banner 上显示成 `off`）。第 5 行是本会话的
/// 会话行（`Session: new` 或 13 字符短 id，与 `/status` 共用 [`slash::session_label`]），
/// `-c` 命中时不去 banner 之外再打一遍会话 id。
///
/// 最后一行只在有未读告警时出现（spec §6.1：启动时若有未读告警，顶部横幅提示）：`unread` 为
/// `Some(n)` 且 `n > 0` 时给 `⚠ {n} unread alerts — run \`ignis alerts\``；`None`（计数读失败，
/// 调用方已打警告）与 `Some(0)` 都保持 5 行。
pub fn banner_lines(
    workspace: &Path,
    kernel: &str,
    skills: usize,
    memory: usize,
    session: Option<&str>,
    unread_alerts: Option<usize>,
) -> Vec<String> {
    let mut lines = vec![
        format!("IGNIS v{}", env!("CARGO_PKG_VERSION")),
        "● Ready".to_string(),
        format!("Workspace: {}", workspace.display()),
        format!("Runtime: {kernel} · Skills: {skills} · Memory: {memory} facts"),
        format!("Session: {}", slash::session_label(session)),
    ];
    if let Some(line) = alerts_banner_line(unread_alerts) {
        lines.push(line);
    }
    lines
}

/// banner 的未读告警行（`Some(n>0)` 才有值）；[`banner_lines`] 改调它，逐字不变。
///
/// `None`（计数读失败，调用方已打警告）与 `Some(0)` 都给 `None`——那一行只在真有未读时出现。
/// TUI 的开机行也从这里取（同一个文案来源，两处不会分家）。
pub fn alerts_banner_line(unread: Option<usize>) -> Option<String> {
    unread
        .filter(|count| *count > 0)
        .map(|count| format!("⚠ {count} unread alerts — run `ignis alerts`"))
}

/// `-c` 的装载（两条入口共用）：未命中返回 `Some(no_session_note())`，由调用方决定打到哪
/// （经典 REPL 是 stdout，TUI 是滚动区）；`flag` 为假时什么都不做、返回 `None`。
///
/// 命中时把本工作区最近一次会话设成当前会话并装载有效历史（banner 的 Session 行会带短 id，
/// 不另打一行）。读取失败是硬错误——用户点名要恢复，静默起步新会话会让人以为历史还在。
pub fn resume_continue(session: &mut Session, flag: bool) -> Result<Option<&'static str>, String> {
    if !flag {
        return Ok(None);
    }
    match session.resume_latest()? {
        Some(_) => Ok(None),
        None => Ok(Some(no_session_note())),
    }
}

/// `--tui` 不可用时的一行 note（逐字，英文 chrome）。
///
/// 唯一定义放在这里（`repl` 不按平台门控）：unix 侧 [`crate::tui::fallback_note`] 只是转发到它，
/// 非 unix 侧的降级（理由逐字 `unsupported platform`）打同一句话——两边不可能分家。
pub fn tui_unavailable_note(reason: &str) -> String {
    format!("note: TUI unavailable ({reason}); falling back to the classic REPL")
}

/// `-c` 未命中「本工作区最近一次会话」时的提示行（英文 chrome，逐字钉住）。
pub fn no_session_note() -> &'static str {
    "note: no previous session in this workspace; starting a new one"
}

/// 回退按行输入时的一行说明（英文 chrome；`reason` 来自 [`choose_reader`] 的第二个返回值）。
pub fn fallback_note(reason: &str) -> String {
    format!("note: line editing unavailable ({reason}); using plain line input")
}

/// 选读取层：终端能承载转义重绘就用行编辑器，否则回退 v0.1 的按行输入。
///
/// 返回 `(读取层, 回退理由)`：编辑器那条路理由为空；回退那条路理由非空（调用方用
/// [`fallback_note`] 打印它）。raw mode 的探针只进入一次就立刻析构还原——真正的守卫由
/// `EditorLineReader::read_line` 按读取区间进出（任务执行与审批期间终端必须是 canonical/echo）。
pub fn choose_reader(history_path: PathBuf) -> (Arc<dyn LineReader>, Option<String>) {
    choose_reader_with(
        history_path,
        std::env::var_os("TERM").as_deref(),
        io::stdout().is_terminal(),
        RawMode::enable,
    )
}

/// 带 `TERM` / stdout-tty / termios 注入的变体（与 `raw::RawMode::supported` 同一手法）：用例
/// 因此不必改进程环境、也不必真的 tty（测试可并行）。
fn choose_reader_with(
    history_path: PathBuf,
    term: Option<&OsStr>,
    stdout_is_terminal: bool,
    enable: impl FnOnce() -> io::Result<RawMode>,
) -> (Arc<dyn LineReader>, Option<String>) {
    // 哑终端（`TERM` 缺失、空串或 `dumb`）上转义重绘会把屏幕打花：宁可少功能，直接回退
    if !RawMode::supported(term) {
        return (Arc::new(StdinReader), Some("TERM is dumb".to_string()));
    }
    // 编辑器只在终端上重绘：stdout 是管道/文件（`ignis > log`）时回退行模式，
    // 否则帧的转义序列会原样写进日志
    if !stdout_is_terminal {
        return (
            Arc::new(StdinReader),
            Some("stdout is not a terminal".to_string()),
        );
    }
    match enable() {
        // 探针只问「进得去吗」：守卫立刻析构还原，终端不在这里停留
        Ok(guard) => {
            drop(guard);
            (Arc::new(EditorLineReader::new(history_path)), None)
        }
        // termios 进不去（不是 tty、已关闭）：理由就是错误本身，整句由 `fallback_note` 拼
        Err(error) => (Arc::new(StdinReader), Some(error.to_string())),
    }
}

/// 启动 REPL；stdin 不是 tty 时拒绝启动（脚本/管道请用 `ignis run`）。
pub async fn run(cli: &Cli) -> Result<ExitCode> {
    if !io::stdin().is_terminal() {
        bail!("交互模式需要 TTY；请用 ignis run");
    }

    // `--tui`：先试全屏界面。不可用时 `tui::start` 只多打一行 note（`Ok(None)`），下面的一切
    // 照旧——banner、`-c` 与读取层选择都走**原路径**（R4：降级不改变经典 REPL 的任何字节）
    if cli.tui {
        #[cfg(unix)]
        {
            if let Some(code) = crate::tui::start(cli).await? {
                return Ok(code);
            }
        }
        // 非 unix：信号流与 termios 都是 unix 的，`tui` 模块整块不在——`--tui` 等价于可用性判定
        // 失败，理由逐字 `unsupported platform`，note 与降级路径不变
        #[cfg(not(unix))]
        {
            println!("{}", tui_unavailable_note("unsupported platform"));
        }
    }

    // 经典路径的起点：先把进程级 SIGINT 处理器装好（一次），句柄活到 `run` 返回。
    //
    // 安装点必须在**任何可交互时机之前**：`tokio::signal::ctrl_c` 的处理器在 future 首次轮询时
    // 才装，而空闲 / 任务 / `/compact` / `/learn` 四段的 watcher 都是「每段现起」——第一段 watcher
    // 被轮询之前到达的 Ctrl+C 会走默认处置直接杀进程（P5 台账记的偶发信号退出）。装在这里，
    // 会话装配、banner 与第一段等待都在它之后；各段 watcher 照旧只管自己那份令牌（监听者各自
    // 成流、互不消耗）。TUI 路径自己管信号（进守卫之前注册），成功时早已返回，不走这一行。
    let _ctrl_c = install_ctrl_c_handler();

    let mut session = Session::new(cli)?;
    // `-c`：装载本工作区最近一次会话（命中时 banner 的 Session 行带短 id；没有会话打一行 note）。
    // 装载规则与 TUI 入口共用一份（[`resume_continue`]）。
    if let Some(note) =
        resume_continue(&mut session, cli.continue_session).map_err(anyhow::Error::msg)?
    {
        println!("{note}");
    }
    // 计数来自会话里那份存储/技能目录（与运行器同一份），读库失败只退回 0 facts
    // 与「不打未读行」（各自已打一行警告）
    let (skills, memory) = session.banner_counts();
    let unread = session.unread_alerts();
    for line in banner_lines(
        session.workspace(),
        session.kernel(),
        skills,
        memory,
        session.current_session(),
        unread,
    ) {
        println!("{line}");
    }
    // 选定非 native 内核时能力提示行跟着 banner 一起给出（`/kernel` 切换时是同一份文案）；
    // native 不加任何行——启动屏的形状保持 5 行（有未读告警时 6 行）
    if let Some(note) = kernel_note(session.kernel()) {
        println!("{note}");
    }

    // 读取层：终端能承载转义重绘就用行编辑器（历史落在 `IGNIS_HOME`/history，目录已由 `Session::new`
    // 里的 `ensure_home` 建好），否则回退 v0.1 的按行输入并说明一句。raw mode 的守卫不在这里
    // 跨读取持有：它在 `EditorLineReader::read_line` 里按读取区间进出，任务执行与审批期间
    // 终端必须是 canonical/echo（审批提示的按行读在 raw mode 下永远等不到一行）。
    let (reader, fallback) = choose_reader(ignis_home().join("history"));
    if let Some(reason) = fallback.as_deref() {
        println!("{}", fallback_note(reason));
    }

    let outcome = repl_loop(&mut session, reader).await;
    // 退出兜底：读取跑在 `next_line` 起的脱离线程里，空闲 Ctrl-C 退出时那条线程还阻塞在
    // `read` 上握着 raw mode 守卫，进程退出不会析构它——按 `editor::raw` 记录的现场做一次
    // best-effort 还原（没有活跃守卫时是无操作），用户的 shell 不会没回显。
    raw::restore_terminal();
    outcome
}

/// REPL 主循环：读取 → 本地分类 → 命令分发 / 任务；返回值原样交回 [`run`]。
///
/// 跨轮上下文归 [`Session`]（`run_task` 成功后换成 `outcome.messages`，`-c` 恢复时装库里的
/// 有效历史）——这里只负责读行与分发，不再自己攒一份 `history`。
async fn repl_loop(session: &mut Session, reader: Arc<dyn LineReader>) -> Result<ExitCode> {
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
            Parsed::Command(command, args) => match slash::dispatch(session, command, &args) {
                Ok(Dispatch::Exit) => return Ok(ExitCode::SUCCESS),
                Ok(Dispatch::Lines(lines)) => {
                    for line in lines {
                        println!("{line}");
                    }
                }
                // `/compact` 需要模型往返：分发只交回意图，这里 `await` 真正的执行
                // （成功打 stdout；错误与命令自己的错误同形，打到 stderr）。执行期间挂与任务轮次
                // 同款的 Ctrl-C 监听：取消令牌传进摘要请求，Ctrl-C 能当场中止（内核收成
                // `Done{Aborted}`，`Session::compact` 交回 `error: compact: summarization failed:
                // aborted` 且不改状态）；执行结束（无论成败）立刻收掉监听任务。
                Ok(Dispatch::Compact) => {
                    let cancel = CancellationToken::new();
                    let watcher = spawn_ctrl_c_watcher(cancel.clone());
                    let compacted = session.compact(cancel).await;
                    watcher.abort();
                    match compacted {
                        Ok(lines) => {
                            for line in lines {
                                println!("{line}");
                            }
                        }
                        Err(error) => eprintln!("error: {error}"),
                    }
                }
                // `/learn` 与 `/compact` 同款：草稿请求也可被 Ctrl-C 中止（取消令牌直达无工具请求），
                // 确认读的是真 stdin（canonical 模式下的读一行，与审批同一时机）。成功打 stdout
                // （落盘回显或 `discarded`），错误打 stderr——生成/校验/落盘失败都不写一个字节。
                Ok(Dispatch::Learn(arg)) => {
                    let cancel = CancellationToken::new();
                    let watcher = spawn_ctrl_c_watcher(cancel.clone());
                    let learned = session.learn(arg.as_deref(), cancel).await;
                    watcher.abort();
                    match learned {
                        Ok(lines) => {
                            for line in lines {
                                println!("{line}");
                            }
                        }
                        Err(error) => eprintln!("error: {error}"),
                    }
                }
                // 命令自己的错误（内核名非法、取值不对、尚未实现的命令）打到 stderr，
                // 与 `main` 的错误行同形
                Err(error) => eprintln!("error: {error}"),
            },
            Parsed::Task(prompt) => {
                session.run_task(prompt, TaskKind::Repl).await?;
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
    use std::ffi::OsStr;
    use std::io;
    use std::path::Path;

    use ignis_core::config::Config;
    use ignis_runtime::test_support::{ScriptStep, ScriptedModel};

    use crate::commands::fixtures::{session_at_home, session_fixture_with_config};

    use super::*;

    /// 回退时的一行说明逐字钉住（英文 chrome，理由来自选读取层的判定）。
    #[test]
    fn fallback_note_is_pinned() {
        assert_eq!(
            fallback_note("TERM is dumb"),
            "note: line editing unavailable (TERM is dumb); using plain line input"
        );
    }

    /// `TERM` 缺失 / `dumb` / 空：不碰 termios，回退 v0.1 的按行输入并给理由。
    #[test]
    fn choose_reader_falls_back_when_term_is_dumb() {
        // 回退路径根本不碰历史，这里只是给一个不指向真实 `~/.ignis` 的占位路径；
        // 探针也不该被调用（否则就不是「不碰 termios」了）
        let history = std::env::temp_dir().join("ignis-cli-repl-fallback-history");

        for term in [None, Some(OsStr::new("dumb")), Some(OsStr::new(""))] {
            let (_reader, reason) = choose_reader_with(history.clone(), term, true, || {
                panic!("{term:?} 是哑终端：不该进 raw mode")
            });
            let reason = reason.expect("回退必须给理由");
            assert_eq!(reason, "TERM is dumb");
            assert_eq!(
                fallback_note(&reason),
                "note: line editing unavailable (TERM is dumb); using plain line input"
            );
        }
    }

    /// stdout 不是 tty（`ignis > log`）：转义重绘会原样写进日志，回退行模式，探针同样不跑。
    #[test]
    fn choose_reader_falls_back_when_stdout_is_not_a_terminal() {
        let history = std::env::temp_dir().join("ignis-cli-repl-stdout-history");
        let (_reader, reason) = choose_reader_with(
            history,
            Some(OsStr::new("xterm-256color")),
            false,
            || -> io::Result<RawMode> { Err(io::Error::other("探针不该被调用")) },
        );

        assert_eq!(reason.as_deref(), Some("stdout is not a terminal"));
    }

    /// termios 进不去 raw mode（不是 tty、已关闭）：同样回退，理由取错误的文本。
    #[test]
    fn choose_reader_falls_back_when_termios_fails() {
        let history = std::env::temp_dir().join("ignis-cli-repl-termios-history");
        let (_reader, reason) = choose_reader_with(
            history,
            Some(OsStr::new("xterm-256color")),
            true,
            || -> io::Result<RawMode> { Err(io::Error::other("不是 tty")) },
        );

        assert_eq!(reason.as_deref(), Some("不是 tty"));
    }

    #[test]
    fn banner_is_the_five_locked_lines() {
        let lines = banner_lines(Path::new("/tmp/ws"), "native", 3, 2, None, None);

        assert_eq!(
            lines,
            [
                // 版本号跟 `CARGO_PKG_VERSION` 联动（钉死字面量会让升级后 banner 说谎）
                format!("IGNIS v{}", env!("CARGO_PKG_VERSION")),
                "● Ready".to_string(),
                "Workspace: /tmp/ws".to_string(),
                "Runtime: native · Skills: 3 · Memory: 2 facts".to_string(),
                "Session: new".to_string(),
            ]
        );
        assert!(lines.contains(&"● Ready".to_string()), "{lines:?}");
    }

    #[test]
    fn banner_counts_zero_facts_instead_of_turning_memory_off() {
        let lines = banner_lines(Path::new("/tmp/ws"), "native", 5, 0, None, None);

        assert_eq!(lines[3], "Runtime: native · Skills: 5 · Memory: 0 facts");
        assert!(!lines[3].contains("off"), "{lines:?}");
    }

    /// 会话行：banner 第 5 行，`Session: new` 或 13 字符短 id（与任务短 id 同一 helper）；
    /// 有未读告警时它的下标往后挪一位（告警行才排第六）。
    #[test]
    fn banner_appends_the_session_line_before_the_alerts() {
        // 没有会话（还没跑过任务 / `-c` 未命中）：`Session: new`
        let fresh = banner_lines(Path::new("/tmp/ws"), "native", 3, 2, None, None);
        assert_eq!(fresh.len(), 5, "{fresh:?}");
        assert_eq!(fresh[4], "Session: new");

        // 有会话：13 字符短 id
        let session_id = "019a1b2c-3d4e-7f00-8000-000000000001";
        let resumed = banner_lines(Path::new("/tmp/ws"), "native", 3, 2, Some(session_id), None);
        assert_eq!(resumed[4], "Session: 019a1b2c-3d4e");

        // 有未读告警：会话行仍在第 5 行，告警行被挤到第 6 行
        let alerting = banner_lines(
            Path::new("/tmp/ws"),
            "native",
            3,
            2,
            Some(session_id),
            Some(4),
        );
        assert_eq!(alerting.len(), 6, "{alerting:?}");
        assert_eq!(alerting[4], "Session: 019a1b2c-3d4e");
        assert_eq!(alerting[5], "⚠ 4 unread alerts — run `ignis alerts`");
    }

    /// `-c` 未命中时的提示行逐字钉住。
    #[test]
    fn no_session_note_is_pinned() {
        assert_eq!(
            no_session_note(),
            "note: no previous session in this workspace; starting a new one"
        );
    }

    /// 未读告警：有就追加一行（点名条数与下一步命令），没有（0 或计数读失败）保持五行的常规形状。
    #[test]
    fn banner_appends_an_unread_alerts_line_only_when_there_are_some() {
        let lines = banner_lines(Path::new("/tmp/ws"), "native", 3, 2, None, Some(4));
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert_eq!(lines[5], "⚠ 4 unread alerts — run `ignis alerts`");
        // 前四行不受影响，会话行仍在第 5 行
        assert_eq!(lines[3], "Runtime: native · Skills: 3 · Memory: 2 facts");
        assert_eq!(lines[4], "Session: new");

        let quiet = banner_lines(Path::new("/tmp/ws"), "native", 3, 2, None, Some(0));
        assert_eq!(quiet.len(), 5, "{quiet:?}");
        assert!(
            !quiet.iter().any(|line| line.contains("unread")),
            "{quiet:?}"
        );

        // 计数读失败（`None`，调用方已打警告）：跳过那一行，banner 形状不变
        let failed = banner_lines(Path::new("/tmp/ws"), "native", 3, 2, None, None);
        assert_eq!(failed, quiet, "读失败与「没有未读」对用户是同一屏");
    }

    /// 未读告警行逐字（`Some(n>0)` 才有值）：经典 banner 与 TUI 的开机行共用这一份。
    #[test]
    fn alerts_banner_line_is_the_classic_alert_copy() {
        assert_eq!(
            alerts_banner_line(Some(4)).as_deref(),
            Some("⚠ 4 unread alerts — run `ignis alerts`")
        );
        assert_eq!(
            alerts_banner_line(Some(1)).as_deref(),
            Some("⚠ 1 unread alerts — run `ignis alerts`"),
            "单数也照原样：文案不按数量变形"
        );
        assert_eq!(alerts_banner_line(Some(0)), None, "没有未读就没有那一行");
        assert_eq!(
            alerts_banner_line(None),
            None,
            "计数读失败（调用方已打警告）也跳过"
        );
        // banner 的第 6 行就是它（既有 5/6 行断言不动）
        assert_eq!(
            banner_lines(Path::new("/tmp/ws"), "native", 3, 2, None, Some(4))[5],
            alerts_banner_line(Some(4)).unwrap()
        );
    }

    /// `-c` 的装载抽出来给两条入口共用（经典 stdout / TUI 滚动区）：flag 为假什么都不做；
    /// 未命中给 [`no_session_note`]；命中给 `None` 且历史真的装载。
    #[tokio::test]
    async fn resume_continue_returns_the_note_only_when_asked_and_missing() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("答复一".into())]);
        let config = Config {
            api_key: Some("test-key".into()),
            model: ignis_runtime::ModelConfig {
                base_url: model.base_url(),
                model: "test-model".into(),
                ..ignis_runtime::ModelConfig::default()
            },
            ..Config::default()
        };
        let (dir, mut session) =
            session_fixture_with_config("repl-resume-continue", "native", config.clone());

        // flag 为假：什么都不做（不装历史、没有 note）
        assert_eq!(resume_continue(&mut session, false), Ok(None));
        assert_eq!(session.current_session(), None);
        assert!(session.history().is_empty());

        // flag 为真 + 空 home：未命中 → 一行 note（打到 stdout 还是滚动区由调用方决定）
        assert_eq!(
            resume_continue(&mut session, true),
            Ok(Some(no_session_note()))
        );
        assert_eq!(session.current_session(), None, "未命中什么都不改");

        // 先跑一轮把会话落库，再从「新进程」的角度恢复：命中 → 没有 note，历史装载
        session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .expect("先跑一轮把会话落库");
        let mut restarted = session_at_home(dir.path(), "native", config);
        assert_eq!(restarted.current_session(), None, "恢复前还没有会话");
        assert_eq!(resume_continue(&mut restarted, true), Ok(None));
        assert!(restarted.current_session().is_some(), "命中后设了当前会话");
        assert_eq!(
            restarted.history().len(),
            2,
            "有效历史装载（user + assistant）"
        );
    }

    /// 非 unix 的降级理由逐字 `unsupported platform`：note 与 unix 侧同一句话（文案只有一处）。
    #[test]
    fn tui_unavailable_note_is_the_single_copy() {
        assert_eq!(
            tui_unavailable_note("unsupported platform"),
            "note: TUI unavailable (unsupported platform); falling back to the classic REPL"
        );
        // unix 侧 `tui::fallback_note` 就是它（同一个实现，逐字不可能分家）
        #[cfg(unix)]
        assert_eq!(
            tui_unavailable_note("TERM is dumb"),
            crate::tui::fallback_note("TERM is dumb")
        );
    }
}
