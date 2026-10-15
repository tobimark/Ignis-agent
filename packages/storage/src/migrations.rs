use rusqlite::Connection;

use crate::db::Result;

/// 迁移脚本：按序执行，`PRAGMA user_version` 记录已应用的条数（自 1 起）。
pub(crate) const MIGRATIONS: &[&str] = &[
    // 001：任务与审批记录表。
    "
CREATE TABLE tasks (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, prompt TEXT NOT NULL, kernel TEXT NOT NULL,
  workspace TEXT NOT NULL, status TEXT NOT NULL, summary TEXT, error TEXT,
  input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
  usage_estimated INTEGER NOT NULL DEFAULT 0,
  started_at INTEGER NOT NULL, ended_at INTEGER
);
CREATE TABLE approvals (
  id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL, tool TEXT NOT NULL,
  summary TEXT NOT NULL, risk TEXT NOT NULL, decision TEXT NOT NULL, mode TEXT NOT NULL,
  decided_at INTEGER NOT NULL
);
CREATE INDEX idx_approvals_task ON approvals(task_id);
",
];

/// 逐条应用未执行的迁移；每条迁移在独立事务中执行并同步推进 `user_version`。
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for (idx, sql) in MIGRATIONS.iter().enumerate().skip(version.max(0) as usize) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        // user_version 存在数据库头部，随本事务一起提交。
        tx.pragma_update(None, "user_version", (idx + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}
