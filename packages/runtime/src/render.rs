//! 外部内核（codex / claude）的 canonical 会话渲染：把 system 提示与消息历史折成一个 prompt 字符串。
//!
//! 外部 CLI 说不了 Ignis 的消息协议（决策 #11）：不依赖它们各自的 `--resume`，每轮把
//! 「system 提示 + 历史 + 本轮输入」渲染成单个字符串交给 CLI 的位置参数。渲染保留文本、工具调用
//! 的概要（工具名 + 紧凑参数 JSON）与工具结果的首行——够模型接着往下走，又不把整份工具输出
//! 塞进 prompt。[`render_external_prompt`] 是两个适配器共用的唯一渲染入口。

use crate::{ChatMessage, Role};

/// 工具结果在 prompt 里保留的字符上限（只取首行）。
const TOOL_RESULT_CHARS: usize = 200;
/// tool 消息拿不到工具名与 id 时的占位（调用方没填 `tool_call_id`）。
const UNKNOWN_TOOL: &str = "unknown";

/// 渲染外部内核的 prompt：`{system}\n\n---\n` 之后按序逐条消息各占一行（assistant 带
/// `tool_calls` 时再追加一行调用概要），空历史只留 system 与分隔线。
///
/// 标签冻结三个（`User:` / `Assistant:` / `Tool {名} →`）：Ignis 的 system 提示单独走
/// `system_prompt` 参数，历史里罕见的 `System:` 消息照常成行——不在冻结集里是刻意的，
/// 但宁可多一行也不静默丢掉。内容为空时不留冒号后的尾随空格（纯工具调用轮的 `Assistant:`）。
pub fn render_external_prompt(system_prompt: &str, messages: &[ChatMessage]) -> String {
    let mut prompt = format!("{system_prompt}\n\n---\n");
    for (index, message) in messages.iter().enumerate() {
        match message.role {
            Role::System => prompt.push_str(&message_line("System:", &message.content)),
            Role::User => prompt.push_str(&message_line("User:", &message.content)),
            Role::Assistant => {
                prompt.push_str(&message_line("Assistant:", &message.content));
                if !message.tool_calls.is_empty() {
                    prompt.push_str(&format!("Assistant: [calls: {}]\n", calls_line(message)));
                }
            }
            Role::Tool => {
                let name = resolve_tool_name(messages, index, message.tool_call_id.as_deref());
                let first_line = message.content.lines().next().unwrap_or("").trim();
                let result = crate::cli_process::truncate(first_line, TOOL_RESULT_CHARS);
                prompt.push_str(&message_line(&format!("Tool {name} →"), &result));
            }
        }
    }
    prompt
}

/// 一行消息：`{head} {内容}`；内容为空时省略分隔空格（不留行尾空格，纯工具调用轮的 `Assistant:`）。
fn message_line(head: &str, content: &str) -> String {
    if content.is_empty() {
        format!("{head}\n")
    } else {
        format!("{head} {content}\n")
    }
}

