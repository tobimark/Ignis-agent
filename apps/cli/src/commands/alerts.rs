//! `ignis alerts [--all] [--read]`：告警收件箱。
//!
//! 默认只看未读；`--all` 连已读一起列；`--read` 先按选中的范围展示、再把**所有**未读翻成已读
//! （`mark_alerts_read` 一把翻牌，不只翻这一屏），最后回一行翻牌条数。一条告警的来源形如
//! `job:{id 前 8}` / `task:{id 前 8}`，两者都没有（CLI 交互里的 `notify_user`）就是 `cli`。

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use ignis_core::config::{ensure_home, ignis_home};
use ignis_storage::{AlertRecord, Db};

use super::now_millis;
use super::task::{format_time, id_prefix, one_line};
use crate::cli::AlertsCommand;

/// 一屏最多列出多少条（与 `task list` 的默认量级一致，避免长尾刷屏）。
const ALERT_LIMIT: usize = 20;

/// `ignis alerts`：展示 + 可选翻牌。
pub fn run(command: &AlertsCommand) -> Result<ExitCode> {
    let out = show_at(&ignis_home(), command.all, command.read)?;
    print!("{out}");
    Ok(ExitCode::SUCCESS)
}

/// 展示（未读 / 含已读）并按需翻牌；返回要打印的全部文本，`home` 可注入便于测试。
fn show_at(home: &Path, all: bool, read: bool) -> Result<String> {
    ensure_home(home)?;
    let db = Db::open(&home.join("ignis.db"))?;
    // 展示范围跟随 `--all`，翻牌永远是「全部未读」：`--read` 的语义是清空收件箱
    let alerts = db.list_alerts(all, ALERT_LIMIT)?;
    let mut out = render_alerts(&alerts, all);
    if read {
        let marked = db.mark_alerts_read(now_millis())?;
        out.push_str(&format!("marked {marked} alerts as read\n"));
    }
    Ok(out)
}

/// 告警列表，一行一条 `{时间}  [{severity}]  {title}  ({source})`；一条都没有时给空态行。
fn render_alerts(alerts: &[AlertRecord], include_read: bool) -> String {
    if alerts.is_empty() {
        return format!("{}\n", empty_line(include_read));
    }
    let mut out = String::new();
    for alert in alerts {
        // 标题是模型可控文本：压成单行，一条告警不会撑成多行
        out.push_str(&format!(
            "{}  [{}]  {}  ({})\n",
            format_time(alert.created_at),
            alert.severity,
            one_line(&alert.title),
            source_cell(alert),
        ));
    }
    out
}

/// 空态：默认视图说没有未读，`--all` 说一条都没有。
fn empty_line(include_read: bool) -> &'static str {
    if include_read {
        "no alerts"
    } else {
        "no unread alerts"
    }
}

