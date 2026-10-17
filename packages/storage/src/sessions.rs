//! 会话 DAO：`sessions` / `session_messages` / `session_compactions` 三张表（迁移 004）。
//!
//! 契约（由上层保证，storage 与 `tasks.status` 一样不做校验）：
//! - `session_messages.role` 只用 `user` / `assistant`：会话只存 **canonical 消息**（用户提示词与
//!   assistant 最终答复），工具细节留在任务 JSONL；
//! - `session_messages.tool_call_id` / `tool_calls` 在 P3 一律写 NULL（列先留着）；
//! - `session_compactions.upto_seq` 是「已被摘要覆盖的最后一条消息的 `seq`」：有效历史 = 最近一次
//!   压缩的摘要 + 其后（`seq > upto_seq`）的消息，原始消息一条不删。
//!
//! 时间戳都是 epoch 毫秒，由调用方传（[`crate::now_millis`]），DAO 不自己取墙钟。

use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Db, Result};
use crate::tasks::escape_like;

/// 新建会话行的字段；`created_at` 与 `updated_at` 都写 `created_at` 的值。
#[derive(Debug, Clone)]
pub struct NewSession {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub kernel: String,
    pub model: Option<String>,
    /// epoch 毫秒。
    pub created_at: i64,
}

/// `sessions` 表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub kernel: String,
    pub model: Option<String>,
    /// epoch 毫秒。
    pub created_at: i64,
    /// epoch 毫秒；每追加一批消息刷新。
    pub updated_at: i64,
}

/// `/sessions` 一行：会话记录 + canonical 消息条数（一条 SQL 取齐，不做 N+1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session: SessionRecord,
    pub message_count: usize,
}

/// 追加到 `session_messages` 的一条 canonical 消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionMessage {
    /// `user` / `assistant`。
    pub role: String,
    pub content: String,
}

/// `session_messages` 表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMessageRecord {
    /// 会话内的序号，从 1 起连续。
    pub seq: i64,
    pub role: String,
    pub content: String,
    /// P3 恒为 `None`。
    pub tool_call_id: Option<String>,
    /// P3 恒为 `None`。
    pub tool_calls: Option<String>,
    /// epoch 毫秒。
    pub created_at: i64,
}

/// `session_compactions` 表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionRecord {
    pub id: i64,
    pub session_id: String,
    /// 已被摘要覆盖的最后一条消息的 `seq`。
    pub upto_seq: i64,
    pub summary: String,
    /// epoch 毫秒。
    pub created_at: i64,
}

const SESSION_COLUMNS: &str = "id, title, workspace, kernel, model, created_at, updated_at";

fn row_to_session(row: &Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.get("id")?,
        title: row.get("title")?,
        workspace: row.get("workspace")?,
        kernel: row.get("kernel")?,
        model: row.get("model")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// 列表查询：会话列 + `count(*) AS message_count`。
fn row_to_summary(row: &Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        session: row_to_session(row)?,
        // count(*) 是 SQLite 的 64 位整数，非负；与 `tasks` 的用量列一样按 usize 传出去
        message_count: row.get::<_, i64>("message_count")? as usize,
    })
}

fn row_to_message(row: &Row<'_>) -> rusqlite::Result<SessionMessageRecord> {
    Ok(SessionMessageRecord {
        seq: row.get("seq")?,
        role: row.get("role")?,
        content: row.get("content")?,
        tool_call_id: row.get("tool_call_id")?,
        tool_calls: row.get("tool_calls")?,
        created_at: row.get("created_at")?,
    })
}

fn row_to_compaction(row: &Row<'_>) -> rusqlite::Result<CompactionRecord> {
    Ok(CompactionRecord {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        upto_seq: row.get("upto_seq")?,
        summary: row.get("summary")?,
        created_at: row.get("created_at")?,
    })
}

