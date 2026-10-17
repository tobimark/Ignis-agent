//! 输出脱敏：把已知密钥从工具输出与日志文本里抹掉。

use serde_json::Value;

/// 脱敏器：密钥集合在构造时固定，[`Redactor::redact`] 只做字面替换。
pub struct Redactor {
    /// 每个密钥的待替换字面形式：JSON 转义形式在前、原文在后（见 [`Redactor::new`]）。
    secrets: Vec<String>,
}

impl Redactor {
    /// 空字符串会被忽略——否则替换会在每个字符之间插入 `***`。
    ///
    /// 每个密钥登记两种字面形式，且**先**登记 JSON 转义形式（[`serde_json::to_string`] 去掉
    /// 两端引号）：JSONL 行是序列化之后的文本，密钥里的 `"` / `\` / 换行在行内分别写作
    /// `\"` / `\\` / `\n`，只替换原文会漏掉这些出现（明文原样落盘）。转义形式在前还避免了
    /// 「密钥含 `"` 时先按原文替换，把行里的结构引号也换成 `***`、整行不再是合法 JSON」。
    pub fn new(secrets: Vec<String>) -> Self {
        let mut forms: Vec<String> = Vec::new();
        for secret in secrets {
            if secret.is_empty() {
                continue;
            }
            // `to_string` 对字符串不会失败；万一失败就只留原文（保守：绝不 panic）
            let quoted = serde_json::to_string(&secret).unwrap_or_default();
            if let Some(escaped) = quoted.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                push_unique(&mut forms, escaped.to_string());
            }
            push_unique(&mut forms, secret);
        }
        Self { secrets: forms }
    }

    /// 每个非空 secret 的所有出现都替换为 `***`。
    pub fn redact(&self, s: &str) -> String {
        let mut redacted = s.to_string();
        for secret in &self.secrets {
            if redacted.contains(secret.as_str()) {
                redacted = redacted.replace(secret.as_str(), "***");
            }
        }
        redacted
    }

    /// 原地递归脱敏一个 JSON 值：只替换**字符串叶子**。
    ///
    /// 对象键是事件字段名 / 工具参数名（不是用户文本），一律不动；数组与对象的形状原样保留；
    /// 数字 / 布尔 / Null 不碰。两条调用链共用本方法：`TaskRunner::redact_for_sink` 转发的
    /// [`ignis_runtime::RuntimeEvent`] 副本，以及 [`crate::tools::ToolRuntime`] 交给审批方的
    /// `ApprovalRequest.args`（交互提示的 `[d]etails` 会原样打印这份 JSON）。
    pub fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.redact(text),
            Value::Array(items) => {
                for item in items {
                    self.redact_value(item);
                }
            }
            Value::Object(fields) => {
                for field in fields.values_mut() {
                    self.redact_value(field);
                }
            }
            // 数字 / 布尔 / Null 不带可替换的文本
            Value::Number(_) | Value::Bool(_) | Value::Null => {}
        }
    }
}

