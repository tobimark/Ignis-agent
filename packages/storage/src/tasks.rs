use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Db, Result, StorageError};

/// 新建任务行的字段；`status` 由 [`Db::insert_task`] 固定写为 `running`。
#[derive(Debug, Clone)]
pub struct NewTask {
    pub id: String,
    pub kind: String,
    pub prompt: String,
    pub kernel: String,
    pub workspace: String,
    /// epoch 毫秒。
    pub started_at: i64,
}

/// `tasks` 表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub id: String,
    pub kind: String,
    pub prompt: String,
    pub kernel: String,
    pub workspace: String,
    /// `running` / `success` / `failed` / `aborted` / `limit_reached`。
    pub status: String,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usage_estimated: bool,
    /// epoch 毫秒。
    pub started_at: i64,
    /// epoch 毫秒；任务未收尾时为 `None`。
    pub ended_at: Option<i64>,
}

const TASK_COLUMNS: &str = "id, kind, prompt, kernel, workspace, status, summary, error, \
                            input_tokens, output_tokens, usage_estimated, started_at, ended_at";

fn row_to_task(row: &Row<'_>) -> rusqlite::Result<TaskRecord> {
    Ok(TaskRecord {
        id: row.get("id")?,
        kind: row.get("kind")?,
        prompt: row.get("prompt")?,
        kernel: row.get("kernel")?,
        workspace: row.get("workspace")?,
        status: row.get("status")?,
        summary: row.get("summary")?,
        error: row.get("error")?,
        // rusqlite 的 u64 读写需 `fallible_uint` 特性，这里按 i64 存取再转换。
        input_tokens: row.get::<_, i64>("input_tokens")? as u64,
        output_tokens: row.get::<_, i64>("output_tokens")? as u64,
        usage_estimated: row.get("usage_estimated")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
    })
}

