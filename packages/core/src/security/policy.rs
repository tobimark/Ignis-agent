//! 审批策略：按风险等级与运行模式给出放行/询问/拒绝，并支持会话级授权。

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard, PoisonError};

use ignis_runtime::RiskLevel;

/// 非交互且未预授权时的拒绝原因。
const DENY_UNATTENDED: &str = "非交互模式未授权";
/// 预授权未开 `--unsafe` 时对危险操作的拒绝原因。
const DENY_UNSAFE_REQUIRED: &str = "危险操作需显式 --unsafe 授权";

/// 审批模式：由 CLI 的 `--yes` / `--unsafe` 与非 tty 检测决定（Task 13 的 `resolve_mode`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// 交互式终端：逐次询问。
    Interactive,
    /// 已预授权；只有 `unsafe_ops` 为真才放行危险操作。
    Preauthorized { unsafe_ops: bool },
    /// 非交互且未预授权：fail-closed。
    Unattended,
}

/// 一次判定结果：`Allow` 直接执行、`Ask` 交给审批方、`Deny(reason)` 拒绝并记录原因。
#[derive(Debug, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny(String),
}

/// 审批策略：模式固定，会话规则只影响 Interactive 下的 Elevated 决策。
pub struct Policy {
    mode: ApprovalMode,
    /// 会话内已授权的 `kind_key`（工具名粒度）。
    session_allowed: Mutex<HashSet<String>>,
}

impl Policy {
    pub fn new(mode: ApprovalMode) -> Self {
        Self {
            mode,
            session_allowed: Mutex::new(HashSet::new()),
        }
    }

    pub fn mode(&self) -> ApprovalMode {
        self.mode
    }

    /// 决策矩阵：`Safe` 恒放行；`Elevated` 在预授权下放行、非交互下拒绝；`Dangerous` 只有
    /// 交互询问与 `--unsafe` 预授权才放行。
    pub fn decide(&self, risk: RiskLevel, kind_key: &str) -> PolicyDecision {
        match risk {
            RiskLevel::Safe => PolicyDecision::Allow,
            RiskLevel::Elevated => match self.mode {
                ApprovalMode::Interactive => {
                    if self.is_session_allowed(kind_key) {
                        PolicyDecision::Allow
                    } else {
                        PolicyDecision::Ask
                    }
                }
                ApprovalMode::Preauthorized { .. } => PolicyDecision::Allow,
                ApprovalMode::Unattended => PolicyDecision::Deny(DENY_UNATTENDED.to_string()),
            },
            RiskLevel::Dangerous => match self.mode {
                ApprovalMode::Interactive => PolicyDecision::Ask,
                ApprovalMode::Preauthorized { unsafe_ops: true } => PolicyDecision::Allow,
                ApprovalMode::Preauthorized { unsafe_ops: false } => {
                    PolicyDecision::Deny(DENY_UNSAFE_REQUIRED.to_string())
                }
                ApprovalMode::Unattended => PolicyDecision::Deny(DENY_UNATTENDED.to_string()),
            },
        }
    }

    /// 会话级授权：仅 Interactive 下把该 `kind_key` 的 Elevated 决策改为 `Allow`。
    pub fn remember_session(&self, kind_key: &str) {
        if self.mode != ApprovalMode::Interactive {
            return;
        }
        self.allowed_kinds().insert(kind_key.to_string());
    }

    fn is_session_allowed(&self, kind_key: &str) -> bool {
        self.allowed_kinds().contains(kind_key)
    }

    /// 会话内已授权集合；锁中毒只可能来自其它线程 panic，这里沿用已有集合并继续，
    /// 避免决策路径本身 panic。
    fn allowed_kinds(&self) -> MutexGuard<'_, HashSet<String>> {
        self.session_allowed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use ignis_runtime::RiskLevel;

    use super::*;

    #[test]
    fn policy_matrix() {
        use ApprovalMode::{Interactive, Preauthorized, Unattended};

        let cases = [
            (Interactive, RiskLevel::Safe, PolicyDecision::Allow),
            (Interactive, RiskLevel::Elevated, PolicyDecision::Ask),
            (Interactive, RiskLevel::Dangerous, PolicyDecision::Ask),
            (
                Preauthorized { unsafe_ops: false },
                RiskLevel::Safe,
                PolicyDecision::Allow,
            ),
            (
                Preauthorized { unsafe_ops: false },
                RiskLevel::Elevated,
                PolicyDecision::Allow,
            ),
            (
                Preauthorized { unsafe_ops: false },
                RiskLevel::Dangerous,
                PolicyDecision::Deny("危险操作需显式 --unsafe 授权".into()),
            ),
            (
                Preauthorized { unsafe_ops: true },
                RiskLevel::Safe,
                PolicyDecision::Allow,
            ),
            (
                Preauthorized { unsafe_ops: true },
                RiskLevel::Elevated,
                PolicyDecision::Allow,
            ),
            (
                Preauthorized { unsafe_ops: true },
                RiskLevel::Dangerous,
                PolicyDecision::Allow,
            ),
            (Unattended, RiskLevel::Safe, PolicyDecision::Allow),
            (
                Unattended,
                RiskLevel::Elevated,
                PolicyDecision::Deny("非交互模式未授权".into()),
            ),
            (
                Unattended,
                RiskLevel::Dangerous,
                PolicyDecision::Deny("非交互模式未授权".into()),
            ),
        ];

        for (mode, risk, want) in cases {
            let policy = Policy::new(mode);
            assert_eq!(policy.mode(), mode);
            assert_eq!(policy.decide(risk, "fs_write"), want, "{mode:?}/{risk:?}");
        }
    }

    #[test]
    fn session_allow_only_relaxes_elevated_under_interactive() {
        let policy = Policy::new(ApprovalMode::Interactive);
        assert_eq!(
            policy.decide(RiskLevel::Elevated, "fs_write"),
            PolicyDecision::Ask
        );

        policy.remember_session("fs_write");
        assert_eq!(
            policy.decide(RiskLevel::Elevated, "fs_write"),
            PolicyDecision::Allow
        );
        // 会话规则按 kind_key 粒度生效
        assert_eq!(
            policy.decide(RiskLevel::Elevated, "shell_run"),
            PolicyDecision::Ask
        );
        // 会话规则不覆盖危险等级
        assert_eq!(
            policy.decide(RiskLevel::Dangerous, "fs_write"),
            PolicyDecision::Ask
        );

        // 非交互模式下会话规则不改变任何决策
        for mode in [
            ApprovalMode::Preauthorized { unsafe_ops: false },
            ApprovalMode::Unattended,
        ] {
            let policy = Policy::new(mode);
            policy.remember_session("fs_write");
            assert_eq!(
                policy.decide(RiskLevel::Elevated, "fs_write"),
                Policy::new(mode).decide(RiskLevel::Elevated, "fs_write"),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn policy_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Policy>();
    }
}
