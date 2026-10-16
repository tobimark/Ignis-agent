//! CLI 骨架：clap 参数结构、退出码映射与命令分发。
//!
//! 命令层把「预期内的失败」（配置读不了、工作区不存在、缺 API key 等）都变成人话 `Err`，
//! 由 `main` 打成 `error: …` 并给退出码 1；终端里永远不出现堆栈。

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use ignis_runtime::RunStatus;

use crate::{commands, repl};

/// 三个执行内核：`--kernel` 的值域、`/kernel` 的校验与 [`commands::mk_runtime`] 共用一份口径。
pub const KERNELS: [&str; 3] = ["native", "codex", "claude"];

/// 主路径内核：`--kernel` 与配置 `default_kernel` 都没给时回落的默认值（完整工具与审批，
/// 见设计文档 §6.3）。
pub const NATIVE_KERNEL: &str = KERNELS[0];

/// `ignis`：本地个人 AI Agent。
#[derive(Debug, Parser)]
#[command(name = "ignis", version, about = "Ignis：本地个人 AI Agent")]
pub struct Cli {
    /// 执行内核（native|codex|claude；不给时取配置里的 default_kernel，默认 native）
    #[arg(
        long,
        global = true,
        value_name = "KERNEL",
        value_parser = clap::builder::PossibleValuesParser::new(KERNELS)
    )]
    pub kernel: Option<String>,

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
    /// 调度任务（list / pause / resume / remove / history）
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
    /// 告警收件箱（默认只看未读）
    Alerts(AlertsCommand),
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
/// 进程生命周期由 `ignis-gateway` 负责（自守护、pidfile + flock、优雅退出）；命令层只管文案与
/// 退出码，实现在 [`commands::daemon`]。
#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// 启动守护进程（默认后台自守护；`--foreground` 时为前台运行）
    Start {
        /// 前台运行调度循环（systemd 的 `ExecStart` 用这条：不脱离终端，SIGTERM 后优雅退出）
        #[arg(long)]
        foreground: bool,
    },
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

/// `ignis schedule` 的子命令。
#[derive(Debug, Subcommand)]
pub enum ScheduleCommand {
    /// 列出调度任务（含暂停的），显示下次到期时刻
    List,
    /// 暂停一个调度任务（守护进程下一次扫描时生效）
    Pause {
        /// 任务 id（`schedule list` 的第一列）
        id: String,
    },
    /// 恢复一个暂停的调度任务
    Resume {
        /// 任务 id
        id: String,
    },
    /// 删除一个调度任务（运行历史保留，`schedule history` 仍可查）
    Remove {
        /// 任务 id
        id: String,
    },
    /// 查看运行历史
    History {
        /// 任务 id；省略时看全部任务的历史
        id: Option<String>,
        /// 最多列出多少条
        #[arg(long, value_name = "N", default_value_t = 20)]
        limit: usize,
    },
}

/// `ignis alerts` 的开关。
#[derive(Debug, Args)]
pub struct AlertsCommand {
    /// 连已读告警一起列出
    #[arg(long)]
    pub all: bool,
    /// 展示后把所有未读告警标记为已读
    #[arg(long)]
    pub read: bool,
}

/// 解析参数后分发：错误一律经 `anyhow::Result` 上抛给 `main`。
///
/// `--kernel` 的值域由 clap 把关、配置里的 `default_kernel` 由 [`commands::resolve_kernel`] 补齐
/// 并同样过白名单，两侧共用一份 [`KERNELS`]；内核可用性由 [`commands::Session::new`] 校验。
pub async fn dispatch(cli: &Cli) -> Result<ExitCode> {
    match &cli.command {
        Some(Command::Run { task }) => commands::run::run(cli, task).await,
        Some(Command::Task { command }) => commands::task::run(command),
        Some(Command::Config { command }) => commands::config::run(command),
        Some(Command::Memory { command }) => commands::memory::run(cli, command),
        Some(Command::Skills { command }) => commands::skills::run(command),
        Some(Command::Doctor) => commands::doctor::run(),
        Some(Command::Daemon { command }) => commands::daemon::run(command).await,
        Some(Command::Schedule { command }) => commands::schedule::run(command),
        Some(Command::Alerts(command)) => commands::alerts::run(command),
        // 无子命令 → REPL
        None => repl::run(cli).await,
    }
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
    fn kernel_whitelist_is_the_three_kernels() {
        // 值域与 `/kernel` 共用一份口径：三个名字逐字钉住，native 是默认值
        assert_eq!(KERNELS, ["native", "codex", "claude"]);
        assert_eq!(NATIVE_KERNEL, KERNELS[0]);
    }

    /// `--kernel` 是**可选项**：不给就得是 `None`（由配置 `default_kernel` 兜底，spec §6.1 的
    /// 「默认取配置」），给了才取值；非法取值仍在解析期被拒绝（值域与 `/kernel` 共用 KERNELS）。
    #[test]
    fn kernel_flag_is_optional_and_validated_by_the_parser() {
        let omitted =
            Cli::try_parse_from(["ignis", "run", "说一句 ok"]).expect("不给 --kernel 应可解析");
        assert_eq!(omitted.kernel, None, "缺省必须是 None，不能钉死成 native");

        let given = Cli::try_parse_from(["ignis", "--kernel", "codex"]).expect("显式内核应可解析");
        assert_eq!(given.kernel.as_deref(), Some("codex"));

        // 非法取值不落到下面（不会变成「未知内核」运行期错误）：clap 解析期即拒绝
        let error = Cli::try_parse_from(["ignis", "--kernel", "gemini"]).unwrap_err();
        assert!(
            error.to_string().contains("native, codex, claude"),
            "{error}"
        );
    }
}
