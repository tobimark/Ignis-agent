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

use ignis_storage::StorageError;
use serde::{Deserialize, Serialize};

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
    use super::FactScope;

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
