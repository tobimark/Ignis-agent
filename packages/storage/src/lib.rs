//! `ignis-storage`：Ignis 的持久化层。
//!
//! 提供 SQLite 连接（打开即建父目录、开启 WAL/外键/busy_timeout、自动迁移）与 `tasks`、
//! `approvals`、`facts`、`jobs`、`job_runs`、`alerts`、`sessions`、`session_messages`、
//! `session_compactions` 九张表的 DAO，以及 JSONL 任务日志
//! （[`TaskLog`]），供上层（TaskRunner、调度守护、CLI 辅助命令）读写。
//!
//! 跨任务契约的字符串取值（写入 `tasks.status`、`approvals.decision`、`approvals.mode`、
//! `facts.scope`、`facts.status`、`jobs.schedule_kind`、`jobs.status`、`job_runs.status`、
//! `alerts.severity`）：
//! - 状态：`running` / `success` / `failed` / `aborted` / `limit_reached`
//! - 决策：`allow` / `allow_session` / `deny`
//! - 模式：`interactive` / `preauthorized` / `unattended`
//! - 事实范围：`user` / `project` / `global`；事实状态：`active` / `superseded`
//! - 调度方式：`interval` / `cron`；job 状态：`active` / `paused`；运行状态同上「状态」
//! - 告警级别：`info` / `warning` / `critical`

mod alerts;
mod approvals;
mod db;
mod facts;
mod jobs;
mod logs;
mod migrations;
mod sessions;
mod tasks;

pub use alerts::{AlertRecord, NewAlert};
pub use approvals::{ApprovalRecord, NewApproval};
pub use db::{Db, StorageError, now_millis, now_secs};
pub use facts::{FactRecord, NewFact};
pub use jobs::{JobRecord, JobRunRecord, NewJob, NewJobRun};
pub use logs::TaskLog;
pub use sessions::{
    CompactionRecord, NewSession, NewSessionMessage, SessionMessageRecord, SessionRecord,
    SessionSummary,
};
pub use tasks::{NewTask, TaskRecord};
