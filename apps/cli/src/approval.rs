//! 审批 UI：模式判定、提示词与选择解析（纯函数）以及两个审批处理器。
//!
//! 交互路径拆成「写提示词 + 解析输入」的纯函数 [`decide_from_lines`]，[`InteractiveApproval`]
//! 只负责把真实 stdin/stdout 接进来，逻辑因此可单测。

use std::io::Write;

use async_trait::async_trait;
use ignis_core::security::ApprovalMode;
use ignis_runtime::{ApprovalDecision, ApprovalHandler, ApprovalRequest, RiskLevel};

/// 连续无效输入的容忍上限，超过即 fail-safe 拒绝。
const MAX_UNKNOWN_INPUTS: usize = 10;

/// 提示词下可选的四个动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// `a`：仅本次允许。
    ApproveOnce,
    /// `s`：本会话内允许同类操作。
    Always,
    /// `r`：拒绝。
    Reject,
    /// `d`：先看参数明细，再问一轮。
    Details,
}

/// 由 `--unsafe` / `--yes` 与 stdin 是否为 tty 决定审批模式。
///
/// `--unsafe` 优先于 `--yes`：显式要求危险操作放行时不应被更弱的预授权覆盖。
pub fn resolve_mode(yes: bool, unsafe_ops: bool, stdin_is_tty: bool) -> ApprovalMode {
    if unsafe_ops {
        ApprovalMode::Preauthorized { unsafe_ops: true }
    } else if yes {
        ApprovalMode::Preauthorized { unsafe_ops: false }
    } else if stdin_is_tty {
        ApprovalMode::Interactive
    } else {
        ApprovalMode::Unattended
    }
}

/// 审批提示词（格式锁定，以 `choice> ` 结尾、不带换行）。
///
/// `risk` 用 `RiskLevel` 的 serde lowercase 名字：`safe` / `elevated` / `dangerous`。
pub fn format_prompt(req: &ApprovalRequest) -> String {
    let risk = match req.risk {
        RiskLevel::Safe => "safe",
        RiskLevel::Elevated => "elevated",
        RiskLevel::Dangerous => "dangerous",
    };

    format!(
        "Approval required\n  Action: {}\n  Risk: {risk}\n\
         [a]pprove  [s]ession-allow  [r]eject  [d]etails\nchoice> ",
        req.summary
    )
}

/// 解析一行输入：大小写与首尾空白都容忍，无法识别返回 `None`。
pub fn parse_choice(line: &str) -> Option<Choice> {
    match line.trim().to_ascii_lowercase().as_str() {
        "a" => Some(Choice::ApproveOnce),
        "s" => Some(Choice::Always),
        "r" => Some(Choice::Reject),
        "d" => Some(Choice::Details),
        _ => None,
    }
}

/// 用脚本化的输入行跑审批循环（纯函数，测试与 [`InteractiveApproval`] 共用）。
///
/// `d` 打印参数的美化 JSON 后重问一轮；其余无效输入重问。输入耗尽（EOF）或连续
/// [`MAX_UNKNOWN_INPUTS`] 次无效输入一律返回 [`ApprovalDecision::Reject`]（fail-safe）。
pub fn decide_from_lines(
    req: &ApprovalRequest,
    mut lines: impl Iterator<Item = String>,
    out: &mut impl Write,
) -> ApprovalDecision {
    let mut unknown = 0;

    loop {
        let _ = write!(out, "{}", format_prompt(req));
        let _ = out.flush();

        let Some(line) = lines.next() else {
            return ApprovalDecision::Reject;
        };

        match parse_choice(&line) {
            Some(Choice::ApproveOnce) => return ApprovalDecision::AllowOnce,
            Some(Choice::Always) => return ApprovalDecision::AllowSession,
            Some(Choice::Reject) => return ApprovalDecision::Reject,
            Some(Choice::Details) => {
                let args = serde_json::to_string_pretty(&req.args)
                    .unwrap_or_else(|_| req.args.to_string());
                let _ = writeln!(out, "{args}");
            }
            None => {
                unknown += 1;
                if unknown >= MAX_UNKNOWN_INPUTS {
                    return ApprovalDecision::Reject;
                }
            }
        }
    }
}

/// 交互式审批：在 `spawn_blocking` 里同步读写终端，避免阻塞 async 运行时。
pub struct InteractiveApproval;

#[async_trait]
impl ApprovalHandler for InteractiveApproval {
    async fn decide(&self, req: ApprovalRequest) -> ApprovalDecision {
        tokio::task::spawn_blocking(move || {
            let stdin = std::io::stdin();
            let mut out = std::io::stdout();
            // 读失败与 EOF 都让迭代器结束 → decide_from_lines 兜底 Reject；非法 UTF-8 字节
            // 按替换字符读出（终端边界），不再掐断后续问答。
            let lines = crate::input::lossy_lines(stdin.lock());

            decide_from_lines(&req, lines, &mut out)
        })
        .await
        // 阻塞任务 panic 或被取消时同样 fail-safe 拒绝。
        .unwrap_or(ApprovalDecision::Reject)
    }
}

/// 仅按策略执行的占位处理器：策略已经给出 Allow/Deny 时不会被问到，一旦被问到一律拒绝。
pub struct PolicyOnlyApproval;

