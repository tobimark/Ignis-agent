//! CLI 骨架：clap 参数结构、退出码映射与命令分发。
//!
//! 命令层把「预期内的失败」（配置读不了、工作区不存在、缺 API key 等）都变成人话 `Err`，
//! 由 `main` 打成 `error: …` 并给退出码 1；终端里永远不出现堆栈。

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
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
    /// 环境自检（home/config/api_key/base_url/workspace/db）
    Doctor,
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

/// 解析参数后分发：错误一律经 `anyhow::Result` 上抛给 `main`。
pub async fn dispatch(cli: &Cli) -> Result<ExitCode> {
    ensure_kernel(&cli.kernel)?;

    match &cli.command {
        Some(Command::Run { task }) => commands::run::run(cli, task).await,
        Some(Command::Task { command }) => commands::task::run(command),
        Some(Command::Config { command }) => commands::config::run(command),
        Some(Command::Doctor) => commands::doctor::run(),
        // 无子命令 → REPL
        None => repl::run(cli).await,
    }
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
