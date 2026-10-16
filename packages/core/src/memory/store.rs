//! [`MemoryStore`]：记忆层的门面（supersede、检索注入、镜像重建、fail-soft）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignis_storage::{Db, FactRecord, NewFact};

use super::mirror;
use super::{
    FactScope, MAX_FACT_KEY_CHARS, MAX_FACT_TEXT_CHARS, MemoryError, normalize_fact_text,
    truncate_fact_field,
};

/// FTS5(trigram) 能查到的最短检索项长度：凑不满一个三元组就没法 MATCH（与 storage 同口径）。
const FTS_MIN_CHARS: usize = 3;
/// 单次注入到任务上下文的检索事实上限（`project` 事实另算，见 [`MemoryStore::prompt_facts`]）。
const RETRIEVED_FACT_LIMIT: usize = 6;
/// 从 prompt 里最多拆出的检索项数：防止长 prompt 把整库事实都捞进上下文。
const RETRIEVAL_TERM_LIMIT: usize = 8;

/// 记忆层：事实的读写与 Markdown 镜像。
///
/// 与 storage 的分工：`facts` 表的 DAO 管行级操作，这里固定上层语义——同
/// `(scope, workspace, key)` 的新值取代旧值、任务上下文的注入规则、镜像重建。镜像只是派生产物，
/// **任何镜像失败都只打一行警告**，事实本身照常入库/改写（fail-soft）。
pub struct MemoryStore {
    db: Arc<Db>,
    mirror_dir: PathBuf,
}

impl MemoryStore {
    /// 绑定数据库与镜像目录；不在构造时碰文件系统（镜像目录按需创建）。
    pub fn new(db: Arc<Db>, mirror_dir: PathBuf) -> Arc<MemoryStore> {
        Arc::new(MemoryStore { db, mirror_dir })
    }

    /// 记住一条事实：同一 `(scope, workspace, key)` 的旧值被新值取代（supersede）。
    ///
    /// 「插新行 + 标旧行」由 storage 的 [`Db::insert_fact_superseding`] 在**一个事务**里完成：
    /// 中途失败（崩溃、`SQLITE_BUSY`、磁盘满）整个事务回滚，不会留下两行同名 `active`；sweep 覆盖
    /// 同 key 的所有 active 行，历史遗留的重复行也会被顺带收敛。
    ///
    /// `scope = Project` 必须带工作区（调用方传 `ToolCtx.workspace`），`User`/`Global` 必须不带；
    /// `key`/`text` 先在 [`fact_field`] 里归一化（空白折叠、控制字符剔除）并按
    /// [`MAX_FACT_KEY_CHARS`]/[`MAX_FACT_TEXT_CHARS`] 截断，归一化后为空才拒绝。成功后尽力重写
    /// 受影响作用域的镜像。
    pub fn remember(
        &self,
        scope: FactScope,
        workspace: Option<&Path>,
        key: &str,
        text: &str,
        source_task_id: Option<&str>,
    ) -> Result<FactRecord, MemoryError> {
        let key = fact_field(key, MAX_FACT_KEY_CHARS, MemoryError::EmptyKey)?;
        let text = fact_field(text, MAX_FACT_TEXT_CHARS, MemoryError::EmptyText)?;
        // 工作区先校验：参数不对就既不落库也不动镜像
        let workspace = workspace_string(scope, workspace)?;

        let id = self.db.insert_fact_superseding(&NewFact {
            scope: scope.as_str().to_owned(),
            workspace: workspace.clone(),
            key,
            text,
            source_task_id: source_task_id.map(str::to_owned),
        })?;

        let record = self.db.get_fact(id)?.ok_or(MemoryError::NotFound { id })?;
        self.refresh_mirror(scope, workspace.as_deref());
        Ok(record)
    }

    /// 查某个 `(scope, workspace, key)` 位置上的 `active` 事实；没有就回 `None`。
    ///
    /// 给元工具 `remember` 用：写入前先看这个位置有没有旧值，写完对比 id 就知道这次是不是真的
    /// supersede 了一条旧事实。`key` 与 [`MemoryStore::remember`] 同口径（[`fact_field`] 的归一化
    /// 与截断），否则带空白的 key 会查不到自己刚写下的那行、把 supersede 误报成首次写入；工作区
    /// 校验也同一套（`project` 必须带、其它范围必须不带）。归一化后为空时照旧查不到（回 `None`）。
    pub fn recall_by_key(
        &self,
        scope: FactScope,
        workspace: Option<&Path>,
        key: &str,
    ) -> Result<Option<FactRecord>, MemoryError> {
        let workspace = workspace_string(scope, workspace)?;
        let key = truncate_fact_field(&normalize_fact_text(key), MAX_FACT_KEY_CHARS);
        Ok(self
            .db
            .active_fact_by_key(scope.as_str(), workspace.as_deref(), &key)?)
    }

    /// 任务上下文要注入的事实：本工作区的 active `project` 事实（无条件、新近在前）+ 按 prompt
    /// 检索到的 `user`/`global` 事实（最多 6 条、新近在前）。
    ///
    /// prompt 会先拆成检索项（见 [`retrieval_terms`]）；一个检索项都没有时跳过 FTS，`project`
    /// 事实照旧。**只有存储/检索失败才返回 `Err`**（调用方按 fail-soft 处理），镜像从不参与。
    pub fn prompt_facts(
        &self,
        workspace: &Path,
        prompt: &str,
    ) -> Result<Vec<FactRecord>, MemoryError> {
        let workspace = workspace.to_string_lossy();
        let mut facts = self.db.active_facts_in_workspace(&workspace)?;
        let mut seen: HashSet<i64> = facts.iter().map(|fact| fact.id).collect();
        let mut retrieved = Vec::new();
        for term in retrieval_terms(prompt) {
            for fact in self.db.search_facts(&term, RETRIEVED_FACT_LIMIT)? {
                // 只收 user/global：project 事实按工作区无条件带上，不该经检索串门
                if !matches!(fact.scope.as_str(), "user" | "global") {
                    continue;
                }
                if seen.insert(fact.id) {
                    retrieved.push(fact);
                }
            }
        }
        // 逐项检索的合并结果按新近度收敛（同毫秒按 id 兜底），顺序稳定可断言
        retrieved.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        retrieved.truncate(RETRIEVED_FACT_LIMIT);
        facts.append(&mut retrieved);
        Ok(facts)
    }

