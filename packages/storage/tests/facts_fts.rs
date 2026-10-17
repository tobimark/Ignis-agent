//! 迁移 002（`facts` + FTS5 trigram 触发器）的集成测试。
//!
//! 迁移与触发器是数据库层面的外部可见行为，因此这里只走公共 API：`Db::open` 验证迁移状态，
//! `with_conn` 直接查 FTS 索引验证三触发器同步，DAO 验证 supersede 与检索分支（长查询走
//! `MATCH`、短查询走转义后的 `LIKE`）。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ignis_storage::{Db, NewFact, StorageError};

/// 唯一临时目录：测试可能并发执行，目录名带进程号与纳秒时间戳。
fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("ignis-facts-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fact(scope: &str, workspace: Option<&str>, key: &str, text: &str) -> NewFact {
    NewFact {
        scope: scope.into(),
        workspace: workspace.map(str::to_owned),
        key: key.into(),
        text: text.into(),
        source_task_id: None,
    }
}

fn user_version(db: &Db) -> i64 {
    db.with_conn(|c| c.query_row("PRAGMA user_version", [], |row| row.get(0)))
        .unwrap()
}

/// `sqlite_master` 里的对象是否存在（`table`/`index`/`trigger`）。
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

/// FTS 索引里某个子串的命中数：绕开 DAO 的 active 过滤，专测触发器同步。
///
/// 查询串自己包一层引号：trigram 分词的 `MATCH` 只有短语形式才是「整段子串」语义，裸写法
/// 会把空格分隔的词拆成各自独立的检索项（首词不足 3 字符时还会整条落空）。
fn fts_hits(db: &Db, query: &str) -> i64 {
    db.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM facts_fts WHERE facts_fts MATCH ?1",
            [format!("\"{query}\"")],
            |row| row.get(0),
        )
    })
    .unwrap()
}

fn ids(records: &[ignis_storage::FactRecord]) -> Vec<i64> {
    records.iter().map(|rec| rec.id).collect()
}

/// 002 引入的 facts 表/FTS5 索引/触发器在**当前全部迁移跑完之后**仍然在（名字不点名某一版
/// 迁移：这里断言的是最新库的形状，迁移 003 只加表、不动 facts）。
#[test]
fn facts_schema_survives_the_latest_migrations() {
    let dir = temp_dir("schema");
    let path = dir.join("ignis.db");

    let db = Db::open(&path).unwrap();
    // 四条迁移都已应用（004 只加会话表与 `tasks` 两列，不动 facts）
    assert_eq!(user_version(&db), 4);
    assert!(object_exists(&db, "table", "facts"));
    // external content 的 FTS5 表也是 sqlite_master 里的 table
    assert!(object_exists(&db, "table", "facts_fts"));
    assert!(object_exists(
        &db,
        "index",
        "idx_facts_scope_workspace_status"
    ));
    for trigger in ["facts_ai", "facts_ad", "facts_au"] {
        assert!(object_exists(&db, "trigger", trigger), "{trigger} 缺失");
    }
    drop(db);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reopening_a_migrated_database_is_idempotent_and_keeps_facts() {
    let dir = temp_dir("idempotent");
    let path = dir.join("ignis.db");

    let db = Db::open(&path).unwrap();
    let id = db
        .insert_fact(&fact(
            "user",
            None,
            "pkg_manager",
            "使用 uv 管理 Python 环境",
        ))
        .unwrap();
    drop(db);

    // 第二次打开：迁移是 no-op，事实行与 FTS 索引都还在
    let db = Db::open(&path).unwrap();
    assert_eq!(user_version(&db), 4);
    assert_eq!(
        db.get_fact(id).unwrap().expect("fact row").text,
        "使用 uv 管理 Python 环境"
    );
    assert_eq!(fts_hits(&db, "uv 管理"), 1);
    drop(db);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn insert_trigger_indexes_the_fact_text() {
    let db = Db::open_in_memory().unwrap();
    db.insert_fact(&fact(
        "project",
        Some("/w/one"),
        "pkg_manager",
        "项目使用 uv 管理 Python 环境",
    ))
    .unwrap();

    assert_eq!(fts_hits(&db, "项目使用"), 1);
    // 否定面：文本里没有的子串不得命中
    assert_eq!(fts_hits(&db, "项目管理"), 0);
}

#[test]
fn update_trigger_drops_the_old_text() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .insert_fact(&fact("user", None, "pkg", "项目使用 uv 管理环境"))
        .unwrap();

    db.update_fact_text(id, "项目改用 pip 管理环境").unwrap();

    assert_eq!(fts_hits(&db, "项目使用"), 0, "旧文本必须从索引里消失");
    assert_eq!(fts_hits(&db, "改用 pip"), 1);
    assert_eq!(
        db.get_fact(id).unwrap().unwrap().text,
        "项目改用 pip 管理环境"
    );
}

#[test]
fn delete_trigger_removes_the_text() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .insert_fact(&fact("user", None, "pkg", "项目使用 uv 管理环境"))
        .unwrap();

    db.delete_fact(id).unwrap();

    assert!(db.get_fact(id).unwrap().is_none());
    assert_eq!(fts_hits(&db, "项目使用"), 0);
    assert!(db.list_facts(true).unwrap().is_empty());
}

