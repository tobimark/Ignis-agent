//! 调度任务 DAO：`jobs` 与 `job_runs` 两张表。
//!
//! `jobs` 存「做什么、按什么节奏、以什么边界执行」，`job_runs` 是每次执行的留痕——一行从
//! `running` 开始（[`Db::insert_job_run`]），由 [`Db::finish_job_run`] 收尾。取值契约见 crate
//! 文档，storage 只存不校验（与 `tasks.status` 一致）。

use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Db, Result, StorageError, now_millis};

/// 新建 job 行的字段；`status` 由 [`Db::insert_job`] 固定写为 `active`，`created_at` 取调用时刻。
#[derive(Debug, Clone)]
pub struct NewJob {
    pub id: String,
    pub name: String,
    pub prompt: String,
    /// `interval` / `cron`。
    pub schedule_kind: String,
    /// interval 的 `<n><s|m|h>`，或 5 字段 cron 表达式。
    pub schedule_expr: String,
    pub kernel: String,
    /// profile 的 serde JSON（工具白名单 + `risk`）。
    pub profile_json: String,
    /// `runtime::Limits` 的 serde JSON。
    pub limits_json: String,
    pub workspace: String,
}

/// `jobs` 表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub id: String,
    pub name: String,
    pub prompt: String,
    /// `interval` / `cron`。
    pub schedule_kind: String,
    pub schedule_expr: String,
    pub kernel: String,
    pub profile_json: String,
    pub limits_json: String,
    pub workspace: String,
    /// `active` / `paused`。
    pub status: String,
    /// epoch 毫秒。
    pub created_at: i64,
    /// 上次调度运行的 epoch 毫秒；从未运行过为 `None`。
    pub last_run_at: Option<i64>,
}

/// 新建运行行的字段；`status` 由 [`Db::insert_job_run`] 固定写为 `running`，`ended_at` 为空。
#[derive(Debug, Clone)]
pub struct NewJobRun {
    pub id: String,
    pub job_id: String,
    /// 本次运行对应的任务 id；任务建好后用 [`Db::update_job_run_task_id`] 回填。
    pub task_id: Option<String>,
    /// epoch 毫秒。
    pub started_at: i64,
}

/// `job_runs` 表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRunRecord {
    pub id: String,
    pub job_id: String,
    pub task_id: Option<String>,
    /// epoch 毫秒。
    pub started_at: i64,
    /// epoch 毫秒；运行未收尾时为 `None`。
    pub ended_at: Option<i64>,
    /// `running` / `success` / `failed` / `aborted` / `limit_reached`。
    pub status: String,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

const JOB_COLUMNS: &str = "id, name, prompt, schedule_kind, schedule_expr, kernel, profile_json, \
                           limits_json, workspace, status, created_at, last_run_at";
const JOB_RUN_COLUMNS: &str = "id, job_id, task_id, started_at, ended_at, status, summary, error, \
                               input_tokens, output_tokens";

fn row_to_job(row: &Row<'_>) -> rusqlite::Result<JobRecord> {
    Ok(JobRecord {
        id: row.get("id")?,
        name: row.get("name")?,
        prompt: row.get("prompt")?,
        schedule_kind: row.get("schedule_kind")?,
        schedule_expr: row.get("schedule_expr")?,
        kernel: row.get("kernel")?,
        profile_json: row.get("profile_json")?,
        limits_json: row.get("limits_json")?,
        workspace: row.get("workspace")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        last_run_at: row.get("last_run_at")?,
    })
}

fn row_to_job_run(row: &Row<'_>) -> rusqlite::Result<JobRunRecord> {
    Ok(JobRunRecord {
        id: row.get("id")?,
        job_id: row.get("job_id")?,
        task_id: row.get("task_id")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        status: row.get("status")?,
        summary: row.get("summary")?,
        error: row.get("error")?,
        // rusqlite 的 u64 读写需 `fallible_uint` 特性，这里按 i64 存取再转换。
        input_tokens: row.get::<_, i64>("input_tokens")? as u64,
        output_tokens: row.get::<_, i64>("output_tokens")? as u64,
    })
}