    /// CLI `memory search`：`>= 3` 个字符走 FTS(trigram) 短语、更短走 `LIKE` 子串回退。
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<FactRecord>, MemoryError> {
        Ok(self.db.search_facts(query, limit)?)
    }

    /// 列出事实（`include_superseded = false` 只回 `active`），新近在前。
    pub fn list(&self, include_superseded: bool) -> Result<Vec<FactRecord>, MemoryError> {
        Ok(self.db.list_facts(include_superseded)?)
    }

    /// 改写一条事实的文本（`updated_at` 随之推进）；行不存在时报 [`MemoryError::NotFound`]。
    ///
    /// `superseded` 行也允许改写（库里留痕的历史由调用方自己决定怎么修）；它不出现在镜像里，
    /// 刷新只是让文件与库保持一致。文本与 [`MemoryStore::remember`] 走同一条口径（归一化 + 截断）。
    pub fn edit(&self, id: i64, text: &str) -> Result<FactRecord, MemoryError> {
        let text = fact_field(text, MAX_FACT_TEXT_CHARS, MemoryError::EmptyText)?;
        let existing = self.db.get_fact(id)?.ok_or(MemoryError::NotFound { id })?;
        self.db.update_fact_text(id, &text)?;
        let record = self.db.get_fact(id)?.ok_or(MemoryError::NotFound { id })?;
        self.refresh_mirror_for(&existing);
        Ok(record)
    }

    /// 删除一条事实（物理删除，旧行不留在库里）；行不存在时报 [`MemoryError::NotFound`]。
    pub fn delete(&self, id: i64) -> Result<(), MemoryError> {
        let existing = self.db.get_fact(id)?.ok_or(MemoryError::NotFound { id })?;
        self.db.delete_fact(id)?;
        self.refresh_mirror_for(&existing);
        Ok(())
    }

    /// 按库里的数据重写所有作用域的镜像文件（覆盖手工改动；不删库外文件）。
    ///
    /// 与写入路径不同，这里的失败会**抛出**：调用方（如 CLI 的显式重建）需要知道镜像没写成。
    pub fn rebuild_mirror(&self) -> Result<(), MemoryError> {
        // superseded 行也算「出现过的作用域」：某工作区只剩 superseded 行时，它的文件也要被重写，
        // 旧文本才不会留在镜像里。
        let mut targets: HashSet<(FactScope, Option<String>)> = HashSet::new();
        for fact in self.db.list_facts(true)? {
            let Some(scope) = FactScope::from_db_str(&fact.scope) else {
                continue; // 库里出现未知 scope：镜像跳过，不 panic
            };
            let workspace = match scope {
                FactScope::Project => fact.workspace.clone(),
                FactScope::User | FactScope::Global => None,
            };
            if scope == FactScope::Project && workspace.is_none() {
                continue; // 没有工作区就起不出文件名
            }
            targets.insert((scope, workspace));
        }
        for (scope, workspace) in targets {
            self.rewrite_mirror_file(scope, workspace.as_deref())?;
        }
        Ok(())
    }

    /// 某个作用域的 active 事实（渲染镜像用），新近在前。
    fn active_facts(
        &self,
        scope: FactScope,
        workspace: Option<&str>,
    ) -> Result<Vec<FactRecord>, MemoryError> {
        match (scope, workspace) {
            (FactScope::Project, Some(workspace)) => {
                Ok(self.db.active_facts_in_workspace(workspace)?)
            }
            // 没有工作区就没有可命名的 project 文件；user/global 忽略偶发绑定值
            (FactScope::Project, None) => Ok(Vec::new()),
            (scope, _) => Ok(self
                .db
                .list_facts(false)?
                .into_iter()
                .filter(|fact| fact.scope == scope.as_str())
                .collect()),
        }
    }

    /// 重写一个作用域的镜像文件；无名可起（`project` 缺工作区）时是 no-op。
    fn rewrite_mirror_file(
        &self,
        scope: FactScope,
        workspace: Option<&str>,
    ) -> Result<(), MemoryError> {
        let Some(path) = mirror::mirror_path(&self.mirror_dir, scope, workspace) else {
            return Ok(());
        };
        let facts = self.active_facts(scope, workspace)?;
        Ok(mirror::write(
            &path,
            &mirror::render(scope, workspace, &facts),
        )?)
    }

    /// best-effort 刷新：任何失败只打一行警告，已经落地的事实不受影响（fail-soft）。
    ///
    /// 警告按失败来源分档：库读不出来（[`MemoryError::Storage`]）意味着镜像不仅没刷新，连「库里
    /// 现在有什么」都不知道，值得单独提示；文件写不进去只是派生物落后一拍。
    fn refresh_mirror(&self, scope: FactScope, workspace: Option<&str>) {
        let Err(err) = self.rewrite_mirror_file(scope, workspace) else {
            return;
        };
        let scope = scope.as_str();
        match &err {
            MemoryError::Storage(source) => eprintln!(
                "warning: 记忆镜像刷新失败（scope={scope}，workspace={workspace:?}）：\
                 读取事实失败，镜像已与数据库不一致，请检查数据库：{source}"
            ),
            other => eprintln!(
                "warning: 记忆镜像刷新失败（scope={scope}，workspace={workspace:?}）：{other}"
            ),
        }
    }

