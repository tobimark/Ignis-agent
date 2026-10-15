//! CLI 骨架：clap 参数结构、退出码映射与命令分发。
//!
//! 命令层把「预期内的失败」（配置读不了、工作区不存在、缺 API key 等）都变成人话 `Err`，
//! 由 `main` 打成 `error: …` 并给退出码 1；终端里永远不出现堆栈。

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use ignis_core::config::ignis_home;
use ignis_gateway::daemon::{self, DaemonStatus, StartOutcome, StopOutcome};
use ignis_runtime::RunStatus;

use crate::{commands, repl};

/// P1 唯一支持的内核；codex/claude 要到 P4。
const NATIVE_KERNEL: &str = "native";

/// `ignis`：本地个人 AI Agent。
#[derive(Debug, Parser)]
#[command(name = "ignis", version, about = "Ignis：本地个人 AI Agent")]
pub struct Cli {
    /// 执行内核（v0.1 仅 native）
    #[arg(long, global = true, value_name = "KERNEL", default_value = NATIVE_KERNEL)]
    pub kernel: String,

    /// 工作区目录（默认取配置里的 workspace_root）
    #[arg(long, global = true, value_name = "DIR")]
    pub workspace: Option<PathBuf>,

    /// 预授权普通操作（危险操作仍需 --unsafe）
    #[arg(long, global = true)]
    pub yes: bool,

    /// 预授权全部操作，含危险操作
    #[arg(long = "unsafe", global = true)]
    pub unsafe_ops: bool,

    /// 无子命令时进入 REPL
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// 子命令。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 执行一次任务
    Run {
        /// 任务内容
        task: String,
    },
    /// 查看任务记录
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// 查看或修改配置
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// 查看或修改长期记忆
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// 查看技能目录
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    /// 环境自检（home/config/api_key/base_url/workspace/db）
    Doctor,
    /// 调度守护进程（start / stop / status）
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

/// `ignis task` 的子命令。
#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// 任务详情：头部、审批记录与事件流
    Show {
        /// 任务 id
        id: String,
    },
    /// 最近的任务
    List {
        /// 最多列出多少条
        #[arg(long, value_name = "N", default_value_t = 20)]
        limit: usize,
    },
}

/// `ignis config` 的子命令。
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// 打印生效配置（环境变量覆盖已应用，api_key 恒为 ***）
    Show,
    /// 写回配置文件里的一个标量项，如 `ignis config set model.model gpt-4o-mini`
    Set {
        /// 点路径（如 model.model、limits.max_steps、default_kernel）
        key: String,
        /// 新值
        value: String,
    },
}

/// `ignis memory` 的子命令。
#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    /// 列出事实（默认只看 active，新近在前）
    List {
        /// 连被取代的历史行一起列出
        #[arg(long)]
        all: bool,
    },
    /// 按关键词检索事实（列与 list 相同）
    Search {
        /// 检索词
        query: String,
    },
    /// 新增一条事实（同 scope/key 的旧值被取代）
    Add {
        /// 作用域：user / project / global
        #[arg(long, value_name = "SCOPE")]
        scope: String,
        /// 短标识，如 pkg_manager（同 key 再写一次即取代旧值）
        #[arg(long, value_name = "KEY")]
        key: String,
        /// 事实内容
        text: String,
    },
    /// 改写一条事实的文本
    Edit {
        /// 事实 id（memory list 的第一列）
        id: String,
        /// 新文本
        text: String,
    },
    /// 删除一条事实
    Delete {
        /// 事实 id（memory list 的第一列）
        id: String,
    },
}

/// `ignis skills` 的子命令。
#[derive(Debug, Subcommand)]
pub enum SkillsCommand {
    /// 列出技能（内置 + `~/.ignis/skills`，按名称排序）
    List,
}

/// `ignis daemon` 的子命令。
///
/// T5 只做最小接线（进程管理），输出打磨与创建后自拉起提示留给 T6。
#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// 启动守护进程（后台自守护）
    Start,
    /// 停止守护进程（幂等：没在跑也是成功）
    Stop,
    /// 查看守护进程状态
    Status,
    /// 前台运行调度循环（自守护 re-exec 与 systemd/Docker 用；不直接面向用户）
    #[command(hide = true)]
    Run {
        /// 前台运行（re-exec 时总是带上）
        #[arg(long)]
        foreground: bool,
    },
}

