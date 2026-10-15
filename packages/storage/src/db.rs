use std::path::Path;
use std::sync::Mutex;

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
}

pub(crate) type Result<T> = std::result::Result<T, StorageError>;

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
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
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
        assert_eq!(user_version(&db), 1);
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
        })
        .unwrap();
        drop(db);

        // 第二次打开同一路径：迁移已是 no-op，既有数据仍在。
        let db = Db::open(&path).unwrap();
        assert_eq!(user_version(&db), 1);
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
        assert_eq!(user_version(&db), 1);
        assert!(db.list_tasks(10).unwrap().is_empty());
    }
}
