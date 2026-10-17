//! `ignis memory list|search|add|edit|delete`：长期记忆的命令行入口。
//!
//! 这一组命令**不装配会话**（不构造模型客户端，因此不需要 API key）：只落地数据目录、打开
//! 任务库，其余读写全部交给 `MemoryStore`——与 agent 用的是同一套语义（supersede、镜像刷新）。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, bail};
use ignis_core::config::{ensure_home, ignis_home, load_config};
use ignis_core::memory::{FactScope, MemoryStore, remembered_notice};
use ignis_storage::{Db, FactRecord};

use super::resolve_workspace;
use super::task::{format_time, one_line, render_table, truncate};
use crate::cli::{Cli, MemoryCommand};

/// `text` 列的字符宽：超出按 unicode 字符截断并补 `…`（按字节切会切坏中文）。
const TEXT_CHARS: usize = 60;
/// `search` 最多返回的事实数：与 `task list` 的默认量级一致，避免长尾刷屏。
const SEARCH_LIMIT: usize = 20;

/// `ignis memory`：五条子命令都只碰记忆库与镜像。
pub fn run(cli: &Cli, command: &MemoryCommand) -> Result<ExitCode> {
    let home = ignis_home();
    let out = match command {
        MemoryCommand::List { all } => list_at(&home, *all)?,
        MemoryCommand::Search { query } => search_at(&home, query)?,
        MemoryCommand::Add { scope, key, text } => {
            let scope = parse_scope(scope)?;
            // project 事实绑定工作区：`--workspace` 优先，否则配置里的 `workspace_root`；
            // 目录不存在时与 `Session::new` 一样在碰库之前报错
            let workspace = match scope {
                FactScope::Project => Some(project_workspace(cli, &home)?),
                FactScope::User | FactScope::Global => None,
            };
            add_at(&home, scope, workspace.as_deref(), key, text)?
        }
        MemoryCommand::Edit { id, text } => edit_at(&home, parse_id(id)?, text)?,
        MemoryCommand::Delete { id } => delete_at(&home, parse_id(id)?)?,
    };
    print!("{out}");
    Ok(ExitCode::SUCCESS)
}

/// 只碰记忆库的轻量装配：落地数据目录 + 打开任务库 + 绑定镜像目录。
///
/// 与 `Session::new` 的唯一区别是不构造模型客户端，所以没有 API key 也能读写记忆。
fn store_at(home: &Path) -> Result<Arc<MemoryStore>> {
    ensure_home(home)?;
    let db = Arc::new(Db::open(&home.join("ignis.db"))?);
    Ok(MemoryStore::new(db, home.join("memory")))
}

/// 列出事实（新近在前）；`all = false` 只看 `active`。
pub(crate) fn list_at(home: &Path, all: bool) -> Result<String> {
    Ok(render_facts(&store_at(home)?.list(all)?))
}

/// 按关键词检索；列与 `list` 完全一致。
///
/// 空白检索词在碰库之前报错：空串会落进 storage 的 `LIKE` 分支（`LIKE '%%'`）并匹配**全部**
/// active 行，看起来像一次「什么都没输入」的全量转储。首尾空白按记忆写入路径的一贯口径裁掉。
pub(crate) fn search_at(home: &Path, query: &str) -> Result<String> {
    let query = query.trim();
    if query.is_empty() {
        bail!("检索词不能为空");
    }
    Ok(render_facts(&store_at(home)?.search(query, SEARCH_LIMIT)?))
}

/// `--scope project` 要绑定的工作区：`--workspace` 优先，否则配置里的 `workspace_root`。
///
/// 与 `Session::new` 共用 [`resolve_workspace`] 的目录守卫：`--workspace /nope` 这种拼错的路径
/// 直接报错，不会存下一条指不到目录的 project 事实。
fn project_workspace(cli: &Cli, home: &Path) -> Result<PathBuf> {
    resolve_workspace(cli, &load_config(home)?)
}

