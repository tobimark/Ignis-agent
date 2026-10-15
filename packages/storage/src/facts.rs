//! 跨会话事实（memory）DAO：`facts` 表 + FTS5(trigram) 检索。
//!
//! 事实用 `(scope, workspace, key)` 定位，同一位置只留一行 `active`；写新值时旧行由
//! [`Db::supersede_fact`] 标成 `superseded` 并记下取代者，检索/列举默认只回 `active`。
//! 全文索引 `facts_fts` 是 external content 表，正文靠迁移 002 的三个触发器同步，DAO 不直接
//! 写它。

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Db, Result, StorageError};

/// 新建事实行的字段。
///
/// `status` 由 [`Db::insert_fact`] 固定写为 `active`，`created_at`/`updated_at` 取调用时刻；
/// `scope = "project"` 时 `workspace` 必须是工作区绝对路径（取值由上层保证，storage 不做校验，
/// 与 `tasks.status` 的处理一致）。
#[derive(Debug, Clone)]
pub struct NewFact {
    /// `user` / `project` / `global`。
    pub scope: String,
    /// 工作区绝对路径；`user`/`global` 事实为 `None`。
    pub workspace: Option<String>,
    pub key: String,
    pub text: String,
    /// 写入这条事实的任务 id；只在表里留痕，不随 [`FactRecord`] 返回。
    pub source_task_id: Option<String>,
}

/// `facts` 表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactRecord {
    pub id: i64,
    /// `user` / `project` / `global`。
    pub scope: String,
    /// 工作区绝对路径；`user`/`global` 事实为 `None`。
    pub workspace: Option<String>,
    pub key: String,
    pub text: String,
    /// `active` / `superseded`。
    pub status: String,
    /// 取代本行的新行 id；只有 `superseded` 行有值。
    pub superseded_by: Option<i64>,
    /// epoch 毫秒。
    pub created_at: i64,
    /// epoch 毫秒；文本被改写时同步推进。
    pub updated_at: i64,
}

const FACT_COLUMNS: &str =
    "id, scope, workspace, key, text, status, superseded_by, created_at, updated_at";
/// 连 `facts_fts` 时列名必须带表前缀：两张表都有名为 `text` 的列。
const FACT_COLUMNS_FROM_FTS: &str = "facts.id, facts.scope, facts.workspace, facts.key, \
                                     facts.text, facts.status, facts.superseded_by, \
                                     facts.created_at, facts.updated_at";
/// 走 FTS5 `MATCH` 的最短查询长度：trigram 分词凑不满一个三元组就没法查。
const FTS_MIN_CHARS: usize = 3;
/// `LIKE` 回退的转义符；SQLite 的 `LIKE` 默认没有转义符，必须显式 `ESCAPE` 声明。
const LIKE_ESCAPE: char = '\\';

