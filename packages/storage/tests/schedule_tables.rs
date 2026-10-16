//! 迁移 003（`jobs` / `job_runs` / `alerts`）的集成测试。
//!
//! 迁移与索引是数据库层面的外部可见行为，因此这里只走公共 API：`Db::open` 验证 `user_version`
//! 与幂等重开，`with_conn` 查 `sqlite_master`/`pragma_index_info` 验证索引，DAO 验证三张表的
//! 往返、排序、去重窗口与清理语义。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ignis_storage::{
    AlertRecord, Db, JobRecord, JobRunRecord, NewAlert, NewJob, NewJobRun, NewTask,
};

/// 唯一临时目录：测试可能并发执行，目录名带进程号与纳秒时间戳。
fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ignis-schedule-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn user_version(db: &Db) -> i64 {
    db.with_conn(|c| c.query_row("PRAGMA user_version", [], |row| row.get(0)))
        .unwrap()
}

/// `sqlite_master` 里的对象是否存在（`table`/`index`）。
fn object_exists(db: &Db, kind: &str, name: &str) -> bool {
    db.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
            [kind, name],
            |row| row.get::<_, i64>(0),
        )
    })
    .unwrap()
        > 0
}

/// 索引的列名，按 `pragma_index_info` 的 `seqno` 升序——顺序即索引的列顺序。
fn index_columns(db: &Db, name: &str) -> Vec<String> {
    db.with_conn(|c| {
        let mut stmt = c.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
        let rows = stmt.query_map([name], |row| row.get::<_, String>(0))?;
        rows.collect()
    })
    .unwrap()
}

/// 表的列名，按 `pragma_table_info` 的 `cid` 升序——顺序即建表时的列顺序。
fn table_columns(db: &Db, table: &str) -> Vec<String> {
    db.with_conn(|c| {
        let mut stmt = c.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
        let rows = stmt.query_map([table], |row| row.get::<_, String>(0))?;
        rows.collect()
    })
    .unwrap()
}

fn job(id: &str) -> NewJob {
    NewJob {
        id: id.into(),
        name: "check disk".into(),
        prompt: format!("prompt {id}"),
        schedule_kind: "interval".into(),
        schedule_expr: "10m".into(),
        kernel: "native".into(),
        profile_json: r#"{"tools":["shell_run"],"risk":"safe"}"#.into(),
        limits_json: r#"{"max_turns":8}"#.into(),
        workspace: "/w".into(),
    }
}

fn job_run(id: &str, job_id: &str, started_at: i64) -> NewJobRun {
    NewJobRun {
        id: id.into(),
        job_id: job_id.into(),
        task_id: None,
        started_at,
    }
}

fn alert(title: &str, dedup_key: &str) -> NewAlert {
    NewAlert {
        severity: "warning".into(),
        title: title.into(),
        body: Some(format!("body {title}")),
        source_job_id: Some("job-1".into()),
        source_task_id: None,
        dedup_key: dedup_key.into(),
    }
}

fn job_ids(jobs: &[JobRecord]) -> Vec<String> {
    jobs.iter().map(|j| j.id.clone()).collect()
}

fn run_ids(runs: &[JobRunRecord]) -> Vec<String> {
    runs.iter().map(|r| r.id.clone()).collect()
}

fn alert_titles(alerts: &[AlertRecord]) -> Vec<String> {
    alerts.iter().map(|a| a.title.clone()).collect()
}

