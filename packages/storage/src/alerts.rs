//! 告警 DAO：`alerts` 表。
//!
//! 去重键由上层拼装；[`Db::find_recent_alert`] 供窗口内去重判断，[`Db::mark_alerts_read`]
//! 供 `ignis alerts --read` 把未读一次性翻牌。`severity` 取值 `info` / `warning` / `critical`，
//! storage 只存不校验（与 `tasks.status` 一致）。

use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Db, Result, now_millis};

/// 新建告警行的字段；`created_at` 取调用时刻，`read_at` 为空（未读）。
#[derive(Debug, Clone)]
pub struct NewAlert {
    /// `info` / `warning` / `critical`。
    pub severity: String,
    pub title: String,
    pub body: Option<String>,
    /// 来源调度任务 id；`None` 表示与 job 无关（如 CLI 交互里的 `notify_user`）。
    pub source_job_id: Option<String>,
    /// 来源任务 id。
    pub source_task_id: Option<String>,
    /// 去重键：同一键在窗口内只落一行。
    pub dedup_key: String,
}

/// `alerts` 表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertRecord {
    pub id: i64,
    /// `info` / `warning` / `critical`。
    pub severity: String,
    pub title: String,
    pub body: Option<String>,
    pub source_job_id: Option<String>,
    pub source_task_id: Option<String>,
    pub dedup_key: String,
    /// epoch 毫秒。
    pub created_at: i64,
    /// epoch 毫秒；未读为 `None`。
    pub read_at: Option<i64>,
}

const ALERT_COLUMNS: &str =
    "id, severity, title, body, source_job_id, source_task_id, dedup_key, created_at, read_at";

fn row_to_alert(row: &Row<'_>) -> rusqlite::Result<AlertRecord> {
    Ok(AlertRecord {
        id: row.get("id")?,
        severity: row.get("severity")?,
        title: row.get("title")?,
        body: row.get("body")?,
        source_job_id: row.get("source_job_id")?,
        source_task_id: row.get("source_task_id")?,
        dedup_key: row.get("dedup_key")?,
        created_at: row.get("created_at")?,
        read_at: row.get("read_at")?,
    })
}