/// 新增一条事实：与元工具 `remember` 同一套语义（同 `(scope, workspace, key)` 的旧值被取代）。
///
/// 回执也走元工具那份 [`remembered_notice`]（core 里的唯一实现，两边逐字节一致）：用的是**入库后**
/// 的 key/text（写入路径已归一化单行并截断），命令行里带换行的原文不会撑成多行回执；
/// `project` 的工作区由调用方解析后传入。
pub(crate) fn add_at(
    home: &Path,
    scope: FactScope,
    workspace: Option<&Path>,
    key: &str,
    text: &str,
) -> Result<String> {
    let store = store_at(home)?;
    // 写入前的 active 行：写完拿 id 一比就知道这次是不是真的取代了旧值
    let previous = store.recall_by_key(scope, workspace, key)?;
    let record = store.remember(scope, workspace, key, text, None)?;

    let superseded = previous.is_some_and(|old| old.id != record.id);
    Ok(format!(
        "{}\n",
        remembered_notice(scope, &record.key, &record.text, superseded)
    ))
}

/// 改写一条事实的文本（镜像随之刷新）；id 不存在时上抛 `memory not found: {id}`。
pub(crate) fn edit_at(home: &Path, id: i64, text: &str) -> Result<String> {
    let record = store_at(home)?.edit(id, text)?;
    Ok(format!("已改写（id={}）：{}\n", record.id, record.text))
}

/// 删除一条事实（镜像随之刷新）；id 不存在时上抛 `memory not found: {id}`。
pub(crate) fn delete_at(home: &Path, id: i64) -> Result<String> {
    store_at(home)?.delete(id)?;
    Ok(format!("已删除（id={id}）\n"))
}

/// 事实表：`ID SCOPE KEY TEXT UPDATED`，一行一条（与 `task list` 同一套渲染）。
fn render_facts(facts: &[FactRecord]) -> String {
    let mut rows = vec![vec![
        "ID".to_string(),
        "SCOPE".to_string(),
        "KEY".to_string(),
        "TEXT".to_string(),
        "UPDATED".to_string(),
    ]];
    for fact in facts {
        rows.push(vec![
            fact.id.to_string(),
            fact.scope.clone(),
            one_line(&fact.key),
            truncate(&one_line(&fact.text), TEXT_CHARS),
            format_time(fact.updated_at),
        ]);
    }
    render_table(&rows)
}

/// 事实 id：`memory list` 的第一列。非数字直接报错（`error: …` 退出 1，不走 clap 的用法错误）。
pub(crate) fn parse_id(raw: &str) -> Result<i64> {
    let Ok(id) = raw.parse::<i64>() else {
        bail!("memory id 必须是数字：{raw}");
    };
    Ok(id)
}