    /// 刷新一条事实所在作用域的镜像；库里出现未知 scope 时跳过（镜像只是派生物）。
    fn refresh_mirror_for(&self, fact: &FactRecord) {
        match FactScope::from_db_str(&fact.scope) {
            Some(scope) => self.refresh_mirror(scope, fact.workspace.as_deref()),
            None => eprintln!("warning: 未知的记忆 scope「{}」，跳过镜像刷新", fact.scope),
        }
    }
}

/// 写入路径的字段口径：归一化（[`normalize_fact_text`]）→ 归一化后为空则按 `empty` 报错 →
/// 截到 `max_chars` 个字符（[`truncate_fact_field`]）。库、镜像与后续系统提示拿到的永远是这一份。
fn fact_field(value: &str, max_chars: usize, empty: MemoryError) -> Result<String, MemoryError> {
    let normalized = normalize_fact_text(value);
    if normalized.is_empty() {
        return Err(empty);
    }
    Ok(truncate_fact_field(&normalized, max_chars))
}

/// 校验并归一化工作区：`project` 必须带、其它范围必须不带，路径转成库里用的字符串。
fn workspace_string(
    scope: FactScope,
    workspace: Option<&Path>,
) -> Result<Option<String>, MemoryError> {
    match (scope, workspace) {
        (FactScope::Project, Some(path)) => Ok(Some(path.to_string_lossy().into_owned())),
        (FactScope::Project, None) => Err(MemoryError::MissingWorkspace),
        (_, Some(_)) => Err(MemoryError::UnexpectedWorkspace),
        (_, None) => Ok(None),
    }
}

/// 从 prompt 里拆出 FTS 检索项，按出现顺序去重，最多 [`RETRIEVAL_TERM_LIMIT`] 个。
///
/// 不把检索项直接当整段 prompt 查：FTS 查询是「要求这串字原样出现」的短语，长 prompt 几乎必然
/// 零命中。两套切法：
/// - **ASCII 之类的词**：按非字母数字字符（空白与标点）切段，丢掉不足 3 字符的段（trigram 查不出
///   两字以下的词）；
/// - **中日韩（Han）字符段**：汉字是字母数字，一整句中文会被切成一个「词」，而整句当短语查必然
///   零命中——检索对主语言失效。所以超过 3 个字的汉字段改发**滑动 3 字窗口**
///   （`这个项目使用` → `这个项`、`个项目`、`项目使`、`目使用`），每个窗口恰好一个三元组。
fn retrieval_terms(prompt: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut run: Option<(usize, bool)> = None; // (起点字节偏移, 是否汉字段)
    for (index, ch) in prompt.char_indices() {
        let han = is_han(ch);
        if ch.is_alphanumeric() && run.is_some_and(|(_, run_han)| run_han == han) {
            continue; // 同一段继续
        }
        // 段在这里结束：先冲掉上一段，再看当前字符能不能开新段（标点与空白两件事都不做）
        if let Some((start, run_han)) = run.take() {
            push_run_terms(&mut terms, &prompt[start..index], run_han);
        }
        if ch.is_alphanumeric() {
            run = Some((index, han));
        }
    }
    if let Some((start, run_han)) = run {
        push_run_terms(&mut terms, &prompt[start..], run_han);
    }
    terms.truncate(RETRIEVAL_TERM_LIMIT);
    terms
}

/// 把一段连续的字母数字字符变成检索项：汉字段超过 3 个字就发滑动 3 字窗口，其余段整段发。
fn push_run_terms(terms: &mut Vec<String>, run: &str, han: bool) {
    if !han || run.chars().nth(FTS_MIN_CHARS).is_none() {
        // 非汉字段整段发；汉字段只有 3 个字时整段就是一个窗口，也走这里
        push_term(terms, run);
        return;
    }
    let chars: Vec<char> = run.chars().collect();
    for window in chars.windows(FTS_MIN_CHARS) {
        push_term(terms, &window.iter().collect::<String>());
    }
}

/// 追加一个检索项：不足 [`FTS_MIN_CHARS`] 字符的丢掉（凑不满一个三元组），已出现过的跳过。
fn push_term(terms: &mut Vec<String>, term: &str) {
    if term.chars().count() < FTS_MIN_CHARS || terms.iter().any(|seen| seen == term) {
        return;
    }
    terms.push(term.to_owned());
}

