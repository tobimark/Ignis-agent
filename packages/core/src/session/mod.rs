//! 会话（session）：REPL 的 canonical 消息落库与有效历史重建。
//!
//! 与 storage 的分工：`sessions` / `session_messages` / `session_compactions` 的行级读写由
//! [`ignis_storage`] 的 DAO 负责，这里固定上层语义：
//!
//! - **canonical 口径**：会话只存用户提示词与 assistant 的最终答复（纯文本），工具轮次留在任务
//!   JSONL；`session_messages.role` 只写 `user` / `assistant`。**失败/中止的轮次只记 user、不记
//!   assistant**——与 v0.1 的内存历史逐条对齐（v0.1 的 `TaskOutcome.messages` 只在成功时带最终
//!   答复），恢复判据因此对两种轮次都成立。
//! - **懒创建时机**：REPL 启动时不写库，**首个任务提交时**才由
//!   [`SessionStore::create_session`] 落一行（空会话不落库）；id 由 `Uuid::now_v7()` 生成，与任务
//!   id 同源（36 字符、可被 `/resume <前缀>` 解析）。
//! - **每轮收口**：[`SessionStore::append_turn`] 按序追加 user（总是）+ assistant（有最终答复
//!   时），并把 `updated_at` 刷成同一次取到的墙钟。
//! - **`seq` 语义**：会话内的序号，从 1 起连续、只增不改；原始消息一条不删——`/compact` 也只往
//!   `session_compactions` 追加一行。
//! - **有效历史重建**（[`SessionStore::effective_history`]）：最近一次 compaction 的摘要消息
//!   （role = User、正文 = [`EARLIER_SUMMARY_PREFIX`] + 空行 + 摘要，与 `/compact` 重建内存历史
//!   时的逐字格式一致）+ 其后（`seq > upto_seq`）的原始消息，按 `seq` 升序。摘要消息没有对应的
//!   `session_messages` 行，它的 `seq` 是被覆盖段的上界（compaction 的 `upto_seq`），供
//!   `/compact` 二次压缩时算新的切点。
//! - **fail-soft**：库里出现契约外的角色（历史遗留、手工改库）时跳过该行并打一行中文警告，绝不
//!   panic、也不静默改写成 assistant。
//!
//! [`SessionStore`] 不持状态、不缓存连接：只包一个 `Arc<Db>`，每个方法经它走到共享连接，构造按需
//! （CLI 的 `Session::store()` 每次新建一个）。

use std::path::Path;
use std::sync::Arc;

use ignis_runtime::{ChatMessage, Role};
use ignis_storage::{
    Db, NewSession, NewSessionMessage, SessionRecord, SessionSummary, StorageError, now_millis,
};

/// 标题上限（字符数，与设计 §6.6 的「前 40 字符」一致）。
pub const SESSION_TITLE_CHARS: usize = 40;
/// 标题为空时的兜底（REPL 不会为空，防御性）。
pub const TITLE_FALLBACK: &str = "untitled";
/// 压缩摘要消息的前缀（有效历史重建与 `/compact` 共用一份）。
pub const EARLIER_SUMMARY_PREFIX: &str = "[Earlier conversation summary]";

/// canonical 消息的角色取值（`session_messages.role` 只写这两个）。
const ROLE_USER: &str = "user";
const ROLE_ASSISTANT: &str = "assistant";

/// 首条用户消息 → 标题：`\r` / `\n` 各换成一个空格、trim、按**字符**截到 40（不按字节、不加省略号）。
///
/// 换行会把 `/sessions` 的一行撑成好几行，所以标题必须单行；按字符截断保证不切坏 UTF-8（中文
/// 标题不会 panic，也不会截出半个字）。截断后为空（全是空白/换行）给 [`TITLE_FALLBACK`]。
pub fn session_title(prompt: &str) -> String {
    let flattened: String = prompt
        .chars()
        .map(|ch| if ch == '\r' || ch == '\n' { ' ' } else { ch })
        .collect();
    let title: String = flattened.trim().chars().take(SESSION_TITLE_CHARS).collect();
    if title.is_empty() {
        TITLE_FALLBACK.to_owned()
    } else {
        title
    }
}

