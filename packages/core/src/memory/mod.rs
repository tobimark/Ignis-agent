//! 长期记忆（memory）：事实（fact）的语义层与人类可读的 Markdown 镜像。
//!
//! 行的读写由 storage 的 `facts` 表 DAO 负责，这里固定上层语义：
//! - **supersede**：同一 `(scope, workspace, key)` 写入新值即把旧行标成 `superseded`，检索与
//!   注入只看最新值（[`MemoryStore::remember`]），支撑「纠正后不再用旧信息」；
//! - **注入**：任务上下文里本工作区的 `project` 事实无条件带上，`user`/`global` 事实按 prompt
//!   拆出的检索项取前 6 条（[`MemoryStore::prompt_facts`]）；
//! - **镜像**：每次写入都把受影响的作用域重写成 `{mirror_dir}/*.md`，供人直接阅读；镜像是
//!   派生物，**写失败只打一行警告**（fail-soft），绝不回滚事实或让调用方失败。
//!
//! 镜像目录由调用方提供（CLI 用 `ignis_home()` 拼），本模块不读环境变量。

mod mirror;
mod store;

pub use store::MemoryStore;

/// 一条事实文本的字符上限。事实会逐字注入**每一次**后续任务的系统提示，写入前就截断
/// （截断点落在字符边界，末尾补 `…`），库、镜像与提示拿到的永远是这一份。
pub(crate) const MAX_FACT_TEXT_CHARS: usize = 500;
/// 一条事实 key 的字符上限；key 与文本同在一行里（镜像与提示），同样在写入前截断。
pub(crate) const MAX_FACT_KEY_CHARS: usize = 120;

/// 归一化事实字段：所有空白（含 `\n`、`\t`）折叠成单个空格、控制字符剔除、首尾空白裁掉。
///
/// 一条事实在系统提示里是一行（`- [scope/key] text`）、在镜像里是一个 bullet：文本里残留的
/// `\n` 会把一条事实撑成好几行，看起来就像另一条指令/另一个 bullet——而事实文本可能是
/// `http_fetch` 之类不可信来源转述过来的。写入前在此收口，后续所有读路径拿到的都是单行文本。
/// 与 [`crate::skills::frontmatter`] 的清洗同一套思路（那边按字段的 `fold_spaces` 开关复用）。
pub(crate) fn normalize_fact_text(value: &str) -> String {
    let mut cleaned = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            // 开头的空白不占位，中间的连续空白合并成一个空格
            pending_space = !cleaned.is_empty();
            continue;
        }
        if ch.is_control() {
            continue;
        }
        if pending_space {
            cleaned.push(' ');
            pending_space = false;
        }
        cleaned.push(ch);
    }
    cleaned
}

/// 把已归一化的事实字段截到 `max_chars` 个字符：截断点落在字符边界，末尾补一个 `…`。
///
/// 超长文本就是这么被挡在库外的（[`MAX_FACT_TEXT_CHARS`]/[`MAX_FACT_KEY_CHARS`]）；库、镜像与
/// 提示里永远是这一份，读路径不需要再截。
pub(crate) fn truncate_fact_field(value: &str, max_chars: usize) -> String {
    let Some((end, _)) = value.char_indices().nth(max_chars) else {
        return value.to_owned();
    };
    let mut truncated = String::with_capacity(end + '…'.len_utf8());
    truncated.push_str(&value[..end]);
    truncated.push('…');
    truncated
}

use ignis_storage::StorageError;
use serde::{Deserialize, Serialize};

/// 「已记住」回执：`已记住（{scope}/{key}）：{text}`；这次写入取代了旧值时追加 `（旧值已失效）`。
///
/// 元工具 `remember` 与 CLI `ignis memory add` **共用这一份实现**：回执逐字节一致，改文案只改
/// 这里（过去 CLI 复制过同一段格式化代码，core 改字就会悄悄劈成两种输出）。
/// `key`/`text` 与写入路径同口径裁掉首尾空白——回执里出现的是真正入库的那份文本。
pub fn remembered_notice(scope: FactScope, key: &str, text: &str, superseded: bool) -> String {
    let mut notice = format!(
        "已记住（{}/{}）：{}",
        scope.as_str(),
        key.trim(),
        text.trim()
    );
    if superseded {
        notice.push_str("（旧值已失效）");
    }
    notice
}

