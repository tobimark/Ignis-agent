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
    /// 这条任务所属的会话（REPL 内提交时给）；`ignis run` 与调度运行为 `None`。
    pub session_id: Option<String>,
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
    /// 这条任务所属的会话（迁移 004 加的列）；`ignis run` 与调度运行为 `None`。
    pub session_id: Option<String>,
    /// 本轮用到的技能名（迁移 004 加的列，逗号分隔；`set_task_skills_used` 写，没记过为 `None`）。
    pub skills_used: Option<String>,
}

const TASK_COLUMNS: &str = "id, kind, prompt, kernel, workspace, status, summary, error, \
                            input_tokens, output_tokens, usage_estimated, started_at, ended_at, \
                            session_id, skills_used";

/// 把 `prefix` 里的 LIKE 元字符转义成字面（配合 `ESCAPE '\'`）。
///
/// 反斜杠必须**先**转，否则 `\%` 会被再转一次变成 `\\%`（仍旧匹配一个字面 `%`，但多一层转义）。
/// 任务 id 与会话 id 的前缀查询共用这一份（见 [`Db::task_ids_with_prefix`] /
/// [`Db::session_ids_with_prefix`]）。
pub(crate) fn escape_like(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len());
    for c in prefix.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

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
        session_id: row.get("session_id")?,
        skills_used: row.get("skills_used")?,
    })
}

impl Db {
    /// 插入一条 `running` 状态的任务行；`session_id` 为 `None` 表示不绑会话。
    pub fn insert_task(&self, t: &NewTask) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO tasks (id, kind, prompt, kernel, workspace, status, started_at,
                 session_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7)",
                params![
                    t.id,
                    t.kind,
                    t.prompt,
                    t.kernel,
                    t.workspace,
                    t.started_at,
                    t.session_id
                ],
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

