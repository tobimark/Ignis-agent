//! 工具调用分片组装：流式 `tool_calls` 的 `arguments` 会跨多个 chunk 到达，
//! 且多个 index 可能交叉，这里按 index 归位、按到达顺序拼接。

use std::collections::BTreeMap;

use crate::ToolCall;
use crate::openai::LlmError;

/// 按 index 组装流式工具调用；`id` / `name` 只在首个分片出现，`arguments` 逐段拼接。
#[derive(Default)]
pub struct ToolCallAssembler {
    slots: BTreeMap<usize, Slot>,
}

/// 单个工具调用的半成品。
#[derive(Default)]
struct Slot {
    id: Option<String>,
    name: Option<String>,
    args: String,
}

impl ToolCallAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一个分片；`None` 字段表示该分片没带这个信息（拼装语义，不覆盖已有值）。
    pub fn push_delta(
        &mut self,
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args_delta: Option<&str>,
    ) {
        let slot = self.slots.entry(index).or_default();
        if let Some(id) = id {
            slot.id = Some(id.to_string());
        }
        if let Some(name) = name {
            slot.name = Some(name.to_string());
        }
        if let Some(args_delta) = args_delta {
            slot.args.push_str(args_delta);
        }
    }

    /// 按 index 升序收口：拼好的 `arguments` 必须是合法 JSON，缺 id / name / arguments
    /// 都算响应格式异常——宁可报错，也不静默丢掉模型发出的调用。
    pub fn finish(&self) -> Result<Vec<ToolCall>, LlmError> {
        self.slots
            .iter()
            .map(|(index, slot)| {
                let id = slot.id.clone().ok_or_else(|| missing("id", *index))?;
                let name = slot.name.clone().ok_or_else(|| missing("name", *index))?;
                if slot.args.is_empty() {
                    return Err(missing("arguments", *index));
                }
                let args = serde_json::from_str(&slot.args).map_err(|err| {
                    LlmError::Malformed(format!("工具调用 {id} 的 arguments 不是合法 JSON：{err}"))
                })?;
                Ok(ToolCall { id, name, args })
            })
            .collect()
    }
}

/// 分片不完整的统一错误。
fn missing(field: &str, index: usize) -> LlmError {
    LlmError::Malformed(format!("工具调用分片不完整：index {index} 缺少 {field}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::ToolCall;

    use super::ToolCallAssembler;

    /// brief 钉死：多个 index 交叉分片按 index 归位，arguments 拼接后再 parse。
    #[test]
    fn tool_call_assembler_merges_interleaved_fragments() {
        let mut a = ToolCallAssembler::new();
        a.push_delta(0, Some("call_1"), Some("fs_read"), Some("{\"pa"));
        a.push_delta(1, Some("call_2"), Some("fs_list"), Some("{}"));
        a.push_delta(0, None, None, Some("th\":\"a.txt\"}"));
        assert_eq!(
            a.finish().unwrap(),
            vec![
                ToolCall {
                    id: "call_1".into(),
                    name: "fs_read".into(),
                    args: json!({"path":"a.txt"})
                },
                ToolCall {
                    id: "call_2".into(),
                    name: "fs_list".into(),
                    args: json!({})
                },
            ]
        );
    }

    /// 拼接结果不是合法 JSON、或分片缺 id/name：必须报「模型响应格式异常」，不能静默丢调用。
    #[test]
    fn tool_call_assembler_rejects_broken_fragments() {
        let mut broken_json = ToolCallAssembler::new();
        broken_json.push_delta(0, Some("call_1"), Some("fs_read"), Some("{\"path\":"));
        let err = broken_json.finish().unwrap_err();
        assert!(err.to_string().contains("模型响应格式异常"), "{err}");

        let mut missing_id = ToolCallAssembler::new();
        missing_id.push_delta(0, None, None, Some("{}"));
        assert!(missing_id.finish().is_err());

        // 没有工具调用时收口应得到空列表
        assert!(ToolCallAssembler::new().finish().unwrap().is_empty());
    }
}