/// 解析参数后分发：错误一律经 `anyhow::Result` 上抛给 `main`。
pub async fn dispatch(cli: &Cli) -> Result<ExitCode> {
    ensure_kernel(&cli.kernel)?;

    match &cli.command {
        Some(Command::Run { task }) => commands::run::run(cli, task).await,
        Some(Command::Task { command }) => commands::task::run(command),
        Some(Command::Config { command }) => commands::config::run(command),
        Some(Command::Memory { command }) => commands::memory::run(cli, command),
        Some(Command::Skills { command }) => commands::skills::run(command),
        Some(Command::Doctor) => commands::doctor::run(),
        Some(Command::Daemon { command }) => run_daemon(command).await,
        // 无子命令 → REPL
        None => repl::run(cli).await,
    }
}

/// `ignis daemon start|stop|status|run`：全部委托给 `ignis-gateway`，这一层只做文案与退出码。
///
/// `start` 由 gateway 自守护（`setsid` + re-exec `daemon run --foreground`）并等到 pidfile 就绪；
/// `stop` 幂等（没在跑也是退出 0）；`run --foreground` 是隐藏入口，前台跑调度循环直到信号。
async fn run_daemon(command: &DaemonCommand) -> Result<ExitCode> {
    let home = ignis_home();
    match command {
        DaemonCommand::Start => match daemon::start(&home)? {
            StartOutcome::Started { pid } => println!("daemon started (pid {pid})"),
        },
        DaemonCommand::Stop => match daemon::stop(&home)? {
            StopOutcome::Stopped { pid } => println!("daemon stopped (pid {pid})"),
            StopOutcome::NotRunning => println!("daemon is not running"),
        },
        DaemonCommand::Status => match daemon::status(&home) {
            DaemonStatus::Running { pid } => println!("daemon running (pid {pid})"),
            DaemonStatus::Stopped => println!("daemon is not running"),
            // 进程死了没清 pidfile：下一次 start 会覆盖它
            DaemonStatus::StalePidfile => println!("daemon is not running (stale pidfile)"),
            // 锁被持有但 pid 读不出来（被改坏/写入未完成）：状态无法确认，不拿猜的 pid 冒充
            // （T6 再统一文案；status 仍只读、退出 0）
            DaemonStatus::BadPidfile { path } => println!(
                "daemon state unknown (unreadable pidfile {}): check the file and retry",
                path.display()
            ),
        },
        DaemonCommand::Run { .. } => daemon::run_foreground(&home).await?,
    }
    Ok(ExitCode::SUCCESS)
}

/// 内核白名单：v0.1 只有 native，其余取值明确告知是 P4 的能力。
fn ensure_kernel(kernel: &str) -> Result<()> {
    if kernel == NATIVE_KERNEL {
        return Ok(());
    }
    bail!("kernel '{kernel}' 在 v0.1 尚未支持（P4 才有 codex/claude）")
}

/// 运行状态 → 进程退出码：成功 0；失败与触限 1；用户中止 130（128 + SIGINT，与 shell 约定一致）。
pub fn exit_code_for(status: RunStatus) -> ExitCode {
    match status {
        RunStatus::Success => ExitCode::SUCCESS,
        RunStatus::Failed | RunStatus::LimitReached => ExitCode::FAILURE,
        RunStatus::Aborted => ExitCode::from(130),
    }
}

#[cfg(test)]
mod tests {
    use std::process::ExitCode;

    use ignis_runtime::RunStatus;

    use super::*;

    #[test]
    fn exit_codes_follow_run_status() {
        // 全表：成功 0；失败与触限 1；用户中止 130（128 + SIGINT，与 shell 约定一致）
        let cases = [
            (RunStatus::Success, 0),
            (RunStatus::Failed, 1),
            (RunStatus::LimitReached, 1),
            (RunStatus::Aborted, 130),
        ];

        for (status, want) in cases {
            assert_eq!(exit_code_for(status), ExitCode::from(want), "{status:?}");
        }
    }

    #[test]
    fn kernel_whitelist_names_the_p4_kernels() {
        assert!(ensure_kernel("native").is_ok());

        let error = ensure_kernel("codex").unwrap_err().to_string();
        assert_eq!(
            error,
            "kernel 'codex' 在 v0.1 尚未支持（P4 才有 codex/claude）"
        );
    }
}
