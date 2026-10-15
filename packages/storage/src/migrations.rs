use rusqlite::Connection;

use crate::db::{Result, StorageError};

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
    // 002：跨会话事实表 + FTS5(trigram) 全文索引。
    //
    // 取值契约（由上层保证，storage 与 `tasks.status` 一样不做校验）：
    // - `scope`：user | project | global；`scope = 'project'` 时 `workspace` 必填（工作区绝对路径）
    // - `status`：active | superseded；被取代的行只留痕，检索/注入/镜像一律只看 active
    // `facts_fts` 是 external content 表（content='facts'），正文不重复存储，索引由下面的三个
    // 触发器同步；索引 idx_facts_scope_workspace_status 服务「按工作区取 active project 事实」。
    "
CREATE TABLE facts (
  id INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL, workspace TEXT, key TEXT NOT NULL,
  text TEXT NOT NULL, status TEXT NOT NULL, superseded_by INTEGER, source_task_id TEXT,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE INDEX idx_facts_scope_workspace_status ON facts(scope, workspace, status);
CREATE VIRTUAL TABLE facts_fts USING fts5(text, content='facts', content_rowid='id', tokenize='trigram');
CREATE TRIGGER facts_ai AFTER INSERT ON facts BEGIN
  INSERT INTO facts_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER facts_ad AFTER DELETE ON facts BEGIN
  INSERT INTO facts_fts(facts_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER facts_au AFTER UPDATE ON facts BEGIN
  INSERT INTO facts_fts(facts_fts, rowid, text) VALUES ('delete', old.id, old.text);
  INSERT INTO facts_fts(rowid, text) VALUES (new.id, new.text);
END;
",
];

/// 逐条应用未执行的迁移；每条迁移在独立事务中执行并同步推进 `user_version`。
///
/// `user_version` 高于 [`MIGRATIONS`] 的条数说明这个库被更新版本的 ignis 迁移过：按旧 schema
/// 读写只会写坏数据，所以显式报错而不是继续。
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > MIGRATIONS.len() as i64 {
        return Err(StorageError::SchemaTooNew {
            found: version,
            supported: MIGRATIONS.len(),
        });
    }
    for (idx, sql) in MIGRATIONS.iter().enumerate().skip(version.max(0) as usize) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        // user_version 存在数据库头部，随本事务一起提交。
        tx.pragma_update(None, "user_version", (idx + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}