impl Db {
    /// 插入一条 `active` 状态的 job 行。
    pub fn insert_job(&self, j: &NewJob) -> Result<()> {
        let now = now_millis();
        self.with_conn(|c| {
            c.prepare_cached(
                "INSERT INTO jobs (id, name, prompt, schedule_kind, schedule_expr, kernel,
                 profile_json, limits_json, workspace, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', ?10)",
            )?
            .execute(params![
                j.id,
                j.name,
                j.prompt,
                j.schedule_kind,
                j.schedule_expr,
                j.kernel,
                j.profile_json,
                j.limits_json,
                j.workspace,
                now
            ])?;
            Ok(())
        })
    }

    /// 按 id 读取 job 行。
    pub fn get_job(&self, id: &str) -> Result<Option<JobRecord>> {
        let sql = format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1");
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            stmt.query_row(params![id], row_to_job).optional()
        })
    }

    /// 全部 job 按创建先后（`created_at` 升序，同刻按 id 升序）排列；`include_paused = false`
    /// 时只回 `active` 行。
    pub fn list_jobs(&self, include_paused: bool) -> Result<Vec<JobRecord>> {
        let filter = if include_paused {
            ""
        } else {
            "WHERE status = 'active'"
        };
        let sql =
            format!("SELECT {JOB_COLUMNS} FROM jobs {filter} ORDER BY created_at ASC, id ASC");
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map([], row_to_job)?;
            rows.collect()
        })
    }

    /// 改写 job 状态（`active` / `paused`）。行不存在时报错。
    pub fn set_job_status(&self, id: &str, status: &str) -> Result<()> {
        let updated = self.with_conn(|c| {
            c.prepare_cached("UPDATE jobs SET status = ?2 WHERE id = ?1")?
                .execute(params![id, status])
        })?;
        if updated == 0 {
            return Err(StorageError::JobNotFound(id.to_string()));
        }
        Ok(())
    }

    /// 物理删除 job 行；该 job 的历史 `job_runs` 留痕不动（两张表之间没有外键）。行不存在时报错。
    pub fn remove_job(&self, id: &str) -> Result<()> {
        let deleted = self.with_conn(|c| {
            c.prepare_cached("DELETE FROM jobs WHERE id = ?1")?
                .execute(params![id])
        })?;
        if deleted == 0 {
            return Err(StorageError::JobNotFound(id.to_string()));
        }
        Ok(())
    }

    /// 记一次调度运行的开始时刻（`last_run_at`）。行不存在时报错。
    pub fn touch_job_last_run(&self, id: &str, at: i64) -> Result<()> {
        let updated = self.with_conn(|c| {
            c.prepare_cached("UPDATE jobs SET last_run_at = ?2 WHERE id = ?1")?
                .execute(params![id, at])
        })?;
        if updated == 0 {
            return Err(StorageError::JobNotFound(id.to_string()));
        }
        Ok(())
    }

    /// 插入一条 `running` 状态的运行行（`ended_at` 为空、用量为 0）。
    pub fn insert_job_run(&self, r: &NewJobRun) -> Result<()> {
        self.with_conn(|c| {
            c.prepare_cached(
                "INSERT INTO job_runs (id, job_id, task_id, started_at, status)
                 VALUES (?1, ?2, ?3, ?4, 'running')",
            )?
            .execute(params![r.id, r.job_id, r.task_id, r.started_at])?;
            Ok(())
        })
    }

    /// 收尾运行行：写入终态、摘要、错误、用量与结束时间。行不存在时报错。
    #[allow(clippy::too_many_arguments)]
    pub fn finish_job_run(
        &self,
        id: &str,
        status: &str,
        summary: Option<&str>,
        error: Option<&str>,
        input_tokens: u64,
        output_tokens: u64,
        ended_at: i64,
    ) -> Result<()> {
        let updated = self.with_conn(|c| {
            c.prepare_cached(
                "UPDATE job_runs SET status = ?1, summary = ?2, error = ?3, input_tokens = ?4,
                 output_tokens = ?5, ended_at = ?6 WHERE id = ?7",
            )?
            .execute(params![
                status,
                summary,
                error,
                input_tokens as i64,
                output_tokens as i64,
                ended_at,
                id
            ])
        })?;
        if updated == 0 {
            return Err(StorageError::JobRunNotFound(id.to_string()));
        }
        Ok(())
    }

    /// 回填运行行对应的任务 id（任务建好后调用）。行不存在时报错。
    pub fn update_job_run_task_id(&self, id: &str, task_id: &str) -> Result<()> {
        let updated = self.with_conn(|c| {
            c.prepare_cached("UPDATE job_runs SET task_id = ?2 WHERE id = ?1")?
                .execute(params![id, task_id])
        })?;
        if updated == 0 {
            return Err(StorageError::JobRunNotFound(id.to_string()));
        }
        Ok(())
    }

    /// 运行行：`job_id` 为 `None` 时回全部，否则只回该 job 的行；按 `started_at` 倒序（同刻按
    /// id 倒序），最多 `limit` 条。
    pub fn list_job_runs(&self, job_id: Option<&str>, limit: usize) -> Result<Vec<JobRunRecord>> {
        let limit = limit as i64;
        let sql = if job_id.is_some() {
            format!(
                "SELECT {JOB_RUN_COLUMNS} FROM job_runs WHERE job_id = ?1
                 ORDER BY started_at DESC, id DESC LIMIT ?2"
            )
        } else {
            format!(
                "SELECT {JOB_RUN_COLUMNS} FROM job_runs ORDER BY started_at DESC, id DESC LIMIT ?1"
            )
        };
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = match job_id {
                Some(job_id) => stmt.query_map(params![job_id, limit], row_to_job_run)?,
                None => stmt.query_map(params![limit], row_to_job_run)?,
            };
            rows.collect()
        })
    }

    /// 把遗留的 `running` 行收成 `failed` / `error = 'interrupted'`（守护进程启动时清场，
    /// 防重启重复执行与悬挂）；`ended_at` 写 `now`，返回收掉的条数。已收尾的行不动。
    pub fn sweep_stale_job_runs(&self, now: i64) -> Result<usize> {
        let swept = self.with_conn(|c| {
            c.prepare_cached(
                "UPDATE job_runs SET ended_at = ?1, status = 'failed', error = 'interrupted'
                 WHERE status = 'running'",
            )?
            .execute(params![now])
        })?;
        Ok(swept)
    }
}

