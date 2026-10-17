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
//! - **`/compact` 的切分与摘要**（[`split_turns`] / [`render_transcript`] / [`summarize_turns`]）：
//!   按「保留最后 [`COMPACT_KEEP_TURNS`] 轮」切出较早轮次，渲染成一次**无工具**的 native 请求交给
//!   模型做摘要；摘要只作为一条 compaction 记录落库，`session_messages` 一行不动。
//!
//! [`SessionStore`] 不持状态、不缓存连接：只包一个 `Arc<Db>`，每个方法经它走到共享连接，构造按需
//! （CLI 的 `Session::store()` 每次新建一个）。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use ignis_runtime::{
    ChatMessage, Role, RunRequest, RunStatus, Runtime, RuntimeEvent, ToolCall, ToolExecutor,
    ToolResult, ToolSpec,
};
use ignis_storage::{
    Db, NewSession, NewSessionMessage, SessionRecord, SessionSummary, StorageError, now_millis,
};
use tokio_util::sync::CancellationToken;

use crate::config::Config;

/// 标题上限（字符数，与设计 §6.6 的「前 40 字符」一致）。
pub const SESSION_TITLE_CHARS: usize = 40;
/// 标题为空时的兜底（REPL 不会为空，防御性）。
pub const TITLE_FALLBACK: &str = "untitled";
/// 压缩摘要消息的前缀（有效历史重建与 `/compact` 共用一份）。
pub const EARLIER_SUMMARY_PREFIX: &str = "[Earlier conversation summary]";
/// 保留的轮数（设计 §6.7：保留最近 2 轮，其余进摘要段）。
pub const COMPACT_KEEP_TURNS: usize = 2;
/// 摘要请求的 system 提示（英文，逐字钉住）。
pub const COMPACT_SYSTEM_PROMPT: &str = "Summarize the older part of this conversation for \
continuity. Preserve the current task state, the user's decisions, unfinished work, and key \
facts and file paths. Reply with the summary only.";

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
    /// 摘要请求失败：`{0}` 是内核交回的人话原因（`Done{Failed}` 的 `error`、非成功终态或流提前结束）。
    #[error("compact: summarization failed: {0}")]
    Summarize(String),
    /// 摘要请求成功但正文为空（全空白也算空）：没有可落库的摘要。
    #[error("compact: summarization returned no text")]
    EmptySummary,
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
                // 摘要覆盖到 `upto_seq`，有效历史从它的下一条原始消息继续（饱和加：正常
                // `upto_seq` 是 ≥1 的 seq，饱和只在异常数据上兜底，绝不 panic）
                compaction.upto_seq.saturating_add(1)
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

    /// 落一条压缩记录：`session_compactions` 追加一行，`session_messages` 一行不动。
    ///
    /// `upto_seq` 是已被摘要覆盖的最后一条消息的 `seq`（[`split_turns`] 切出的被压缩段的上界），
    /// 有效历史（[`SessionStore::effective_history`]）据此决定摘要消息之后从哪条原始消息继续。
    /// `created_at` 取这一次的墙钟。
    pub fn record_compaction(
        &self,
        session_id: &str,
        upto_seq: i64,
        summary: &str,
    ) -> Result<(), SessionError> {
        self.db
            .insert_compaction(session_id, upto_seq, summary, now_millis())?;
        Ok(())
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

/// 按「保留最后 `keep` 轮」切分有效历史：`user` 消息是轮次起点，切点 = 倒数第 `keep` 条 user 消息的
/// 下标；不足 `keep` 轮时被压缩段为空（返回 `(vec![], 全体)`）。
///
/// 一次反向扫描找切点：切点落在 user 上，保留段因此以一条 user 开头（不足 `keep` 轮时切点在开头，
/// 保留段是全体）。末轮没有答复（失败轮次只记 user）不影响切点；对不齐的尾部跟着它前面那条 user
/// 一起留在保留段里。
pub fn split_turns(
    messages: &[HistoryMessage],
    keep: usize,
) -> (Vec<HistoryMessage>, Vec<HistoryMessage>) {
    let mut seen = 0usize;
    let mut cut = 0usize;
    for (index, message) in messages.iter().enumerate().rev() {
        if message.role == Role::User {
            seen += 1;
            if seen == keep {
                cut = index;
                break;
            }
        }
    }
    let mut older = messages.to_vec();
    let retained = older.split_off(cut);
    (older, retained)
}

/// 旧轮次 → 摘要请求的 user 正文：每条 `user: {content}` / `assistant: {content}`，空行分隔。
///
/// 内容原样（换行保留）；空表给空串（调用方只在被压缩段非空时才发起请求）。
pub fn render_transcript(messages: &[HistoryMessage]) -> String {
    messages
        .iter()
        .map(|message| format!("{}: {}", role_word(message.role), message.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 一次无工具的 native 请求：`system` = [`COMPACT_SYSTEM_PROMPT`]、`user` = `transcript`，
/// 累加 `TextDelta` 作为摘要；`Done{Failed}`、其余非成功终态与**无终态**（流提前结束）都报
/// [`SessionError::Summarize`]，摘要正文为空（全空白也算）报 [`SessionError::EmptySummary`]。
///
/// 请求带 `config.limits` 与 `config.model`（调用方传会话的生效配置，`/model` 的会话级覆盖因此对摘要
/// 也生效）。`tools()` 恒空：摘要请求不带工具，模型真回了工具调用也只会收到一条失败结果。
pub async fn summarize_turns(
    runtime: Arc<dyn Runtime>,
    config: &Config,
    transcript: &str,
    cancel: CancellationToken,
) -> Result<String, SessionError> {
    let request = RunRequest {
        messages: vec![ChatMessage {
            role: Role::User,
            content: transcript.to_string(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }],
        system_prompt: COMPACT_SYSTEM_PROMPT.to_string(),
        model: config.model.clone(),
        limits: config.limits,
        executor: Arc::new(ToolLessExecutor),
    };

    let mut stream = runtime.run(request, cancel).await;
    let mut summary = String::new();
    let mut status: Option<RunStatus> = None;
    let mut failure: Option<String> = None;
    while let Some(event) = stream.next().await {
        match event {
            RuntimeEvent::TextDelta { text } => summary.push_str(&text),
            // 恰好一条 `Done` 收束整条流；记下终态与内核给的原因
            RuntimeEvent::Done {
                status: done,
                error,
                ..
            } => {
                status = Some(done);
                failure = error;
            }
            // 摘要请求不带工具、也不看推理：其余事件（Usage / ReasoningDelta / ToolExec…）忽略
            _ => {}
        }
    }

    match status {
        Some(RunStatus::Success) => {
            if summary.trim().is_empty() {
                return Err(SessionError::EmptySummary);
            }
            Ok(summary)
        }
        Some(done) => Err(SessionError::Summarize(
            failure.unwrap_or_else(|| done.as_str().to_string()),
        )),
        // 流结束却没有 `Done`：把缺失终态也报成摘要失败，不静默落半截正文
        None => Err(SessionError::Summarize(
            "model stream ended without a final status".to_string(),
        )),
    }
}

/// 摘要请求的工具执行器：`specs()` 恒空 → 请求体里没有 `tools` 字段。
///
/// 模型真的回了工具调用时给一条失败结果（这是防御性分支：摘要提示不含工具、请求也不带工具描述）。
struct ToolLessExecutor;

#[async_trait]
impl ToolExecutor for ToolLessExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn summarize(&self, _call: &ToolCall) -> String {
        String::new()
    }

    async fn execute(&self, _call: ToolCall, _cancel: CancellationToken) -> ToolResult {
        ToolResult {
            ok: false,
            output: Some("compact request has no tools".to_string()),
        }
    }
}

/// canonical 角色 → 正文标签（canonical 历史只用 user / assistant；另两值只为穷尽枚举）。
fn role_word(role: Role) -> &'static str {
    match role {
        Role::User => ROLE_USER,
        Role::Assistant => ROLE_ASSISTANT,
        Role::System => "system",
        Role::Tool => "tool",
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
///
/// 导出给命令层：`/compact` 重建内存历史时用同一份拼法，有效历史（[`SessionStore::effective_history`]）
/// 与内存视图因此逐字对得上。
pub fn summary_message(summary: &str) -> String {
    format!("{EARLIER_SUMMARY_PREFIX}\n\n{summary}")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_runtime::native::NativeRuntime;
    use ignis_runtime::openai::OpenAiClient;
    use ignis_runtime::test_support::{
        MockRuntime, ScriptStep, ScriptedModel, ScriptedModelHandle,
    };
    use ignis_runtime::{ModelConfig, Role, RunStatus, Runtime, RuntimeEvent};
    use ignis_storage::{Db, NewSession, NewSessionMessage, now_millis};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::{
        COMPACT_KEEP_TURNS, COMPACT_SYSTEM_PROMPT, EARLIER_SUMMARY_PREFIX, HistoryMessage,
        SESSION_TITLE_CHARS, SessionError, SessionStore, TITLE_FALLBACK, render_transcript,
        session_title, split_turns, summarize_turns,
    };
    use crate::config::Config;

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

    /// 用 `(role, content)` 造一段有效历史：`seq` 从 1 起连续（库里就是这样的行序）。
    fn history(entries: &[(Role, &str)]) -> Vec<HistoryMessage> {
        entries
            .iter()
            .enumerate()
            .map(|(index, (role, content))| HistoryMessage {
                seq: index as i64 + 1,
                role: *role,
                content: (*content).to_string(),
            })
            .collect()
    }

    /// 指向脚本化假模型的 native 配置（摘要请求真的走一遍 HTTP → SSE，不碰网络）。
    fn scripted_config(model: &ScriptedModelHandle) -> Config {
        Config {
            api_key: Some("test-key".into()),
            model: ModelConfig {
                base_url: model.base_url(),
                model: "test-model".into(),
                ..ModelConfig::default()
            },
            ..Config::default()
        }
    }

    /// 与 `commands::kernel::mk_runtime("native", …)` 同形的运行器：native + 假模型的 base_url。
    fn native_runtime(config: &Config) -> Arc<dyn Runtime> {
        Arc::new(NativeRuntime::new(
            OpenAiClient::new(&config.model, "test-key".into()).unwrap(),
        ))
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

    /// 切分：user 消息是轮次起点，切点落在**倒数第 `keep` 条 user** 的下标；不足 `keep` 轮时被压缩段
    /// 为空（保留段是全体）。
    #[test]
    fn split_turns_keeps_the_last_two_turns() {
        // 三轮六条：最后两轮保留，第一轮（user + assistant）进摘要段
        let messages = history(&[
            (Role::User, "第一句"),
            (Role::Assistant, "答复一"),
            (Role::User, "第二句"),
            (Role::Assistant, "答复二"),
            (Role::User, "第三句"),
            (Role::Assistant, "答复三"),
        ]);
        assert_eq!(COMPACT_KEEP_TURNS, 2);
        let (older, retained) = split_turns(&messages, COMPACT_KEEP_TURNS);
        assert_eq!(
            older.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            ["第一句", "答复一"]
        );
        assert_eq!(
            retained
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["第二句", "答复二", "第三句", "答复三"]
        );
        // 切出来的是副本：原表一条不动
        assert_eq!(messages.len(), 6);
        assert_eq!(older[0].seq, 1);
        assert_eq!(retained[0].seq, 3);

        // 刚好两轮：没有可压的旧轮次
        let two_turns = history(&[
            (Role::User, "第一句"),
            (Role::Assistant, "答复一"),
            (Role::User, "第二句"),
            (Role::Assistant, "答复二"),
        ]);
        let (older, retained) = split_turns(&two_turns, COMPACT_KEEP_TURNS);
        assert!(older.is_empty(), "两轮没有可压段：{older:?}");
        assert_eq!(retained, two_turns);

        // 三条消息（user/assistant 对不齐：末轮没有答复）：切点仍落在 user 边界——
        // 只数出两条 user，不足 `keep` 轮，被压缩段为空
        let misaligned = history(&[
            (Role::User, "第一句"),
            (Role::Assistant, "答复一"),
            (Role::User, "第二句"),
        ]);
        let (older, retained) = split_turns(&misaligned, COMPACT_KEEP_TURNS);
        assert!(older.is_empty(), "两条 user 不足 keep 轮：{older:?}");
        assert_eq!(retained, misaligned);

        // 对不齐但 user 多于 keep：切点落在 user 上（保留段从那条 user 开始，不切在别处）
        let longer = history(&[
            (Role::User, "一"),
            (Role::Assistant, "答一"),
            (Role::User, "二"),
            (Role::Assistant, "答二"),
            (Role::User, "三"),
        ]);
        let (older, retained) = split_turns(&longer, 1);
        assert_eq!(
            older.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            ["一", "答一", "二", "答二"]
        );
        assert_eq!(
            retained
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["三"]
        );
    }

    /// 摘要请求的正文：每条 `{role}: {content}`，条目之间空行；角色用 canonical 两值。
    #[test]
    fn render_transcript_labels_roles_and_separates_turns() {
        let messages = history(&[
            (Role::User, "第一句\n第二行"),
            (Role::Assistant, "答复一"),
            (Role::User, "第二句"),
        ]);
        assert_eq!(
            render_transcript(&messages),
            "user: 第一句\n第二行\n\nassistant: 答复一\n\nuser: 第二句"
        );
        // 空表给空串（调用方只在被压缩段非空时才发起摘要请求）
        assert_eq!(render_transcript(&[]), "");
    }

    /// 一次无工具的 native 请求：system 是钉死的英文提示、user 是渲染好的正文，请求体没有 `tools`。
    #[tokio::test]
    async fn summarize_turns_uses_the_pinned_system_prompt_without_tools() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("摘要正文".into())]);
        let config = scripted_config(&model);
        let older = history(&[(Role::User, "第一句"), (Role::Assistant, "答复一")]);
        let transcript = render_transcript(&older);

        let summary = summarize_turns(
            native_runtime(&config),
            &config,
            &transcript,
            CancellationToken::new(),
        )
        .await
        .expect("脚本化模型应给出摘要");
        assert_eq!(summary, "摘要正文");

        let requests = model.requests();
        assert_eq!(requests.len(), 1, "{requests:#?}");
        // 请求体的第一条消息就是 system（文本来自 `RunRequest.system_prompt`）
        assert_eq!(
            requests[0]["messages"][0],
            json!({"role": "system", "content": COMPACT_SYSTEM_PROMPT})
        );
        // 第二条是渲染好的正文，且整条请求只有这两条消息
        assert_eq!(requests[0]["messages"][1]["role"], "user");
        assert_eq!(requests[0]["messages"][1]["content"], json!(transcript));
        assert_eq!(requests[0]["messages"].as_array().unwrap().len(), 2);
        // 无工具执行器 → 请求体里没有 `tools` 字段
        assert!(
            requests[0].get("tools").is_none(),
            "摘要请求不该带工具：{:#?}",
            requests[0]
        );
    }

    /// 模型失败（内核折成 `Done{Failed}`）→ `SessionError::Summarize`，原因带上内核给的原文。
    #[tokio::test]
    async fn summarize_turns_reports_a_failed_model() {
        let model = ScriptedModel::start(vec![ScriptStep::Error {
            status: 401,
            body: r#"{"error":"bad key"}"#.into(),
        }]);
        let config = scripted_config(&model);

        let error = summarize_turns(
            native_runtime(&config),
            &config,
            "user: 你好",
            CancellationToken::new(),
        )
        .await
        .expect_err("失败的模型不该给出摘要");
        assert!(matches!(error, SessionError::Summarize(_)), "{error:?}");
        // 展示前缀逐字（`{0}` 是内核给的人话原因）
        assert!(
            error
                .to_string()
                .starts_with("compact: summarization failed: "),
            "{error}"
        );
    }

    /// 摘要正文全空白（模型只发了空白）→ `EmptySummary`：没有可落库的摘要，不落一段空白。
    #[tokio::test]
    async fn summarize_turns_rejects_a_whitespace_only_summary() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("   \n\t ".into())]);
        let config = scripted_config(&model);

        let error = summarize_turns(
            native_runtime(&config),
            &config,
            "user: 你好",
            CancellationToken::new(),
        )
        .await
        .expect_err("全空白的正文不算摘要");
        assert!(matches!(error, SessionError::EmptySummary), "{error:?}");
        assert_eq!(error.to_string(), "compact: summarization returned no text");
    }

    /// 流结束却没有 `Done`（内核实现缺陷）→ 固定的人话错误，不静默落半截正文。
    #[tokio::test]
    async fn summarize_turns_reports_a_stream_that_ends_without_a_status() {
        let runtime = MockRuntime::new(vec![RuntimeEvent::TextDelta {
            text: "半截".into(),
        }]);

        let error = summarize_turns(
            runtime,
            &Config::default(),
            "user: 你好",
            CancellationToken::new(),
        )
        .await
        .expect_err("没有终态的流必须报错");
        assert!(matches!(error, SessionError::Summarize(_)), "{error:?}");
        assert_eq!(
            error.to_string(),
            "compact: summarization failed: model stream ended without a final status"
        );
    }

    /// `Done{Failed}` 的内核原因原样透传进 `Summarize` 的展示文本（不吞、不改写）。
    #[tokio::test]
    async fn summarize_turns_passes_the_failure_reason_through() {
        let runtime = MockRuntime::new(vec![RuntimeEvent::Done {
            status: RunStatus::Failed,
            summary: None,
            error: Some("failure message".into()),
        }]);

        let error = summarize_turns(
            runtime,
            &Config::default(),
            "user: 你好",
            CancellationToken::new(),
        )
        .await
        .expect_err("失败的模型不该给出摘要");
        assert!(matches!(error, SessionError::Summarize(_)), "{error:?}");
        assert_eq!(
            error.to_string(),
            "compact: summarization failed: failure message"
        );
    }

    /// `record_compaction` 只往 `session_compactions` 追加一行：原始消息一条不删，
    /// `upto_seq` / `summary` 原样读回，有效历史据此重建成「摘要 + 保留段」。
    #[test]
    fn record_compaction_appends_a_row_and_keeps_the_raw_messages() {
        let env = Env::new("record-compaction");
        let (db, store) = open(&env.db_path());
        let session = store
            .create_session(&env.workspace(), "native", None, "压一压")
            .unwrap();
        for (prompt, reply) in [("一", "答一"), ("二", "答二"), ("三", "答三")] {
            store.append_turn(&session.id, prompt, Some(reply)).unwrap();
        }

        store
            .record_compaction(&session.id, 4, "前两轮的摘要")
            .unwrap();

        let record = db
            .latest_compaction(&session.id)
            .unwrap()
            .expect("compaction 行");
        assert_eq!(record.upto_seq, 4);
        assert_eq!(record.summary, "前两轮的摘要");
        assert!(record.created_at >= session.created_at, "{record:?}");
        // 原始行一条不删（压缩只是有效历史的视图）
        assert_eq!(db.session_messages_from(&session.id, 1).unwrap().len(), 6);
        let history = store.effective_history(&session.id).unwrap();
        assert_eq!(history.len(), 3, "{history:?}");
        assert_eq!(
            history[0].content,
            format!("{EARLIER_SUMMARY_PREFIX}\n\n前两轮的摘要")
        );
        assert_eq!(history[1].content, "三");
    }
}
