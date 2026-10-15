//! 输出脱敏：把已知密钥从工具输出与日志文本里抹掉。

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

    #[test]
    fn redactor_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Redactor>();
    }
}