impl Db {
    /// 插入一条 `running` 状态的任务行。
    pub fn insert_task(&self, t: &NewTask) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO tasks (id, kind, prompt, kernel, workspace, status, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6)",
                params![t.id, t.kind, t.prompt, t.kernel, t.workspace, t.started_at],
            )?;
            Ok(())
        })
    }

    /// 收尾任务：写入终态、摘要、错误、用量与结束时间。任务行不存在时报错。
    #[allow(clippy::too_many_arguments)]
    pub fn finish_task(
        &self,
        id: &str,
        status: &str,
        summary: Option<&str>,
        error: Option<&str>,
        input_tokens: u64,
        output_tokens: u64,
        usage_estimated: bool,
        ended_at: i64,
    ) -> Result<()> {
        let updated = self.with_conn(|c| {
            c.prepare_cached(
                "UPDATE tasks SET status = ?1, summary = ?2, error = ?3, input_tokens = ?4,
                 output_tokens = ?5, usage_estimated = ?6, ended_at = ?7
                 WHERE id = ?8",
            )?
            .execute(params![
                status,
                summary,
                error,
                input_tokens as i64,
                output_tokens as i64,
                usage_estimated,
                ended_at,
                id,
            ])
        })?;
        if updated == 0 {
            return Err(StorageError::TaskNotFound(id.to_string()));
        }
        Ok(())
    }

    /// 按 id 读取任务行。
    pub fn get_task(&self, id: &str) -> Result<Option<TaskRecord>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1");
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            stmt.query_row(params![id], row_to_task).optional()
        })
    }

    /// 最近的任务行：按 `started_at` 倒序，最多 `limit` 条。
    pub fn list_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>> {
        let sql =
            format!("SELECT {TASK_COLUMNS} FROM tasks ORDER BY started_at DESC, id DESC LIMIT ?1");
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![limit as i64], row_to_task)?;
            rows.collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Db;
    use crate::tasks::{NewTask, StorageError};

    fn new_task(id: &str, started_at: i64) -> NewTask {
        NewTask {
            id: id.into(),
            kind: "run".into(),
            prompt: format!("prompt {id}"),
            kernel: "native".into(),
            workspace: "/w".into(),
            started_at,
        }
    }

    #[test]
    fn insert_task_starts_as_running() {
        let db = Db::open_in_memory().unwrap();
        db.insert_task(&new_task("01", 1)).unwrap();

        let rec = db.get_task("01").unwrap().expect("task row");
        assert_eq!(rec.status, "running");
        assert_eq!(rec.id, "01");
        assert_eq!(rec.kind, "run");
        assert_eq!(rec.prompt, "prompt 01");
        assert_eq!(rec.kernel, "native");
        assert_eq!(rec.workspace, "/w");
        assert_eq!(rec.started_at, 1);
        assert_eq!(rec.summary, None);
        assert_eq!(rec.error, None);
        assert_eq!(rec.input_tokens, 0);
        assert_eq!(rec.output_tokens, 0);
        assert!(!rec.usage_estimated);
        assert_eq!(rec.ended_at, None);
    }

    #[test]
    fn finish_task_records_terminal_state_and_usage() {
        let db = Db::open_in_memory().unwrap();
        db.insert_task(&new_task("01", 1)).unwrap();

        db.finish_task("01", "success", Some("done"), None, 12, 34, false, 99)
            .unwrap();

        let rec = db.get_task("01").unwrap().unwrap();
        assert_eq!(rec.status, "success");
        assert_eq!(rec.summary.as_deref(), Some("done"));
        assert_eq!(rec.error, None);
        assert_eq!(rec.input_tokens, 12);
        assert_eq!(rec.output_tokens, 34);
        assert!(!rec.usage_estimated);
        assert_eq!(rec.ended_at, Some(99));
    }

    #[test]
    fn finish_task_records_error_and_estimated_usage() {
        let db = Db::open_in_memory().unwrap();
        db.insert_task(&new_task("02", 5)).unwrap();

        db.finish_task("02", "failed", None, Some("boom"), 7, 8, true, 42)
            .unwrap();

        let rec = db.get_task("02").unwrap().unwrap();
        assert_eq!(rec.status, "failed");
        assert_eq!(rec.summary, None);
        assert_eq!(rec.error.as_deref(), Some("boom"));
        assert_eq!((rec.input_tokens, rec.output_tokens), (7, 8));
        assert!(rec.usage_estimated);
        assert_eq!(rec.ended_at, Some(42));
    }

    #[test]
    fn terminal_statuses_are_stored_verbatim() {
        let db = Db::open_in_memory().unwrap();
        // RunStatus 的四个终态由上层（Task 12）原样写入，storage 不做校验或规范化。
        for (idx, status) in ["success", "failed", "aborted", "limit_reached"]
            .iter()
            .enumerate()
        {
            let id = format!("0{}", idx + 1);
            db.insert_task(&new_task(&id, idx as i64 + 1)).unwrap();
            db.finish_task(&id, status, None, None, 0, 0, false, 100)
                .unwrap();
            assert_eq!(db.get_task(&id).unwrap().unwrap().status, *status);
        }
    }

    #[test]
    fn get_task_returns_none_for_unknown_id() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.get_task("nope").unwrap().is_none());
    }

    #[test]
    fn finish_task_reports_unknown_id() {
        let db = Db::open_in_memory().unwrap();
        let err = db
            .finish_task("nope", "failed", None, None, 0, 0, false, 1)
            .unwrap_err();
        assert!(matches!(err, StorageError::TaskNotFound(id) if id == "nope"));
    }

    #[test]
    fn list_tasks_returns_newest_first_up_to_limit() {
        let db = Db::open_in_memory().unwrap();
        db.insert_task(&new_task("01", 10)).unwrap();
        db.insert_task(&new_task("02", 30)).unwrap();
        db.insert_task(&new_task("03", 20)).unwrap();

        let all = db.list_tasks(10).unwrap();
        let ids: Vec<&str> = all.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["02", "03", "01"]);

        let limited = db.list_tasks(2).unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].id, "02");
        assert_eq!(limited[1].id, "03");
    }
}
