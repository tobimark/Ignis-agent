//! 工具输出与参数辅助。

use serde_json::Value;

use super::ToolError;

/// 把输出截到 `limit` 字节以内；截断点回退到字符边界（不切坏 UTF-8），并追加尾部标记
/// `\n… [truncated: output exceeded {limit} bytes]`，其余情况下原样返回。
pub fn truncate_output(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = String::with_capacity(end + 64);
    output.push_str(&s[..end]);
    output.push_str(&format!("\n… [truncated: output exceeded {limit} bytes]"));
    output
}

/// 取必填字符串参数；缺失或类型不对都给出可操作的错误。
pub(crate) fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    optional_str(args, key)?
        .ok_or_else(|| ToolError::message(format!("缺少必需的参数 `{key}`（字符串）")))
}

/// 取可选字符串参数；字段缺失与显式 `null` 等价，类型不对报错。
pub(crate) fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| ToolError::message(format!("参数 `{key}` 必须是字符串"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_returned_unchanged() {
        assert_eq!(truncate_output("hello", 5), "hello");
        assert_eq!(truncate_output("", 0), "");
    }

    #[test]
    fn truncation_keeps_char_boundaries_and_appends_marker() {
        // 每个 `é` 占 2 字节，limit=5 落在字符中间：必须回退到 4 字节处
        let text = "é".repeat(10);
        let output = truncate_output(&text, 5);
        assert_eq!(output, "éé\n… [truncated: output exceeded 5 bytes]");
        assert!(!output.contains('\u{fffd}'));
    }
}