#[test]
fn supersede_marks_the_old_row_and_leaves_only_the_new_one_active() {
    let db = Db::open_in_memory().unwrap();
    let old = db
        .insert_fact(&fact(
            "project",
            Some("/w/one"),
            "pkg_manager",
            "项目使用 uv 管理",
        ))
        .unwrap();
    let new = db
        .insert_fact(&fact(
            "project",
            Some("/w/one"),
            "pkg_manager",
            "项目改用 pip 管理",
        ))
        .unwrap();

    db.supersede_fact(old, new).unwrap();

    let old_rec = db.get_fact(old).unwrap().unwrap();
    assert_eq!(old_rec.status, "superseded");
    assert_eq!(old_rec.superseded_by, Some(new));
    // 旧文本本身没被改写（只有身份变了）
    assert_eq!(old_rec.text, "项目使用 uv 管理");

    // active 查询只回新行
    let active = db
        .active_fact_by_key("project", Some("/w/one"), "pkg_manager")
        .unwrap()
        .expect("active row");
    assert_eq!((active.id, active.status.as_str()), (new, "active"));

    // 再纠正一次：仍在 active 的只有最后一行
    let newest = db
        .insert_fact(&fact(
            "project",
            Some("/w/one"),
            "pkg_manager",
            "项目改用 poetry 管理",
        ))
        .unwrap();
    db.supersede_fact(new, newest).unwrap();
    assert_eq!(
        db.active_fact_by_key("project", Some("/w/one"), "pkg_manager")
            .unwrap()
            .map(|rec| rec.id),
        Some(newest)
    );
    assert_eq!(ids(&db.list_facts(false).unwrap()), vec![newest]);
    assert_eq!(db.list_facts(true).unwrap().len(), 3);

    // 同 key 的另一个工作区不受影响
    let other = db
        .insert_fact(&fact("project", Some("/w/two"), "pkg_manager", "本地事实"))
        .unwrap();
    assert_eq!(
        db.active_fact_by_key("project", Some("/w/two"), "pkg_manager")
            .unwrap()
            .map(|rec| rec.id),
        Some(other)
    );
    // user/global 事实没有工作区维度：workspace 传 None 才查得到
    let user = db
        .insert_fact(&fact("user", None, "pkg_manager", "全局偏好"))
        .unwrap();
    assert_eq!(
        db.active_fact_by_key("user", None, "pkg_manager")
            .unwrap()
            .map(|rec| rec.id),
        Some(user)
    );
    assert!(
        db.active_fact_by_key("user", Some("/w/one"), "pkg_manager")
            .unwrap()
            .is_none()
    );
}

#[test]
fn search_uses_like_for_short_queries_and_escapes_wildcards() {
    let db = Db::open_in_memory().unwrap();
    let literal = db
        .insert_fact(&fact("user", None, "env", "配置在 a_b.env，取 50%b 的额度"))
        .unwrap();
    let decoy = db
        .insert_fact(&fact(
            "user",
            None,
            "plain",
            "配置在 axb.env，取 500b 的额度",
        ))
        .unwrap();

    // 2 个字符 → LIKE 回退；`_` 按字面量匹配，不当通配符
    assert_eq!(ids(&db.search_facts("a_", 10).unwrap()), vec![literal]);
    // `%` 同理：注入面不得让通配符生效
    let hits = db.search_facts("%b", 10).unwrap();
    assert_eq!(ids(&hits), vec![literal]);
    assert!(!ids(&hits).contains(&decoy));
}

#[test]
fn search_hits_chinese_substrings_of_three_chars_or_more() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .insert_fact(&fact("user", None, "env", "项目使用 uv 管理 Python 环境"))
        .unwrap();

    // 4 个 unicode 字符的中文子串（FTS trigram 分支）
    let hits = db.search_facts("项目使用", 10).unwrap();
    assert_eq!(ids(&hits), vec![id]);
    assert_eq!(hits[0].text, "项目使用 uv 管理 Python 环境");
    // 边界：恰好 3 个字符
    assert_eq!(ids(&db.search_facts("项目使", 10).unwrap()), vec![id]);
    // 2 个字符仍能命中（LIKE 分支）
    assert_eq!(ids(&db.search_facts("项目", 10).unwrap()), vec![id]);
    // 含空格的多词查询按「整段子串」处理（首词 2 个字符也一样）
    assert_eq!(ids(&db.search_facts("uv 管理", 10).unwrap()), vec![id]);
    // 不是连续子串就不命中：若按空格拆词做 OR，这里会误命中 `Python`
    assert!(db.search_facts("uv Python", 10).unwrap().is_empty());
    // 没写过的子串不得命中
    assert!(db.search_facts("项目部署", 10).unwrap().is_empty());
}

