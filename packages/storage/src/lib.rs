//! `ignis-storage`：Ignis 的持久化层。
//!
//! 提供 SQLite 连接（打开即建父目录、开启 WAL/外键、自动迁移）与 `tasks`、`approvals`
//! 两张表的 DAO，供上层（TaskRunner、CLI 辅助命令）读写。
//!
//! 跨任务契约的字符串取值（写入 `tasks.status`、`approvals.decision`、`approvals.mode`）：
//! - 状态：`running` / `success` / `failed` / `aborted` / `limit_reached`
//! - 决策：`allow` / `allow_session` / `deny`
//! - 模式：`interactive` / `preauthorized` / `unattended`

mod approvals;
mod db;
mod migrations;
mod tasks;

pub use approvals::{ApprovalRecord, NewApproval};
pub use db::{Db, StorageError};
pub use tasks::{NewTask, TaskRecord};