/// 去重后追加：没有 `"` / `\` 之类的字符时，转义形式与原文相同，不必登记两次。
fn push_unique(forms: &mut Vec<String>, form: String) {
    if !forms.contains(&form) {
        forms.push(form);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_every_occurrence_of_each_secret() {
        let redactor = Redactor::new(vec!["sk-x".into(), "hunter2".into()]);
        assert_eq!(redactor.redact("a sk-x b sk-x c"), "a *** b *** c");
        assert_eq!(redactor.redact("pw=hunter2"), "pw=***");
        assert_eq!(redactor.redact("nothing here"), "nothing here");
    }

    #[test]
    fn empty_secrets_are_skipped() {
        let redactor = Redactor::new(vec![String::new(), "sk-y".into()]);
        // 空串若不跳过，替换会在每个字符之间插入 ***
        let redacted = redactor.redact("token=sk-y");
        assert_eq!(redacted, "token=***");
        assert!(!redacted.contains("sk-y"));

        assert_eq!(Redactor::new(Vec::new()).redact("plain"), "plain");
    }

    #[test]
    fn secrets_are_replaced_literally() {
        // 密钥里的正则元字符不得被当作模式
        let redactor = Redactor::new(vec!["a.b*c".into()]);
        assert_eq!(redactor.redact("xa.b*c"), "x***");
        assert_eq!(redactor.redact("axbxc"), "axbxc");
    }

    /// 密钥含 `"` 与 `\` 时，整行脱敏看到的是**序列化后**的 JSON：行里是 `\"` 与 `\\`，
    /// 只替换原文会漏掉，明文就这样进了 JSONL。替换还必须保住行的 JSON 合法形状。
    #[test]
    fn redacts_the_json_escaped_form_of_secrets() {
        let secret = r#"sk-"a\b"#;
        let redactor = Redactor::new(vec![secret.to_string()]);
        let line = serde_json::to_string(&json!({
            "type": "tool_result",
            "output": format!("token={secret}\n"),
            "summary": format!("Read {secret} from notes.txt"),
        }))
        .unwrap();
        assert!(line.contains(r#"sk-\"a\\b"#), "{line}");

        let redacted = redactor.redact(&line);

        assert!(!redacted.contains("sk-"), "{redacted}");
        assert!(!redacted.contains(r#"\"a\\b"#), "{redacted}");
        // 转义替换不得把行改成非法 JSON，也不得把行里的结构引号一起抹掉
        let parsed: serde_json::Value =
            serde_json::from_str(&redacted).expect("脱敏后的行必须仍是合法 JSON");
        assert_eq!(parsed["output"], json!("token=***\n"));
        assert_eq!(parsed["summary"], json!("Read *** from notes.txt"));

        // 原文形式（非 JSON 文本里的密钥）照旧被替换
        assert_eq!(redactor.redact(&format!("pw={secret}")), "pw=***");
    }

    /// [`Redactor::redact_value`] 只替换字符串叶子：对象键是事件字段名 / 工具参数名（不是用户
    /// 文本，原样保留），数组与对象的形状原样保留，数字 / 布尔 / Null 不碰。
    #[test]
    fn redact_value_replaces_string_leaves_only() {
        let redactor = Redactor::new(vec!["sk-test-secret".into()]);

        let mut value = json!({
            // 键名（字段名 / 参数名）不是值：一律不动
            "sk-test-secret_note": "键名不动",
            "command": "echo sk-test-secret",
            "nested": {
                "list": ["sk-test-secret", 7, true, null],
                "deep": { "text": "a sk-test-secret b" },
            },
            "count": 3,
            "enabled": false,
            "missing": null,
        });

        redactor.redact_value(&mut value);

        assert_eq!(
            value,
            json!({
                "sk-test-secret_note": "键名不动",
                "command": "echo ***",
                "nested": {
                    "list": ["***", 7, true, null],
                    "deep": { "text": "a *** b" },
                },
                "count": 3,
                "enabled": false,
                "missing": null,
            })
        );
        // 标量与形状逐项复核：数字 / 布尔 / Null 不参与替换，数组长度与对象字段数不变
        assert_eq!(value["count"], json!(3));
        assert_eq!(value["enabled"], json!(false));
        assert!(value["missing"].is_null());
        assert_eq!(value["nested"]["list"].as_array().unwrap().len(), 4);
        assert_eq!(value["nested"].as_object().unwrap().len(), 2);

        // 密钥只出现在值里时（审批参数的实际形状），整值序列化后不含明文
        let mut args = json!({
            "command": "echo sk-test-secret",
            "nested": { "list": ["sk-test-secret"] },
        });
        redactor.redact_value(&mut args);
        let serialized = serde_json::to_string(&args).unwrap();
        assert!(!serialized.contains("sk-test-secret"), "{serialized}");

        // 不带密钥的值原样返回（空密钥集合同理）
        let mut plain = json!({"n": 1, "s": "nothing to hide"});
        redactor.redact_value(&mut plain);
        assert_eq!(plain, json!({"n": 1, "s": "nothing to hide"}));
        Redactor::new(Vec::new()).redact_value(&mut plain);
        assert_eq!(plain, json!({"n": 1, "s": "nothing to hide"}));
    }

    #[test]
    fn redactor_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Redactor>();
    }
}