/// 有效历史的一条消息：canonical 内容 + 它在 `session_messages` 里的 `seq`。
///
/// 摘要消息没有对应的行，`seq` 取它覆盖的压缩段上界（compaction 的 `upto_seq`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub seq: i64,
    pub role: Role,
    pub content: String,
}

impl HistoryMessage {
    /// 还原成送进内核的一条消息；canonical 消息是纯文本，工具两列恒为空。
    pub fn to_chat_message(&self) -> ChatMessage {
        ChatMessage {
            role: self.role,
            content: self.content.clone(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
}

/// 会话存储错误：把 storage 的错误透传上去，展示与 fail-soft 由调用方决定。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// SQLite 读写 / IO / JSON 失败。
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// 会话存储：`Arc<Db>` 之上的薄封装（无自有状态，构造按需）。
pub struct SessionStore {
    db: Arc<Db>,
}

impl SessionStore {
    pub fn new(db: Arc<Db>) -> SessionStore {
        SessionStore { db }
    }

    /// 懒创建：id 由 `Uuid::now_v7()` 生成（与任务 id 同源），标题取 `first_prompt`。
    ///
    /// `workspace` 按 `display()` 落库（查询走同一个转换 `workspace_key`），`created_at` 与
    /// `updated_at` 都是这一次的墙钟。返回的记录就是刚写入的那一行（DAO 的契约：新行的两个时间戳
    /// 同值），不再回读一遍。
    pub fn create_session(
        &self,
        workspace: &Path,
        kernel: &str,
        model: Option<&str>,
        first_prompt: &str,
    ) -> Result<SessionRecord, SessionError> {
        let created_at = now_millis();
        let record = SessionRecord {
            id: uuid::Uuid::now_v7().to_string(),
            title: session_title(first_prompt),
            workspace: workspace_key(workspace),
            kernel: kernel.to_owned(),
            model: model.map(str::to_owned),
            created_at,
            updated_at: created_at,
        };
        self.db.insert_session(&NewSession {
            id: record.id.clone(),
            title: record.title.clone(),
            workspace: record.workspace.clone(),
            kernel: record.kernel.clone(),
            model: record.model.clone(),
            created_at: record.created_at,
        })?;
        Ok(record)
    }

    /// 一轮结束：按序追加 user（总是）+ assistant（`reply` 为 `Some` 时），刷新 `updated_at`；
    /// 返回写入的条数。
    ///
    /// 失败/中止的轮次 `reply` 是 `None`，只落 user 一条（canonical 口径见模块文档）；整轮消息与
    /// `updated_at` 用的是同一次 `now_millis()`。会话不存在时由外键拒绝（`Err`），不是静默丢弃。
    pub fn append_turn(
        &self,
        session_id: &str,
        prompt: &str,
        reply: Option<&str>,
    ) -> Result<usize, SessionError> {
        let mut messages = Vec::with_capacity(2);
        messages.push(NewSessionMessage {
            role: ROLE_USER.to_owned(),
            content: prompt.to_owned(),
        });
        if let Some(reply) = reply {
            messages.push(NewSessionMessage {
                role: ROLE_ASSISTANT.to_owned(),
                content: reply.to_owned(),
            });
        }
        Ok(self
            .db
            .append_session_messages(session_id, &messages, now_millis())?)
    }

    /// 有效历史：最近一次 compaction 的摘要消息（前缀 [`EARLIER_SUMMARY_PREFIX`]，role = User）
    /// + 其后（`seq > upto_seq`）的消息；未知会话给空表。
    ///
    /// 这是「恢复后与退出前的 canonical 历史逐条相等」被比对的那一份视图：没压缩过时就是原始行
    /// （`seq` 升序），压缩过时第一段是摘要消息、其后是保留段。库里的原始行一条不动。
    pub fn effective_history(&self, session_id: &str) -> Result<Vec<HistoryMessage>, SessionError> {
        let mut history = Vec::new();
        let from_seq = match self.db.latest_compaction(session_id)? {
            Some(compaction) => {
                history.push(HistoryMessage {
                    seq: compaction.upto_seq,
                    role: Role::User,
                    content: summary_message(&compaction.summary),
                });
                // 摘要覆盖到 `upto_seq`，有效历史从它的下一条原始消息继续
                compaction.upto_seq + 1
            }
            // 没压缩过：`seq` 从 1 起连续，等价于「全部消息」
            None => 1,
        };
        for row in self.db.session_messages_from(session_id, from_seq)? {
            match role_of(&row.role) {
                Some(role) => history.push(HistoryMessage {
                    seq: row.seq,
                    role,
                    content: row.content,
                }),
                None => eprintln!(
                    "warning: 未知的会话消息角色「{}」（session={session_id}，seq={}），跳过",
                    row.role, row.seq
                ),
            }
        }
        Ok(history)
    }

    /// 本工作区最近一次更新的会话（`ignis -c` 的恢复点）；没有会话给 `None`。
    pub fn latest_for_workspace(
        &self,
        workspace: &Path,
    ) -> Result<Option<SessionRecord>, SessionError> {
        Ok(self
            .db
            .latest_session_for_workspace(&workspace_key(workspace))?)
    }

    /// 本工作区的会话列表（`/sessions`），新近在前，最多 `limit` 条。
    ///
    /// `SessionSummary::message_count` 是 `session_messages` 的**原始行数**（含已被 compaction
    /// 覆盖、不在有效历史里的前缀），不是有效历史的条数——`/sessions` 展示时知情即可。
    pub fn list_for_workspace(
        &self,
        workspace: &Path,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        Ok(self
            .db
            .list_sessions_for_workspace(&workspace_key(workspace), limit)?)
    }

    /// `/resume <前缀>`：先按前缀取本工作区的会话 id（新近在前），再逐条读回记录，顺序不变。
    ///
    /// LIKE 元字符按字面处理（storage 的口径）；空前缀等价于「本工作区的所有会话」，准入闸门在命令
    /// 层。两条查询之间行被删时跳过（P3 没有删除路径，纯防御），不是错误。
    pub fn sessions_with_prefix(
        &self,
        workspace: &Path,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<SessionRecord>, SessionError> {
        let workspace = workspace_key(workspace);
        let mut records = Vec::new();
        for id in self.db.session_ids_with_prefix(&workspace, prefix, limit)? {
            if let Some(record) = self.db.get_session(&id)? {
                records.push(record);
            }
        }
        Ok(records)
    }
}

/// 会话行里的工作区字符串：创建与查询都走这一份转换，两侧永远对得上。
fn workspace_key(workspace: &Path) -> String {
    workspace.display().to_string()
}

/// canonical 角色映射：只认 `user` / `assistant`；未知角色给 `None`（调用方跳过并警告）。
fn role_of(role: &str) -> Option<Role> {
    match role {
        ROLE_USER => Some(Role::User),
        ROLE_ASSISTANT => Some(Role::Assistant),
        _ => None,
    }
}

/// 摘要消息的正文：前缀 + 空行 + 摘要（与 `/compact` 重建内存历史时的逐字格式一致）。
fn summary_message(summary: &str) -> String {
    format!("{EARLIER_SUMMARY_PREFIX}\n\n{summary}")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_runtime::Role;
    use ignis_storage::{Db, NewSession, NewSessionMessage, now_millis};

    use super::{
        EARLIER_SUMMARY_PREFIX, HistoryMessage, SESSION_TITLE_CHARS, SessionStore, TITLE_FALLBACK,
        session_title,
    };

    /// 唯一临时目录：测试并发跑，目录名带进程号与进程内自增序号（与 memory/doctor 同一手法）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("ignis-session-{name}-{}-{seq}", std::process::id()));
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

    /// 一个测试的沙箱：临时目录 + 真实 DB 文件（绝不碰 `~/.ignis`）。
    struct Env {
        dir: TempDir,
    }

    impl Env {
        fn new(name: &str) -> Env {
            Env {
                dir: TempDir::new(name),
            }
        }

        /// 沙箱内的一条路径（`workspace` / `other` / `nowhere` 都这么拼）。
        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        /// 固定给 `Db::open` 的库文件路径（10 次启停用例跨轮次重开同一个文件）。
        fn db_path(&self) -> PathBuf {
            self.path("ignis.db")
        }

        fn workspace(&self) -> PathBuf {
            self.path("workspace")
        }
    }

    /// 打开真实 DB 文件并把会话存储绑上去；`Arc<Db>` 让断言与存储共用同一连接。
    fn open(path: &Path) -> (Arc<Db>, SessionStore) {
        let db = Arc::new(Db::open(path).unwrap());
        let store = SessionStore::new(Arc::clone(&db));
        (db, store)
    }

    fn user(content: &str) -> NewSessionMessage {
        NewSessionMessage {
            role: "user".into(),
            content: content.into(),
        }
    }

    /// 手写一条会话行（显式时间戳，排序断言不依赖墙钟）。
    fn session_row(id: &str, workspace: &Path, at: i64) -> NewSession {
        NewSession {
            id: id.into(),
            title: format!("title {id}"),
            workspace: workspace.display().to_string(),
            kernel: "native".into(),
            model: None,
            created_at: at,
        }
    }

    #[test]
    fn session_title_truncates_by_chars_and_collapses_newlines() {
        // `\n` 换成一个空格、首尾空白裁掉
        assert_eq!(session_title("第一行\n第二行"), "第一行 第二行");
        assert_eq!(session_title("  修复 这个 bug  "), "修复 这个 bug");
        // `\r` / `\n` 各换一个空格：CRLF 的两个字符就是两个空格
        assert_eq!(session_title("第一行\r\n第二行"), "第一行  第二行");

        // 按字符截断：41 个汉字 → 40 个汉字（按字节切会切坏 UTF-8）
        let han = "汉".repeat(41);
        let title = session_title(&han);
        assert_eq!(title.chars().count(), SESSION_TITLE_CHARS);
        assert_eq!(title, "汉".repeat(40));
        // 不加省略号
        assert!(!title.ends_with('…'));

        // 超长中文同理（60 个字符 → 40 个字符，仍是合法的 UTF-8 前缀）
        let long: String = "测试".repeat(30);
        assert_eq!(session_title(&long), "测试".repeat(20));
    }

    #[test]
    fn session_title_falls_back_when_empty() {
        assert_eq!(TITLE_FALLBACK, "untitled");
        assert_eq!(session_title(""), TITLE_FALLBACK);
        assert_eq!(session_title("  \n\r\n\t "), TITLE_FALLBACK);
    }

    #[test]
    fn create_session_writes_the_first_prompt_as_the_title() {
        let env = Env::new("create");
        let (db, store) = open(&env.db_path());

        let before = now_millis();
        let record = store
            .create_session(
                &env.workspace(),
                "native",
                Some("MiniMax-M2"),
                "第一行\n第二行",
            )
            .unwrap();
        let after = now_millis();

        // id 是 36 字符的 uuid（小写十六进制与 `-`，可被 `/resume <前缀>` 解析），与任务 id 同源
        assert_eq!(record.id.len(), 36);
        assert!(
            record.id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "{}",
            record.id
        );
        assert_eq!(
            uuid::Uuid::parse_str(&record.id).unwrap().get_version_num(),
            7
        );

        assert_eq!(record.title, "第一行 第二行");
        assert_eq!(record.workspace, env.workspace().display().to_string());
        assert_eq!(record.kernel, "native");
        assert_eq!(record.model.as_deref(), Some("MiniMax-M2"));
        // 懒创建：`created_at` 与 `updated_at` 同值，都是这次写入的墙钟
        assert!(record.created_at >= before && record.created_at <= after);
        assert_eq!(record.updated_at, record.created_at);
        // 返回的记录就是落库的那一行
        assert_eq!(db.get_session(&record.id).unwrap().unwrap(), record);

        // model 可空（外置内核没有模型名）
        let bare = store
            .create_session(&env.workspace(), "codex", None, "无模型")
            .unwrap();
        assert_eq!(bare.model, None);
        assert_eq!(db.get_session(&bare.id).unwrap().unwrap(), bare);

        // 空会话不落库是调用方的懒创建时机；这里只保证两次调用拿到不同的 id
        assert_ne!(record.id, bare.id);
    }

    #[test]
    fn append_turn_writes_user_then_assistant_and_bumps_updated_at() {
        let env = Env::new("append-turn");
        let (db, store) = open(&env.db_path());
        let session = store
            .create_session(&env.workspace(), "native", None, "你好")
            .unwrap();

        assert_eq!(
            store
                .append_turn(&session.id, "你好", Some("答复一"))
                .unwrap(),
            2
        );
        let first = db.session_messages_from(&session.id, 1).unwrap();
        assert_eq!(first.iter().map(|m| m.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(first[0].role, "user");
        assert_eq!(first[0].content, "你好");
        assert_eq!(first[1].role, "assistant");
        assert_eq!(first[1].content, "答复一");
        let after_first = db.get_session(&session.id).unwrap().unwrap().updated_at;
        assert!(after_first >= session.updated_at, "{after_first}");

        // 第二轮：`seq` 续写、`updated_at` 不小于第一批（同一毫秒也算通过）
        assert_eq!(
            store
                .append_turn(&session.id, "再来一轮", Some("答复二"))
                .unwrap(),
            2
        );
        let all = db.session_messages_from(&session.id, 1).unwrap();
        let actual: Vec<(&str, &str)> = all
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(
            actual,
            [
                ("user", "你好"),
                ("assistant", "答复一"),
                ("user", "再来一轮"),
                ("assistant", "答复二"),
            ]
        );
        let after_second = db.get_session(&session.id).unwrap().unwrap().updated_at;
        assert!(
            after_second >= after_first,
            "{after_second} < {after_first}"
        );
        // 一批消息与 `updated_at` 同源（同一次 `now_millis()`）
        assert_eq!(all[2].created_at, after_second);
        assert_eq!(all[3].created_at, after_second);
        // 追加不动 `created_at`
        assert_eq!(
            db.get_session(&session.id).unwrap().unwrap().created_at,
            session.created_at
        );
    }

    #[test]
    fn append_turn_without_a_reply_writes_the_user_message_only() {
        let env = Env::new("append-no-reply");
        let (db, store) = open(&env.db_path());
        let session = store
            .create_session(&env.workspace(), "native", None, "失败轮次")
            .unwrap();

        // 失败/中止轮次：与 v0.1 的内存历史逐条对齐——记 user、不记 assistant
        assert_eq!(
            store.append_turn(&session.id, "这条会失败", None).unwrap(),
            1
        );
        let rows = db.session_messages_from(&session.id, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[0].role, "user");
        assert_eq!(rows[0].content, "这条会失败");

        let history = store.effective_history(&session.id).unwrap();
        assert_eq!(
            history,
            [HistoryMessage {
                seq: 1,
                role: Role::User,
                content: "这条会失败".into(),
            }]
        );

        // 下一轮照常续写（seq 从 2 起）
        assert_eq!(
            store
                .append_turn(&session.id, "再试一次", Some("这次成了"))
                .unwrap(),
            2
        );
        assert_eq!(
            db.session_messages_from(&session.id, 1)
                .unwrap()
                .iter()
                .map(|m| m.seq)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    #[test]
    fn effective_history_is_empty_for_an_unknown_session() {
        let env = Env::new("unknown-session");
        let (_db, store) = open(&env.db_path());

        // 未知会话给空表，不报错（`-c` 命中空工作区、`/resume` 用错 id 都走这条路）
        assert!(
            store
                .effective_history("019a1b2c-0000-7000-8000-000000000000")
                .unwrap()
                .is_empty()
        );

        // 有行但没消息的会话同样给空表（空会话不落库，防御性）
        let session = store
            .create_session(&env.workspace(), "native", None, "没跑过任务")
            .unwrap();
        assert!(store.effective_history(&session.id).unwrap().is_empty());
    }

    #[test]
    fn effective_history_prepends_the_latest_compaction_summary() {
        let env = Env::new("compaction");
        let (db, store) = open(&env.db_path());
        let session = store
            .create_session(&env.workspace(), "native", None, "压一压")
            .unwrap();
        for (prompt, reply) in [("一", "答一"), ("二", "答二"), ("三", "答三")] {
            store.append_turn(&session.id, prompt, Some(reply)).unwrap();
        }

        // 前两轮（seq 1..=4）被摘要覆盖：有效历史 = 摘要消息 + seq 5/6
        db.insert_compaction(&session.id, 4, "前两轮的摘要", 42)
            .unwrap();
        let history = store.effective_history(&session.id).unwrap();
        assert_eq!(
            history,
            [
                HistoryMessage {
                    seq: 4,
                    role: Role::User,
                    content: format!("{EARLIER_SUMMARY_PREFIX}\n\n前两轮的摘要"),
                },
                HistoryMessage {
                    seq: 5,
                    role: Role::User,
                    content: "三".into(),
                },
                HistoryMessage {
                    seq: 6,
                    role: Role::Assistant,
                    content: "答三".into(),
                },
            ]
        );

        // 摘要消息：前缀逐字、正文含摘要、没有对应的 `session_messages` 行
        let summary = &history[0];
        assert!(
            summary
                .content
                .starts_with("[Earlier conversation summary]")
        );
        assert!(summary.content.contains("前两轮的摘要"));
        assert_eq!(summary.to_chat_message().role, Role::User);
        assert!(summary.to_chat_message().tool_calls.is_empty());
        assert_eq!(summary.to_chat_message().tool_call_id, None);

        // 原始消息一条不删：6 条都还在库里
        assert_eq!(db.session_messages_from(&session.id, 1).unwrap().len(), 6);

        // 再压一次：取最近一次（`id` 最大），旧摘要不叠加
        db.insert_compaction(&session.id, 5, "更晚的摘要", 100)
            .unwrap();
        let again = store.effective_history(&session.id).unwrap();
        assert_eq!(again.iter().map(|m| m.seq).collect::<Vec<_>>(), [5, 6]);
        assert_eq!(
            again[0].content,
            format!("{EARLIER_SUMMARY_PREFIX}\n\n更晚的摘要")
        );
        assert_eq!(again[1].role, Role::Assistant);
    }

    /// 角色映射只认 `user` / `assistant`：未知角色跳过（fail-soft），不改写成 assistant。
    #[test]
    fn effective_history_skips_unknown_roles() {
        let env = Env::new("unknown-role");
        let (db, store) = open(&env.db_path());
        let session = store
            .create_session(&env.workspace(), "native", None, "角色")
            .unwrap();
        store
            .append_turn(&session.id, "问题", Some("答复"))
            .unwrap();
        // 库里出现契约外的角色（上层保证写入，但历史遗留/手工改库都可能）：跳过，不 panic
        db.append_session_messages(
            &session.id,
            &[
                NewSessionMessage {
                    role: "tool".into(),
                    content: "工具输出".into(),
                },
                NewSessionMessage {
                    role: "User".into(),
                    content: "大小写不同也算未知".into(),
                },
            ],
            7,
        )
        .unwrap();

        let history = store.effective_history(&session.id).unwrap();
        assert_eq!(
            history,
            [
                HistoryMessage {
                    seq: 1,
                    role: Role::User,
                    content: "问题".into(),
                },
                HistoryMessage {
                    seq: 2,
                    role: Role::Assistant,
                    content: "答复".into(),
                },
            ]
        );
        // 原始行照旧都在（历史视图跳过不等于删库）
        assert_eq!(db.session_messages_from(&session.id, 1).unwrap().len(), 4);
    }

    #[test]
    fn latest_for_workspace_scopes_to_the_workspace() {
        let env = Env::new("latest");
        let (db, store) = open(&env.db_path());
        let other_workspace = env.path("other");

        let same_a = store
            .create_session(&env.workspace(), "native", None, "本区 A")
            .unwrap();
        let same_b = store
            .create_session(&env.workspace(), "native", None, "本区 B")
            .unwrap();
        let other = store
            .create_session(&other_workspace, "native", None, "别区")
            .unwrap();

        // 别的工作区的会话更「新」也不影响本工作区；排序看的是 `updated_at`
        db.append_session_messages(&other.id, &[user("别区")], 5_000)
            .unwrap();
        db.append_session_messages(&same_a.id, &[user("A")], 1_000)
            .unwrap();
        db.append_session_messages(&same_b.id, &[user("B")], 2_000)
            .unwrap();

        let latest = store
            .latest_for_workspace(&env.workspace())
            .unwrap()
            .expect("本工作区有会话");
        assert_eq!(latest.id, same_b.id);
        assert_eq!(latest.updated_at, 2_000);
        assert_eq!(
            store
                .latest_for_workspace(&other_workspace)
                .unwrap()
                .unwrap()
                .id,
            other.id
        );

        // 没有会话的工作区给 None（`-c` 靠它打 no_session_note）
        assert!(
            store
                .latest_for_workspace(&env.path("nowhere"))
                .unwrap()
                .is_none()
        );

        // 列表同口径：本工作区两条、新近在前、limit 截断
        let list = store.list_for_workspace(&env.workspace(), 10).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session.id, same_b.id);
        assert_eq!(list[1].session.id, same_a.id);
        assert_eq!(list[0].message_count, 1);
        assert_eq!(
            store
                .list_for_workspace(&env.workspace(), 1)
                .unwrap()
                .first()
                .unwrap()
                .session
                .id,
            same_b.id
        );
    }

    #[test]
    fn sessions_with_prefix_returns_newest_first() {
        let env = Env::new("prefix");
        let (db, store) = open(&env.db_path());
        let other_workspace = env.path("other");

        for (id, workspace, at) in [
            ("aa01", env.workspace(), 10),
            ("aa02", env.workspace(), 20),
            ("aa03", env.workspace(), 30),
            ("ab01", other_workspace.clone(), 40),
        ] {
            db.insert_session(&session_row(id, &workspace, at)).unwrap();
        }

        let hits = store
            .sessions_with_prefix(&env.workspace(), "aa", 10)
            .unwrap();
        assert_eq!(
            hits.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["aa03", "aa02", "aa01"]
        );
        assert_eq!(hits[0].title, "title aa03");
        assert_eq!(hits[0].updated_at, 30);

        // limit 截断：取的是最新的一批
        let limited = store
            .sessions_with_prefix(&env.workspace(), "aa", 2)
            .unwrap();
        assert_eq!(
            limited.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["aa03", "aa02"]
        );
        // 别的工作区里的同前缀会话不出现
        assert!(
            store
                .sessions_with_prefix(&env.workspace(), "ab", 10)
                .unwrap()
                .is_empty()
        );

        // 真实生成的 uuid 也能被前缀解析（存储 → DAO 的接线）
        let generated = store
            .create_session(&env.workspace(), "native", None, "uuid 前缀")
            .unwrap();
        let by_uuid_prefix = store
            .sessions_with_prefix(&env.workspace(), &generated.id[..8], 10)
            .unwrap();
        assert_eq!(by_uuid_prefix.len(), 1);
        assert_eq!(by_uuid_prefix[0].id, generated.id);
    }

    /// 验收锚点（设计 §6.6/§7）：10 次「发消息 → 关库 → 重开 → 恢复」后有效历史逐条相等。
    #[test]
    fn ten_close_reopen_cycles_restore_the_canonical_messages() {
        let env = Env::new("ten-cycles");
        let db_path = env.db_path();
        let mut expected: Vec<HistoryMessage> = Vec::new();
        let mut session_id = String::new();

        for turn in 1..=10_usize {
            let prompt = format!("第 {turn} 轮提问");
            let reply = format!("第 {turn} 轮答复");
            expected.push(HistoryMessage {
                seq: (turn * 2 - 1) as i64,
                role: Role::User,
                content: prompt.clone(),
            });
            expected.push(HistoryMessage {
                seq: (turn * 2) as i64,
                role: Role::Assistant,
                content: reply.clone(),
            });

            // 开库：第 1 轮懒创建会话行，其后每轮追加一轮 canonical 消息
            let db = Arc::new(Db::open(&db_path).unwrap());
            let store = SessionStore::new(Arc::clone(&db));
            if turn == 1 {
                session_id = store
                    .create_session(&env.workspace(), "native", Some("MiniMax-M2"), &prompt)
                    .unwrap()
                    .id;
            }
            assert_eq!(
                store
                    .append_turn(&session_id, &prompt, Some(&reply))
                    .unwrap(),
                2
            );
            drop(store);
            drop(db);

            // 关库（等价于进程退出）：重新打开这个库文件，读回的有效历史逐条相等
            let reader = Arc::new(Db::open(&db_path).unwrap());
            let store = SessionStore::new(Arc::clone(&reader));
            let history = store.effective_history(&session_id).unwrap();
            assert_eq!(history, expected, "第 {turn} 次重开后历史不一致");

            if turn == 10 {
                // 20 条原始 canonical 行（10 轮 × user + assistant），一次不多一次不少
                let list = store.list_for_workspace(&env.workspace(), 10).unwrap();
                assert_eq!(list.len(), 1);
                assert_eq!(list[0].session.id, session_id);
                assert_eq!(list[0].message_count, 20);
            }
            drop(store);
            drop(reader);
        }
    }
}
