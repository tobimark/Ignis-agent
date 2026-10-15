//! 安全：路径沙箱、敏感文件识别、危险命令判定、审批策略与输出脱敏。
//!
//! 这是「安全底线」的唯一入口：文件与 Shell 只能经 [`resolve_in_workspace`] 落到工作区内，
//! 危险命令由 [`is_dangerous_command`] 识别，是否执行一律由 [`Policy`] 决定。

mod dangerous;
mod policy;
mod redact;
mod sandbox;
mod sensitive;

pub use dangerous::is_dangerous_command;
pub use policy::{ApprovalMode, Policy, PolicyDecision};
pub use redact::Redactor;
pub use sandbox::{SecurityError, resolve_in_workspace};
pub use sensitive::is_sensitive;