/// assistant 的工具调用概要：`名字(紧凑 JSON)`，逐条以 `, ` 相连。
///
/// 参数用 `serde_json::to_string` 的紧凑形式（`{"path":"a.txt"}`）；序列化失败只可能是不可序列化
/// 的值，那种调用退成 `{}`，不因一条坏参数丢掉整行概要。
fn calls_line(message: &ChatMessage) -> String {
    message
        .tool_calls
        .iter()
        .map(|call| {
            let args = serde_json::to_string(&call.args).unwrap_or_else(|_| "{}".into());
            format!("{}({args})", call.name)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// tool 消息的工具名：扫它**前面**的 assistant `tool_calls`，按 `tool_call_id` 匹配（最近的调用
/// 优先）；匹配不上退回 id 本身，连 id 都没有时用 [`UNKNOWN_TOOL`]。
fn resolve_tool_name(messages: &[ChatMessage], index: usize, id: Option<&str>) -> String {
    let Some(id) = id.map(str::trim).filter(|id| !id.is_empty()) else {
        return UNKNOWN_TOOL.to_string();
    };
    messages[..index]
        .iter()
        .rev()
        .flat_map(|message| message.tool_calls.iter())
        .find(|call| call.id == id)
        .map(|call| call.name.clone())
        .unwrap_or_else(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{ChatMessage, Role, ToolCall};

    /// system 提示（渲染只做拼接，不加工正文）。
    const SYSTEM: &str = "你是 Ignis";

    fn user(content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    fn assistant(content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    /// 带工具调用的 assistant 消息（`content` 可空，真机常见纯调用轮）。
    fn assistant_with_calls(content: &str, calls: Vec<(&str, &str, Value)>) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_calls: calls
                .into_iter()
                .map(|(id, name, args)| ToolCall {
                    id: id.into(),
                    name: name.into(),
                    args,
                })
                .collect(),
        }
    }

    /// tool 消息：`tool_call_id` 为 `None` 复刻调用方没填 id 的形状。
    fn tool(content: &str, tool_call_id: Option<&str>) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: tool_call_id.map(str::to_string),
            tool_calls: Vec::new(),
        }
    }

    /// 空历史：只有 system 与分隔线，后面什么都不追加（连多余的空行都没有）。
    #[test]
    fn empty_history_renders_only_the_separator() {
        assert_eq!(render_external_prompt(SYSTEM, &[]), "你是 Ignis\n\n---\n");
    }

    /// 多轮历史按序逐条渲染：角色名首字母大写、一条一行。
    #[test]
    fn renders_a_multi_turn_conversation_in_order() {
        let messages = vec![
            user("第一个问题"),
            assistant("第一个答复"),
            user("第二个问题"),
        ];

        assert_eq!(
            render_external_prompt(SYSTEM, &messages),
            "你是 Ignis\n\n---\n\
             User: 第一个问题\n\
             Assistant: 第一个答复\n\
             User: 第二个问题\n"
        );
    }

    /// assistant 的 `tool_calls` 追加一行：`名字(紧凑 JSON)`，逐条以 `, ` 相连。
    #[test]
    fn assistant_tool_calls_render_as_a_compact_line() {
        let messages = vec![assistant_with_calls(
            "先读文件",
            vec![
                ("call_1", "fs_read", json!({"path": "a.txt"})),
                ("call_2", "fs_list", json!({})),
            ],
        )];

        assert_eq!(
            render_external_prompt(SYSTEM, &messages),
            "你是 Ignis\n\n---\n\
             Assistant: 先读文件\n\
             Assistant: [calls: fs_read({\"path\":\"a.txt\"}), fs_list({})]\n"
        );
    }

    /// tool 消息的工具名由渲染器自己解析：扫前面的 assistant `tool_calls` 按 id 匹配；
    /// 结果只取首行（多行输出的后续行不出现）。
    #[test]
    fn tool_messages_resolve_the_name_from_the_preceding_call() {
        let messages = vec![
            user("读一下"),
            assistant_with_calls("", vec![("call_1", "fs_read", json!({"path": "a.txt"}))]),
            tool("文件第一行\n文件第二行", Some("call_1")),
            user("谢谢"),
        ];

        assert_eq!(
            render_external_prompt(SYSTEM, &messages),
            "你是 Ignis\n\n---\n\
             User: 读一下\n\
             Assistant:\n\
             Assistant: [calls: fs_read({\"path\":\"a.txt\"})]\n\
             Tool fs_read → 文件第一行\n\
             User: 谢谢\n"
        );
    }

    /// 只认「前面」的调用：匹配不上（id 未知、或调用出现在 tool 消息之后）就退回 id 本身；
    /// 连 id 都没有时用占位名。
    #[test]
    fn tool_name_falls_back_to_the_id_when_no_preceding_call_matches() {
        let messages = vec![
            tool("孤儿结果", Some("call_9")),
            assistant_with_calls("", vec![("call_9", "fs_read", json!({}))]),
            tool("没有 id 的结果", None),
        ];

        assert_eq!(
            render_external_prompt(SYSTEM, &messages),
            "你是 Ignis\n\n---\n\
             Tool call_9 → 孤儿结果\n\
             Assistant:\n\
             Assistant: [calls: fs_read({})]\n\
             Tool unknown → 没有 id 的结果\n"
        );
    }

    /// 工具结果首行按字符截断到 200（多字节安全，附省略号），超长结果的剩余部分不出现。
    #[test]
    fn tool_result_takes_the_first_line_and_truncates_to_200_chars() {
        let long = "字".repeat(TOOL_RESULT_CHARS + 25);
        let messages = vec![
            assistant_with_calls("", vec![("call_1", "shell_run", json!({"command": "x"}))]),
            tool(&format!("{long}\n尾巴"), Some("call_1")),
        ];

        let rendered = render_external_prompt(SYSTEM, &messages);

        assert_eq!(
            rendered,
            format!(
                "你是 Ignis\n\n---\nAssistant:\nAssistant: [calls: shell_run({{\"command\":\"x\"}})]\n\
                 Tool shell_run → {}…\n",
                "字".repeat(TOOL_RESULT_CHARS)
            )
        );
        assert!(!rendered.contains("尾巴"), "{rendered}");
        // 短结果不加工（不加省略号）
        assert!(
            render_external_prompt(SYSTEM, &[tool("短结果", Some("call_1"))])
                .ends_with("Tool call_1 → 短结果\n"),
        );
    }

    /// system 角色的消息（历史里罕见）照常成行，不被静默丢掉。
    #[test]
    fn system_messages_are_rendered_too() {
        let messages = vec![ChatMessage {
            role: Role::System,
            content: "只读模式".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }];

        assert_eq!(
            render_external_prompt(SYSTEM, &messages),
            "你是 Ignis\n\n---\nSystem: 只读模式\n"
        );
    }

    /// 空内容的消息不留行尾空格（纯工具调用轮的 `Assistant:`，旧写法是 `Assistant: `）；
    /// 非空内容与既有的三标签逐字节不变。
    #[test]
    fn empty_content_leaves_no_trailing_space() {
        let messages = vec![
            user(""),
            assistant_with_calls("", vec![("call_1", "fs_read", json!({}))]),
            tool("", Some("call_1")),
        ];

        let rendered = render_external_prompt(SYSTEM, &messages);

        assert_eq!(
            rendered,
            "你是 Ignis\n\n---\n\
             User:\n\
             Assistant:\n\
             Assistant: [calls: fs_read({})]\n\
             Tool fs_read →\n"
        );
        assert!(
            !rendered.lines().any(|line| line.ends_with(' ')),
            "任何一行都不该以空格收尾：{rendered:?}"
        );
    }
}
