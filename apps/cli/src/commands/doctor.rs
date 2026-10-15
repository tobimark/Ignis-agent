//! `ignis doctor`：环境自检，逐行 `✓/✗ {name}: {detail}`，任一 ✗ → 退出码 1。

use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use ignis_core::config::{ignis_home, load_config};
use ignis_core::doctor::{CheckResult, run_checks};

/// 执行全部检查并打印结论。
pub fn run() -> Result<ExitCode> {
    let home = ignis_home();
    // 配置解析失败是自检要报告的结论之一：不让它变成 `error:` 直接退出
    let loaded = load_config(&home);
    let results = run_checks(&home, &loaded);

    let mut out = std::io::stdout();
    let all_ok = report(&mut out, &results)?;
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// 逐行打印 `✓/✗ {name}: {detail}`，返回是否全部通过（调用方据此定退出码）。
pub fn report(out: &mut impl Write, results: &[CheckResult]) -> Result<bool> {
    let mut all_ok = true;
    for result in results {
        let mark = if result.ok { '✓' } else { '✗' };
        writeln!(out, "{mark} {}: {}", result.name, result.detail)?;
        all_ok &= result.ok;
    }
    Ok(all_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(name: &'static str, ok: bool, detail: &str) -> CheckResult {
        CheckResult {
            name,
            ok,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn report_marks_every_check_and_fails_when_any_check_fails() {
        let mut out = Vec::new();
        let all_ok = report(
            &mut out,
            &[
                result("home", true, "目录可用：/tmp/ignis"),
                result("db", true, "数据库可用（迁移完成）：/tmp/ignis/ignis.db"),
            ],
        )
        .unwrap();
        assert!(all_ok);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "✓ home: 目录可用：/tmp/ignis\n\
             ✓ db: 数据库可用（迁移完成）：/tmp/ignis/ignis.db\n"
        );

        let mut out = Vec::new();
        let all_ok = report(
            &mut out,
            &[
                result("home", true, "目录可用：/tmp/ignis"),
                result(
                    "base_url",
                    false,
                    "无法连接 127.0.0.1:1：Connection refused",
                ),
            ],
        )
        .unwrap();
        assert!(!all_ok, "任一 ✗ 就要以非零退出码收场");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "✓ home: 目录可用：/tmp/ignis\n\
             ✗ base_url: 无法连接 127.0.0.1:1：Connection refused\n"
        );
    }
}
