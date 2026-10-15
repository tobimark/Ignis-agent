//! `ignis schedule list|pause|resume|remove|history`：调度任务的命令行入口。
//!
//! 这一组命令只碰 `jobs` / `job_runs` 两张表：暂停与恢复改 `jobs.status`，删除是物理删行
//! （历史运行留痕不动，`history` 因此对已删任务照常可查）。`list` 的 next due 用守护进程同一份
//! [`next_due`] 现算——两边不会各算一套「下次什么时候跑」。

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use ignis_core::config::{ensure_home, ignis_home};
use ignis_core::scheduler::{NEVER_DUE, next_due, parse_schedule};
use ignis_storage::{Db, JobRecord, JobRunRecord};

use super::now_secs;
use super::task::{format_time, one_line, render_table, truncate};
use crate::cli::ScheduleCommand;

/// `history` 里摘要/错误列的字符上限：与 `memory list` 的 text 列同一量级。
const SUMMARY_CHARS: usize = 60;

/// `ignis schedule`：列名单、暂停/恢复/删除与看运行历史。
pub fn run(command: &ScheduleCommand) -> Result<ExitCode> {
    let home = ignis_home();
    let out = match command {
        ScheduleCommand::List => list_at(&home)?,
        ScheduleCommand::Pause { id } => set_status_at(&home, id, Toggle::Pause)?,
        ScheduleCommand::Resume { id } => set_status_at(&home, id, Toggle::Resume)?,
        ScheduleCommand::Remove { id } => remove_at(&home, id)?,
        ScheduleCommand::History { id, limit } => history_at(&home, id.as_deref(), *limit)?,
    };
    print!("{out}");
    Ok(ExitCode::SUCCESS)
}

/// 打开调度库；数据目录先落地（新建时 0700，与其它子命令同一口径）。
fn db_at(home: &Path) -> Result<Db> {
    ensure_home(home)?;
    Ok(Db::open(&home.join("ignis.db"))?)
}

/// 暂停/恢复的目标。
#[derive(Debug, Clone, Copy)]
enum Toggle {
    Pause,
    Resume,
}

impl Toggle {
    /// 落库的 `jobs.status` 取值。
    fn status(self) -> &'static str {
        match self {
            Toggle::Pause => "paused",
            Toggle::Resume => "active",
        }
    }

    /// 回执里的动词。
    fn verb(self) -> &'static str {
        match self {
            Toggle::Pause => "paused",
            Toggle::Resume => "resumed",
        }
    }
}

/// 名单（含暂停的）：`ID  NAME  STATUS  KIND/EXPR  LAST RUN  NEXT DUE`。
fn list_at(home: &Path) -> Result<String> {
    let jobs = db_at(home)?.list_jobs(true)?;
    Ok(render_jobs(&jobs, now_secs()))
}

/// 暂停或恢复：改 `status`，回执点名完整 id（短 id 就是要能整串复制回命令行）与任务名。
fn set_status_at(home: &Path, id: &str, toggle: Toggle) -> Result<String> {
    let db = db_at(home)?;
    let job = find_job(&db, id)?;
    db.set_job_status(id, toggle.status())?;
    Ok(format!(
        "schedule {}: {} ({})\n",
        toggle.verb(),
        id,
        one_line(&job.name)
    ))
}

/// 删除：物理删 `jobs` 行，历史 `job_runs` 留痕不动（回执里点一句）。
fn remove_at(home: &Path, id: &str) -> Result<String> {
    let db = db_at(home)?;
    let job = find_job(&db, id)?;
    db.remove_job(id)?;
    Ok(format!(
        "schedule removed: {} ({}) (run history kept)\n",
        id,
        one_line(&job.name)
    ))
}

/// 运行历史：按开始时刻倒序；`id` 省略时看全部任务的（含已删任务的留痕）。
///
/// 显式 `id` 既不是现存 job、也没有任何运行留痕时，报 `schedule not found: {id}`——拼错的 id
/// 不能得到一张和「还没跑过」一样的空表。存在性判定是「job 行 **或** 运行留痕」：`remove` 只删
/// `jobs` 行，删掉的 job 历史照常可查。
fn history_at(home: &Path, id: Option<&str>, limit: usize) -> Result<String> {
    let db = db_at(home)?;
    let Some(id) = id else {
        return Ok(render_runs(&db.list_job_runs(None, limit)?));
    };
    // 存在性探测单独用 limit=1：limit=0 时主查询本来就空，不能把「没限量出来」当「不存在」
    let known = db.get_job(id)?.is_some() || !db.list_job_runs(Some(id), 1)?.is_empty();
    if !known {
        return Err(anyhow!("schedule not found: {id}"));
    }
    Ok(render_runs(&db.list_job_runs(Some(id), limit)?))
}

