//! `ignis run <task>`：一次性执行任务，按运行状态给退出码。

use std::process::ExitCode;

use anyhow::Result;
use ignis_core::task::TaskKind;

use crate::cli::{Cli, exit_code_for};
use crate::commands::{Session, kernel_note};

/// 执行一次任务（无历史轮次）；结束行由 [`Session::run_task`] 打印。
///
/// 内核失败、触限与用户中止都不是 CLI 错误：它们照常返回 `Ok`，只是退出码不同。
pub async fn run(cli: &Cli, task: &str) -> Result<ExitCode> {
    let mut session = Session::new(cli)?;
    // 选定非 native 内核时先把能力边界说清楚（工具、记忆写、用量各少什么）
    if let Some(note) = kernel_note(session.kernel()) {
        println!("{note}");
    }
    let outcome = session
        .run_task(task.to_string(), Vec::new(), TaskKind::Run)
        .await?;

    Ok(exit_code_for(outcome.status))
}