#[cfg(test)]
mod tests {
    use crate::db::{Db, StorageError, now_millis};
    use crate::jobs::{JobRecord, JobRunRecord, NewJob, NewJobRun};

    fn new_job(id: &str) -> NewJob {
        NewJob {
            id: id.into(),
            name: format!("job {id}"),
            prompt: format!("prompt {id}"),
            schedule_kind: "cron".into(),
            schedule_expr: "0 9 * * 1-5".into(),
            kernel: "native".into(),
            profile_json: r#"{"tools":["shell_run"],"risk":"elevated"}"#.into(),
            limits_json: r#"{"max_turns":8}"#.into(),
            workspace: "/w".into(),
        }
    }

    fn new_run(id: &str, job_id: &str, started_at: i64) -> NewJobRun {
        NewJobRun {
            id: id.into(),
            job_id: job_id.into(),
            task_id: None,
            started_at,
        }
    }

    fn job_ids(jobs: &[JobRecord]) -> Vec<String> {
        jobs.iter().map(|j| j.id.clone()).collect()
    }

    fn run_ids(runs: &[JobRunRecord]) -> Vec<String> {
        runs.iter().map(|r| r.id.clone()).collect()
    }

    #[test]
    fn insert_job_roundtrips_as_an_active_row() {
        let db = Db::open_in_memory().unwrap();
        let before = now_millis();
        db.insert_job(&new_job("01")).unwrap();
        let after = now_millis();

        let rec = db.get_job("01").unwrap().expect("job row");
        assert_eq!(rec.id, "01");
        assert_eq!(rec.name, "job 01");
        assert_eq!(rec.prompt, "prompt 01");
        assert_eq!(rec.schedule_kind, "cron");
        assert_eq!(rec.schedule_expr, "0 9 * * 1-5");
        assert_eq!(rec.kernel, "native");
        assert_eq!(
            rec.profile_json,
            r#"{"tools":["shell_run"],"risk":"elevated"}"#
        );
        assert_eq!(rec.limits_json, r#"{"max_turns":8}"#);
        assert_eq!(rec.workspace, "/w");
        assert_eq!(rec.status, "active");
        assert!(
            rec.created_at >= before && rec.created_at <= after,
            "created_at = {} 不在 [{before}, {after}] 内",
            rec.created_at
        );
        assert_eq!(rec.last_run_at, None);

        assert!(db.get_job("nope").unwrap().is_none());
    }

    #[test]
    fn list_jobs_orders_by_created_at_and_can_hide_paused() {
        let db = Db::open_in_memory().unwrap();
        for id in ["01", "02", "03"] {
            db.insert_job(&new_job(id)).unwrap();
        }
        // 三行通常落在同一毫秒，同刻按 id 升序；直接改写 created_at 证明确实按它排序
        assert_eq!(job_ids(&db.list_jobs(true).unwrap()), ["01", "02", "03"]);
        db.with_conn(|c| {
            c.execute_batch(
                "UPDATE jobs SET created_at = 20 WHERE id = '01';
                 UPDATE jobs SET created_at = 10 WHERE id = '02';",
            )
        })
        .unwrap();
        assert_eq!(job_ids(&db.list_jobs(true).unwrap()), ["02", "01", "03"]);

        db.set_job_status("01", "paused").unwrap();
        assert_eq!(job_ids(&db.list_jobs(false).unwrap()), ["02", "03"]);
        assert_eq!(db.list_jobs(true).unwrap().len(), 3);
    }

    #[test]
    fn set_job_status_and_touch_last_run_persist() {
        let db = Db::open_in_memory().unwrap();
        db.insert_job(&new_job("01")).unwrap();

        db.set_job_status("01", "paused").unwrap();
        assert_eq!(db.get_job("01").unwrap().unwrap().status, "paused");
        db.set_job_status("01", "active").unwrap();
        assert_eq!(db.get_job("01").unwrap().unwrap().status, "active");

        db.touch_job_last_run("01", 1_700_000_000_000).unwrap();
        let rec = db.get_job("01").unwrap().unwrap();
        assert_eq!(rec.last_run_at, Some(1_700_000_000_000));
        // 只动 last_run_at
        assert_eq!(rec.status, "active");
        assert_eq!(rec.name, "job 01");
    }

    #[test]
    fn job_mutations_report_unknown_ids() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            matches!(db.set_job_status("nope", "paused").unwrap_err(), StorageError::JobNotFound(id) if id == "nope")
        );
        assert!(
            matches!(db.touch_job_last_run("nope", 1).unwrap_err(), StorageError::JobNotFound(id) if id == "nope")
        );
        assert!(
            matches!(db.remove_job("nope").unwrap_err(), StorageError::JobNotFound(id) if id == "nope")
        );