/// 事实的作用域，取值与 `facts.scope` 一一对应（serde 用小写，供元工具参数解析）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactScope {
    /// 跨项目的用户偏好与环境信息（`workspace` 必须为空）。
    User,
    /// 某个工作区的事实（`workspace` 必须是该工作区的绝对路径）。
    Project,
    /// 所有项目通用的知识（`workspace` 必须为空）。
    Global,
}

impl FactScope {
    /// 写入 `facts.scope` 的字面量。
    pub fn as_str(&self) -> &'static str {
        match self {
            FactScope::User => "user",
            FactScope::Project => "project",
            FactScope::Global => "global",
        }
    }

    /// 反解 `facts.scope`；库里出现未知取值时返回 `None`（跳过该行，不 panic）。
    pub(crate) fn from_db_str(value: &str) -> Option<FactScope> {
        match value {
            "user" => Some(FactScope::User),
            "project" => Some(FactScope::Project),
            "global" => Some(FactScope::Global),
            _ => None,
        }
    }
}

/// 记忆层错误。
///
/// [`MemoryError::NotFound`] 是给 CLI 看的信号：`memory edit/delete <不存在的 id>` 会打印
/// `error: memory not found: {id}`；其余是校验类错误，消息均为中文人话。
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// SQLite 读写失败。
    #[error(transparent)]
    Storage(StorageError),
    /// 写镜像文件失败；只有显式的 [`MemoryStore::rebuild_mirror`] 会把它抛出来。
    #[error("镜像写入失败：{0}")]
    Io(#[from] std::io::Error),
    /// 读改或删除一条不存在的事实。
    #[error("memory not found: {id}")]
    NotFound { id: i64 },
    /// `key` 为空或只有空白。
    #[error("记忆的 key 不能为空")]
    EmptyKey,
    /// `text` 为空或只有空白。
    #[error("记忆的 text 不能为空")]
    EmptyText,
    /// `scope = project` 却没带工作区。
    #[error("project 范围的记忆必须带上工作区")]
    MissingWorkspace,
    /// `scope` 为 `user`/`global` 却带了工作区。
    #[error("user/global 范围的记忆不能绑定工作区")]
    UnexpectedWorkspace,
}

/// `StorageError::FactNotFound` 也映射成 [`MemoryError::NotFound`]：跨进程读写时先查到行、
/// 写下去时行已消失的情况下，调用方看到的仍是同一个「找不到」信号。
impl From<StorageError> for MemoryError {
    fn from(source: StorageError) -> Self {
        match source {
            StorageError::FactNotFound(id) => MemoryError::NotFound { id },
            other => MemoryError::Storage(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FactScope, remembered_notice};

    #[test]
    fn remembered_notice_is_the_one_receipt_shared_with_the_cli() {
        // 逐字钉死：元工具与 CLI 都走这一份实现（CLI 只额外补一个换行）
        assert_eq!(
            remembered_notice(
                FactScope::User,
                "pkg_manager",
                "使用 uv 管理 Python 依赖",
                false
            ),
            "已记住（user/pkg_manager）：使用 uv 管理 Python 依赖"
        );
        assert_eq!(
            remembered_notice(FactScope::Global, "editor", "偏好 Neovim", true),
            "已记住（global/editor）：偏好 Neovim（旧值已失效）"
        );
        // 与写入路径同口径裁掉首尾空白：回执里是真正入库的那份文本
        assert_eq!(
            remembered_notice(FactScope::Project, "  editor  ", "  偏好 Neovim  ", false),
            "已记住（project/editor）：偏好 Neovim"
        );
    }

    #[test]
    fn fact_scope_round_trips_as_lowercase_json() {
        for (scope, literal) in [
            (FactScope::User, "user"),
            (FactScope::Project, "project"),
            (FactScope::Global, "global"),
        ] {
            assert_eq!(scope.as_str(), literal);
            assert_eq!(
                serde_json::to_string(&scope).unwrap(),
                format!("\"{literal}\"")
            );
            assert_eq!(
                serde_json::from_str::<FactScope>(&format!("\"{literal}\"")).unwrap(),
                scope
            );
        }
        // 元工具参数不接受其它拼写
        assert!(serde_json::from_str::<FactScope>("\"Project\"").is_err());
    }
}
