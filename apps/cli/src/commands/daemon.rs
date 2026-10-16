//! `ignis daemon start|stop|status`（外加隐藏的 `daemon run --foreground`）与任务结束后的自拉起提示。
//!
//! 进程生命周期本身在 `ignis-gateway` 里（自守护 re-exec、pidfile + `flock` 单实例锁、SIGTERM
//! 优雅退出），这一层只做文案与退出码。`start` 默认自守护：阻塞调用（等 pidfile 就绪），交给
//! `tokio::task::spawn_blocking` 跑，不占着 async 线程；`start --foreground` 与 `run --foreground`
//! 是同一个前台入口（systemd 的 `ExecStart` 写的是前者），直接阻塞到 SIGTERM/SIGINT。
//!
//! 自拉起：`ignis run` / REPL 跑完一轮后，若本轮出现过 `schedule_create` 而守护进程没在跑，
//! 交互模式问一次「现在启动吗」，非交互模式只打一行提示；每会话至多一次。

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow, bail};
use ignis_core::config::ignis_home;
use ignis_gateway::daemon::{self, DaemonStatus, StartOutcome, StopOutcome};
use tokio::sync::oneshot;

use crate::cli::DaemonCommand;

/// 交互模式下的一次性提问（brief 钉死的文案）。
const START_PROMPT: &str = "Daemon is not running; start it now? [y/N] ";
/// 非交互模式下的提示行（每会话至多一次）。
const START_HINT: &str =
    "hint: daemon is not running; run `ignis daemon start` to enable schedules";

/// `ignis daemon`：start/stop/status 与隐藏的前台入口。
pub async fn run(command: &DaemonCommand) -> Result<ExitCode> {
    let home = ignis_home();
    match command {
        // 前台运行：与 `run --foreground` 同一个实现（spec §6.9 的 ExecStart 走这条），
        // 不退出直到收到 SIGTERM/SIGINT
        DaemonCommand::Start { foreground: true } => daemon::run_foreground(&home).await?,
        DaemonCommand::Start { foreground: false } => match start_daemon(&home).await? {
            StartOutcome::Started { pid } => println!("daemon started (pid {pid})"),
        },
        DaemonCommand::Stop => match stop_daemon(&home).await? {
            StopOutcome::Stopped { pid } => println!("daemon stopped (pid {pid})"),
            // 幂等：本来就没在跑也是成功
            StopOutcome::NotRunning => println!("daemon is not running"),
        },
        DaemonCommand::Status => {
            let line = status_report(&daemon::status(&home), &daemon::pid_file(&home))?;
            println!("{line}");
        }
        // 隐藏入口：自守护 re-exec 与 systemd/Docker 总是显式带 `--foreground`，漏了就报人话
        DaemonCommand::Run { foreground } => {
            ensure_foreground(*foreground)?;
            daemon::run_foreground(&home).await?
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// 隐藏入口 `daemon run` 的入口校验：必须显式 `--foreground`。
///
/// 不带旗标时这条命令既不是后台启动（那是 `daemon start`）、也不是明确的前台意图——静默按前台
/// 跑会把调度循环卡在一个用户以为「启动完就退」的终端上。
fn ensure_foreground(foreground: bool) -> Result<()> {
    if foreground {
        return Ok(());
    }
    bail!("daemon run 必须显式带 --foreground（后台启动请用 `ignis daemon start`）")
}

/// `status` 的读数 → 一行文案；坏 pidfile 是唯一的失败态（上抛后 `main` 打成 `error:` 并给退出 1）。
///
/// `pid_path` 是探测用的 pidfile（`{home}/ignis.pid`）：残留态要点名它，用户才知道该动哪个文件。
fn status_report(status: &DaemonStatus, pid_path: &Path) -> Result<String> {
    match status {
        DaemonStatus::Running { pid } => Ok(format!("daemon running (pid {pid})")),
        DaemonStatus::Stopped => Ok("daemon is not running".to_string()),
        // 进程死了没清 pidfile：仍报「没在跑」，并说清下一次 start 会覆盖它
        DaemonStatus::StalePidfile => Ok(format!(
            "daemon is not running (stale pidfile {}; the next start replaces it)",
            pid_path.display()
        )),
        // 锁被持有但 pid 读不出来（被改坏、写入未完成）：状态认不出，不拿猜的 pid 冒充。
        // 措辞与 `ignis-gateway` 的 `GatewayError::BadPidfile` 同一口径。
        DaemonStatus::BadPidfile { path } => bail!(
            "pidfile 不可读：内容不是合法 pid（{}）——文件被改坏或写入未完成，先检查（必要时删除）它再重试",
            path.display()
        ),
    }
}

/// `daemon::start` 是阻塞调用（自守护要等 pidfile 就绪，上限 2s）：丢给阻塞线程池，别占 async 线程。
async fn start_daemon(home: &Path) -> Result<StartOutcome> {
    let home = home.to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || daemon::start(&home))
        .await
        .map_err(|error| anyhow!("启动守护进程的阻塞任务未能收尾：{error}"))?;
    Ok(outcome?)
}

/// `daemon::stop` 同样阻塞：SIGTERM 之后最多等 12s（在调度循环 10s 的优雅收尾之上），必要时升级
/// SIGKILL。
async fn stop_daemon(home: &Path) -> Result<StopOutcome> {
    let home = home.to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || daemon::stop(&home))
        .await
        .map_err(|error| anyhow!("停止守护进程的阻塞任务未能收尾：{error}"))?;
    Ok(outcome?)
}

/// 任务结束后的自拉起动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autostart {
    /// 交互模式：问一次「现在启动吗」，答 `y` 才拉起。
    Prompt,
    /// 非交互模式：打印一行提示。
    Hint,
    /// 不打扰：没建过调度任务、守护进程已在跑，或本会话已经提示过。
    None,
}

