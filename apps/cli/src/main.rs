// 命令层（`ignis run` 与 REPL）在 Task 14 接入；本任务先落地可单测的渲染器与审批 UI。
pub mod approval;
pub mod render;

fn main() {
    println!("ignis v{}", env!("CARGO_PKG_VERSION"));
}