#[async_trait]
impl ApprovalHandler for PolicyOnlyApproval {
    async fn decide(&self, _req: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Reject
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ignis_core::security::ApprovalMode;
    use ignis_runtime::{ApprovalDecision, ApprovalHandler, ApprovalRequest, RiskLevel};
    use serde_json::json;

    use super::*;

    fn request(risk: RiskLevel) -> ApprovalRequest {
        ApprovalRequest {
            tool: "fs_write".to_string(),
            summary: "Writing a.txt".to_string(),
            args: json!({ "path": "a.txt", "recursive": true }),
            risk,
        }
    }

    #[test]
    fn parse_choice_maps_keys() {
        assert_eq!(parse_choice("a"), Some(Choice::ApproveOnce));
        assert_eq!(parse_choice("S"), Some(Choice::Always));
        assert_eq!(parse_choice("r"), Some(Choice::Reject));
        assert_eq!(parse_choice("D"), Some(Choice::Details));
        assert_eq!(parse_choice("x"), None);
        // 大小写与首尾空白都归一（stdin 的行带换行符）
        assert_eq!(parse_choice("  A\n"), Some(Choice::ApproveOnce));
        assert_eq!(parse_choice(""), None);
        assert_eq!(parse_choice("approve"), None);
    }

    #[test]
    fn format_prompt_locks_chrome_and_risk_names() {
        let want = concat!(
            "Approval required\n",
            "  Action: Writing a.txt\n",
            "  Risk: elevated\n",
            "[a]pprove  [s]ession-allow  [r]eject  [d]etails\n",
            "choice> ",
        );
        assert_eq!(format_prompt(&request(RiskLevel::Elevated)), want);

        // risk 用 serde 的 lowercase 名字
        for (risk, name) in [
            (RiskLevel::Safe, "safe"),
            (RiskLevel::Elevated, "elevated"),
            (RiskLevel::Dangerous, "dangerous"),
        ] {
            let prompt = format_prompt(&request(risk));
            assert!(prompt.contains(&format!("\n  Risk: {name}\n")), "{prompt}");
        }
    }

    #[test]
    fn decide_from_lines_handles_details_then_approve() {
        let mut out = Vec::new();
        let lines = ["d", "a"].into_iter().map(String::from);

        let decision = decide_from_lines(&request(RiskLevel::Elevated), lines, &mut out);

        let printed = String::from_utf8(out).expect("UTF-8 提示输出");
        assert_eq!(decision, ApprovalDecision::AllowOnce);
        assert!(printed.contains("Approval required"), "{printed}");
        assert!(printed.contains("  Action: Writing a.txt"), "{printed}");
        // 参数明细是美化 JSON（serde_json 默认按键排序）
        assert!(printed.contains("{\n  \"path\""), "{printed}");
        assert!(printed.contains("\"recursive\": true"), "{printed}");
        // `d` 之后重新问一轮：提示词出现两次
        assert_eq!(printed.matches("choice> ").count(), 2, "{printed}");
    }

    #[test]
    fn decide_from_lines_maps_remaining_choices() {
        let cases = [
            ("s", ApprovalDecision::AllowSession),
            ("R", ApprovalDecision::Reject),
            // 无效输入会重问，直到给出有效选择
            ("?", ApprovalDecision::Reject),
        ];

        for (line, want) in cases {
            let mut out = Vec::new();
            let lines = [line, "r"].into_iter().map(String::from);
            let decision = decide_from_lines(&request(RiskLevel::Safe), lines, &mut out);
            assert_eq!(decision, want, "{line}");
        }
    }

    #[test]
    fn decide_from_lines_rejects_on_eof_and_after_ten_unknown_inputs() {
        // EOF（输入耗尽）fail-safe 拒绝
        let mut out = Vec::new();
        assert_eq!(
            decide_from_lines(&request(RiskLevel::Safe), std::iter::empty(), &mut out),
            ApprovalDecision::Reject
        );

        // 连续 10 次无效输入后兜底拒绝，不再无限重问
        let mut out = Vec::new();
        let lines = std::iter::repeat_n("?".to_string(), 10);
        assert_eq!(
            decide_from_lines(&request(RiskLevel::Safe), lines, &mut out),
            ApprovalDecision::Reject
        );
        let printed = String::from_utf8(out).expect("UTF-8 提示输出");
        assert_eq!(printed.matches("choice> ").count(), 10, "{printed}");
    }

    #[test]
    fn resolve_mode_precedence() {
        use ApprovalMode::{Interactive, Preauthorized, Unattended};

        // (--yes, --unsafe, stdin 是 tty) → 模式：--unsafe 优先于 --yes，其次 tty
        let cases = [
            (true, true, true, Preauthorized { unsafe_ops: true }),
            (true, true, false, Preauthorized { unsafe_ops: true }),
            (false, true, true, Preauthorized { unsafe_ops: true }),
            (false, true, false, Preauthorized { unsafe_ops: true }),
            (true, false, true, Preauthorized { unsafe_ops: false }),
            (true, false, false, Preauthorized { unsafe_ops: false }),
            (false, false, true, Interactive),
            (false, false, false, Unattended),
        ];

        for (yes, unsafe_ops, stdin_is_tty, want) in cases {
            assert_eq!(
                resolve_mode(yes, unsafe_ops, stdin_is_tty),
                want,
                "yes={yes} unsafe={unsafe_ops} tty={stdin_is_tty}"
            );
        }
    }

    #[tokio::test]
    async fn policy_only_approval_always_rejects() {
        // Task 14 以 `Arc<dyn ApprovalHandler>` 注入，这里顺带钉住两个处理器都能直接强制转换
        let handler: Arc<dyn ApprovalHandler> = Arc::new(PolicyOnlyApproval);
        assert_eq!(
            handler.decide(request(RiskLevel::Safe)).await,
            ApprovalDecision::Reject
        );

        // 交互式处理器的 stdin 循环由 decide_from_lines 覆盖，这里只钉 trait 对象可用
        let _interactive: Arc<dyn ApprovalHandler> = Arc::new(InteractiveApproval);
    }
}