/// 自拉起的判定（纯函数，四个条件的组合全表可钉死）。
///
/// 只有「本轮建过调度任务 + 守护进程没在跑 + 本会话还没提示过」才行动；交互模式提问，
/// 非交互模式打提示行。
pub fn autostart_action(
    saw_schedule_create: bool,
    daemon_running: bool,
    interactive: bool,
    already_prompted: bool,
) -> Autostart {
    if !saw_schedule_create || daemon_running || already_prompted {
        return Autostart::None;
    }
    if interactive {
        Autostart::Prompt
    } else {
        Autostart::Hint
    }
}

/// 任务结束后的自拉起：判定 → 提示/提问 → 需要时拉起守护进程。
///
/// 动作一旦发生就把会话标记成「已提示」（每会话至多一次）。状态探测是只读的、不阻塞；拉起失败
/// 只打 `error:` 一行——任务本身已经跑完了，自拉起失败不改变 `ignis run` 的退出码。
pub async fn autostart(home: &Path, saw_schedule_create: bool, already_prompted: &AtomicBool) {
    let action = autostart_action(
        saw_schedule_create,
        matches!(daemon::status(home), DaemonStatus::Running { .. }),
        std::io::stdin().is_terminal(),
        already_prompted.load(Ordering::Relaxed),
    );
    // 动作发生过就记下来（REPL 跨轮次共享同一个会话）
    if action != Autostart::None {
        already_prompted.store(true, Ordering::Relaxed);
    }

    match action {
        Autostart::None => {}
        Autostart::Hint => println!("{START_HINT}"),
        Autostart::Prompt => {
            if !ask_yes_no().await {
                return;
            }
            match start_daemon(home).await {
                Ok(StartOutcome::Started { pid }) => println!("daemon started (pid {pid})"),
                Err(error) => eprintln!("error: {error}"),
            }
        }
    }
}

