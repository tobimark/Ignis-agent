//! 工具层：`Tool` 契约、注册表与内置工具（文件工具、shell、HTTP）。
//!
//! 每个工具自行解析参数，路径一律先经 [`crate::security::resolve_in_workspace`] 落到工作区内；
//! 失败返回 [`ToolError`]（消息原样作为工具结果回填给模型），成功返回纯文本输出。

mod fs;
mod http;
mod meta;
mod notify;
mod runtime;
mod shell;
pub mod util;

pub use meta::meta_tools;
pub use runtime::ToolRuntime;

use std::path::Path;

use async_trait::async_trait;
use ignis_runtime::{Limits, ToolSpec};
use tokio_util::sync::CancellationToken;

/// 工具执行错误：消息会原样作为工具结果输出，需说明「哪条路径 / 哪个参数出了什么问题」。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(String);

impl ToolError {
    /// 用一段可读消息构造错误。
    pub fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// 一次工具调用的上下文：工作区根、取消信号、运行上限与来源身份。
///
/// `task_id` / `job_id` 是工具可见的 provenance：[`ToolRuntime`] 填上本次任务的 id，调度运行
/// 还会透传来源 job id（`ignis run` / REPL 的交互运行没有 job，为 `None`）。`notify_user` 用
/// 它们填 `alerts.source_task_id` / `source_job_id`，模型无需自己传这两个 id。
pub struct ToolCtx<'a> {
    pub workspace: &'a Path,
    pub cancel: CancellationToken,
    pub limits: &'a Limits,
    pub task_id: Option<String>,
    pub job_id: Option<String>,
}

/// 工具契约：`spec` 是暴露给模型的描述，`run` 执行并返回文本输出。
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, args: &serde_json::Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError>;
}

/// 工具注册表：`specs` 的顺序即注册顺序，也就是模型看到的工具表顺序。
#[derive(Default)]
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    /// 注册一个工具：后注册的排在工具表末尾。
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// 全部工具的规格。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    /// 按名查工具；未注册返回 `None`。
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .map(|tool| tool.as_ref())
    }
}

/// v0.1 内置工具集：四个文件工具 + `shell_run` + `http_fetch`。
pub fn default_registry() -> Registry {
    let mut registry = Registry::default();
    registry.register(Box::new(fs::FsRead));
    registry.register(Box::new(fs::FsList));
    registry.register(Box::new(fs::FsWrite));
    registry.register(Box::new(fs::FsEdit));
    registry.register(Box::new(shell::ShellRun));
    registry.register(Box::new(http::HttpFetch::new()));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_exposes_the_builtin_tools() {
        let registry = default_registry();
        let specs = registry.specs();
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "fs_read",
                "fs_list",
                "fs_write",
                "fs_edit",
                "shell_run",
                "http_fetch"
            ]
        );

        for spec in &specs {
            assert!(!spec.description.is_empty(), "{}", spec.name);
            assert_eq!(spec.parameters["type"], "object", "{}", spec.name);
        }
        // 参数契约：文件工具除 fs_list 外都必须给 path，shell_run 必须给 command，http_fetch 必须给 url
        for (name, required) in [
            ("fs_read", "path"),
            ("fs_write", "path"),
            ("fs_edit", "path"),
            ("shell_run", "command"),
            ("http_fetch", "url"),
        ] {
            let parameters = &registry.get(name).unwrap().spec().parameters;
            assert!(
                parameters["required"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(required)),
                "{name}: {parameters}"
            );
        }
        assert!(registry.get("fs_missing").is_none());
    }
}