    /// 记录这条任务用到的技能名（逗号分隔落库）；空表写 NULL。任务行不存在时报错。
    ///
    /// 与 [`Db::finish_task`] 分开：finish 的形状是 P1 钉死的，会话路径只补这一列（`task show`
    /// 与 `/tasks` 展示用）。
    pub fn set_task_skills_used(&self, id: &str, skills_used: &[String]) -> Result<()> {
        // 空表落 NULL 而不是空串：展示面靠 `None` 判「没记」
        let joined = if skills_used.is_empty() {
            None
        } else {
            Some(skills_used.join(","))
        };
        let updated = self.with_conn(|c| {
            c.prepare_cached("UPDATE tasks SET skills_used = ?1 WHERE id = ?2")?
                .execute(params![joined, id])
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

    /// 本工作区最近一条**成功**任务（`/learn` 的默认目标）：
    /// `WHERE workspace = ?1 AND status = 'success' ORDER BY started_at DESC, id DESC LIMIT 1`。
    ///
    /// 只读：没有成功任务（含空库）给 `None`，由命令层给「没有可学的任务」的人话错误。用
    /// 「拉 N 条再过滤」会在别的任务更密的工作区漏掉目标，故这里单独一条 SQL。
    pub fn latest_successful_task_for_workspace(
        &self,
        workspace: &str,
    ) -> Result<Option<TaskRecord>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM tasks WHERE workspace = ?1 AND status = 'success'
             ORDER BY started_at DESC, id DESC LIMIT 1"
        );
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            stmt.query_row(params![workspace], row_to_task).optional()
        })
    }

    /// 按前缀查任务 id（`task show <前缀>` 的解析用）：以 `prefix` 开头的 id，按 `started_at`
    /// 倒序（与 [`Db::list_tasks`] 同一口径）最多 `limit` 条。
    ///
    /// `prefix` 里的 LIKE 元字符（`\` `%` `_`）按字面处理：调用方虽然只放 `[0-9a-f-]` 过来，
    /// DAO 自身不该留一个「传 `%` 就查全表」的口子。空前缀等价于「所有 id」——准入闸门在命令层。
    pub fn task_ids_with_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        let pattern = format!("{}%", escape_like(prefix));
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id FROM tasks WHERE id LIKE ?1 ESCAPE '\\'
                 ORDER BY started_at DESC, id DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![pattern, limit as i64], |row| row.get(0))?;
            rows.collect()
        })
    }

    /// 把遗留的 `kind = 'schedule'` 且 `status = 'running'` 的任务行收成 `failed` /
    /// `error = 'interrupted'`（`ended_at` 写 `now`），返回收掉的条数。
    ///
    /// 只有守护进程在启动清场时调用：`kind = 'schedule'` 只有它写，pidfile 上的独占锁又保证同一个
    /// home 只有一个守护进程——所以这里收掉的每一行都是「上一个守护进程被 SIGKILL / 崩溃时留下的」
    /// 调度任务（`job_runs` 由 [`Db::sweep_stale_job_runs`] 收，这一行不随着一起收就会永远挂着
    /// `running`）。`run` / `repl` 的任务行不归它管：那两类的收尾在各自的进程里。
    pub fn sweep_stale_scheduled_tasks(&self, now: i64) -> Result<usize> {
        let swept = self.with_conn(|c| {
            c.prepare_cached(
                "UPDATE tasks SET status = 'failed', error = 'interrupted', ended_at = ?1
                 WHERE kind = 'schedule' AND status = 'running'",
            )?
            .execute(params![now])
        })?;
        Ok(swept)
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
            session_id: None,
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
        // 迁移 004 的两个新列：没给会话就是不绑（`ignis run` 的口径），技能名还没记
        assert_eq!(rec.session_id, None);
        assert_eq!(rec.skills_used, None);
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

    /// 按前缀查 id：命中多条（新近在前）、按 `limit` 截断、不命中给空表；LIKE 元字符按字面。
    #[test]
    fn task_ids_with_prefix_matches_literally_up_to_the_limit() {
        let db = Db::open_in_memory().unwrap();
        for (id, started) in [
            ("019a1b2c-3d4e-7f00-8000-000000000001", 10),
            ("019a1b2c-3d4e-7f00-8000-000000000002", 20),
            ("019a1b2c-ffff-7f00-8000-000000000003", 30),
        ] {
            db.insert_task(&new_task(id, started)).unwrap();
        }

        let all = db.task_ids_with_prefix("019a1b2c", 10).unwrap();
        assert_eq!(all.len(), 3);
        // 新近在前：与 `list_tasks` 同一口径
        assert_eq!(all[0], "019a1b2c-ffff-7f00-8000-000000000003");
        assert_eq!(all[2], "019a1b2c-3d4e-7f00-8000-000000000001");

        let limited = db.task_ids_with_prefix("019a1b2c-3d4e", 1).unwrap();
        assert_eq!(limited, ["019a1b2c-3d4e-7f00-8000-000000000002"]);

        assert!(db.task_ids_with_prefix("ffff", 10).unwrap().is_empty());
        // 空前缀等价于「所有 id」（调用方的准入闸门不给空串，DAO 不做二次猜测）
        assert_eq!(db.task_ids_with_prefix("", 10).unwrap().len(), 3);

        // LIKE 元字符按字面：`_` 不该变成「任意一个字符」，`%` 不该变成「任意后缀」
        db.insert_task(&new_task("01_", 40)).unwrap();
        db.insert_task(&new_task("01x", 50)).unwrap();
        assert_eq!(db.task_ids_with_prefix("01_", 10).unwrap(), ["01_"]);
        assert_eq!(db.task_ids_with_prefix("01x", 10).unwrap(), ["01x"]);
        assert!(db.task_ids_with_prefix("%", 10).unwrap().is_empty());
    }

    /// `/learn` 的默认目标：只认本工作区的成功任务，跨工作区与非成功状态都不选；一条成功都不剩时
    /// 给 `None`（每条终态都用 `finish_task` 写）。
    #[test]
    fn latest_successful_task_for_workspace_is_scoped_and_skips_non_success() {
        let db = Db::open_in_memory().unwrap();
        // 本工作区：running、成功、以及更晚的中止任务
        db.insert_task(&new_task("01", 10)).unwrap();
        db.insert_task(&new_task("02", 20)).unwrap();
        db.finish_task("02", "success", Some("done"), None, 1, 2, false, 30)
            .unwrap();
        db.insert_task(&new_task("03", 40)).unwrap();
        db.finish_task("03", "aborted", None, None, 0, 0, false, 50)
            .unwrap();
        // 另一个工作区里更晚的一条成功：不影响本工作区
        let mut other = new_task("04", 60);
        other.workspace = "/other".into();
        db.insert_task(&other).unwrap();
        db.finish_task("04", "success", None, None, 0, 0, false, 70)
            .unwrap();

        let found = db
            .latest_successful_task_for_workspace("/w")
            .unwrap()
            .expect("本工作区有一条成功任务");
        assert_eq!(found.id, "02");
        assert_eq!(found.status, "success");
        assert_eq!(found.workspace, "/w");
        // 落库面与 `get_task` 同一份列：字段逐个还原
        assert_eq!(found, db.get_task("02").unwrap().unwrap());

        // 唯一那条成功改成中止：本工作区没有可学的任务
        db.finish_task("02", "aborted", None, None, 0, 0, false, 80)
            .unwrap();
        assert!(
            db.latest_successful_task_for_workspace("/w")
                .unwrap()
                .is_none(),
            "非成功状态一条都不选"
        );
        // 查询确实按工作区隔离：另一个工作区照旧查到它自己的成功任务
        assert_eq!(
            db.latest_successful_task_for_workspace("/other")
                .unwrap()
                .unwrap()
                .id,
            "04"
        );
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

    /// 启动清场只收 `kind = 'schedule'` 的 `running` 行：`run` / `repl` 的活行与已经收尾的行
    /// 一个字段都不动。
    #[test]
    fn sweep_stale_scheduled_tasks_collects_only_running_schedule_rows() {
        let db = Db::open_in_memory().unwrap();
        // 上一个守护进程被 SIGKILL 留下的调度任务：运行行被别的 sweep 收了，这一行还挂着
        let mut stale = new_task("01", 10);
        stale.kind = "schedule".into();
        db.insert_task(&stale).unwrap();
        // 交互运行（kind=run）的活行：不归它管
        db.insert_task(&new_task("02", 20)).unwrap();
        // 已经收尾的调度任务：不动
        let mut done = new_task("03", 30);
        done.kind = "schedule".into();
        db.insert_task(&done).unwrap();
        db.finish_task("03", "success", Some("done"), None, 1, 2, false, 40)
            .unwrap();

        assert_eq!(db.sweep_stale_scheduled_tasks(99).unwrap(), 1);
        // 再扫一遍没有可收的行
        assert_eq!(db.sweep_stale_scheduled_tasks(100).unwrap(), 0);

        let swept = db.get_task("01").unwrap().unwrap();
        assert_eq!(swept.status, "failed");
        assert_eq!(swept.error.as_deref(), Some("interrupted"));
        assert_eq!(swept.ended_at, Some(99));

        let untouched = db.get_task("02").unwrap().unwrap();
        assert_eq!(untouched.status, "running");
        assert_eq!(untouched.ended_at, None);

        let finished = db.get_task("03").unwrap().unwrap();
        assert_eq!(finished.status, "success");
        assert_eq!(finished.summary.as_deref(), Some("done"));
        assert_eq!(finished.ended_at, Some(40));
    }
}
