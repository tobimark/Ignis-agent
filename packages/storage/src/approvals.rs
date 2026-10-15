use rusqlite::params;

use crate::db::{Db, Result};

/// 新建审批记录的字段；取值契约见 crate 文档（`decision`、`mode`）。
#[derive(Debug, Clone)]
pub struct NewApproval {
    pub task_id: String,
    pub tool: String,
    pub summary: String,
    /// `RiskLevel` 的 serde 小写名：`safe` / `elevated` / `dangerous`。
    pub risk: String,
    /// `allow` / `allow_session` / `deny`。
    pub decision: String,
    /// `interactive` / `preauthorized` / `unattended`。
    pub mode: String,
    /// epoch 毫秒。
    pub decided_at: i64,
}

/// `approvals` 表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRecord {
    pub id: i64,
    pub task_id: String,
    pub tool: String,
    pub summary: String,
    pub risk: String,
    pub decision: String,
    pub mode: String,
    /// epoch 毫秒。
    pub decided_at: i64,
}

const APPROVAL_COLUMNS: &str = "id, task_id, tool, summary, risk, decision, mode, decided_at";

fn row_to_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRecord> {
    Ok(ApprovalRecord {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        tool: row.get("tool")?,
        summary: row.get("summary")?,
        risk: row.get("risk")?,
        decision: row.get("decision")?,
        mode: row.get("mode")?,
        decided_at: row.get("decided_at")?,
    })
}

impl Db {
    /// 追加一条审批记录（无论放行还是拒绝都要留痕）。
    pub fn insert_approval(&self, a: &NewApproval) -> Result<()> {
        self.with_conn(|c| {
            c.prepare_cached(
                "INSERT INTO approvals (task_id, tool, summary, risk, decision, mode, decided_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                a.task_id,
                a.tool,
                a.summary,
                a.risk,
                a.decision,
                a.mode,
                a.decided_at
            ])?;
            Ok(())
        })
    }

    /// 某个任务的审批记录，按决定先后（自增 id）升序排列。
    pub fn list_approvals(&self, task_id: &str) -> Result<Vec<ApprovalRecord>> {
        let sql =
            format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE task_id = ?1 ORDER BY id ASC");
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![task_id], row_to_approval)?;
            rows.collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::approvals::NewApproval;
    use crate::db::Db;

    fn approval(task_id: &str, tool: &str, decision: &str, decided_at: i64) -> NewApproval {
        NewApproval {
            task_id: task_id.into(),
            tool: tool.into(),
            summary: format!("Writing {tool}"),
            risk: "elevated".into(),
            decision: decision.into(),
            mode: "interactive".into(),
            decided_at,
        }
    }

    #[test]
    fn insert_approval_roundtrips_in_decision_order() {
        let db = Db::open_in_memory().unwrap();
        db.insert_approval(&approval("01", "fs_write", "allow", 1))
            .unwrap();
        db.insert_approval(&approval("01", "shell_run", "deny", 2))
            .unwrap();
        db.insert_approval(&approval("02", "fs_read", "allow_session", 3))
            .unwrap();

        let recs = db.list_approvals("01").unwrap();
        assert_eq!(recs.len(), 2);

        assert_eq!(recs[0].id, 1);
        assert_eq!(recs[0].task_id, "01");
        assert_eq!(recs[0].tool, "fs_write");
        assert_eq!(recs[0].summary, "Writing fs_write");
        assert_eq!(recs[0].risk, "elevated");
        assert_eq!(recs[0].decision, "allow");
        assert_eq!(recs[0].mode, "interactive");
        assert_eq!(recs[0].decided_at, 1);

        assert_eq!(recs[1].id, 2);
        assert_eq!(recs[1].task_id, "01");
        assert_eq!(recs[1].tool, "shell_run");
        assert_eq!(recs[1].summary, "Writing shell_run");
        assert_eq!(recs[1].decision, "deny");
        assert_eq!(recs[1].decided_at, 2);

        // 别的任务的行不会混进来。
        let other = db.list_approvals("02").unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].id, 3);
        assert_eq!(other[0].decision, "allow_session");
    }

    #[test]
    fn list_approvals_is_empty_for_unknown_task() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.list_approvals("nope").unwrap().is_empty());
    }
}