/// 打印提问并读一行；答案裁掉首尾空白后只认 `y`（大小写不敏感）。
///
/// 读走一条脱离的 std 线程（与 REPL 的输入读取同一条路）：用户在提示符上按 Ctrl-C 退出时，
/// 进程收尾不会被这个可能永不返回的终端读卡住（`spawn_blocking` 会被运行时收尾等待）。
async fn ask_yes_no() -> bool {
    print!("{START_PROMPT}");
    let _ = std::io::stdout().flush();

    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let answer = std::io::stdin().lock().read_line(&mut line).map(|_| line);
        let _ = tx.send(answer);
    });
    matches!(rx.await, Ok(Ok(line)) if line.trim().eq_ignore_ascii_case("y"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// 探测用的 pidfile 路径（不要求存在）。
    fn pid_path() -> PathBuf {
        PathBuf::from("/tmp/ignis-home/ignis.pid")
    }

    /// `status` 的四种读数各给一行/一条错误：在跑与没在跑是成功态（退出 0），坏 pidfile 是失败态。
    #[test]
    fn status_report_maps_every_state() {
        let path = pid_path();

        assert_eq!(
            status_report(&DaemonStatus::Running { pid: 42 }, &path).unwrap(),
            "daemon running (pid 42)"
        );
        assert_eq!(
            status_report(&DaemonStatus::Stopped, &path).unwrap(),
            "daemon is not running"
        );

        // 残留 pidfile：仍报「没在跑」，但要点名文件并说清下一步（重启后会覆盖它）
        let stale = status_report(&DaemonStatus::StalePidfile, &path).unwrap();
        assert!(stale.contains("not running"), "{stale}");
        assert!(stale.contains("stale pidfile"), "{stale}");
        assert!(stale.contains("/tmp/ignis-home/ignis.pid"), "{stale}");

        // 坏 pidfile：唯一的失败态（`error:` + 退出码 1），文案要说清文件与下一步
        let bad = status_report(&DaemonStatus::BadPidfile { path: path.clone() }, &path)
            .unwrap_err()
            .to_string();
        assert!(bad.contains("pidfile"), "{bad}");
        assert!(bad.contains("/tmp/ignis-home/ignis.pid"), "{bad}");
        assert!(bad.contains("合法 pid"), "{bad}");
    }

    /// 判定全表：16 种组合逐个钉死——提问只在「建过 + 没在跑 + 交互 + 没问过」这一个角上。
    #[test]
    fn autostart_action_truth_table() {
        // (saw_schedule_create, daemon_running, interactive, already_prompted) → 期望
        let cases = [
            // 本轮没建过调度任务：无论别的条件如何都不打扰
            (false, false, true, false, Autostart::None),
            (false, false, false, false, Autostart::None),
            (false, true, true, false, Autostart::None),
            (false, true, false, false, Autostart::None),
            // 建过、守护进程已经在跑：不用提示
            (true, true, true, false, Autostart::None),
            (true, true, false, false, Autostart::None),
            // 建过、没在跑、本会话还没提示过：交互提问、非交互打提示
            (true, false, true, false, Autostart::Prompt),
            (true, false, false, false, Autostart::Hint),
            // 本会话已经提示过：不再打扰（两种模式都算提示过）
            (true, false, true, true, Autostart::None),
            (true, false, false, true, Autostart::None),
            (true, true, true, true, Autostart::None),
            (true, true, false, true, Autostart::None),
            (false, false, true, true, Autostart::None),
            (false, false, false, true, Autostart::None),
            (false, true, true, true, Autostart::None),
            (false, true, false, true, Autostart::None),
        ];

        for (saw, running, interactive, prompted, want) in cases {
            assert_eq!(
                autostart_action(saw, running, interactive, prompted),
                want,
                "saw={saw} running={running} interactive={interactive} prompted={prompted}"
            );
        }

        // 全表的四条判别：两种动作各有且仅有一次出现
        let (mut prompts, mut hints) = (0, 0);
        for saw in [false, true] {
            for running in [false, true] {
                for interactive in [false, true] {
                    for prompted in [false, true] {
                        match autostart_action(saw, running, interactive, prompted) {
                            Autostart::Prompt => prompts += 1,
                            Autostart::Hint => hints += 1,
                            Autostart::None => {}
                        }
                    }
                }
            }
        }
        assert_eq!((prompts, hints), (1, 1), "16 种组合里两种动作各一次");
    }

    /// 两段文案是契约（brief 钉死）：改字要连测试一起改。
    #[test]
    fn prompt_and_hint_texts_are_pinned() {
        assert_eq!(START_PROMPT, "Daemon is not running; start it now? [y/N] ");
        assert_eq!(
            START_HINT,
            "hint: daemon is not running; run `ignis daemon start` to enable schedules"
        );
    }

    /// 隐藏入口的入口校验：`daemon run` 必须显式带 `--foreground`（自守护 re-exec 总是带上）；
    /// 漏了就报人话错误，并指向用户多半真正想要的那条命令。
    #[test]
    fn daemon_run_requires_an_explicit_foreground_flag() {
        assert!(ensure_foreground(true).is_ok(), "显式 --foreground 应放行");

        let error = ensure_foreground(false).unwrap_err().to_string();
        assert!(error.contains("--foreground"), "{error}");
        assert!(error.contains("daemon start"), "{error}");
    }
}
