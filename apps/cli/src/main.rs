//! `ignis` 可执行入口：解析参数、分发命令，把错误打成 `error: {人话}`。

pub mod approval;
pub mod brand;
pub mod cli;
pub mod commands;
pub mod editor;
pub mod input;
pub mod render;
pub mod repl;
// TUI（Beta）整块按平台门控：信号流、termios 与 `TIOCGWINSZ` 都是 unix 的；非 unix 上
// `--tui` 由入口层按「不可用」处理，走同一条降级 note + 经典路径。
#[cfg(unix)]
pub mod tui;

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
