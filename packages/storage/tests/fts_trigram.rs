//! FTS5 + trigram 能力门槛（设计文档风险 #3）：bundled SQLite 必须支持中文子串检索。
//!
//! 已知约束：trigram 分词的 `MATCH` 查询要求查询串不少于 3 个 unicode 字符（少于一个三元组
//! 就无法切词）；更短的子串要走 trigram 索引加速的 `LIKE`——P2 的记忆检索按这个边界选择
//! 查询方式，因此这里两条路径都断言。

#[test]
fn bundled_sqlite_supports_fts5_trigram_chinese_substring() {
    let db = ignis_storage::Db::open_in_memory().unwrap();
    db.with_conn(|c| {
        c.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(x, tokenize='trigram');
             INSERT INTO t(x) VALUES ('项目使用 uv 管理 Python 环境');",
        )
    })
    .unwrap();
    let n: i64 = db
        .with_conn(|c| {
            c.query_row("SELECT count(*) FROM t WHERE t MATCH 'Python'", [], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(n, 1);
    // 中文子串检索：trigram 的 MATCH 要求查询串不少于 3 个 unicode 字符（`项目使用` 是 4 个），
    // 这条断言证明的是「中文子串真的能被检索到」，不只是「bundled SQLite 认 fts5」
    let n: i64 = db
        .with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM t WHERE t MATCH '项目使用'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(n, 1);
    // 不在文本里的中文串不得命中（MATCH 的否定面）
    let n: i64 = db
        .with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM t WHERE t MATCH '项目管理'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(n, 0);
    let n: i64 = db
        .with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM t WHERE x LIKE '%uv 管理%'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(n, 1);
}