        // 行在时是普通的 Ok；删掉之后再动它就报 not found
        db.insert_job(&new_job("01")).unwrap();
        db.remove_job("01").unwrap();
        assert!(
            matches!(db.set_job_status("01", "active").unwrap_err(), StorageError::JobNotFound(id) if id == "01")
        );
    }

    #[test]
    fn remove_job_keeps_the_run_history() {
        let db = Db::open_in_memory().unwrap();
        db.insert_job(&new_job("01")).unwrap();
        db.insert_job_run(&new_run("r1", "01", 1)).unwrap();

        db.remove_job("01").unwrap();

        assert!(db.get_job("01").unwrap().is_none());
        assert_eq!(run_ids(&db.list_job_runs(Some("01"), 10).unwrap()), ["r1"]);
    }

    #[test]
    fn insert_job_run_starts_as_running() {
        let db = Db::open_in_memory().unwrap();
        db.insert_job(&new_job("01")).unwrap();
        db.insert_job_run(&new_run("r1", "01", 42)).unwrap();

        let rec = &db.list_job_runs(Some("01"), 10).unwrap()[0];
        assert_eq!(rec.id, "r1");
        assert_eq!(rec.job_id, "01");
        assert_eq!(rec.task_id, None);
        assert_eq!(rec.started_at, 42);
        assert_eq!(rec.ended_at, None);
        assert_eq!(rec.status, "running");
        assert_eq!(rec.summary, None);
        assert_eq!(rec.error, None);
        assert_eq!((rec.input_tokens, rec.output_tokens), (0, 0));
    }

    #[test]
    fn finish_job_run_records_terminal_state_and_usage() {
        let db = Db::open_in_memory().unwrap();
        db.insert_job(&new_job("01")).unwrap();
        db.insert_job_run(&new_run("r1", "01", 1)).unwrap();

        db.finish_job_run("r1", "failed", None, Some("boom"), 7, 8, 99)
            .unwrap();

        let rec = &db.list_job_runs(Some("01"), 10).unwrap()[0];
        assert_eq!(rec.status, "failed");
        assert_eq!(rec.summary, None);
        assert_eq!(rec.error.as_deref(), Some("boom"));
        assert_eq!((rec.input_tokens, rec.output_tokens), (7, 8));
        assert_eq!(rec.ended_at, Some(99));
        assert_eq!(rec.started_at, 1);
    }

    #[test]
    fn update_job_run_task_id_backfills_after_the_fact() {
        let db = Db::open_in_memory().unwrap();
        db.insert_job_run(&new_run("r1", "01", 1)).unwrap();

        db.update_job_run_task_id("r1", "task-7").unwrap();

        let rec = &db.list_job_runs(Some("01"), 10).unwrap()[0];
        assert_eq!(rec.task_id.as_deref(), Some("task-7"));
        // 只动 task_id
        assert_eq!(rec.status, "running");
        assert_eq!(rec.ended_at, None);
    }

    #[test]
    fn run_mutations_report_unknown_ids() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            matches!(db.finish_job_run("nope", "failed", None, None, 0, 0, 1).unwrap_err(), StorageError::JobRunNotFound(id) if id == "nope")
        );
        assert!(
            matches!(db.update_job_run_task_id("nope", "t1").unwrap_err(), StorageError::JobRunNotFound(id) if id == "nope")
        );
    }

    #[test]
    fn list_job_runs_filters_orders_and_limits() {
        let db = Db::open_in_memory().unwrap();
        db.insert_job_run(&new_run("r1", "01", 10)).unwrap();
        db.insert_job_run(&new_run("r2", "01", 30)).unwrap();
        db.insert_job_run(&new_run("r3", "02", 20)).unwrap();
        // 同一起始时刻按 id 倒序
        db.insert_job_run(&new_run("r4", "01", 30)).unwrap();

        assert_eq!(
            run_ids(&db.list_job_runs(None, 10).unwrap()),
            ["r4", "r2", "r3", "r1"]
        );
        assert_eq!(
            run_ids(&db.list_job_runs(Some("01"), 10).unwrap()),
            ["r4", "r2", "r1"]
        );
        assert_eq!(
            run_ids(&db.list_job_runs(Some("01"), 2).unwrap()),
            ["r4", "r2"]
        );
        assert!(db.list_job_runs(Some("nope"), 10).unwrap().is_empty());
    }

    #[test]
    fn sweep_stale_job_runs_only_collects_running_rows() {
        let db = Db::open_in_memory().unwrap();
        db.insert_job(&new_job("01")).unwrap();
        db.insert_job_run(&new_run("r1", "01", 10)).unwrap();
        db.insert_job_run(&new_run("r2", "01", 20)).unwrap();
        db.insert_job_run(&new_run("r3", "01", 30)).unwrap();
        db.finish_job_run("r3", "aborted", Some("已中止"), None, 0, 0, 35)
            .unwrap();

        // 只收 running 的两行，返回条数；再扫一遍无事可做
        assert_eq!(db.sweep_stale_job_runs(100).unwrap(), 2);
        assert_eq!(db.sweep_stale_job_runs(200).unwrap(), 0);

        let runs = db.list_job_runs(Some("01"), 10).unwrap();
        let by_id = |id: &str| runs.iter().find(|r| r.id == id).unwrap();
        for id in ["r1", "r2"] {
            let rec = by_id(id);
            assert_eq!(rec.status, "failed", "{id}");
            assert_eq!(rec.error.as_deref(), Some("interrupted"), "{id}");
            assert_eq!(rec.ended_at, Some(100), "{id}");
        }
        // 已收尾的行一个字段都不动
        let done = by_id("r3");
        assert_eq!(done.status, "aborted");
        assert_eq!(done.summary.as_deref(), Some("已中止"));
        assert_eq!(done.error, None);
        assert_eq!(done.ended_at, Some(35));
    }
}