/// 按 id 取任务行；未知 id 报 `schedule not found: {id}`（`main` 打成 `error:` 并给退出码 1）。
fn find_job(db: &Db, id: &str) -> Result<JobRecord> {
    match db.get_job(id)? {
        Some(job) => Ok(job),
        None => Err(anyhow!("schedule not found: {id}")),
    }
}

/// 名单表：`ID  NAME  STATUS  KIND/EXPR  LAST RUN  NEXT DUE`；ID 列给完整的短 id。
fn render_jobs(jobs: &[JobRecord], now: i64) -> String {
    let mut rows = vec![vec![
        "ID".to_string(),
        "NAME".to_string(),
        "STATUS".to_string(),
        "KIND/EXPR".to_string(),
        "LAST RUN".to_string(),
        "NEXT DUE".to_string(),
    ]];
    for job in jobs {
        rows.push(vec![
            job.id.clone(),
            one_line(&job.name),
            job.status.clone(),
            one_line(&format!("{} {}", job.schedule_kind, job.schedule_expr)),
            job.last_run_at.map_or_else(|| "-".to_string(), format_time),
            next_due_cell(job, now),
        ]);
    }
    render_table(&rows)
}

/// next due 单元格：永不到期渲染成 `never`，表达式坏掉（手工改库）给 `?`。
fn next_due_cell(job: &JobRecord, now: i64) -> String {
    let Ok(spec) = parse_schedule(&job.schedule_kind, &job.schedule_expr) else {
        return "?".to_string();
    };
    // interval 的节奏从上次运行起算（库里存毫秒，`next_due` 用秒），与守护进程同一口径；
    // cron 只看表达式，`last_fired` 不参与
    let last_fired = job.last_run_at.map(|millis| millis.div_euclid(1_000));
    format_due(next_due(&spec, now, last_fired))
}

/// 到期时刻的展示：`NEVER_DUE` 是哨兵（不是真实时刻），渲染成 `never`；其余给 RFC 3339 短形。
fn format_due(due_secs: i64) -> String {
    if due_secs == NEVER_DUE {
        return "never".to_string();
    }
    format_time(due_secs.saturating_mul(1_000))
}

/// 历史表：`ID  STATUS  STARTED  ENDED  TOKENS  SUMMARY/ERROR`；ID 列给完整的运行 id。
fn render_runs(runs: &[JobRunRecord]) -> String {
    let mut rows = vec![vec![
        "ID".to_string(),
        "STATUS".to_string(),
        "STARTED".to_string(),
        "ENDED".to_string(),
        "TOKENS".to_string(),
        "SUMMARY/ERROR".to_string(),
    ]];
    for run in runs {
        rows.push(vec![
            run.id.clone(),
            run.status.clone(),
            format_time(run.started_at),
            // 还没收尾的运行：结束时间给 `-`，不编时刻
            run.ended_at.map_or_else(|| "-".to_string(), format_time),
            format!("in={} out={}", run.input_tokens, run.output_tokens),
            outcome_cell(run),
        ]);
    }
    render_table(&rows)
}