fn row_to_fact(row: &Row<'_>) -> rusqlite::Result<FactRecord> {
    Ok(FactRecord {
        id: row.get("id")?,
        scope: row.get("scope")?,
        workspace: row.get("workspace")?,
        key: row.get("key")?,
        text: row.get("text")?,
        status: row.get("status")?,
        superseded_by: row.get("superseded_by")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// 当前 epoch 毫秒；系统时钟早于 `UNIX_EPOCH` 时取 0（不 panic）。
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// FTS5 的 `MATCH` 查询串：整体包成一个短语，内部引号翻倍转义。
///
/// 不这样处理的话，模型/用户给出的普通文本里的 `"`、`*`、`OR` 会被当成 FTS5 查询语法，
/// 轻则检索式被改写，重则直接报错。
fn fts_phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// `LIKE` 的模式串：`%`、`_` 与转义符自身都按字面量匹配（先转义再包两端的 `%`）。
fn like_pattern(query: &str) -> String {
    let mut pattern = String::with_capacity(query.len() + 2);
    pattern.push('%');
    for ch in query.chars() {
        if ch == LIKE_ESCAPE || ch == '%' || ch == '_' {
            pattern.push(LIKE_ESCAPE);
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

impl Db {
    /// 插入一条 `active` 事实，返回新行 id。
    pub fn insert_fact(&self, fact: &NewFact) -> Result<i64> {
        let now = now_millis();
        self.with_conn(|c| {
            c.prepare_cached(
                "INSERT INTO facts (scope, workspace, key, text, status, superseded_by,
                 source_task_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'active', NULL, ?5, ?6, ?6)",
            )?
            .execute(params![
                fact.scope,
                fact.workspace,
                fact.key,
                fact.text,
                fact.source_task_id,
                now
            ])?;
            Ok(c.last_insert_rowid())
        })
    }

    /// 把 `old_id` 标成被 `by_id` 取代：只改身份，不动文本与时间。行不存在时报错。
    pub fn supersede_fact(&self, old_id: i64, by_id: i64) -> Result<()> {
        let updated = self.with_conn(|c| {
            c.prepare_cached(
                "UPDATE facts SET status = 'superseded', superseded_by = ?2 WHERE id = ?1",
            )?
            .execute(params![old_id, by_id])
        })?;
        if updated == 0 {
            return Err(StorageError::FactNotFound(old_id));
        }
        Ok(())
    }

    /// 按 `(scope, workspace, key)` 取 `active` 行。
    ///
    /// `workspace` 用 `IS` 比较：`None` 与具体字符串都按「相等」处理，同 key 的不同工作区
    /// 各自独立。
    pub fn active_fact_by_key(
        &self,
        scope: &str,
        workspace: Option<&str>,
        key: &str,
    ) -> Result<Option<FactRecord>> {
        let sql = format!(
            "SELECT {FACT_COLUMNS} FROM facts
             WHERE scope = ?1 AND workspace IS ?2 AND key = ?3 AND status = 'active'
             ORDER BY id DESC LIMIT 1"
        );
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            stmt.query_row(params![scope, workspace, key], row_to_fact)
                .optional()
        })
    }

    /// 某工作区的全部 `active` `project` 事实，新近的在前。
    pub fn active_facts_in_workspace(&self, workspace: &str) -> Result<Vec<FactRecord>> {
        let sql = format!(
            "SELECT {FACT_COLUMNS} FROM facts
             WHERE scope = 'project' AND workspace = ?1 AND status = 'active'
             ORDER BY updated_at DESC, id DESC"
        );
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![workspace], row_to_fact)?;
            rows.collect()
        })
    }

    /// 全部事实，新近的在前；`include_superseded = false` 时只回 `active` 行。
    pub fn list_facts(&self, include_superseded: bool) -> Result<Vec<FactRecord>> {
        let filter = if include_superseded {
            ""
        } else {
            "WHERE status = 'active'"
        };
        let sql =
            format!("SELECT {FACT_COLUMNS} FROM facts {filter} ORDER BY updated_at DESC, id DESC");
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map([], row_to_fact)?;
            rows.collect()
        })
    }

    /// 检索事实：`query.chars().count() >= 3` 走 FTS5(trigram) 的 `MATCH`（`bm25` 相关度优先、
    /// 同分按新近度），更短的查询走 `LIKE` 回退并把 `%`/`_` 当字面量。
    ///
    /// 两个分支都只看 `text` 列，也都只回 `active` 行——被取代的旧文本绝不重新浮出。空查询
    /// 在 `LIKE` 分支下会命中全部 active 行，是否检索由上层按「查询至少 3 个字符」自己把关。
    pub fn search_facts(&self, query: &str, limit: usize) -> Result<Vec<FactRecord>> {
        let limit = limit as i64;
        if query.chars().count() >= FTS_MIN_CHARS {
            let sql = format!(
                "SELECT {FACT_COLUMNS_FROM_FTS} FROM facts_fts
                 JOIN facts ON facts.id = facts_fts.rowid
                 WHERE facts_fts MATCH ?1 AND facts.status = 'active'
                 ORDER BY bm25(facts_fts), facts.updated_at DESC, facts.id DESC LIMIT ?2"
            );
            let match_query = fts_phrase(query);
            self.with_conn(|c| {
                let mut stmt = c.prepare_cached(&sql)?;
                let rows = stmt.query_map(params![match_query, limit], row_to_fact)?;
                rows.collect()
            })
        } else {
            let sql = format!(
                "SELECT {FACT_COLUMNS} FROM facts
                 WHERE status = 'active' AND text LIKE ?1 ESCAPE '\\'
                 ORDER BY updated_at DESC, id DESC LIMIT ?2"
            );
            let pattern = like_pattern(query);
            self.with_conn(|c| {
                let mut stmt = c.prepare_cached(&sql)?;
                let rows = stmt.query_map(params![pattern, limit], row_to_fact)?;
                rows.collect()
            })
        }
    }

    /// 按 id 读取一行（`superseded` 行也照回）。
    pub fn get_fact(&self, id: i64) -> Result<Option<FactRecord>> {
        let sql = format!("SELECT {FACT_COLUMNS} FROM facts WHERE id = ?1");
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            stmt.query_row(params![id], row_to_fact).optional()
        })
    }

    /// 改写事实文本并推进 `updated_at`（FTS 索引由触发器同步）。行不存在时报错。
    pub fn update_fact_text(&self, id: i64, text: &str) -> Result<()> {
        let now = now_millis();
        let updated = self.with_conn(|c| {
            c.prepare_cached("UPDATE facts SET text = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![id, text, now])
        })?;
        if updated == 0 {
            return Err(StorageError::FactNotFound(id));
        }
        Ok(())
    }

    /// 物理删除一行（FTS 索引由触发器同步）。行不存在时报错。
    pub fn delete_fact(&self, id: i64) -> Result<()> {
        let deleted = self.with_conn(|c| {
            c.prepare_cached("DELETE FROM facts WHERE id = ?1")?
                .execute(params![id])
        })?;
        if deleted == 0 {
            return Err(StorageError::FactNotFound(id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::{fts_phrase, like_pattern, now_millis};
    use crate::db::{Db, StorageError};
    use crate::facts::NewFact;

    fn fact(scope: &str, workspace: Option<&str>, key: &str, text: &str) -> NewFact {
        NewFact {
            scope: scope.into(),
            workspace: workspace.map(str::to_owned),
            key: key.into(),
            text: text.into(),
            source_task_id: None,
        }
    }

    fn ids(records: &[super::FactRecord]) -> Vec<i64> {
        records.iter().map(|rec| rec.id).collect()
    }

    #[test]
    fn insert_fact_roundtrips_as_an_active_row() {
        let db = Db::open_in_memory().unwrap();
        let before = now_millis();
        let id = db
            .insert_fact(&fact("user", None, "pkg_manager", "使用 uv"))
            .unwrap();
        let after = now_millis();

        let rec = db.get_fact(id).unwrap().expect("fact row");
        assert_eq!(rec.id, id);
        assert_eq!(rec.scope, "user");
        assert_eq!(rec.workspace, None);
        assert_eq!(rec.key, "pkg_manager");
        assert_eq!(rec.text, "使用 uv");
        assert_eq!(rec.status, "active");
        assert_eq!(rec.superseded_by, None);
        assert!(
            rec.created_at >= before && rec.created_at <= after,
            "created_at = {} 不在 [{before}, {after}] 内",
            rec.created_at
        );
        assert_eq!(rec.updated_at, rec.created_at);

        assert!(db.get_fact(id + 1).unwrap().is_none());
    }

    #[test]
    fn insert_fact_persists_workspace_and_source_task_id() {
        let db = Db::open_in_memory().unwrap();
        let mut new = fact("project", Some("/w/one"), "pkg_manager", "使用 uv");
        new.source_task_id = Some("01".into());
        let id = db.insert_fact(&new).unwrap();

        assert_eq!(
            db.get_fact(id).unwrap().unwrap().workspace.as_deref(),
            Some("/w/one")
        );
        // source_task_id 只在表里留痕，不随 FactRecord 返回
        let stored: Option<String> = db
            .with_conn(|c| {
                c.query_row(
                    "SELECT source_task_id FROM facts WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(stored.as_deref(), Some("01"));
    }

    #[test]
    fn active_fact_by_key_only_returns_active_rows() {
        let db = Db::open_in_memory().unwrap();
        let old = db.insert_fact(&fact("user", None, "k", "旧值")).unwrap();
        let new = db.insert_fact(&fact("user", None, "k", "新值")).unwrap();
        db.supersede_fact(old, new).unwrap();

        let active = db.active_fact_by_key("user", None, "k").unwrap().unwrap();
        assert_eq!((active.id, active.text.as_str()), (new, "新值"));

        // 同 key 不同 scope 互不影响
        assert!(
            db.active_fact_by_key("global", None, "k")
                .unwrap()
                .is_none()
        );
        // 同 key 不同 key 名当然也查不到
        assert!(
            db.active_fact_by_key("user", None, "other")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn active_facts_in_workspace_is_scoped_to_that_workspace() {
        let db = Db::open_in_memory().unwrap();
        let kept = db
            .insert_fact(&fact("project", Some("/w/a"), "k1", "A 的事实"))
            .unwrap();
        let replaced = db
            .insert_fact(&fact("project", Some("/w/a"), "k2", "A 的旧事实"))
            .unwrap();
        let replacement = db
            .insert_fact(&fact("project", Some("/w/a"), "k2", "A 的新事实"))
            .unwrap();
        db.supersede_fact(replaced, replacement).unwrap();
        db.insert_fact(&fact("project", Some("/w/b"), "k1", "B 的事实"))
            .unwrap();
        db.insert_fact(&fact("user", None, "k1", "用户级事实"))
            .unwrap();

        // 只回本工作区的 active project 事实；同 key 的 B 工作区与 user 行都不算
        assert_eq!(
            ids(&db.active_facts_in_workspace("/w/a").unwrap()),
            vec![replacement, kept]
        );
        assert_eq!(
            ids(&db.active_facts_in_workspace("/w/b").unwrap()),
            vec![
                db.active_fact_by_key("project", Some("/w/b"), "k1")
                    .unwrap()
                    .unwrap()
                    .id
            ]
        );
        assert!(
            db.active_facts_in_workspace("/w/missing")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn list_facts_can_include_superseded_rows() {
        let db = Db::open_in_memory().unwrap();
        let old = db.insert_fact(&fact("user", None, "k", "旧值")).unwrap();
        let new = db.insert_fact(&fact("user", None, "k", "新值")).unwrap();
        db.supersede_fact(old, new).unwrap();

        assert_eq!(ids(&db.list_facts(false).unwrap()), vec![new]);

        let mut all = ids(&db.list_facts(true).unwrap());
        all.sort();
        assert_eq!(all, vec![old, new]);
    }

    #[test]
    fn update_fact_text_rewrites_text_and_advances_updated_at() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_fact(&fact("user", None, "k", "旧值")).unwrap();
        let created = db.get_fact(id).unwrap().unwrap().created_at;

        // 毫秒时钟：睡过一格再改，才能断言时间真的被推进
        thread::sleep(Duration::from_millis(5));
        db.update_fact_text(id, "新值").unwrap();

        let rec = db.get_fact(id).unwrap().unwrap();
        assert_eq!(rec.text, "新值");
        assert_eq!(rec.created_at, created);
        assert!(rec.updated_at > created, "{rec:?}");
    }

    #[test]
    fn supersede_keeps_text_and_only_changes_identity() {
        let db = Db::open_in_memory().unwrap();
        let old = db
            .insert_fact(&fact("project", Some("/w"), "k", "旧值"))
            .unwrap();
        let new = db
            .insert_fact(&fact("project", Some("/w"), "k", "新值"))
            .unwrap();

        db.supersede_fact(old, new).unwrap();

        let rec = db.get_fact(old).unwrap().unwrap();
        assert_eq!(rec.text, "旧值");
        assert_eq!(rec.status, "superseded");
        assert_eq!(rec.superseded_by, Some(new));
        // 新行不受影响
        let rec = db.get_fact(new).unwrap().unwrap();
        assert_eq!((rec.status.as_str(), rec.superseded_by), ("active", None));
    }

    #[test]
    fn missing_rows_are_reported_as_not_found() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_fact(&fact("user", None, "k", "值")).unwrap();
        let missing = id + 99;

        assert!(
            matches!(db.supersede_fact(missing, id).unwrap_err(), StorageError::FactNotFound(m) if m == missing)
        );
        assert!(
            matches!(db.update_fact_text(missing, "x").unwrap_err(), StorageError::FactNotFound(m) if m == missing)
        );
        assert!(
            matches!(db.delete_fact(missing).unwrap_err(), StorageError::FactNotFound(m) if m == missing)
        );

        // 行在时是普通的 Ok；删掉之后再动它就报 not found
        db.update_fact_text(id, "新值").unwrap();
        db.delete_fact(id).unwrap();
        assert!(
            matches!(db.supersede_fact(id, id).unwrap_err(), StorageError::FactNotFound(m) if m == id)
        );
    }

    #[test]
    fn search_respects_the_limit_and_falls_back_for_short_queries() {
        let db = Db::open_in_memory().unwrap();
        let first = db
            .insert_fact(&fact("user", None, "k1", "uv 环境一句话"))
            .unwrap();
        let second = db
            .insert_fact(&fact("user", None, "k2", "uv 环境另一句"))
            .unwrap();

        // 长查询走 FTS：两条都命中，limit 生效
        let hits = db.search_facts("uv 环境", 10).unwrap();
        let mut all = ids(&hits);
        all.sort();
        assert_eq!(all, vec![first, second]);
        assert_eq!(db.search_facts("uv 环境", 1).unwrap().len(), 1);

        // 短查询走 LIKE：`uv` 两个字符也能命中
        assert_eq!(db.search_facts("uv", 10).unwrap().len(), 2);
        // 没写过的子串不命中
        assert!(db.search_facts("poetry", 10).unwrap().is_empty());
    }

    #[test]
    fn fts_phrase_wraps_and_escapes_quotes() {
        assert_eq!(fts_phrase("项目使用"), "\"项目使用\"");
        assert_eq!(fts_phrase("say \"hi"), "\"say \"\"hi\"");
    }

    #[test]
    fn like_pattern_escapes_wildcards_and_the_escape_char_itself() {
        assert_eq!(like_pattern("uv"), "%uv%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        assert_eq!(like_pattern("50%"), "%50\\%%");
        // 转义符自身也要转义，否则 `\a` 会吃掉后面那个字符
        assert_eq!(like_pattern("\\a"), "%\\\\a%");
    }
}
