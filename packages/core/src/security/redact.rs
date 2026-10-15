//! 输出脱敏：把已知密钥从工具输出与日志文本里抹掉。

/// 脱敏器：密钥集合在构造时固定，[`Redactor::redact`] 只做字面替换。
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// 空字符串会被忽略——否则替换会在每个字符之间插入 `***`。
    pub fn new(secrets: Vec<String>) -> Self {
        Self {
            secrets: secrets.into_iter().filter(|s| !s.is_empty()).collect(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn redactor_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Redactor>();
    }
}
