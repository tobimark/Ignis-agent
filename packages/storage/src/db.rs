use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::migrations::migrate;

/// 持久化层错误。
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// SQLite 语句执行或取值转换失败。
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// 建目录等文件系统操作失败。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// 任务日志行等 JSON 互转失败。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// 试图收尾一个不存在的任务行。
    #[error("task not found: {0}")]
    TaskNotFound(String),
    /// 试图读改一条不存在的事实行。
    #[error("fact not found: {0}")]
    FactNotFound(i64),
    /// 试图读改一条不存在的调度任务行。
    #[error("job not found: {0}")]
    JobNotFound(String),
    /// 试图读改一条不存在的调度运行行。
    #[error("job run not found: {0}")]
    JobRunNotFound(String),
    /// 数据库的 schema 版本高于本程序已知的迁移数（被更新版本的 ignis 升级过）。
    #[error(
        "数据库版本过新：user_version={found}，本程序只支持 {supported} 个迁移（请升级 ignis）"
    )]
    SchemaTooNew { found: i64, supported: usize },
}

pub(crate) type Result<T> = std::result::Result<T, StorageError>;

/// 当前 epoch 毫秒；系统时钟早于 `UNIX_EPOCH` 时取 0（不 panic）。
///
/// 新建行的 `created_at` 等时间戳由 DAO 自己取，调用方不用传时钟；上层的调度循环、告警与 CLI
/// 也复用这一份（`core` / `gateway` / CLI 都依赖 storage，不再各抄一遍 `SystemTime` 换算）。
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// 当前 epoch 秒（`now_millis()` 向下取整，与 [`now_millis`] 同一把尺子）。
pub fn now_secs() -> i64 {
    now_millis().div_euclid(1_000)
}