/// 摘要/错误单元格：摘要在前、错误在后（都没有给 `-`），压成单行并截断。
fn outcome_cell(run: &JobRunRecord) -> String {
    match run.summary.as_deref().or(run.error.as_deref()) {
        Some(text) => truncate(&one_line(text), SUMMARY_CHARS),
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_storage::{NewJob, NewJobRun};

    use super::*;

    /// `2026-10-15T12:00:00Z` 的 epoch 毫秒（与 task 侧测试同源）。
    const STARTED_AT: i64 = 1_792_065_600_000;
    /// 上一整天同一时刻：用来钉「不补跑」（错过的时点不被补上，next due 从 now 重新起算）。
    const YESTERDAY: i64 = STARTED_AT - 86_400_000;

    /// 单个测试独占的临时 home，析构时清理。
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-cli-schedule-{}-{label}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 一行拆成单元格：表格用两个空格分隔，列宽不足处补空格。
    fn cells(line: &str) -> Vec<&str> {
        line.split("  ")
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect()
    }

    /// 一条 job 行（id / 名字 / 节奏由用例给定，其余取固定的合法值）。
    fn job(id: &str, name: &str, kind: &str, expr: &str) -> NewJob {
        NewJob {
            id: id.into(),
            name: name.into(),
            prompt: format!("检查 {name}"),
            schedule_kind: kind.into(),
            schedule_expr: expr.into(),
            kernel: "native".into(),
            profile_json: r#"{"tools":["fs_read"],"risk":"elevated"}"#.into(),
            limits_json: r#"{"max_steps":5}"#.into(),
            workspace: "/w".into(),
        }
    }

    /// 名单行：完整短 id、name、status、kind/expr、last run、next due（现算，永不触发打 `never`）。
    #[test]
    fn rows_show_the_locked_columns_and_compute_next_due() {
        let db = Db::open_in_memory().unwrap();
        for (id, name, kind, expr) in [
            (
                "019a1b2c-3d4e-7f00-8000-000000000001",
                "disk-watch",
                "interval",
                "10m",
            ),
            (
                "019a1b2c-3d4e-7f00-8000-000000000002",
                "feb-30",
                "cron",
                "0 0 30 2 *",
            ),
            (
                "019a1b2c-3d4e-7f00-8000-000000000003",
                "old-note",
                "interval",
                "2h",
            ),
        ] {
            db.insert_job(&job(id, name, kind, expr)).unwrap();
        }
        // 1 号：刚跑过（保持节奏，next due = last_run + 10m）
        db.touch_job_last_run("019a1b2c-3d4e-7f00-8000-000000000001", STARTED_AT)
            .unwrap();
        // 2 号：显式暂停（名单要连暂停的也列出来）
        db.set_job_status("019a1b2c-3d4e-7f00-8000-000000000002", "paused")
            .unwrap();
        // 3 号：昨天跑过（错过的时点不补，next due 从 now 重新起算）
        db.touch_job_last_run("019a1b2c-3d4e-7f00-8000-000000000003", YESTERDAY)
            .unwrap();

        let out = render_jobs(&db.list_jobs(true).unwrap(), STARTED_AT / 1000);

        assert_eq!(
            cells(out.lines().next().unwrap()),
            ["ID", "NAME", "STATUS", "KIND/EXPR", "LAST RUN", "NEXT DUE"],
            "{out}"
        );
        assert_eq!(out.lines().count(), 4, "{out}");
        assert_eq!(
            cells(out.lines().nth(1).unwrap()),
            [
                "019a1b2c-3d4e-7f00-8000-000000000001",
                "disk-watch",
                "active",
                "interval 10m",
                "2026-10-15T12:00:00Z",
                "2026-10-15T12:10:00Z",
            ],
            "{out}"
        );
        // cron 在搜索窗内不可达：哨兵值渲染成 never（不是 2920 亿年后的一个时刻）
        assert_eq!(
            cells(out.lines().nth(2).unwrap()),
            [
                "019a1b2c-3d4e-7f00-8000-000000000002",
                "feb-30",
                "paused",
                "cron 0 0 30 2 *",
                "-",
                "never",
            ],
            "{out}"
        );
        // 从没跑过 / 上次运行已过期：next due 从 now 起算
        assert_eq!(
            cells(out.lines().nth(3).unwrap()),
            [
                "019a1b2c-3d4e-7f00-8000-000000000003",
                "old-note",
                "active",
                "interval 2h",
                "2026-10-14T12:00:00Z",
                "2026-10-15T14:00:00Z",
            ],
            "{out}"
        );
    }

    /// 到期展示函数：哨兵给 `never`（哨兵由常量给出，不写字面量），其余按 RFC 3339 短形。
    #[test]
    fn due_cell_renders_the_never_sentinel_as_a_word() {
        assert_eq!(format_due(NEVER_DUE), "never");
        assert_eq!(format_due(STARTED_AT / 1000), "2026-10-15T12:00:00Z");

        // 手工改库改坏的表达式：给 `?`，不让一行坏数据拖垮整张名单
        let broken = record("01", "nonsense", "every 10 minutes", None);
        assert_eq!(next_due_cell(&broken, STARTED_AT / 1000), "?");

        // 到点时刻走的是守护进程那一份 `next_due`：cron 不可达 → 哨兵 → `never`
        let unreachable = record("02", "cron", "0 0 30 2 *", None);
        assert_eq!(next_due_cell(&unreachable, STARTED_AT / 1000), "never");
    }

    /// 暂停/恢复改的是 `jobs.status`，回执点名完整 id 与任务名。
    #[test]
    fn pause_and_resume_flip_the_status_and_echo_the_job() {
        let home = TempHome::new("toggle");
        let id = "019a1b2c-3d4e-7f00-8000-000000000001";
        db_at(home.path())
            .unwrap()
            .insert_job(&job(id, "disk-watch", "interval", "10m"))
            .unwrap();

        assert_eq!(
            set_status_at(home.path(), id, Toggle::Pause).unwrap(),
            format!("schedule paused: {id} (disk-watch)\n")
        );
        assert_eq!(
            db_at(home.path())
                .unwrap()
                .get_job(id)
                .unwrap()
                .unwrap()
                .status,
            "paused"
        );

        assert_eq!(
            set_status_at(home.path(), id, Toggle::Resume).unwrap(),
            format!("schedule resumed: {id} (disk-watch)\n")
        );
        assert_eq!(
            db_at(home.path())
                .unwrap()
                .get_job(id)
                .unwrap()
                .unwrap()
                .status,
            "active"
        );
    }

    /// 删除是物理删除，但运行历史留痕（回执点一句，`history` 照常查得到）。
    #[test]
    fn remove_drops_the_job_row_and_keeps_the_run_history() {
        let home = TempHome::new("remove");
        let id = "019a1b2c-3d4e-7f00-8000-000000000001";
        let db = db_at(home.path()).unwrap();
        db.insert_job(&job(id, "disk-watch", "interval", "10m"))
            .unwrap();
        db.insert_job_run(&NewJobRun {
            id: "run-1".into(),
            job_id: id.into(),
            task_id: None,
            started_at: STARTED_AT,
        })
        .unwrap();
        drop(db);

        assert_eq!(
            remove_at(home.path(), id).unwrap(),
            format!("schedule removed: {id} (disk-watch) (run history kept)\n")
        );

        let db = db_at(home.path()).unwrap();
        assert!(db.get_job(id).unwrap().is_none(), "jobs 行应被物理删除");
        assert_eq!(db.list_job_runs(Some(id), 10).unwrap().len(), 1, "历史留痕");
    }

    /// 未知 id：改动型命令与 `history` 都报 `schedule not found: {id}`（`error:` + 退出码 1）。
    #[test]
    fn unknown_ids_are_reported_in_the_cli_error_style() {
        let home = TempHome::new("missing");

        for error in [
            set_status_at(home.path(), "nope", Toggle::Pause)
                .unwrap_err()
                .to_string(),
            set_status_at(home.path(), "nope", Toggle::Resume)
                .unwrap_err()
                .to_string(),
            remove_at(home.path(), "nope").unwrap_err().to_string(),
            // 历史：显式 id 既不命中 job 行也没有留痕时不能静默空表；limit=0 也不能被空结果带偏
            history_at(home.path(), Some("nope"), 10)
                .unwrap_err()
                .to_string(),
            history_at(home.path(), Some("nope"), 0)
                .unwrap_err()
                .to_string(),
        ] {
            assert_eq!(error, "schedule not found: nope");
        }
    }

    /// `history <id>` 的存在性判定是「job 行 **或** 运行留痕」：删掉的 job 历史照常可查，
    /// 而两者都没有的拼错 id 得到与改动型命令同一句错误。
    #[test]
    fn history_rejects_unknown_ids_but_serves_removed_jobs() {
        let home = TempHome::new("history-missing");
        // 空库里查未知 id：报错，而不是一张与「还没跑过」无法区分的空表
        assert_eq!(
            history_at(home.path(), Some("nope"), 10)
                .unwrap_err()
                .to_string(),
            "schedule not found: nope"
        );

        // 删掉的 job：`jobs` 行没了，但运行留痕还在 → 照常给表
        let id = "019a1b2c-3d4e-7f00-8000-000000000001";
        let db = db_at(home.path()).unwrap();
        db.insert_job(&job(id, "disk-watch", "interval", "10m"))
            .unwrap();
        db.insert_job_run(&NewJobRun {
            id: "run-1".into(),
            job_id: id.into(),
            task_id: None,
            started_at: STARTED_AT,
        })
        .unwrap();
        drop(db);

        remove_at(home.path(), id).unwrap();
        let out = history_at(home.path(), Some(id), 10).unwrap();
        assert!(out.contains("run-1"), "{out}");
        // limit=0：存在性探测不受「主查询本来就空」影响，给表头而不是 not found
        assert_eq!(
            history_at(home.path(), Some(id), 0)
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    /// 历史表：倒序、`--limit` 生效、摘要/错误截断、未收尾的运行给 `-`，删掉任务后仍可查。
    #[test]
    fn history_lists_runs_newest_first_with_truncated_outcomes() {
        let home = TempHome::new("history");
        let id = "019a1b2c-3d4e-7f00-8000-000000000001";
        let db = db_at(home.path()).unwrap();
        db.insert_job(&job(id, "disk-watch", "interval", "10m"))
            .unwrap();
        // 三个运行：最早的仍在跑、中间的失败（错误超长）、最新的成功
        for (run, started) in [
            ("run-1", STARTED_AT),
            ("run-2", STARTED_AT + 60_000),
            ("run-3", STARTED_AT + 120_000),
        ] {
            db.insert_job_run(&NewJobRun {
                id: run.into(),
                job_id: id.into(),
                task_id: None,
                started_at: started,
            })
            .unwrap();
        }
        db.finish_job_run(
            "run-3",
            "failed",
            None,
            Some(&"错".repeat(70)),
            7,
            3,
            STARTED_AT + 125_000,
        )
        .unwrap();
        db.finish_job_run(
            "run-2",
            "success",
            Some("检查完成"),
            None,
            12,
            34,
            STARTED_AT + 65_000,
        )
        .unwrap();

        let out = history_at(home.path(), Some(id), 10).unwrap();

        assert_eq!(
            cells(out.lines().next().unwrap()),
            [
                "ID",
                "STATUS",
                "STARTED",
                "ENDED",
                "TOKENS",
                "SUMMARY/ERROR"
            ],
            "{out}"
        );
        assert_eq!(out.lines().count(), 4, "{out}");
        assert_eq!(
            cells(out.lines().nth(1).unwrap()),
            [
                "run-3",
                "failed",
                "2026-10-15T12:02:00Z",
                "2026-10-15T12:02:05Z",
                "in=7 out=3",
                &format!("{}…", "错".repeat(60)),
            ],
            "{out}"
        );
        assert_eq!(
            cells(out.lines().nth(2).unwrap()),
            [
                "run-2",
                "success",
                "2026-10-15T12:01:00Z",
                "2026-10-15T12:01:05Z",
                "in=12 out=34",
                "检查完成",
            ],
            "{out}"
        );
        // 还在跑的那条：结束时间给 `-`（不是编一个时刻），摘要/错误都没有也给 `-`
        let first = cells(out.lines().nth(3).unwrap());
        assert_eq!(first[0], "run-1");
        assert_eq!(first[1], "running");
        assert_eq!(first[3], "-");
        assert_eq!(first[4], "in=0 out=0");
        assert_eq!(first[5], "-");

        // `--limit` 生效：表头 + 一条
        assert_eq!(
            history_at(home.path(), Some(id), 1)
                .unwrap()
                .lines()
                .count(),
            2
        );
        // 缺省 id：看全部任务的历史
        assert_eq!(
            history_at(home.path(), None, 10).unwrap().lines().count(),
            4
        );
        // 任务删掉后历史照常可查（留痕的用处）
        remove_at(home.path(), id).unwrap();
        assert!(
            history_at(home.path(), Some(id), 10)
                .unwrap()
                .contains("run-3")
        );
    }

    /// 一条 job 记录（不走库）：坏表达式之类的入库前校验拦不住的情形用它构造。
    fn record(id: &str, kind: &str, expr: &str, last_run_at: Option<i64>) -> JobRecord {
        let job = job(id, "subject", kind, expr);
        JobRecord {
            id: job.id,
            name: job.name,
            prompt: job.prompt,
            schedule_kind: job.schedule_kind,
            schedule_expr: job.schedule_expr,
            kernel: job.kernel,
            profile_json: job.profile_json,
            limits_json: job.limits_json,
            workspace: job.workspace,
            status: "active".into(),
            created_at: STARTED_AT,
            last_run_at,
        }
    }
}