/// `--scope` 的字面量：与 `facts.scope` 的取值一一对应。
pub(crate) fn parse_scope(raw: &str) -> Result<FactScope> {
    match raw {
        "user" => Ok(FactScope::User),
        "project" => Ok(FactScope::Project),
        "global" => Ok(FactScope::Global),
        other => bail!("--scope 只能是 user、project 或 global，收到「{other}」"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_core::memory::FactScope;
    use ignis_storage::FactRecord;

    use super::*;

    /// `2026-10-15T12:00:00Z` 的 epoch 毫秒（与 task 侧测试同源）。
    const UPDATED_AT: i64 = 1_792_065_600_000;

    /// 单个测试独占的临时 home，析构时清理。
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-cli-memory-{}-{label}-{}",
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

    /// 一行拆成单元格：表格用两个空格分隔，列宽不足处补空格（列宽由最宽单元格决定）。
    fn cells(line: &str) -> Vec<&str> {
        line.split("  ")
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect()
    }

    /// 只有 `--workspace` 一项不同的最小 `Cli`（其余取默认值，命令自身不参与解析）。
    fn cli_at(workspace: Option<&Path>) -> Cli {
        Cli {
            // 不给 `--kernel`：与真实默认路径一致（由配置 `default_kernel` 兜底）
            kernel: None,
            workspace: workspace.map(Path::to_path_buf),
            yes: false,
            unsafe_ops: false,
            continue_session: false,
            command: None,
        }
    }

    fn fact(
        id: i64,
        scope: &str,
        workspace: Option<&str>,
        key: &str,
        text: &str,
        status: &str,
    ) -> FactRecord {
        FactRecord {
            id,
            scope: scope.into(),
            workspace: workspace.map(str::to_owned),
            key: key.into(),
            text: text.into(),
            status: status.into(),
            superseded_by: None,
            created_at: UPDATED_AT,
            updated_at: UPDATED_AT,
        }
    }

    #[test]
    fn rows_show_id_scope_key_truncated_text_and_updated_at() {
        let long = "改".repeat(70);
        let facts = vec![
            fact(1, "user", None, "pkg_manager", "使用 uv 管理依赖", "active"),
            fact(2, "project", Some("/w/a"), "editor", &long, "superseded"),
        ];

        let out = render_facts(&facts);

        // 表头 + 一行一条事实，列序就是钉死的五列
        assert_eq!(out.lines().count(), 3, "{out}");
        assert_eq!(
            cells(out.lines().nth(1).unwrap()),
            [
                "1",
                "user",
                "pkg_manager",
                "使用 uv 管理依赖",
                "2026-10-15T12:00:00Z"
            ],
            "{out}"
        );
        // text 列按**字符**截到 60（中文不会被切坏），超出补 `…`
        assert!(out.contains(&format!("{}…", "改".repeat(60))), "{out}");
        assert!(!out.contains(&"改".repeat(61)), "{out}");
    }

    #[test]
    fn add_writes_through_the_store_and_list_shows_it() {
        let home = TempHome::new("add-list");

        let out = add_at(
            home.path(),
            FactScope::User,
            None,
            "pkg_manager",
            "使用 uv 管理依赖",
        )
        .unwrap();
        // 与元工具 `remember` 的回显同形
        assert_eq!(out, "已记住（user/pkg_manager）：使用 uv 管理依赖\n");

        let listed = list_at(home.path(), false).unwrap();
        assert!(listed.contains("pkg_manager"), "{listed}");
        assert!(listed.contains("使用 uv 管理依赖"), "{listed}");

        // 同位置的第二次写入取代旧值：默认列表只剩新值，`--all` 才看得到历史行
        let out = add_at(
            home.path(),
            FactScope::User,
            None,
            "pkg_manager",
            "改用 pip",
        )
        .unwrap();
        assert_eq!(out, "已记住（user/pkg_manager）：改用 pip（旧值已失效）\n");
        let listed = list_at(home.path(), false).unwrap();
        assert!(listed.contains("改用 pip"), "{listed}");
        assert!(!listed.contains("使用 uv 管理依赖"), "{listed}");
        let all = list_at(home.path(), true).unwrap();
        assert!(all.contains("使用 uv 管理依赖"), "{all}");
        assert!(all.contains("改用 pip"), "{all}");

        // project 事实落在调用方给出的工作区上（CLI 层解析 `--workspace`/配置后传进来）
        add_at(
            home.path(),
            FactScope::Project,
            Some(Path::new("/w/a")),
            "env",
            "A 用 uv",
        )
        .unwrap();
        let stored = store_at(home.path())
            .unwrap()
            .recall_by_key(FactScope::Project, Some(Path::new("/w/a")), "env")
            .unwrap()
            .expect("project 事实应落在给定工作区");
        assert_eq!(stored.workspace.as_deref(), Some("/w/a"));
        assert_eq!(stored.text, "A 用 uv");
    }

    #[test]
    fn search_renders_the_same_rows_as_list() {
        let home = TempHome::new("search");
        add_at(
            home.path(),
            FactScope::User,
            None,
            "pkg_manager",
            "使用 uv 管理依赖",
        )
        .unwrap();
        add_at(home.path(), FactScope::User, None, "editor", "偏好 Neovim").unwrap();

        // 两个字符走 LIKE 子串回退，四个字符走 FTS(trigram)：两条路都渲染成同一种行
        let hit = search_at(home.path(), "uv").unwrap();
        assert!(hit.contains("pkg_manager"), "{hit}");
        assert!(hit.contains("使用 uv 管理依赖"), "{hit}");
        assert!(!hit.contains("Neovim"), "{hit}");
        assert_eq!(
            search_at(home.path(), "管理依赖").unwrap().lines().count(),
            2
        );

        // 没有命中时只剩表头
        assert_eq!(search_at(home.path(), "poetry").unwrap().lines().count(), 1);
    }

    #[test]
    fn search_rejects_a_blank_query_before_touching_the_store() {
        let home = TempHome::new("blank-query");
        add_at(
            home.path(),
            FactScope::User,
            None,
            "pkg_manager",
            "使用 uv 管理依赖",
        )
        .unwrap();

        // 空串会落进 storage 的 LIKE 分支（`LIKE '%%'`）匹配全部 active 行，必须在命令层挡掉：
        // 三种空白形态都报同一句话（`main` 打成 `error: 检索词不能为空`，退出 1）
        for query in ["", "   ", "\t\n"] {
            assert_eq!(
                search_at(home.path(), query).unwrap_err().to_string(),
                "检索词不能为空",
                "{query:?}"
            );
        }

        // 首尾空白裁掉后仍有内容：照常检索（表头 + 一条命中）
        assert_eq!(search_at(home.path(), "  uv  ").unwrap().lines().count(), 2);
    }

    #[test]
    fn edit_and_delete_go_through_the_store() {
        let home = TempHome::new("edit-delete");
        add_at(
            home.path(),
            FactScope::User,
            None,
            "pkg_manager",
            "使用 pip",
        )
        .unwrap();
        let id = store_at(home.path()).unwrap().list(false).unwrap()[0].id;

        // 入库前裁掉首尾空白，回显的是真正落库的那份文本
        assert_eq!(
            edit_at(home.path(), id, " 改用 uv ").unwrap(),
            format!("已改写（id={id}）：改用 uv\n")
        );
        let listed = list_at(home.path(), false).unwrap();
        assert!(listed.contains("改用 uv"), "{listed}");
        assert!(!listed.contains("使用 pip"), "{listed}");

        assert_eq!(
            delete_at(home.path(), id).unwrap(),
            format!("已删除（id={id}）\n")
        );
        let listed = list_at(home.path(), false).unwrap();
        assert!(!listed.contains("改用 uv"), "{listed}");
        assert_eq!(listed.lines().count(), 1, "{listed}");
    }

    #[test]
    fn unknown_and_non_numeric_ids_are_reported_in_the_cli_error_style() {
        let home = TempHome::new("missing");

        // 不存在的 id：`main` 会打成 `error: memory not found: 42` 并给退出码 1
        for error in [
            edit_at(home.path(), 42, "新文本").unwrap_err().to_string(),
            delete_at(home.path(), 42).unwrap_err().to_string(),
        ] {
            assert_eq!(error, "memory not found: 42");
        }

        // 非数字 id 在碰库之前就报错（而不是 clap 的用法错误 + 退出码 2）
        assert_eq!(parse_id("42").unwrap(), 42);
        assert_eq!(
            parse_id("abc").unwrap_err().to_string(),
            "memory id 必须是数字：abc"
        );
    }

    #[test]
    fn scope_literals_are_validated_before_touching_the_store() {
        assert_eq!(parse_scope("user").unwrap(), FactScope::User);
        assert_eq!(parse_scope("project").unwrap(), FactScope::Project);
        assert_eq!(parse_scope("global").unwrap(), FactScope::Global);

        let error = parse_scope("Project").unwrap_err().to_string();
        assert_eq!(
            error,
            "--scope 只能是 user、project 或 global，收到「Project」"
        );
    }

    #[test]
    fn project_scope_rejects_a_workspace_that_is_not_a_directory() {
        let dir = TempHome::new("workspace-guard");
        let typo = dir.path().join("nope");

        // 拼错的 `--workspace`：与 `Session::new` 逐字同一句报错（`main` 打成
        // `error: 工作区不存在或不是目录：…`，退出 1），不会存下一条指不到目录的 project 事实
        let error = project_workspace(&cli_at(Some(&typo)), dir.path())
            .unwrap_err()
            .to_string();
        assert_eq!(error, format!("工作区不存在或不是目录：{}", typo.display()));

        // 文件也不算工作区（与 `is_dir` 守卫同口径）
        let file = dir.path().join("file");
        fs::write(&file, "占位").unwrap();
        assert!(project_workspace(&cli_at(Some(&file)), dir.path()).is_err());

        // 真实目录照常解析
        let workspace = dir.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        assert_eq!(
            project_workspace(&cli_at(Some(&workspace)), dir.path()).unwrap(),
            workspace
        );
    }
}