/// SQLite 连接持有者：内部为 `Mutex<Connection>`，DAO 一律经 [`Db::with_conn`] 串行访问。
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// 打开（必要时创建）数据库文件：建父目录、开启 WAL 与外键、执行迁移。
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::prepare(Connection::open(path)?)
    }

    /// 打开内存库（测试用）。
    ///
    /// `:memory:` 上 `journal_mode` 只能是 `memory`，故此处不校验该 PRAGMA 的返回值。
    pub fn open_in_memory() -> Result<Db> {
        Self::prepare(Connection::open_in_memory()?)
    }

    /// 设置连接级 PRAGMA 并应用迁移。
    fn prepare(conn: Connection) -> Result<Db> {
        // busy_timeout：CLI 与 daemon 共库时，写锁冲突等 5s 再报错，而不是立刻失败
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;
        migrate(&conn)?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    /// 在连接上执行闭包；闭包返回的 `rusqlite` 错误会转换为 [`StorageError`]。
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        // 锁中毒说明持锁线程 panic 了；连接本身仍可用，直接取回继续。
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&conn).map_err(StorageError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::db::Db;

    /// 唯一临时目录：测试可能并发执行，目录名带进程号与纳秒时间戳。
    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ignis-storage-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn table_exists(db: &Db, name: &str) -> bool {
        db.with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |row| row.get::<_, i64>(0),
            )
        })
        .unwrap()
            > 0
    }

    fn user_version(db: &Db) -> i64 {
        db.with_conn(|c| c.query_row("PRAGMA user_version", [], |row| row.get(0)))
            .unwrap()
    }

    #[test]
    fn open_creates_schema_and_bumps_user_version() {
        let dir = temp_dir("schema");
        let path = dir.join("ignis.db");

        let db = Db::open(&path).unwrap();
        assert!(table_exists(&db, "tasks"));
        assert!(table_exists(&db, "approvals"));
        assert!(table_exists(&db, "facts"));
        assert!(table_exists(&db, "facts_fts"));
        assert!(table_exists(&db, "jobs"));
        assert!(table_exists(&db, "job_runs"));
        assert!(table_exists(&db, "alerts"));
        assert!(table_exists(&db, "sessions"));
        assert!(table_exists(&db, "session_messages"));
        assert!(table_exists(&db, "session_compactions"));
        assert_eq!(user_version(&db), 4);
        drop(db);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_same_path_twice_is_a_noop_and_keeps_data() {
        let dir = temp_dir("idempotent");
        let path = dir.join("ignis.db");

        let db = Db::open(&path).unwrap();
        db.insert_task(&crate::NewTask {
            id: "01".into(),
            kind: "run".into(),
            prompt: "p".into(),
            kernel: "native".into(),
            workspace: "/w".into(),
            started_at: 1,
            session_id: None,
        })
        .unwrap();
        drop(db);

        // 第二次打开同一路径：迁移已是 no-op，既有数据仍在。
        let db = Db::open(&path).unwrap();
        assert_eq!(user_version(&db), 4);
        assert!(db.get_task("01").unwrap().is_some());
        drop(db);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_enables_wal_and_foreign_keys() {
        let dir = temp_dir("pragmas");
        let path = dir.join("ignis.db");

        let db = Db::open(&path).unwrap();
        let journal_mode: String = db
            .with_conn(|c| c.query_row("PRAGMA journal_mode", [], |row| row.get(0)))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        let foreign_keys: i64 = db
            .with_conn(|c| c.query_row("PRAGMA foreign_keys", [], |row| row.get(0)))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        drop(db);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_creates_missing_parent_directories() {
        let dir = temp_dir("parents");
        let path = dir.join("nested/deeper/ignis.db");

        let db = Db::open(&path).unwrap();
        assert!(table_exists(&db, "tasks"));
        drop(db);
        assert!(path.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_in_memory_is_migrated_and_usable() {
        let db = Db::open_in_memory().unwrap();
        assert!(table_exists(&db, "tasks"));
        assert!(table_exists(&db, "approvals"));
        assert!(table_exists(&db, "sessions"));
        assert_eq!(user_version(&db), 4);
        assert!(db.list_tasks(10).unwrap().is_empty());
    }

    /// v3 → v4 升级：造一个**真实的 v3 库**（逐条执行 001–003 的脚本、`user_version` 停在 3），再交给
    /// [`Db::open`]；004 的三张表应补齐、`user_version` 推到 4，001 时代写下的任务行一个字段不少，
    /// 两个新列是 `ALTER TABLE` 对旧行的默认值 `NULL`。
    ///
    /// 不能「先开一个 v4 库、写行、再把 `user_version` 降回 3 重开」：004 对 `tasks` 是
    /// `ALTER TABLE ADD COLUMN`，重跑会因列已存在而失败，测的就不是真实的旧库升级路径。
    /// 用例放在这里是因为要按序执行 `MIGRATIONS`（crate 内可见），集成测试只走公共 API。
    #[test]
    fn a_v3_database_upgrades_to_v4_and_keeps_earlier_rows() {
        let dir = temp_dir("upgrade-v3");
        let path = dir.join("ignis.db");

        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in &crate::migrations::MIGRATIONS[..3] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 3i64).unwrap();
        conn.execute(
            "INSERT INTO tasks (id, kind, prompt, kernel, workspace, status, summary, error,
             input_tokens, output_tokens, usage_estimated, started_at, ended_at)
             VALUES ('v3-task', 'run', '旧版本写下的任务', 'native', '/w', 'success', '已完成',
             NULL, 3, 4, 0, 42, 77)",
            [],
        )
        .unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
        drop(conn);

        let db = Db::open(&path).unwrap();
        assert_eq!(user_version(&db), 4);
        assert!(table_exists(&db, "sessions"));
        assert!(table_exists(&db, "session_messages"));
        assert!(table_exists(&db, "session_compactions"));

        let task = db.get_task("v3-task").unwrap().expect("旧任务行");
        assert_eq!(task.kind, "run");
        assert_eq!(task.prompt, "旧版本写下的任务");
        assert_eq!(task.kernel, "native");
        assert_eq!(task.workspace, "/w");
        assert_eq!(task.status, "success");
        assert_eq!(task.summary.as_deref(), Some("已完成"));
        assert_eq!(task.error, None);
        assert_eq!((task.input_tokens, task.output_tokens), (3, 4));
        assert!(!task.usage_estimated);
        assert_eq!(task.started_at, 42);
        assert_eq!(task.ended_at, Some(77));
        // 两个新列直接按 SQL 查：`ALTER TABLE` 给旧行填的是默认值 NULL，与 DAO 的字段映射无关
        let (session_id, skills_used): (Option<String>, Option<String>) = db
            .with_conn(|c| {
                c.query_row(
                    "SELECT session_id, skills_used FROM tasks WHERE id = 'v3-task'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .unwrap();
        assert_eq!(session_id, None);
        assert_eq!(skills_used, None);
        drop(db);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