impl Db {
    /// 插入一条会话行：`updated_at` 与 `created_at` 同值。
    pub fn insert_session(&self, s: &NewSession) -> Result<()> {
        self.with_conn(|c| {
            c.prepare_cached(
                "INSERT INTO sessions (id, title, workspace, kernel, model, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            )?
            .execute(params![
                s.id,
                s.title,
                s.workspace,
                s.kernel,
                s.model,
                s.created_at
            ])?;
            Ok(())
        })
    }

    /// 按 id 读取会话行。
    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>> {
        let sql = format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1");
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            stmt.query_row(params![id], row_to_session).optional()
        })
    }

    /// 本工作区最近一次更新的会话（`ignis -c` 的恢复点）：`updated_at` 倒序，同刻按 `id` 倒序。
    pub fn latest_session_for_workspace(&self, workspace: &str) -> Result<Option<SessionRecord>> {
        let sql = format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE workspace = ?1
             ORDER BY updated_at DESC, id DESC LIMIT 1"
        );
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            stmt.query_row(params![workspace], row_to_session)
                .optional()
        })
    }

    /// 本工作区的会话列表（`/sessions`）：与 [`Db::latest_session_for_workspace`] 同一排序，最多
    /// `limit` 条；每行带 canonical 消息条数（相关子查询一次取齐，不做 N+1）。
    pub fn list_sessions_for_workspace(
        &self,
        workspace: &str,
        limit: usize,
    ) -> Result<Vec<SessionSummary>> {
        let sql = format!(
            "SELECT {SESSION_COLUMNS},
                    (SELECT count(*) FROM session_messages m WHERE m.session_id = s.id)
                      AS message_count
             FROM sessions s WHERE s.workspace = ?1
             ORDER BY s.updated_at DESC, s.id DESC LIMIT ?2"
        );
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![workspace, limit as i64], row_to_summary)?;
            rows.collect()
        })
    }

    /// 按前缀查本工作区的会话 id（`/resume <前缀>` 的解析用）：排序与
    /// [`Db::list_sessions_for_workspace`] 相同，最多 `limit` 条。
    ///
    /// `prefix` 里的 LIKE 元字符（`\` `%` `_`）按字面处理（复用 [`escape_like`]，与
    /// [`Db::task_ids_with_prefix`] 同一口径）。空前缀等价于「本工作区的所有会话」——准入闸门在
    /// 命令层，DAO 不做二次猜测。
    pub fn session_ids_with_prefix(
        &self,
        workspace: &str,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        let pattern = format!("{}%", escape_like(prefix));
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id FROM sessions WHERE workspace = ?1 AND id LIKE ?2 ESCAPE '\\'
                 ORDER BY updated_at DESC, id DESC LIMIT ?3",
            )?;
            let rows =
                stmt.query_map(params![workspace, pattern, limit as i64], |row| row.get(0))?;
            rows.collect()
        })
    }

    /// 按序追加一批 canonical 消息：`seq` 从会话当前最大值续写，写完把 `updated_at` 刷成 `now`；
    /// 返回写入的条数。空批次是 no-op（连 `updated_at` 都不动）。
    ///
    /// 整批一个事务（`unchecked_transaction`，与 `migrations::migrate` 同一手法——闭包只拿得到
    /// `&Connection`）：中途失败不留半批消息。`session_id` 不存在时由外键拒绝（`Db::prepare` 已开
    /// `PRAGMA foreign_keys`），不是静默写入。
    pub fn append_session_messages(
        &self,
        session_id: &str,
        messages: &[NewSessionMessage],
        now: i64,
    ) -> Result<usize> {
        if messages.is_empty() {
            return Ok(0);
        }
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let start: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM session_messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )?;
            {
                let mut insert = tx.prepare_cached(
                    "INSERT INTO session_messages
                     (session_id, seq, role, content, tool_call_id, tool_calls, created_at)
                     VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5)",
                )?;
                for (offset, message) in messages.iter().enumerate() {
                    insert.execute(params![
                        session_id,
                        start + offset as i64 + 1,
                        message.role,
                        message.content,
                        now,
                    ])?;
                }
            }
            tx.prepare_cached("UPDATE sessions SET updated_at = ?1 WHERE id = ?2")?
                .execute(params![now, session_id])?;
            tx.commit()?;
            Ok(messages.len())
        })
    }

    /// 读取 `seq >= from_seq` 的消息（有效历史重建：跳过已被压缩覆盖的前缀），按 `seq` 升序。
    pub fn session_messages_from(
        &self,
        session_id: &str,
        from_seq: i64,
    ) -> Result<Vec<SessionMessageRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT seq, role, content, tool_call_id, tool_calls, created_at
                 FROM session_messages WHERE session_id = ?1 AND seq >= ?2 ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map(params![session_id, from_seq], row_to_message)?;
            rows.collect()
        })
    }

    /// 落一份压缩摘要；`upto_seq` 是已被覆盖的最后一条消息的 `seq`（原始消息一条不删）。
    pub fn insert_compaction(
        &self,
        session_id: &str,
        upto_seq: i64,
        summary: &str,
        now: i64,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.prepare_cached(
                "INSERT INTO session_compactions (session_id, upto_seq, summary, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![session_id, upto_seq, summary, now])?;
            Ok(())
        })
    }

    /// 最近一次压缩（`id` 最大的那条）：有效历史的起点；没有压缩过时给 `None`。
    pub fn latest_compaction(&self, session_id: &str) -> Result<Option<CompactionRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id, session_id, upto_seq, summary, created_at FROM session_compactions
                 WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
            )?;
            stmt.query_row(params![session_id], row_to_compaction)
                .optional()
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::StorageError;
    use crate::db::Db;
    use crate::sessions::{NewSession, NewSessionMessage};
    use crate::tasks::NewTask;

    /// 查 `sqlite_master` 里有没有某个对象（表/索引）。
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

    /// `PRAGMA table_info(<table>)` 的列名表。
    fn table_columns(db: &Db, table: &str) -> Vec<String> {
        db.with_conn(|c| {
            let mut stmt = c.prepare(&format!("PRAGMA table_info({table})"))?;
            let rows = stmt.query_map([], |row| row.get::<_, String>("name"))?;
            rows.collect()
        })
        .unwrap()
    }

    fn user_version(db: &Db) -> i64 {
        db.with_conn(|c| c.query_row("PRAGMA user_version", [], |row| row.get(0)))
            .unwrap()
    }

    /// 造一条会话行；`at` 同时当 `created_at` / `updated_at` 用（排序断言看的是它，不用墙钟）。
    fn new_session(id: &str, workspace: &str, at: i64) -> NewSession {
        NewSession {
            id: id.into(),
            title: format!("title {id}"),
            workspace: workspace.into(),
            kernel: "native".into(),
            model: Some("MiniMax-M2".into()),
            created_at: at,
        }
    }

    /// 造一条任务行（`skills_used` 的用例要拿它当靶子）。
    fn new_task(id: &str) -> NewTask {
        NewTask {
            id: id.into(),
            kind: "run".into(),
            prompt: format!("prompt {id}"),
            kernel: "native".into(),
            workspace: "/w".into(),
            started_at: 1,
            session_id: None,
        }
    }

    fn user(content: &str) -> NewSessionMessage {
        NewSessionMessage {
            role: "user".into(),
            content: content.into(),
        }
    }

    fn assistant(content: &str) -> NewSessionMessage {
        NewSessionMessage {
            role: "assistant".into(),
            content: content.into(),
        }
    }

    /// 迁移 004 的成果：三张表、两个索引、`tasks` 两个新列，`user_version` 推到 4。
    #[test]
    fn migration_004_creates_the_session_tables_and_task_columns() {
        let db = Db::open_in_memory().unwrap();

        for table in ["sessions", "session_messages", "session_compactions"] {
            assert!(object_exists(&db, "table", table), "缺表: {table}");
        }
        for index in ["idx_session_messages_seq", "idx_sessions_workspace"] {
            assert!(object_exists(&db, "index", index), "缺索引: {index}");
        }

        let columns = table_columns(&db, "tasks");
        assert!(columns.iter().any(|c| c == "session_id"), "{columns:?}");
        assert!(columns.iter().any(|c| c == "skills_used"), "{columns:?}");

        assert_eq!(user_version(&db), 4);
    }

    #[test]
    fn insert_and_get_session_round_trips_every_field() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&new_session("01", "/w", 100)).unwrap();

        let rec = db.get_session("01").unwrap().expect("session row");
        assert_eq!(rec.id, "01");
        assert_eq!(rec.title, "title 01");
        assert_eq!(rec.workspace, "/w");
        assert_eq!(rec.kernel, "native");
        assert_eq!(rec.model.as_deref(), Some("MiniMax-M2"));
        assert_eq!(rec.created_at, 100);
        // 新落库的行两个时间戳同值（此后只有 updated_at 被追加刷新）
        assert_eq!(rec.updated_at, 100);

        // model 可空：没有模型的会话照样能落库、能读回
        let mut bare = new_session("02", "/w", 200);
        bare.model = None;
        db.insert_session(&bare).unwrap();
        assert_eq!(db.get_session("02").unwrap().unwrap().model, None);

        assert!(db.get_session("nope").unwrap().is_none());
    }

    /// 最近会话只看本工作区；同工作区取 `updated_at` 最大者，同刻按 `id` 倒序。
    #[test]
    fn latest_session_for_workspace_ignores_other_workspaces_and_picks_the_newest_updated_at() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&new_session("a1", "/w", 10)).unwrap();
        db.insert_session(&new_session("a2", "/w", 30)).unwrap();
        db.insert_session(&new_session("b1", "/other", 40)).unwrap();
        db.insert_session(&new_session("b2", "/other", 50)).unwrap();

        let latest = db.latest_session_for_workspace("/w").unwrap().expect("row");
        assert_eq!(latest.id, "a2");
        assert_eq!(latest.updated_at, 30);

        // 别的工作区更「新」也不影响本工作区
        assert_eq!(
            db.latest_session_for_workspace("/other")
                .unwrap()
                .unwrap()
                .id,
            "b2"
        );
        assert!(
            db.latest_session_for_workspace("/nowhere")
                .unwrap()
                .is_none()
        );

        // 同一 `updated_at` 时按 `id` 倒序（新的 id 在前）
        db.insert_session(&new_session("a3", "/w", 30)).unwrap();
        assert_eq!(
            db.latest_session_for_workspace("/w").unwrap().unwrap().id,
            "a3"
        );
    }

    #[test]
    fn list_sessions_for_workspace_counts_messages_and_orders_newest_first() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&new_session("s1", "/w", 10)).unwrap();
        db.insert_session(&new_session("s2", "/w", 20)).unwrap();
        db.insert_session(&new_session("s3", "/w", 30)).unwrap();
        db.insert_session(&new_session("other", "/other", 40))
            .unwrap();

        // 追加消息会刷新 `updated_at`：s1(50) / s3(60) 的顺序因此换过来
        db.append_session_messages("s1", &[user("u1"), assistant("a1")], 50)
            .unwrap();
        db.append_session_messages("s3", &[user("u2")], 60).unwrap();

        let list = db.list_sessions_for_workspace("/w", 10).unwrap();
        let ids: Vec<&str> = list.iter().map(|s| s.session.id.as_str()).collect();
        assert_eq!(ids, vec!["s3", "s1", "s2"]);
        assert_eq!(list[0].message_count, 1);
        assert_eq!(list[1].message_count, 2);
        assert_eq!(list[2].message_count, 0);
        assert_eq!(list[1].session.title, "title s1");

        let limited = db.list_sessions_for_workspace("/w", 2).unwrap();
        let ids: Vec<&str> = limited.iter().map(|s| s.session.id.as_str()).collect();
        assert_eq!(ids, vec!["s3", "s1"]);

        assert!(
            db.list_sessions_for_workspace("/nowhere", 10)
                .unwrap()
                .is_empty()
        );
    }

    /// 前缀解析：只看本工作区、新近在前、`limit` 截断；LIKE 元字符按字面处理。
    #[test]
    fn session_ids_with_prefix_is_workspace_scoped_and_literal() {
        let db = Db::open_in_memory().unwrap();
        for (id, workspace, at) in [
            ("aa01", "/w", 10),
            ("aa02", "/w", 20),
            ("aa03", "/w", 30),
            ("ab01", "/other", 40),
            ("a_01", "/w", 50),
            ("ax01", "/w", 60),
        ] {
            db.insert_session(&new_session(id, workspace, at)).unwrap();
        }

        assert_eq!(
            db.session_ids_with_prefix("/w", "aa", 10).unwrap(),
            ["aa03", "aa02", "aa01"]
        );
        assert_eq!(
            db.session_ids_with_prefix("/w", "aa", 2).unwrap(),
            ["aa03", "aa02"]
        );
        // 别的工作区里的同前缀 id 不出现
        assert!(
            db.session_ids_with_prefix("/w", "ab", 10)
                .unwrap()
                .is_empty()
        );

        // LIKE 元字符按字面：`_` 不是「任意一个字符」、`%` 不是「任意后缀」
        assert_eq!(
            db.session_ids_with_prefix("/w", "a_", 10).unwrap(),
            ["a_01"]
        );
        assert_eq!(
            db.session_ids_with_prefix("/w", "ax", 10).unwrap(),
            ["ax01"]
        );
        assert!(
            db.session_ids_with_prefix("/w", "%", 10)
                .unwrap()
                .is_empty()
        );

        // 空前缀等价「本工作区的所有会话」（准入闸门在命令层，DAO 不做二次猜测）
        assert_eq!(db.session_ids_with_prefix("/w", "", 10).unwrap().len(), 5);
    }

    /// 追加消息：`seq` 从当前最大值续写、`updated_at` 刷成 `now`、空批次是 no-op。
    #[test]
    fn append_session_messages_assigns_contiguous_seqs_and_bumps_updated_at() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&new_session("s1", "/w", 10)).unwrap();

        let appended = db
            .append_session_messages("s1", &[user("hi"), assistant("hello")], 100)
            .unwrap();
        assert_eq!(appended, 2);

        let first = db.session_messages_from("s1", 1).unwrap();
        assert_eq!(first.iter().map(|m| m.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(first[0].role, "user");
        assert_eq!(first[0].content, "hi");
        assert_eq!(first[1].role, "assistant");
        assert_eq!(first[1].content, "hello");
        assert_eq!(first[0].created_at, 100);
        // P3 的 canonical 消息只存纯文本，两个工具列一律 NULL
        assert_eq!(first[0].tool_call_id, None);
        assert_eq!(first[0].tool_calls, None);
        assert_eq!(db.get_session("s1").unwrap().unwrap().updated_at, 100);

        // 第二批从已有最大 seq 续写
        assert_eq!(
            db.append_session_messages("s1", &[user("again")], 200)
                .unwrap(),
            1
        );
        let all = db.session_messages_from("s1", 1).unwrap();
        assert_eq!(all.iter().map(|m| m.seq).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(all[2].content, "again");
        assert_eq!(db.get_session("s1").unwrap().unwrap().updated_at, 200);
        assert_eq!(db.get_session("s1").unwrap().unwrap().created_at, 10);

        // 空批次是 no-op：不改 updated_at、不写行、返回 0
        assert_eq!(db.append_session_messages("s1", &[], 300).unwrap(), 0);
        assert_eq!(db.get_session("s1").unwrap().unwrap().updated_at, 200);
        assert_eq!(db.session_messages_from("s1", 1).unwrap().len(), 3);
    }

    /// 不存在的会话：外键拒绝（`Db::prepare` 已开 `PRAGMA foreign_keys`），不是静默写入。
    #[test]
    fn append_to_an_unknown_session_is_an_error() {
        let db = Db::open_in_memory().unwrap();

        let err = db
            .append_session_messages("nope", &[user("hi")], 1)
            .unwrap_err();
        assert!(matches!(err, StorageError::Sqlite(_)), "{err}");
        assert!(err.to_string().contains("FOREIGN KEY"), "{err}");

        // 空批次仍旧是 no-op：没有行要写，也就没有外键可违
        assert_eq!(db.append_session_messages("nope", &[], 1).unwrap(), 0);
    }

    /// 有效历史重建：压缩过的前缀（`seq <= upto_seq`）不进来。
    #[test]
    fn session_messages_from_skips_the_compacted_prefix() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&new_session("s1", "/w", 10)).unwrap();
        db.append_session_messages(
            "s1",
            &[user("u1"), assistant("a1"), user("u2"), assistant("a2")],
            20,
        )
        .unwrap();
        db.insert_compaction("s1", 2, "前两轮的摘要", 30).unwrap();

        // 有效历史 = 最近一次压缩之后的消息：seq >= upto_seq + 1
        let rest = db.session_messages_from("s1", 3).unwrap();
        assert_eq!(rest.iter().map(|m| m.seq).collect::<Vec<_>>(), [3, 4]);
        assert_eq!(rest[0].content, "u2");
        assert_eq!(rest[1].content, "a2");

        // 从 0 / 1 起读时什么都不跳（原始消息一条不删）
        assert_eq!(db.session_messages_from("s1", 0).unwrap().len(), 4);
        assert_eq!(db.session_messages_from("s1", 1).unwrap().len(), 4);
    }

    #[test]
    fn insert_compaction_and_latest_compaction_return_the_greatest_id() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&new_session("s1", "/w", 10)).unwrap();
        db.insert_session(&new_session("s2", "/w", 10)).unwrap();

        db.insert_compaction("s1", 2, "第一份", 100).unwrap();
        db.insert_compaction("s1", 6, "第二份", 200).unwrap();
        db.insert_compaction("s2", 4, "别家会话", 300).unwrap();

        let latest = db.latest_compaction("s1").unwrap().expect("compaction row");
        assert_eq!(latest.id, 2);
        assert_eq!(latest.session_id, "s1");
        assert_eq!(latest.upto_seq, 6);
        assert_eq!(latest.summary, "第二份");
        assert_eq!(latest.created_at, 200);

        assert_eq!(db.latest_compaction("s2").unwrap().unwrap().upto_seq, 4);
        assert!(db.latest_compaction("s3").unwrap().is_none());
    }

    /// `skills_used`：逗号分隔落库；空表写 NULL（`task show` / `/tasks` 靠 None 判「没记」）；
    /// 未知 id 与 `finish_task` 同一口径报 `TaskNotFound`。
    #[test]
    fn set_task_skills_used_writes_a_joined_list_and_empty_writes_null() {
        let db = Db::open_in_memory().unwrap();
        let mut task = new_task("01");
        task.session_id = Some("s1".into());
        db.insert_task(&task).unwrap();

        let rec = db.get_task("01").unwrap().unwrap();
        assert_eq!(rec.session_id.as_deref(), Some("s1"));
        assert_eq!(rec.skills_used, None);

        db.set_task_skills_used("01", &["a".to_string(), "b".to_string()])
            .unwrap();
        assert_eq!(
            db.get_task("01").unwrap().unwrap().skills_used.as_deref(),
            Some("a,b")
        );

        db.set_task_skills_used("01", &[]).unwrap();
        assert_eq!(db.get_task("01").unwrap().unwrap().skills_used, None);

        let err = db.set_task_skills_used("nope", &["a".into()]).unwrap_err();
        assert!(matches!(err, StorageError::TaskNotFound(id) if id == "nope"));

        // 只动 skills_used：状态与关联列不跟着变
        let rec = db.get_task("01").unwrap().unwrap();
        assert_eq!(rec.status, "running");
        assert_eq!(rec.session_id.as_deref(), Some("s1"));
    }
}
