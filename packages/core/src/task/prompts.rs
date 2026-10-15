//! 系统提示词：人格、工作区与工具用法拼成内核的 system prompt。

use std::path::Path;

use ignis_runtime::ToolSpec;

/// 组装 system prompt：人格一行 + 工作区路径 + 工具使用要求 + 「用用户的语言回答」。
///
/// 工具逐个点名（完整描述由内核的 `tools` 字段下发），让模型知道手上有什么、在哪儿干活。
pub fn build_system_prompt(workspace: &Path, tools: &[ToolSpec]) -> String {
    let names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You are Ignis, a personal AI agent that does real work with tools.\n\
         Workspace: {}\n\
         Use the tools instead of guessing; prefer absolute paths inside the workspace, and keep \
         replies concise.\n\
         Available tools: {names}\n\
         Answer in the user's language.",
        workspace.display()
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn system_prompt_mentions_workspace_and_tools() {
        let tools = crate::tools::default_registry().specs();

        let prompt = build_system_prompt(Path::new("/tmp/ignis-ws"), &tools);

        assert!(prompt.contains("/tmp/ignis-ws"), "{prompt}");
        for name in [
            "fs_read",
            "fs_list",
            "fs_write",
            "fs_edit",
            "shell_run",
            "http_fetch",
        ] {
            assert!(prompt.contains(name), "{prompt}");
        }
        // 人格一行 + 「用用户的语言回答」
        assert!(prompt.contains("Ignis"), "{prompt}");
        assert!(prompt.to_lowercase().contains("language"), "{prompt}");
    }
}