/// 是否是「中日韩统一表意文字」（Han）的码位：这类字符对 FTS5(trigram) 是按字切三元组的，
/// 一整句当短语查必然零命中（见 [`retrieval_terms`]）。含扩展 A/B 与兼容表意区。
fn is_han(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x2_0000..=0x2_FA1F
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_storage::{Db, FactRecord, NewFact};

    use super::{RETRIEVAL_TERM_LIMIT, retrieval_terms};
    use crate::memory::{
        FactScope, MAX_FACT_KEY_CHARS, MAX_FACT_TEXT_CHARS, MemoryError, MemoryStore,
    };
    use crate::task::prompts::build_system_prompt;

    /// 唯一临时目录：测试并发跑，目录名带进程号与进程内自增序号。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("ignis-memory-{name}-{}-{seq}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 内存库 + 临时镜像目录的记忆存储。
    struct Env {
        store: Arc<MemoryStore>,
        db: Arc<Db>,
        dir: TempDir,
    }

    impl Env {
        fn new(name: &str) -> Env {
            let dir = TempDir::new(name);
            let db = Arc::new(Db::open_in_memory().unwrap());
            let store = MemoryStore::new(db.clone(), dir.path().join("memory"));
            Env { store, db, dir }
        }

        fn mirror_path(&self, file: &str) -> PathBuf {
            self.dir.path().join("memory").join(file)
        }

        fn mirror(&self, file: &str) -> String {
            fs::read_to_string(self.mirror_path(file)).unwrap()
        }
    }

    fn ids(facts: &[FactRecord]) -> Vec<i64> {
        facts.iter().map(|fact| fact.id).collect()
    }

    /// 某 `(scope, workspace, key)` 的 `active` 行数：直接数库里的行，不经过读路径的口径。
    fn active_row_count(env: &Env, scope: &str, workspace: Option<&str>, key: &str) -> i64 {
        env.db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM facts
                     WHERE scope = ?1 AND workspace IS ?2 AND key = ?3 AND status = 'active'",
                    (scope, workspace, key),
                    |row| row.get(0),
                )
            })
            .unwrap()
    }

    /// 模拟旧实现「插新行 → 标旧行」两步中途失败后的库状态：同 key 落两行 `active`。
    fn stranded_active_row(env: &Env, scope: &str, key: &str, text: &str) -> i64 {
        env.db
            .insert_fact(&NewFact {
                scope: scope.into(),
                workspace: None,
                key: key.into(),
                text: text.into(),
                source_task_id: None,
            })
            .unwrap()
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn second_correction_leaves_only_the_newest_fact_visible() {
        let env = Env::new("supersede-chain");
        let first = env
            .store
            .remember(FactScope::User, None, "pkg_manager", "使用 pip", None)
            .unwrap();
        let second = env
            .store
            .remember(FactScope::User, None, "pkg_manager", "使用 poetry", None)
            .unwrap();
        let third = env
            .store
            .remember(
                FactScope::User,
                None,
                "pkg_manager",
                "现在改用 uv 装依赖",
                None,
            )
            .unwrap();

        // 两次纠正留下 1 条 active + 2 条 superseded，链条指向前一条的后继
        assert_eq!(env.store.list(true).unwrap().len(), 3);
        assert_eq!(env.store.list(false).unwrap().len(), 1);
        let old = env.db.get_fact(first.id).unwrap().unwrap();
        assert_eq!(
            (old.status.as_str(), old.superseded_by),
            ("superseded", Some(second.id))
        );
        let middle = env.db.get_fact(second.id).unwrap().unwrap();
        assert_eq!(
            (middle.status.as_str(), middle.superseded_by),
            ("superseded", Some(third.id))
        );

        // CLI 检索只回最新
        assert_eq!(
            ids(&env.store.search("改用 uv", 10).unwrap()),
            vec![third.id]
        );
        // 注入只回最新（prompt 的检索项命中旧文本也捞不出来，因为只查 active）
        let injected = env
            .store
            .prompt_facts(Path::new("/w/a"), "记住：现在改用 uv 装依赖")
            .unwrap();
        assert_eq!(ids(&injected), vec![third.id]);
    }

    #[test]
    fn remember_supersedes_into_exactly_one_active_row() {
        let env = Env::new("one-active-row");
        let old = env
            .store
            .remember(
                FactScope::Project,
                Some(Path::new("/w/a")),
                "pkg_manager",
                "使用 pip",
                None,
            )
            .unwrap();
        let new = env
            .store
            .remember(
                FactScope::Project,
                Some(Path::new("/w/a")),
                "pkg_manager",
                "使用 uv",
                None,
            )
            .unwrap();

        // 库里数出来只有一行 active（不是「读路径看起来只有一行」）
        assert_eq!(
            active_row_count(&env, "project", Some("/w/a"), "pkg_manager"),
            1
        );
        let stored = env.db.get_fact(old.id).unwrap().unwrap();
        assert_eq!(
            (
                stored.status.as_str(),
                stored.superseded_by,
                stored.text.as_str()
            ),
            ("superseded", Some(new.id), "使用 pip")
        );
        // 别的 key 不凭空多出 active 行
        assert_eq!(active_row_count(&env, "project", Some("/w/a"), "editor"), 0);
    }

    #[test]
    fn recall_by_key_reads_the_active_row_at_the_exact_position() {
        let env = Env::new("recall-by-key");
        env.store
            .remember(FactScope::User, None, "pkg_manager", "使用 pip", None)
            .unwrap();
        let latest = env
            .store
            .remember(FactScope::User, None, "pkg_manager", "使用 uv", None)
            .unwrap();

        // 只回 active 行；key 的前后空白按写入路径同样的口径裁掉
        assert_eq!(
            env.store
                .recall_by_key(FactScope::User, None, "  pkg_manager  ")
                .unwrap(),
            Some(latest)
        );
        assert_eq!(
            env.store
                .recall_by_key(FactScope::User, None, "editor")
                .unwrap(),
            None
        );
        // 位置算错就查不到：换个 scope 或换个工作区都不算同一条
        assert_eq!(
            env.store
                .recall_by_key(FactScope::Global, None, "pkg_manager")
                .unwrap(),
            None
        );
        assert_eq!(
            env.store
                .recall_by_key(FactScope::Project, Some(Path::new("/w/a")), "pkg_manager")
                .unwrap(),
            None
        );
        // project 位置必须带工作区（与 `remember` 同一套校验）
        assert!(matches!(
            env.store
                .recall_by_key(FactScope::Project, None, "pkg_manager"),
            Err(MemoryError::MissingWorkspace)
        ));

        env.store
            .remember(
                FactScope::Project,
                Some(Path::new("/w/a")),
                "pkg_manager",
                "A 用 uv",
                None,
            )
            .unwrap();
        assert!(
            env.store
                .recall_by_key(FactScope::Project, Some(Path::new("/w/a")), "pkg_manager")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn remember_heals_a_stranded_duplicate_active_row() {
        let env = Env::new("stranded-duplicate");
        // 旧实现「插新行 → 标旧行」两步之间失败留下的现场：同 key 两行 active
        let older = stranded_active_row(&env, "user", "pkg_manager", "使用 pip");
        let newer = stranded_active_row(&env, "user", "pkg_manager", "使用 poetry");
        assert_eq!(active_row_count(&env, "user", None, "pkg_manager"), 2);

        let latest = env
            .store
            .remember(FactScope::User, None, "pkg_manager", "使用 uv", None)
            .unwrap();

        // 写入一次就把所有滞留行收敛：只留最新一行 active，两行旧行都指向它
        assert_eq!(active_row_count(&env, "user", None, "pkg_manager"), 1);
        assert_eq!(env.store.list(false).unwrap().len(), 1);
        for id in [older, newer] {
            let row = env.db.get_fact(id).unwrap().unwrap();
            assert_eq!(
                (row.status.as_str(), row.superseded_by),
                ("superseded", Some(latest.id)),
                "id={id}"
            );
        }
        // 读路径（检索与注入）也只回最新值
        assert_eq!(
            ids(&env.store.search("使用 uv", 10).unwrap()),
            vec![latest.id]
        );
    }

    #[test]
    fn workspaces_are_isolated() {
        let env = Env::new("workspace-isolation");
        let a = env
            .store
            .remember(
                FactScope::Project,
                Some(Path::new("/w/a")),
                "pkg_manager",
                "A 用 uv",
                None,
            )
            .unwrap();
        let b = env
            .store
            .remember(
                FactScope::Project,
                Some(Path::new("/w/b")),
                "pkg_manager",
                "B 用 poetry",
                None,
            )
            .unwrap();

        // 同 key 的两个工作区各自 active，谁也没被取代
        assert_eq!(env.store.list(false).unwrap().len(), 2);
        assert_eq!(env.db.get_fact(a.id).unwrap().unwrap().status, "active");
        assert_eq!(env.db.get_fact(b.id).unwrap().unwrap().status, "active");

        // A 的上下文只带 A 的 project 事实；prompt 里出现 B 的原文也捞不进 B 的行
        assert_eq!(
            ids(&env
                .store
                .prompt_facts(Path::new("/w/a"), "环境和依赖")
                .unwrap()),
            vec![a.id]
        );
        assert_eq!(
            ids(&env
                .store
                .prompt_facts(Path::new("/w/a"), "B 用 poetry")
                .unwrap()),
            vec![a.id]
        );
        // 没有事实的工作区不会凭空拿到别人的
        assert!(
            env.store
                .prompt_facts(Path::new("/w/nowhere"), "环境和依赖")
                .unwrap()
                .is_empty()
        );

        // 两个工作区的镜像各自独立
        assert!(env.mirror("project-_w_a.md").contains("A 用 uv"));
        assert!(env.mirror("project-_w_b.md").contains("B 用 poetry"));
    }

    #[test]
    fn chinese_queries_use_fts_and_short_queries_fall_back_to_like() {
        let env = Env::new("chinese-search");
        let uv = env
            .store
            .remember(
                FactScope::User,
                None,
                "python_env",
                "这个项目使用 uv 管理 Python 环境",
                None,
            )
            .unwrap();
        env.store
            .remember(FactScope::User, None, "editor", "偏好 Neovim", None)
            .unwrap();

        // 4 个字符（含中文与空格）走 FTS(trigram) 的 MATCH
        assert_eq!(
            ids(&env.store.search("管理 Python", 10).unwrap()),
            vec![uv.id]
        );
        // 2 个字符走 LIKE 子串回退
        assert_eq!(ids(&env.store.search("uv", 10).unwrap()), vec![uv.id]);
        // 没写过的东西两个分支都不回
        assert!(env.store.search("poetry", 10).unwrap().is_empty());
    }

    #[test]
    fn remember_validates_scope_workspace_and_blank_input() {
        let env = Env::new("validation");

        let err = env
            .store
            .remember(FactScope::Project, None, "k", "值", None)
            .unwrap_err();
        assert!(matches!(err, MemoryError::MissingWorkspace), "{err}");

        let err = env
            .store
            .remember(FactScope::User, Some(Path::new("/w")), "k", "值", None)
            .unwrap_err();
        assert!(matches!(err, MemoryError::UnexpectedWorkspace), "{err}");
        assert!(
            env.store
                .remember(FactScope::Global, Some(Path::new("/w")), "k", "值", None)
                .is_err()
        );

        let err = env
            .store
            .remember(FactScope::User, None, "  \n", "值", None)
            .unwrap_err();
        assert!(matches!(err, MemoryError::EmptyKey), "{err}");
        assert_eq!(err.to_string(), "记忆的 key 不能为空");
        let err = env
            .store
            .remember(FactScope::User, None, "k", " \t ", None)
            .unwrap_err();
        assert!(matches!(err, MemoryError::EmptyText), "{err}");
        assert_eq!(err.to_string(), "记忆的 text 不能为空");

        // 控制字符剔除后为空：同样按空值拒绝（`\u{7}` 之类的「看不见的输入」不算内容）
        let err = env
            .store
            .remember(FactScope::User, None, "\u{7}\u{8}", "值", None)
            .unwrap_err();
        assert!(matches!(err, MemoryError::EmptyKey), "{err}");
        let err = env
            .store
            .remember(FactScope::User, None, "k", "\n\t\u{7}", None)
            .unwrap_err();
        assert!(matches!(err, MemoryError::EmptyText), "{err}");
        assert!(matches!(
            env.store.edit(0, " \u{1b} "),
            Err(MemoryError::EmptyText)
        ));

        // 校验失败不留任何行（内存库与镜像都干净）
        assert!(env.store.list(true).unwrap().is_empty());
        assert!(!env.mirror_path("user.md").exists());
    }

    /// 一条事实会逐字进系统提示与 Markdown 镜像：`\n` 能凭一条事实撑出好几行（伪装成另一条
    /// 指令 / 另一个 bullet），所以写入前就把空白折叠成单个空格、控制字符剔除。
    #[test]
    fn remember_collapses_multiline_facts_into_a_single_line() {
        let env = Env::new("single-line-fact");
        let record = env
            .store
            .remember(
                FactScope::User,
                None,
                "note\nkey\u{7}",
                "第一行\n第二行\t第三行\u{7}\u{1b}结束",
                None,
            )
            .unwrap();

        assert_eq!(record.key, "note key");
        assert_eq!(record.text, "第一行 第二行 第三行结束");

        // 库里就是归一化后的那一份（直读表，不经过 DAO 的口径）
        let stored: (String, String) = env
            .db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT key, text FROM facts WHERE id = ?1",
                    [record.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .unwrap();
        assert_eq!(
            stored,
            ("note key".to_owned(), "第一行 第二行 第三行结束".to_owned())
        );

        // 镜像：一条事实一个 bullet，换行撑不出额外行
        let mirror = env.mirror("user.md");
        assert_eq!(mirror.lines().count(), 1, "{mirror}");
        assert!(
            mirror.contains("- [user/note key] 第一行 第二行 第三行结束（更新："),
            "{mirror}"
        );

        // 后续任务的系统提示：这条事实同样只占一行
        let prompt = build_system_prompt(Path::new("/w/a"), &[], false, &[record], &[]);
        let injected: Vec<&str> = prompt
            .lines()
            .filter(|line| line.starts_with("- [user/"))
            .collect();
        assert_eq!(
            injected,
            vec!["- [user/note key] 第一行 第二行 第三行结束"],
            "{prompt}"
        );
    }

    /// 文本与 key 都在写入前截到上限（末尾补 `…`）：库、镜像与提示不会再持有更长的文本。
    #[test]
    fn remember_caps_the_text_and_key_at_the_stored_limit() {
        let env = Env::new("cap-fields");
        let long_text = "好".repeat(MAX_FACT_TEXT_CHARS + 200);
        let long_key = "k".repeat(MAX_FACT_KEY_CHARS + 40);
        let record = env
            .store
            .remember(FactScope::Global, None, &long_key, &long_text, None)
            .unwrap();

        let expected_text = format!("{}…", "好".repeat(MAX_FACT_TEXT_CHARS));
        let expected_key = format!("{}…", "k".repeat(MAX_FACT_KEY_CHARS));
        assert_eq!(record.text, expected_text);
        assert_eq!(record.key, expected_key);
        assert_eq!(record.text.chars().count(), MAX_FACT_TEXT_CHARS + 1);
        assert_eq!(record.key.chars().count(), MAX_FACT_KEY_CHARS + 1);

        // 库里与镜像里都是截断后的那一份，而且只占一行
        assert_eq!(
            env.db.get_fact(record.id).unwrap().unwrap().text,
            expected_text
        );
        let mirror = env.mirror("global.md");
        assert_eq!(mirror.lines().count(), 1, "{}", mirror.len());
        assert!(mirror.contains(&expected_text), "{}", mirror.len());

        // `edit` 走同一条写入口径
        let edited = env.store.edit(record.id, &long_text).unwrap();
        assert_eq!(edited.text, expected_text);
        assert_eq!(
            env.db.get_fact(record.id).unwrap().unwrap().text,
            expected_text
        );
    }

    #[test]
    fn prompt_facts_derives_terms_and_always_includes_project_facts() {
        let env = Env::new("prompt-facts");
        let project = env
            .store
            .remember(
                FactScope::Project,
                Some(Path::new("/w/a")),
                "pkg_manager",
                "使用 uv 管理依赖",
                None,
            )
            .unwrap();
        let user = env
            .store
            .remember(
                FactScope::User,
                None,
                "workflow",
                "跑测试一律用 pytest",
                None,
            )
            .unwrap();
        let global = env
            .store
            .remember(
                FactScope::Global,
                None,
                "timezone",
                "默认时区是 Asia/Shanghai",
                None,
            )
            .unwrap();
        let unrelated = env
            .store
            .remember(FactScope::User, None, "editor", "偏好 Neovim", None)
            .unwrap();

        // 长 prompt：拆出的检索项命中 user/global 事实，project 事实无条件排在最前
        let prompt = "跑测试一律用 pytest，时区按 Asia/Shanghai。";
        let facts = env.store.prompt_facts(Path::new("/w/a"), prompt).unwrap();
        assert_eq!(facts.len(), 3, "{facts:?}");
        assert_eq!(facts[0].id, project.id, "project 事实必须无条件注入");
        let rest = ids(&facts[1..]);
        assert!(
            rest.contains(&user.id) && rest.contains(&global.id),
            "{rest:?}"
        );
        assert!(
            !rest.contains(&unrelated.id),
            "无关事实不得被注入：{rest:?}"
        );

        // 2 个字符的 prompt：一个检索项都拆不出，只回 project 事实
        assert_eq!(
            ids(&env.store.prompt_facts(Path::new("/w/a"), "uv").unwrap()),
            vec![project.id]
        );
    }

    #[test]
    fn prompt_facts_keeps_at_most_six_hits() {
        let env = Env::new("prompt-limit");
        for i in 0..8 {
            env.store
                .remember(
                    FactScope::User,
                    None,
                    &format!("k{i}"),
                    "环境工具统一用 uv 管理",
                    None,
                )
                .unwrap();
        }
        env.store
            .remember(
                FactScope::Global,
                None,
                "shared",
                "环境工具统一用 uv 管理",
                None,
            )
            .unwrap();

        let facts = env
            .store
            .prompt_facts(Path::new("/w/a"), "环境工具统一用 uv 管理")
            .unwrap();
        assert_eq!(facts.len(), 6);

        // 新近度收敛：按 (updated_at, id) 倒序，顺序稳定可断言
        let keys: Vec<(i64, i64)> = facts
            .iter()
            .map(|fact| (fact.updated_at, fact.id))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(keys, sorted, "检索结果必须按 (updated_at, id) 倒序");
    }

    #[test]
    fn prompt_facts_never_returns_other_workspaces_project_facts() {
        let env = Env::new("prompt-scope-filter");
        for i in 0..8 {
            env.store
                .remember(
                    FactScope::User,
                    None,
                    &format!("k{i}"),
                    "环境工具统一用 uv 管理",
                    None,
                )
                .unwrap();
        }
        // 另一个工作区的 project 事实命中同一个词，且因为最后写入而排进检索的前 6
        let other = env
            .store
            .remember(
                FactScope::Project,
                Some(Path::new("/w/b")),
                "k",
                "环境工具统一用 uv 管理",
                None,
            )
            .unwrap();

        let facts = env
            .store
            .prompt_facts(Path::new("/w/a"), "环境工具统一用 uv 管理")
            .unwrap();

        // 每个检索项先取 6 条、再按 scope 过滤：那条 project 行占掉一个名额后落选，只剩 5 条
        assert_eq!(facts.len(), 5, "{facts:?}");
        assert!(!facts.iter().any(|fact| fact.id == other.id), "{facts:?}");
        assert!(
            facts
                .iter()
                .all(|fact| matches!(fact.scope.as_str(), "user" | "global")),
            "{facts:?}"
        );
    }

    #[test]
    fn retrieval_terms_split_on_punctuation_and_cap_at_eight() {
        assert_eq!(retrieval_terms("a bb ccc ddd ccc"), vec!["ccc", "ddd"]);
        // 中文长段改发 3 字滑窗（整段当短语查在 trigram FTS 里要求逐字相连，长句几乎必然零命中）
        assert_eq!(
            retrieval_terms("这个项目使用 uv 管理 Python 环境"),
            vec!["这个项", "个项目", "项目使", "目使用", "Python"]
        );
        // 4 个汉字给两条滑窗；2 个汉字依旧凑不满一个三元组，整段丢掉
        assert_eq!(retrieval_terms("环境工具"), vec!["环境工", "境工具"]);
        assert!(retrieval_terms("环境").is_empty());
        assert_eq!(
            retrieval_terms("请用 uv，管理(环境)；pytest"),
            vec!["pytest"]
        );
        assert_eq!(
            retrieval_terms("t01 t02 t03 t04 t05 t06 t07 t08 t09"),
            vec!["t01", "t02", "t03", "t04", "t05", "t06", "t07", "t08"]
        );
        assert!(retrieval_terms("uv 走 好").is_empty());
        // 滑窗按出现顺序去重
        assert_eq!(
            retrieval_terms("这个项目这个项目"),
            vec!["这个项", "个项目", "项目这", "目这个"]
        );
        // 上限在滑窗建完后再收口
        let capped = retrieval_terms("一二三四五六七八九十甲乙丙丁戊己庚辛壬癸");
        assert_eq!(capped.len(), RETRIEVAL_TERM_LIMIT);
        assert_eq!(capped[0], "一二三");
        assert!(capped.iter().all(|term| term.chars().count() == 3));
    }

    /// 中文 prompt 常常一个标点都没有：整句在 FTS 里是「要求这串字原样出现」的短语，长句几乎
    /// 必然零命中，`user`/`global` 事实就永远检索不到。3 字滑窗是这条路径的兜底。
    #[test]
    fn unpunctuated_chinese_prompts_retrieve_user_facts() {
        let env = Env::new("cjk-prompt-retrieval");
        let fact = env
            .store
            .remember(
                FactScope::User,
                None,
                "python_env",
                "这个项目使用 uv 管理 Python 环境",
                None,
            )
            .unwrap();

        // 负例：prompt 与事实文本没有一句逐字相同，且全无分隔符（没有 `，`）
        let prompt = "这个项目使用什么依赖管理工具";
        assert!(!prompt.contains('，'), "本用例故意不带标点");
        let facts = env.store.prompt_facts(Path::new("/w/a"), prompt).unwrap();
        assert_eq!(ids(&facts), vec![fact.id]);

        // 2 个字符的 prompt 依旧拆不出检索项，跳过 FTS
        assert!(retrieval_terms("今晚").is_empty());
        assert!(
            env.store
                .prompt_facts(Path::new("/w/a"), "今晚")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn mirror_lists_active_facts_only_and_follows_edit_and_delete() {
        let env = Env::new("mirror-edit-delete");
        let old = env
            .store
            .remember(FactScope::User, None, "pkg_manager", "使用 pip", None)
            .unwrap();
        env.store
            .remember(FactScope::User, None, "pkg_manager", "使用 uv", None)
            .unwrap();
        let editor = env
            .store
            .remember(FactScope::User, None, "editor", "偏好 Neovim", None)
            .unwrap();

        // 每条 active 事实一行：`- [scope/key] text（更新：RFC3339）`
        let content = env.mirror("user.md");
        let line = content
            .lines()
            .find(|line| line.contains("user/pkg_manager"))
            .unwrap_or_else(|| panic!("pkg_manager 行缺失：{content}"))
            .to_owned();
        let (head, stamp) = line.split_once("（更新：").expect("时间戳后缀");
        assert_eq!(head, "- [user/pkg_manager] 使用 uv");
        let stamp = stamp.strip_suffix('）').expect("右括号");
        chrono::DateTime::parse_from_rfc3339(stamp).expect("RFC3339 时间戳");
        assert!(
            !content.contains("使用 pip"),
            "superseded 文本不得出现：{content}"
        );

        // 编辑后镜像跟着改
        env.store.edit(editor.id, "偏好 Helix").unwrap();
        let content = env.mirror("user.md");
        assert!(content.contains("偏好 Helix"), "{content}");
        assert!(!content.contains("Neovim"), "{content}");

        // 删除后镜像是剩下的 active 事实
        env.store.delete(editor.id).unwrap();
        assert_eq!(env.mirror("user.md"), format!("{line}\n"));

        // 库里仍留着被取代的旧行，只是不再 active
        assert!(
            env.store
                .list(true)
                .unwrap()
                .iter()
                .any(|fact| fact.id == old.id)
        );
        assert_eq!(env.store.list(false).unwrap().len(), 1);
    }

    #[test]
    fn project_mirror_is_named_after_the_workspace_and_starts_with_a_title() {
        let env = Env::new("project-mirror");
        env.store
            .remember(
                FactScope::Project,
                Some(Path::new("/home/dev/app")),
                "pkg_manager",
                "使用 uv",
                None,
            )
            .unwrap();

        let content = env.mirror("project-_home_dev_app.md");
        assert!(
            content.starts_with("# 项目记忆：/home/dev/app\n"),
            "{content}"
        );
        assert!(
            content.contains("- [project/pkg_manager] 使用 uv（更新："),
            "{content}"
        );
        // 只有受影响的那个文件被重写
        assert!(!env.mirror_path("user.md").exists());
        assert!(!env.mirror_path("global.md").exists());
    }

    #[test]
    fn remember_records_the_source_task_id() {
        let env = Env::new("source-task");
        let record = env
            .store
            .remember(FactScope::User, None, "k", "值", Some("task-1"))
            .unwrap();

        let stored: Option<String> = env
            .db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT source_task_id FROM facts WHERE id = ?1",
                    [record.id],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(stored.as_deref(), Some("task-1"));
    }

    #[cfg(unix)]
    #[test]
    fn mirror_files_are_written_0600() {
        use std::os::unix::fs::PermissionsExt;

        let env = Env::new("mirror-mode");
        env.store
            .remember(FactScope::User, None, "k", "值", None)
            .unwrap();
        assert_eq!(mode(&env.mirror_path("user.md")), 0o600);

        // 已存在的宽权限文件在重写时被收紧
        fs::set_permissions(
            env.mirror_path("user.md"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        env.store
            .remember(FactScope::User, None, "k", "新值", None)
            .unwrap();
        assert_eq!(mode(&env.mirror_path("user.md")), 0o600);
    }

    #[test]
    fn unwritable_mirror_path_keeps_remember_ok() {
        let env = Env::new("unwritable-mirror");
        // 镜像目录的位置先被一个普通文件占住：`create_dir_all` 对「已存在的非目录」一定失败，
        // root 也一样（不受 CAP_DAC_OVERRIDE 影响），不像只读目录那样会被特权绕过。
        let blocked = env.dir.path().join("memory");
        fs::write(&blocked, "占位文件").unwrap();

        let record = env
            .store
            .remember(FactScope::User, None, "pkg_manager", "使用 uv", None)
            .expect("镜像写不进去不该让 remember 失败");

        // 事实已入库，镜像没写出来、占位文件也没被动过
        assert_eq!(env.db.get_fact(record.id).unwrap().unwrap().text, "使用 uv");
        assert_eq!(fs::read_to_string(&blocked).unwrap(), "占位文件");
        assert!(!env.mirror_path("user.md").exists());
    }

    #[test]
    fn rebuild_mirror_rewrites_every_scope_present() {
        let env = Env::new("rebuild");
        env.store
            .remember(FactScope::User, None, "editor", "偏好 Neovim", None)
            .unwrap();
        env.store
            .remember(
                FactScope::Global,
                None,
                "timezone",
                "常用时区 Asia/Shanghai",
                None,
            )
            .unwrap();
        env.store
            .remember(
                FactScope::Project,
                Some(Path::new("/w/b")),
                "pkg_manager",
                "旧文本",
                None,
            )
            .unwrap();
        let newest = env
            .store
            .remember(
                FactScope::Project,
                Some(Path::new("/w/b")),
                "pkg_manager",
                "新文本",
                None,
            )
            .unwrap();

        // 手工破坏镜像（用户误改、误删）
        fs::write(env.mirror_path("user.md"), "被改坏了").unwrap();
        fs::remove_file(env.mirror_path("project-_w_b.md")).unwrap();

        env.store.rebuild_mirror().unwrap();

        assert!(
            env.mirror("user.md")
                .contains("- [user/editor] 偏好 Neovim（更新：")
        );
        assert!(
            env.mirror("global.md")
                .contains("- [global/timezone] 常用时区 Asia/Shanghai（更新：")
        );
        // 只剩一条 active 的工作区文件被重写成「标题 + 新文本」，旧文本不残留
        let content = env.mirror("project-_w_b.md");
        assert_eq!(content.lines().count(), 2, "{content}");
        assert!(content.starts_with("# 项目记忆：/w/b\n"), "{content}");
        assert!(
            content.contains("新文本") && !content.contains("旧文本"),
            "{content}"
        );

        // 删掉最后一条 active 事实后重建：文件只剩标题（工作区仍因 superseded 行而存在）
        env.store.delete(newest.id).unwrap();
        fs::write(env.mirror_path("project-_w_b.md"), "被改坏了").unwrap();
        env.store.rebuild_mirror().unwrap();
        assert_eq!(env.mirror("project-_w_b.md"), "# 项目记忆：/w/b\n");
    }

    #[test]
    fn edit_and_delete_report_not_found_for_unknown_ids() {
        let env = Env::new("not-found");
        let id = env
            .store
            .remember(FactScope::User, None, "k", "值", None)
            .unwrap()
            .id;
        let missing = id + 99;

        let err = env.store.edit(missing, "改一下").unwrap_err();
        assert!(
            matches!(err, MemoryError::NotFound { id } if id == missing),
            "{err}"
        );
        assert_eq!(err.to_string(), format!("memory not found: {missing}"));

        let err = env.store.delete(missing).unwrap_err();
        assert!(
            matches!(err, MemoryError::NotFound { id } if id == missing),
            "{err}"
        );
        assert_eq!(err.to_string(), format!("memory not found: {missing}"));

        // 空白文本先被拒；已存在的那条不受影响
        assert!(matches!(
            env.store.edit(id, "  "),
            Err(MemoryError::EmptyText)
        ));
        assert_eq!(env.store.list(false).unwrap().len(), 1);
    }
}