#[test]
fn migration_003_creates_the_schedule_tables_and_bumps_user_version() {
    let dir = temp_dir("schema");
    let path = dir.join("ignis.db");

    let db = Db::open(&path).unwrap();
    // 三条迁移都已应用
    assert_eq!(user_version(&db), 3);
    for table in ["jobs", "job_runs", "alerts"] {
        assert!(object_exists(&db, "table", table), "{table} 缺失");
    }
    // 列与列序与计划的 DDL 逐字对齐（下游 SQL 可能按位置取列）
    assert_eq!(
        table_columns(&db, "jobs"),
        vec![
            "id",
            "name",
            "prompt",
            "schedule_kind",
            "schedule_expr",
            "kernel",
            "profile_json",
            "limits_json",
            "workspace",
            "status",
            "created_at",
            "last_run_at",
        ]
    );
    assert_eq!(
        table_columns(&db, "job_runs"),
        vec![
            "id",
            "job_id",
            "task_id",
            "started_at",
            "ended_at",
            "status",
            "summary",
            "error",
            "input_tokens",
            "output_tokens",
        ]
    );
    assert_eq!(
        table_columns(&db, "alerts"),
        vec![
            "id",
            "severity",
            "title",
            "body",
            "source_job_id",
            "source_task_id",
            "dedup_key",
            "created_at",
            "read_at",
        ]
    );
    // 索引的列与计划的 (job_id, started_at) / (dedup_key, created_at) / (read_at) 对齐
    assert_eq!(
        index_columns(&db, "idx_job_runs_job_started"),
        vec!["job_id", "started_at"]
    );
    assert_eq!(
        index_columns(&db, "idx_alerts_dedup_created"),
        vec!["dedup_key", "created_at"]
    );
    assert_eq!(index_columns(&db, "idx_alerts_read"), vec!["read_at"]);
    drop(db);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// v2 → v3 升级：一个只跑到 002 的库（`user_version = 2`，没有 003 的三张表）打开后应建出
/// `jobs` / `job_runs` / `alerts`、把 `user_version` 推到 3，且 001/002 期间的既有数据（这里：
/// 一条任务行）一条不少。手工回退版本号 + 删表就是「旧版 ignis 写的库」的最小复现。
#[test]
fn a_v2_database_upgrades_to_v3_and_keeps_earlier_rows() {
    let dir = temp_dir("upgrade");
    let path = dir.join("ignis.db");

    // 先开一个最新库，写入 001 时代的任务行，再把 003 的成果抹掉并退回版本号 2
    let db = Db::open(&path).unwrap();
    let task = NewTask {
        id: "task-1".into(),
        kind: "run".into(),
        prompt: "旧版本写下的任务".into(),
        kernel: "native".into(),
        workspace: "/w".into(),
        started_at: 42,
    };
    db.insert_task(&task).unwrap();
    db.with_conn(|c| {
        c.execute_batch(
            "DROP TABLE jobs;
             DROP TABLE job_runs;
             DROP TABLE alerts;
             PRAGMA user_version = 2",
        )
    })
    .unwrap();
    // 前提：003 的三张表确实不在了（否则这条用例什么也没验证）
    for table in ["jobs", "job_runs", "alerts"] {
        assert!(!object_exists(&db, "table", table), "{table} 应已被删掉");
    }
    assert_eq!(user_version(&db), 2);
    drop(db);

    // 再打开：003 重新应用，版本推到 3，旧数据仍在
    let db = Db::open(&path).unwrap();
    assert_eq!(user_version(&db), 3);
    for table in ["jobs", "job_runs", "alerts"] {
        assert!(object_exists(&db, "table", table), "{table} 缺失");
    }
    let row = db.get_task("task-1").unwrap().expect("旧任务行应还在");
    assert_eq!(row.status, "running");
    assert_eq!(row.prompt, "旧版本写下的任务");
    assert_eq!(row.started_at, 42);
    // 新表可用：升级之后 003 的 DAO 能正常读写
    db.insert_job(&job("job-1")).unwrap();
    assert_eq!(job_ids(&db.list_jobs(false).unwrap()), ["job-1"]);
    drop(db);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reopening_a_migrated_database_is_idempotent_and_keeps_schedule_rows() {
    let dir = temp_dir("idempotent");
    let path = dir.join("ignis.db");

    let db = Db::open(&path).unwrap();
    db.insert_job(&job("job-1")).unwrap();
    db.insert_job_run(&job_run("run-1", "job-1", 10)).unwrap();
    db.insert_alert(&alert("磁盘告警", "job-1|磁盘告警"))
        .unwrap();
    drop(db);

    // 第二次打开：迁移是 no-op，三张表的数据都还在
    let db = Db::open(&path).unwrap();
    assert_eq!(user_version(&db), 3);
    assert!(db.get_job("job-1").unwrap().is_some());
    assert_eq!(
        run_ids(&db.list_job_runs(Some("job-1"), 10).unwrap()),
        ["run-1"]
    );
    assert_eq!(
        alert_titles(&db.list_alerts(true, 10).unwrap()),
        ["磁盘告警"]
    );
    drop(db);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn jobs_and_job_runs_round_trip_and_keep_their_ordering() {
    let dir = temp_dir("roundtrip");
    let path = dir.join("ignis.db");
    let db = Db::open(&path).unwrap();

    // jobs：按 created_at 升序；paused 行只在 include_paused 时出现
    db.insert_job(&job("job-1")).unwrap();
    db.insert_job(&job("job-2")).unwrap();
    db.set_job_status("job-2", "paused").unwrap();

    let rec = db.get_job("job-1").unwrap().expect("job row");
    assert_eq!(rec.name, "check disk");
    assert_eq!(rec.prompt, "prompt job-1");
    assert_eq!(rec.schedule_kind, "interval");
    assert_eq!(rec.schedule_expr, "10m");
    assert_eq!(rec.kernel, "native");
    assert_eq!(rec.profile_json, r#"{"tools":["shell_run"],"risk":"safe"}"#);
    assert_eq!(rec.limits_json, r#"{"max_turns":8}"#);
    assert_eq!(rec.workspace, "/w");
    assert_eq!(rec.status, "active");
    assert_eq!(rec.last_run_at, None);
    assert!(rec.created_at > 0);

    assert_eq!(job_ids(&db.list_jobs(false).unwrap()), ["job-1"]);
    assert_eq!(job_ids(&db.list_jobs(true).unwrap()), ["job-1", "job-2"]);
    assert_eq!(db.get_job("job-2").unwrap().unwrap().status, "paused");

    // job_runs：按 started_at 倒序，可按 job 过滤
    db.insert_job_run(&job_run("run-1", "job-1", 10)).unwrap();
    db.insert_job_run(&job_run("run-2", "job-1", 30)).unwrap();
    db.insert_job_run(&job_run("run-3", "job-2", 20)).unwrap();
    db.update_job_run_task_id("run-1", "task-1").unwrap();
    db.finish_job_run("run-2", "success", Some("done"), None, 12, 34, 40)
        .unwrap();

    assert_eq!(
        run_ids(&db.list_job_runs(None, 10).unwrap()),
        ["run-2", "run-3", "run-1"]
    );
    assert_eq!(
        run_ids(&db.list_job_runs(Some("job-1"), 10).unwrap()),
        ["run-2", "run-1"]
    );
    assert_eq!(
        run_ids(&db.list_job_runs(Some("job-1"), 1).unwrap()),
        ["run-2"]
    );

    let runs = db.list_job_runs(Some("job-1"), 10).unwrap();
    // 未收尾的行：running、task_id 由 runner 事后回填
    let running = runs.iter().find(|r| r.id == "run-1").unwrap();
    assert_eq!(running.task_id.as_deref(), Some("task-1"));
    assert_eq!(running.status, "running");
    assert_eq!(running.ended_at, None);
    assert_eq!((running.input_tokens, running.output_tokens), (0, 0));
    // 收尾的行：终态、摘要、用量与结束时间
    let done = runs.iter().find(|r| r.id == "run-2").unwrap();
    assert_eq!(done.status, "success");
    assert_eq!(done.summary.as_deref(), Some("done"));
    assert_eq!(done.error, None);
    assert_eq!(done.ended_at, Some(40));
    assert_eq!((done.input_tokens, done.output_tokens), (12, 34));

    // 记一次调度运行时间
    db.touch_job_last_run("job-1", 1_700_000_000_000).unwrap();
    assert_eq!(
        db.get_job("job-1").unwrap().unwrap().last_run_at,
        Some(1_700_000_000_000)
    );

    // 删除 job 只删 job 行，运行历史留痕
    db.remove_job("job-2").unwrap();
    assert!(db.get_job("job-2").unwrap().is_none());
    assert_eq!(
        run_ids(&db.list_job_runs(Some("job-2"), 10).unwrap()),
        ["run-3"]
    );

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn alerts_round_trip_in_newest_first_order_and_flip_to_read() {
    let dir = temp_dir("alerts");
    let path = dir.join("ignis.db");
    let db = Db::open(&path).unwrap();

    db.insert_alert(&alert("第一次", "job-1|第一次")).unwrap();
    db.insert_alert(&alert("第二次", "job-1|第二次")).unwrap();

    let rows = db.list_alerts(true, 10).unwrap();
    assert_eq!(alert_titles(&rows), ["第二次", "第一次"]);
    let newest = rows.first().unwrap();
    assert_eq!(newest.severity, "warning");
    assert_eq!(newest.body.as_deref(), Some("body 第二次"));
    assert_eq!(newest.source_job_id.as_deref(), Some("job-1"));
    assert_eq!(newest.source_task_id, None);
    assert_eq!(newest.dedup_key, "job-1|第二次");
    assert_eq!(newest.read_at, None);
    assert!(newest.created_at > 0);
    assert_eq!(db.list_alerts(false, 10).unwrap().len(), 2);
    assert_eq!(db.unread_alert_count().unwrap(), 2);

    // 未读一次翻牌，返回翻掉的条数；再调用不再改已读行的时间戳
    assert_eq!(db.mark_alerts_read(1_700_000_000_000).unwrap(), 2);
    assert_eq!(db.mark_alerts_read(1_700_000_001_000).unwrap(), 0);
    assert_eq!(db.unread_alert_count().unwrap(), 0);
    assert!(db.list_alerts(false, 10).unwrap().is_empty());
    let read = db.list_alerts(true, 10).unwrap();
    assert!(read.iter().all(|a| a.read_at == Some(1_700_000_000_000)));

    // 去重查询：窗口内命中最近那条，窗口外 1ms 不命中
    let created_at = newest.created_at;
    assert_eq!(
        db.find_recent_alert("job-1|第二次", 900, created_at)
            .unwrap()
            .map(|a| a.id),
        Some(newest.id)
    );
    assert!(
        db.find_recent_alert("job-1|第二次", 900, created_at + 900_000)
            .unwrap()
            .is_some()
    );
    assert!(
        db.find_recent_alert("job-1|第二次", 900, created_at + 900_001)
            .unwrap()
            .is_none()
    );

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn find_recent_alert_only_hits_inside_the_dedup_window() {
    let db = Db::open_in_memory().unwrap();
    db.insert_alert(&alert("磁盘告警", "k")).unwrap();
    let created_at = db.list_alerts(true, 10).unwrap()[0].created_at;

    // 窗口内（含刚落的行）命中
    let hit = db
        .find_recent_alert("k", 900, created_at)
        .unwrap()
        .expect("窗口内应有命中");
    assert_eq!(hit.title, "磁盘告警");
    // 边界：now - window 恰好等于 created_at 时仍算窗口内
    assert!(
        db.find_recent_alert("k", 900, created_at + 900_000)
            .unwrap()
            .is_some()
    );
    // 去重键不同则互不影响
    assert!(
        db.find_recent_alert("other", 900, created_at)
            .unwrap()
            .is_none()
    );
}

#[test]
fn sweep_stale_job_runs_collects_only_running_rows() {
    let db = Db::open_in_memory().unwrap();
    db.insert_job(&job("job-1")).unwrap();
    db.insert_job_run(&job_run("run-1", "job-1", 10)).unwrap();
    db.insert_job_run(&job_run("run-2", "job-1", 20)).unwrap();
    db.insert_job_run(&job_run("run-3", "job-1", 30)).unwrap();
    db.finish_job_run("run-3", "success", Some("done"), None, 1, 2, 40)
        .unwrap();

    // 只有两行遗留的 running 被收，返回条数
    assert_eq!(db.sweep_stale_job_runs(99).unwrap(), 2);
    // 再扫一遍没有可收的行
    assert_eq!(db.sweep_stale_job_runs(100).unwrap(), 0);

    let runs = db.list_job_runs(Some("job-1"), 10).unwrap();
    let by_id = |id: &str| runs.iter().find(|r| r.id == id).unwrap();
    for id in ["run-1", "run-2"] {
        let row = by_id(id);
        assert_eq!(row.status, "failed", "{id}");
        assert_eq!(row.error.as_deref(), Some("interrupted"), "{id}");
        assert_eq!(row.ended_at, Some(99), "{id}");
    }
    // 已收尾的行一个字段都不动
    let done = by_id("run-3");
    assert_eq!(done.status, "success");
    assert_eq!(done.summary.as_deref(), Some("done"));
    assert_eq!(done.error, None);
    assert_eq!(done.ended_at, Some(40));
}