impl Db {
    /// 追加一条未读告警。
    pub fn insert_alert(&self, a: &NewAlert) -> Result<()> {
        let now = now_millis();
        self.with_conn(|c| {
            c.prepare_cached(
                "INSERT INTO alerts (severity, title, body, source_job_id, source_task_id,
                 dedup_key, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                a.severity,
                a.title,
                a.body,
                a.source_job_id,
                a.source_task_id,
                a.dedup_key,
                now
            ])?;
            Ok(())
        })
    }

    /// 去重窗口查询：同 `dedup_key` 且 `created_at >= now - window_secs * 1000` 的最新一行
    /// （同刻取 id 最大者）；窗口内没有就回 `None`。`window_secs` 传秒，换算成毫秒在内部做。
    pub fn find_recent_alert(
        &self,
        dedup_key: &str,
        window_secs: i64,
        now: i64,
    ) -> Result<Option<AlertRecord>> {
        let threshold = now - window_secs * 1000;
        let sql = format!(
            "SELECT {ALERT_COLUMNS} FROM alerts
             WHERE dedup_key = ?1 AND created_at >= ?2
             ORDER BY created_at DESC, id DESC LIMIT 1"
        );
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            stmt.query_row(params![dedup_key, threshold], row_to_alert)
                .optional()
        })
    }

    /// 告警列表：按 `created_at` 倒序（同刻按 id 倒序），最多 `limit` 条；`include_read = false`
    /// 时只回未读行。
    pub fn list_alerts(&self, include_read: bool, limit: usize) -> Result<Vec<AlertRecord>> {
        let filter = if include_read {
            ""
        } else {
            "WHERE read_at IS NULL"
        };
        let sql = format!(
            "SELECT {ALERT_COLUMNS} FROM alerts {filter} ORDER BY created_at DESC, id DESC LIMIT ?1"
        );
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![limit as i64], row_to_alert)?;
            rows.collect()
        })
    }

    /// 把所有未读告警标成已读（`read_at = now`），返回翻牌的条数；已读行不动。
    pub fn mark_alerts_read(&self, now: i64) -> Result<usize> {
        let flipped = self.with_conn(|c| {
            c.prepare_cached("UPDATE alerts SET read_at = ?1 WHERE read_at IS NULL")?
                .execute(params![now])
        })?;
        Ok(flipped)
    }

    /// 未读告警条数（`ignis alerts` 默认视图与 banner 用）。
    pub fn unread_alert_count(&self) -> Result<usize> {
        let count = self.with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM alerts WHERE read_at IS NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
        })?;
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use crate::alerts::{AlertRecord, NewAlert};
    use crate::db::{Db, now_millis};

    fn new_alert(title: &str, dedup_key: &str) -> NewAlert {
        NewAlert {
            severity: "warning".into(),
            title: title.into(),
            body: Some(format!("body {title}")),
            source_job_id: Some("job-1".into()),
            source_task_id: None,
            dedup_key: dedup_key.into(),
        }
    }

    fn titles(alerts: &[AlertRecord]) -> Vec<String> {
        alerts.iter().map(|a| a.title.clone()).collect()
    }

    #[test]
    fn insert_alert_roundtrips_as_an_unread_row() {
        let db = Db::open_in_memory().unwrap();
        let before = now_millis();
        db.insert_alert(&NewAlert {
            severity: "critical".into(),
            title: "磁盘快满了".into(),
            body: Some("根分区 92%".into()),
            source_job_id: Some("job-1".into()),
            source_task_id: Some("task-9".into()),
            dedup_key: "job-1|磁盘快满了".into(),
        })
        .unwrap();
        let after = now_millis();

        let rec = &db.list_alerts(true, 10).unwrap()[0];
        assert_eq!(rec.id, 1);
        assert_eq!(rec.severity, "critical");
        assert_eq!(rec.title, "磁盘快满了");
        assert_eq!(rec.body.as_deref(), Some("根分区 92%"));
        assert_eq!(rec.source_job_id.as_deref(), Some("job-1"));
        assert_eq!(rec.source_task_id.as_deref(), Some("task-9"));
        assert_eq!(rec.dedup_key, "job-1|磁盘快满了");
        assert!(
            rec.created_at >= before && rec.created_at <= after,
            "created_at = {} 不在 [{before}, {after}] 内",
            rec.created_at
        );
        assert_eq!(rec.read_at, None);

        // 可选字段为空的行也照常落库
        let mut sparse = new_alert("提示", "k2");
        sparse.severity = "info".into();
        sparse.body = None;
        sparse.source_job_id = None;
        db.insert_alert(&sparse).unwrap();
        let rec = db
            .list_alerts(true, 10)
            .unwrap()
            .into_iter()
            .find(|a| a.dedup_key == "k2")
            .unwrap();
        assert_eq!(rec.id, 2);
        assert_eq!(rec.body, None);
        assert_eq!(rec.source_job_id, None);
        assert_eq!(rec.source_task_id, None);
    }

    #[test]
    fn list_alerts_orders_newest_first_and_can_hide_read() {
        let db = Db::open_in_memory().unwrap();
        for (title, key) in [("一", "k1"), ("二", "k2"), ("三", "k3")] {
            db.insert_alert(&new_alert(title, key)).unwrap();
        }
        // 同刻按 id 倒序：后落的在前
        assert_eq!(
            titles(&db.list_alerts(true, 10).unwrap()),
            ["三", "二", "一"]
        );

        // 改写 created_at，证明确实按它排序
        db.with_conn(|c| {
            c.execute_batch(
                "UPDATE alerts SET created_at = 30 WHERE title = '一';
                 UPDATE alerts SET created_at = 10 WHERE title = '二';
                 UPDATE alerts SET created_at = 20 WHERE title = '三';",
            )
        })
        .unwrap();
        assert_eq!(
            titles(&db.list_alerts(true, 10).unwrap()),
            ["一", "三", "二"]
        );
        assert_eq!(titles(&db.list_alerts(true, 2).unwrap()), ["一", "三"]);

        db.mark_alerts_read(1).unwrap();
        assert!(db.list_alerts(false, 10).unwrap().is_empty());
        assert_eq!(db.list_alerts(true, 10).unwrap().len(), 3);
    }

    #[test]
    fn find_recent_alert_returns_the_newest_row_in_the_window() {
        let db = Db::open_in_memory().unwrap();
        db.insert_alert(&new_alert("旧", "k")).unwrap();
        db.insert_alert(&new_alert("新", "k")).unwrap();
        db.with_conn(|c| {
            c.execute_batch(
                "UPDATE alerts SET created_at = 1000000 WHERE title = '旧';
                 UPDATE alerts SET created_at = 1000500 WHERE title = '新';",
            )
        })
        .unwrap();

        // 窗口（1s）里两行都在 → 取最新那条
        let hit = db
            .find_recent_alert("k", 1, 1_000_900)
            .unwrap()
            .expect("窗口内应命中");
        assert_eq!(hit.title, "新");
        // 边界：阈值恰好压在 新 行的 created_at 上仍算窗口内
        assert_eq!(
            db.find_recent_alert("k", 1, 1_001_500)
                .unwrap()
                .map(|a| a.title),
            Some("新".into())
        );
        // 阈值越过 新 行 → 窗口内没有命中
        assert!(db.find_recent_alert("k", 1, 1_001_600).unwrap().is_none());
        // 去重键不同互不影响
        assert!(
            db.find_recent_alert("other", 1, 1_000_900)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mark_alerts_read_flips_only_unread_rows() {
        let db = Db::open_in_memory().unwrap();
        db.insert_alert(&new_alert("一", "k1")).unwrap();
        db.insert_alert(&new_alert("二", "k2")).unwrap();

        assert_eq!(db.mark_alerts_read(1000).unwrap(), 2);
        // 已读行不再翻一次，时间戳也不被后一次调用改写
        assert_eq!(db.mark_alerts_read(2000).unwrap(), 0);
        db.insert_alert(&new_alert("三", "k3")).unwrap();
        assert_eq!(db.mark_alerts_read(3000).unwrap(), 1);

        let rows = db.list_alerts(true, 10).unwrap();
        let by_key = |key: &str| rows.iter().find(|a| a.dedup_key == key).unwrap();
        assert_eq!(by_key("k1").read_at, Some(1000));
        assert_eq!(by_key("k2").read_at, Some(1000));
        assert_eq!(by_key("k3").read_at, Some(3000));
        assert!(db.list_alerts(false, 10).unwrap().is_empty());
    }

    #[test]
    fn unread_alert_count_counts_unread_rows() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.unread_alert_count().unwrap(), 0);

        db.insert_alert(&new_alert("一", "k1")).unwrap();
        db.insert_alert(&new_alert("二", "k2")).unwrap();
        assert_eq!(db.unread_alert_count().unwrap(), 2);

        db.mark_alerts_read(7).unwrap();
        assert_eq!(db.unread_alert_count().unwrap(), 0);
        db.insert_alert(&new_alert("三", "k3")).unwrap();
        assert_eq!(db.unread_alert_count().unwrap(), 1);
        assert_eq!(titles(&db.list_alerts(false, 10).unwrap()), ["三"]);
    }
}
