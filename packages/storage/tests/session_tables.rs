//! 迁移 004（`sessions` / `session_messages` / `session_compactions` 与 `tasks` 两个新列）的集成测试。
//!
//! 这里只走公共 API，覆盖真实文件库上的「落盘 → 关库 → 重开」与连接级的外键约束。
//! **v3 → v4 的升级用例不在这里**：要构造一个真实的 v3 库就得按序执行 001–003 的迁移脚本，而
//! `MIGRATIONS` 只对 crate 内可见（用「先建 v4 库再降 `user_version` 重开」的假 v3 库会撞上 004 的
//! `ALTER TABLE` 重复列）。那条用例放在 `src/db.rs` 的内联测试里。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ignis_storage::{Db, NewSession, NewSessionMessage, StorageError};

/// 唯一临时目录：测试可能并发执行，目录名带进程号与纳秒时间戳。
fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ignis-sessions-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn message(role: &str, content: &str) -> NewSessionMessage {
    NewSessionMessage {
        role: role.into(),
        content: content.into(),
    }
}

/// 真实文件库：会话行 / 消息 / 压缩摘要关库重开后逐字段一致，`user_version` 停在 4。
#[test]
fn session_rows_survive_a_close_and_reopen() {
    let dir = temp_dir("reopen");
    let path = dir.join("ignis.db");

    let db = Db::open(&path).unwrap();
    db.insert_session(&NewSession {
        id: "s1".into(),
        title: "改一下配置".into(),
        workspace: "/w".into(),
        kernel: "native".into(),
        model: Some("MiniMax-M2".into()),
        created_at: 10,
    })
    .unwrap();
    db.append_session_messages(
        "s1",
        &[
            message("user", "改一下配置"),
            message("assistant", "改好了"),
        ],
        20,
    )
    .unwrap();
    db.insert_compaction("s1", 2, "前两轮的摘要", 30).unwrap();
    let version: i64 = db
        .with_conn(|c| c.query_row("PRAGMA user_version", [], |row| row.get(0)))
        .unwrap();
    assert_eq!(version, 4);
    drop(db);

    let db = Db::open(&path).unwrap();
    let session = db.get_session("s1").unwrap().expect("session row");
    assert_eq!(session.title, "改一下配置");
    assert_eq!(session.model.as_deref(), Some("MiniMax-M2"));
    assert_eq!((session.created_at, session.updated_at), (10, 20));

    let messages = db.session_messages_from("s1", 1).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "改一下配置");
    assert_eq!(messages[1].seq, 2);
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].content, "改好了");

    let compaction = db.latest_compaction("s1").unwrap().expect("compaction row");
    assert_eq!(compaction.upto_seq, 2);
    assert_eq!(compaction.summary, "前两轮的摘要");

    let summaries = db.list_sessions_for_workspace("/w", 10).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].session.id, "s1");
    assert_eq!(summaries[0].message_count, 2);
    assert_eq!(db.session_ids_with_prefix("/w", "s", 10).unwrap(), ["s1"]);
    drop(db);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// 文件库上 `PRAGMA foreign_keys` 同样是开的：孤儿消息被拒，且一行都没落。
#[test]
fn foreign_keys_reject_orphan_messages_on_a_file_database() {
    let dir = temp_dir("fk");
    let path = dir.join("ignis.db");

    let db = Db::open(&path).unwrap();
    let err = db
        .append_session_messages("missing", &[message("user", "hi")], 1)
        .unwrap_err();
    assert!(matches!(err, StorageError::Sqlite(_)), "{err}");
    assert!(db.session_messages_from("missing", 1).unwrap().is_empty());
    drop(db);

    std::fs::remove_dir_all(&dir).unwrap();
}