/// 来源列：`job:{前 8}` 优先，其次 `task:{前 8}`，两者都没有给 `cli`。
fn source_cell(alert: &AlertRecord) -> String {
    if let Some(job) = alert.source_job_id.as_deref() {
        format!("job:{}", id_prefix(job))
    } else if let Some(task) = alert.source_task_id.as_deref() {
        format!("task:{}", id_prefix(task))
    } else {
        "cli".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_storage::NewAlert;

    use super::*;

    /// `2026-10-15T12:00:00Z` 的 epoch 毫秒（与 task 侧测试同源）。
    const CREATED_AT: i64 = 1_792_065_600_000;

    /// 单个测试独占的临时 home，析构时清理。
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-cli-alerts-{}-{label}-{}",
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

    /// 一条告警行；`created_at` 由用例改写（DAO 自己取时钟）。
    fn alert(
        title: &str,
        severity: &str,
        source_job_id: Option<&str>,
        source_task_id: Option<&str>,
    ) -> NewAlert {
        NewAlert {
            severity: severity.into(),
            title: title.into(),
            body: None,
            source_job_id: source_job_id.map(str::to_owned),
            source_task_id: source_task_id.map(str::to_owned),
            dedup_key: format!("{title}:k"),
        }
    }

    /// 行格式与来源兜底：`{时间}  [{severity}]  {title}  (job/task/cli)`，标题压成单行。
    #[test]
    fn rows_carry_time_severity_title_and_source() {
        let rows = vec![
            AlertRecord {
                id: 3,
                severity: "critical".into(),
                title: "磁盘满了\n根分区 99%".into(),
                body: Some("正文不进列表".into()),
                source_job_id: Some("019a1b2c-3d4e-7f00-8000-000000000001".into()),
                source_task_id: None,
                dedup_key: "k".into(),
                created_at: CREATED_AT,
                read_at: None,
            },
            AlertRecord {
                id: 2,
                severity: "warning".into(),
                title: "任务失败：disk-watch".into(),
                body: None,
                source_job_id: None,
                source_task_id: Some("019a1b2c-3d4e-7f00-8000-000000000002".into()),
                dedup_key: "k".into(),
                created_at: CREATED_AT,
                read_at: None,
            },
            AlertRecord {
                id: 1,
                severity: "info".into(),
                title: "构建完成".into(),
                body: None,
                source_job_id: None,
                source_task_id: None,
                dedup_key: "k".into(),
                created_at: CREATED_AT,
                read_at: Some(CREATED_AT),
            },
        ];

        let out = render_alerts(&rows, true);

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert_eq!(
            lines[0],
            "2026-10-15T12:00:00Z  [critical]  磁盘满了 根分区 99%  (job:019a1b2c)"
        );
        assert_eq!(
            lines[1],
            "2026-10-15T12:00:00Z  [warning]  任务失败：disk-watch  (task:019a1b2c)"
        );
        assert_eq!(lines[2], "2026-10-15T12:00:00Z  [info]  构建完成  (cli)");

        // 空态：默认视图与 `--all` 各一句
        assert_eq!(render_alerts(&[], false), "no unread alerts\n");
        assert_eq!(render_alerts(&[], true), "no alerts\n");
        assert_eq!(empty_line(false), "no unread alerts");
        assert_eq!(empty_line(true), "no alerts");
    }

    /// `--read` 语义：先展示、再一把翻掉全部未读，最后回一行条数；已读的不再翻第二次。
    #[test]
    fn read_displays_the_rows_first_and_then_marks_every_unread_row() {
        let home = TempHome::new("read");
        let db = Db::open(&home.path().join("ignis.db")).unwrap();
        db.insert_alert(&alert("磁盘 92%", "warning", Some("job-1"), None))
            .unwrap();
        db.insert_alert(&alert("构建失败", "critical", None, None))
            .unwrap();
        drop(db);

        let out = show_at(home.path(), false, true).unwrap();

        // 两行告警在前、翻牌行在最后（先展示再标记）
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(
            lines[0].contains("磁盘 92%") || lines[1].contains("磁盘 92%"),
            "{out}"
        );
        assert!(
            lines[0].contains("构建失败") || lines[1].contains("构建失败"),
            "{out}"
        );
        assert_eq!(lines[2], "marked 2 alerts as read", "{out}");
        assert!(lines[2].starts_with("marked "), "{out}");

        // 全都翻完：默认视图空，`--all` 仍看得到两行
        let db = Db::open(&home.path().join("ignis.db")).unwrap();
        assert_eq!(db.unread_alert_count().unwrap(), 0);
        drop(db);
        assert_eq!(
            show_at(home.path(), false, false).unwrap(),
            "no unread alerts\n"
        );
        assert_eq!(
            show_at(home.path(), true, false).unwrap().lines().count(),
            2
        );
        // 再翻一次是 0 条（已读的不动）
        assert!(
            show_at(home.path(), true, true)
                .unwrap()
                .ends_with("marked 0 alerts as read\n")
        );
    }

    /// 默认只看未读、`--all` 含已读：范围由 `list_alerts` 的过滤给出。
    #[test]
    fn the_default_view_hides_read_alerts() {
        let home = TempHome::new("scope");
        let db = Db::open(&home.path().join("ignis.db")).unwrap();
        // 先落一条并翻成已读，再落一条未读的（`mark_alerts_read` 一把翻全部）
        db.insert_alert(&alert("已读的", "info", None, None))
            .unwrap();
        assert_eq!(db.mark_alerts_read(CREATED_AT).unwrap(), 1);
        db.insert_alert(&alert("未读的", "info", None, None))
            .unwrap();
        drop(db);

        let unread = show_at(home.path(), false, false).unwrap();
        assert!(unread.contains("未读的"), "{unread}");
        assert!(!unread.contains("已读的"), "{unread}");

        let all = show_at(home.path(), true, false).unwrap();
        assert!(all.contains("未读的"), "{all}");
        assert!(all.contains("已读的"), "{all}");
    }
}