#[test]
fn search_never_returns_superseded_facts() {
    let db = Db::open_in_memory().unwrap();
    let old = db
        .insert_fact(&fact(
            "user",
            None,
            "pkg_manager",
            "项目使用 uv 管理 Python 环境",
        ))
        .unwrap();
    let new = db
        .insert_fact(&fact(
            "user",
            None,
            "pkg_manager",
            "项目改用 pip 管理 Python 环境",
        ))
        .unwrap();
    db.supersede_fact(old, new).unwrap();

    // FTS 分支（≥3 字符）：索引里还有旧文本，但 active 过滤必须把它挡掉
    assert!(db.search_facts("项目使用", 10).unwrap().is_empty());
    // LIKE 分支（<3 字符）：只剩新行
    assert_eq!(ids(&db.search_facts("项目", 10).unwrap()), vec![new]);
}

#[test]
fn search_orders_by_relevance_then_recency_and_respects_limit() {
    let db = Db::open_in_memory().unwrap();
    // 两行文本相同 → bm25 相同，按新近度倒序
    let older = db
        .insert_fact(&fact("user", None, "a", "uv 管理 Python 环境"))
        .unwrap();
    let newer = db
        .insert_fact(&fact("user", None, "b", "uv 管理 Python 环境"))
        .unwrap();
    assert_eq!(
        ids(&db.search_facts("Python 环境", 10).unwrap()),
        vec![newer, older]
    );
    assert_eq!(
        ids(&db.search_facts("Python 环境", 1).unwrap()),
        vec![newer]
    );

    // 相关度更高（查询串在文本里出现更多次）的行排在前面
    let weak = db.insert_fact(&fact("user", None, "c", "uv 环境")).unwrap();
    let strong = db
        .insert_fact(&fact(
            "user",
            None,
            "d",
            "uv 环境 uv 环境 uv 环境 uv 环境 uv 环境",
        ))
        .unwrap();
    let hits = db.search_facts("uv 环境", 10).unwrap();
    assert_eq!(
        ids(&hits)[..2],
        [strong, weak],
        "bm25 应把命中更多的行排在前：{hits:?}"
    );
}

#[test]
fn search_treats_fts_syntax_as_literal_text() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .insert_fact(&fact("user", None, "k", "提示 say \"hi 就结束"))
        .unwrap();

    // 孤立引号在 FTS5 查询语法里是错误：必须按字面量检索
    assert_eq!(ids(&db.search_facts("say \"hi", 10).unwrap()), vec![id]);
    // `OR` 之类的运算符不得被当成检索语言
    assert!(db.search_facts("hi OR 结束", 10).unwrap().is_empty());
}

#[test]
fn busy_timeout_is_set_for_file_and_memory_connections() {
    let dir = temp_dir("busy-timeout");
    let path = dir.join("ignis.db");

    let db = Db::open(&path).unwrap();
    let timeout: i64 = db
        .with_conn(|c| c.query_row("PRAGMA busy_timeout", [], |row| row.get(0)))
        .unwrap();
    assert_eq!(timeout, 5000);
    drop(db);

    // CLI 测试与上层多用内存库，同一套 PRAGMA 必须都生效
    let db = Db::open_in_memory().unwrap();
    let timeout: i64 = db
        .with_conn(|c| c.query_row("PRAGMA busy_timeout", [], |row| row.get(0)))
        .unwrap();
    assert_eq!(timeout, 5000);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_database_newer_than_the_program_is_rejected() {
    let dir = temp_dir("too-new");
    let path = dir.join("ignis.db");

    let db = Db::open(&path).unwrap();
    db.with_conn(|c| c.pragma_update(None, "user_version", 99i64))
        .unwrap();
    drop(db);

    let err = match Db::open(&path) {
        Ok(_) => panic!("数据库版本高于程序时 open 必须失败"),
        Err(err) => err,
    };
    let StorageError::SchemaTooNew { found, supported } = &err else {
        panic!("应报「数据库版本过新」：{err:?}");
    };
    assert_eq!(*found, 99);
    let message = err.to_string();
    assert!(message.contains("99"), "错误信息要带数据库版本：{message}");
    assert!(
        message.contains(&supported.to_string()),
        "错误信息要带本程序支持的迁移条数：{message}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
