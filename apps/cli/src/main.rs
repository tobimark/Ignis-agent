//! `ignis` 可执行入口：解析参数、分发命令，把错误打成 `error: {人话}`。

pub mod approval;
pub mod cli;
pub mod commands;
pub mod render;
pub mod repl;

use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();

    match cli::dispatch(&cli).await {
        Ok(code) => code,
        // anyhow 的 Display 只给人话；Debug（错误链/堆栈）不进终端
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
