//! `ignis doctor`：环境自检，逐行 `✓/!/✗ {name}: {detail}`。
//!
//! 检查项与顺序即输出契约：core 六项（home → config → api_key → base_url → workspace → db）
//! 之后是 codex / claude 两个外部内核探测行，配置里的内核名非法时再补一行 `kernel`。
//! 退出码只看 `Fail`：任一 ✗ → 1；只有 `!` 警告（未装外部 CLI、选了外部内核时缺 Ignis 密钥）
//! 照常 0——自检不该因为一个没用上的内核没装就变红。

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::Result;
use ignis_core::config::{ignis_home, load_config};
use ignis_core::doctor::{CheckLevel, CheckResult, run_checks};

use crate::cli::Cli;
use crate::commands::kernel::{KERNELS, NATIVE_KERNEL, find_program, resolve_kernel};

/// 执行全部检查并打印结论。
pub fn run(cli: &Cli) -> Result<ExitCode> {
    let home = ignis_home();
    // 配置解析失败是自检要报告的结论之一：不让它变成 `error:` 直接退出
    let loaded = load_config(&home);
    // 生效内核与 `run`/REPL 走同一条解析链（`--kernel` > 配置 default_kernel > native）：
    // 它决定缺 CLI、缺 Ignis 密钥是 ✗ 还是 `!`
    let configured = loaded
        .as_ref()
        .map(|config| config.default_kernel.as_str())
        .unwrap_or(NATIVE_KERNEL);
    let (selected, kernel_error) = resolve_selection(cli.kernel.as_deref(), configured);

    let mut results = run_checks(&home, &loaded, selected == NATIVE_KERNEL);
    results.extend(kernel_error);
    results.extend(kernel_checks(selected, std::env::var_os("PATH").as_deref()));

    let mut out = std::io::stdout();
    report(&mut out, &results)
}

/// 生效内核的解析（与 `run`/REPL 同一条链）：`--kernel` > 配置 `default_kernel` > `native`。
///
/// 配置里的内核名非法（手改或拼错）是自检要报告的结论之一：不静默回落，也不中断其余检查——
/// 返回兜底的 native 与一行 `kernel` ✗，其余「当前内核」口径都按 native 报。
fn resolve_selection(flag: Option<&str>, configured: &str) -> (&'static str, Option<CheckResult>) {
    match resolve_kernel(flag, configured) {
        Ok(kernel) => (kernel, None),
        Err(error) => (
            NATIVE_KERNEL,
            Some(CheckResult {
                name: "kernel",
                level: CheckLevel::Fail,
                detail: error,
            }),
        ),
    }
}

/// 外部内核的探测行，顺序固定为 [`KERNELS`] 里 native 之后的名字（codex → claude）。
///
/// 探测复用 [`find_program`] 这份 PATH 注入缝：用例因此不必改进程环境（测试可并行）。
fn kernel_checks(selected: &str, path: Option<&OsStr>) -> Vec<CheckResult> {
    KERNELS
        .iter()
        .copied()
        .filter(|kernel| *kernel != NATIVE_KERNEL)
        .map(|name| kernel_check(name, selected, path))
        .collect()
}

/// 单个外部内核：`{name} --version` 跑通 → ✓ + 版本短串。
///
/// 没装或跑不通时，选中该内核 → ✗（`ignis run --kernel {name}` 会直接失败），未选中 → `!`
/// 警告（装了才用得上，不该拦住整套自检）。
fn kernel_check(name: &'static str, selected: &str, path: Option<&OsStr>) -> CheckResult {
    let level = if selected == name {
        CheckLevel::Fail
    } else {
        CheckLevel::Warn
    };
    let Some(binary) = find_program(name, path) else {
        return CheckResult {
            name,
            level,
            detail: format!(
                "未安装 {name} CLI（当前内核 {selected}）：请先安装 {name} 并确保它在 PATH 中"
            ),
        };
    };
    match probe_version(&binary) {
        Ok(version) => CheckResult {
            name,
            level: CheckLevel::Ok,
            detail: format!("已安装：{version}"),
        },
        Err(reason) => CheckResult {
            name,
            level,
            detail: format!("{name} CLI 不可用（当前内核 {selected}）：{reason}"),
        },
    }
}

/// 跑 `{binary} --version`，取首行非空输出作为版本短串。
fn probe_version(binary: &Path) -> Result<String, String> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "信号终止".to_string());
        return Err(format!("--version 退出码 {code}"));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "--version 没有输出".to_string())
}

/// 逐行打印 `✓/!/✗ {name}: {detail}`；返回进程退出码：任一 ✗ → 1；只有 `!` 警告照常 0。
pub fn report(out: &mut impl Write, results: &[CheckResult]) -> Result<ExitCode> {
    let mut failed = false;
    for result in results {
        let mark = match result.level {
            CheckLevel::Ok => '✓',
            CheckLevel::Warn => '!',
            CheckLevel::Fail => '✗',
        };
        writeln!(out, "{mark} {}: {}", result.name, result.detail)?;
        failed |= result.level == CheckLevel::Fail;
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::commands::kernel::fixtures::{TempDir, set_executable};

    fn result(name: &'static str, level: CheckLevel, detail: &str) -> CheckResult {
        CheckResult {
            name,
            level,
            detail: detail.to_string(),
        }
    }

    /// 三个级别的记号与退出码：✓ 通过、`!` 只警告（照常 0）、✗ 才让 doctor 变红。
    #[test]
    fn report_marks_every_level_and_only_failures_change_the_exit_code() {
        let mut out = Vec::new();
        let code = report(
            &mut out,
            &[
                result("home", CheckLevel::Ok, "目录可用：/tmp/ignis"),
                result(
                    "codex",
                    CheckLevel::Warn,
                    "未安装 codex CLI（当前内核 native）：请先安装 codex 并确保它在 PATH 中",
                ),
            ],
        )
        .unwrap();
        assert_eq!(code, ExitCode::SUCCESS, "只有 ! 警告时要以 0 收场");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("✓ home: 目录可用：/tmp/ignis\n"), "{text}");
        assert!(text.contains("! codex: 未安装 codex CLI"), "{text}");
        assert!(!text.contains('✗'), "{text}");

        let mut out = Vec::new();
        let code = report(
            &mut out,
            &[
                result(
                    "codex",
                    CheckLevel::Fail,
                    "未安装 codex CLI（当前内核 codex）：请先安装 codex 并确保它在 PATH 中",
                ),
                result("claude", CheckLevel::Warn, "未安装 claude CLI"),
            ],
        )
        .unwrap();
        assert_eq!(code, ExitCode::FAILURE, "任一 ✗ 就要以非零退出码收场");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("✗ codex: 未安装 codex CLI"), "{text}");
        assert!(text.contains("! claude: 未安装 claude CLI"), "{text}");
    }

    /// 生效内核的解析链与非法配置名：`--kernel` 压过配置；配置里拼错的名字单列一行 ✗
    /// （其余内核口径按 native 兜底），而不是静默当成 native 或中断检查。
    #[test]
    fn resolve_selection_prefers_the_flag_and_reports_a_bad_configured_name() {
        assert_eq!(resolve_selection(Some("codex"), "claude"), ("codex", None));
        assert_eq!(resolve_selection(None, "claude"), ("claude", None));
        assert_eq!(resolve_selection(None, "native"), (NATIVE_KERNEL, None));

        let (selected, error) = resolve_selection(None, "gemini");
        assert_eq!(
            selected, NATIVE_KERNEL,
            "解析不出来时按 native 口径继续自检"
        );
        let line = error.expect("非法配置名要单独报一行");
        assert_eq!(line.name, "kernel");
        assert_eq!(line.level, CheckLevel::Fail);
        assert!(line.detail.contains("未知内核 'gemini'"), "{line:?}");
    }

    /// 没装外部内核：未选中 → `!` 警告（含安装提示），选中 → ✗。
    #[test]
    fn kernel_checks_warn_for_absent_clis_and_fail_when_that_kernel_is_selected() {
        let dir = TempDir::new("doctor-absent");
        // 空目录当 PATH：两个外部 CLI 都没装
        let path = Some(dir.path().as_os_str());

        let results = kernel_checks("native", path);
        assert_eq!(
            results.iter().map(|result| result.name).collect::<Vec<_>>(),
            ["codex", "claude"]
        );
        for result in &results {
            assert_eq!(result.level, CheckLevel::Warn, "{result:?}");
            assert!(result.detail.contains("未安装"), "{result:?}");
            assert!(
                result
                    .detail
                    .contains(&format!("请先安装 {} 并确保它在 PATH 中", result.name)),
                "{result:?}"
            );
        }

        // 选中 codex：它变 ✗，claude 仍只是 `!`（缺 CLI 不该让整套自检失败）
        let results = kernel_checks("codex", path);
        assert_eq!(results[0].level, CheckLevel::Fail, "{:?}", results[0]);
        assert_eq!(results[1].level, CheckLevel::Warn, "{:?}", results[1]);
    }

    /// 装上的内核给 ✓ + 版本短串；找到二进制但 `--version` 跑不通按「不可用」如实报
    /// （选中时 ✗，未选中时 `!`）。
    #[test]
    fn kernel_checks_report_the_installed_version_and_flag_a_broken_cli() {
        let dir = TempDir::new("doctor-installed");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            bin.join("codex"),
            "#!/bin/sh\nprintf '%s\\n' 'codex-cli 9.9.9'\n",
        )
        .unwrap();
        set_executable(&bin.join("codex"));
        fs::write(bin.join("claude"), "#!/bin/sh\nexit 3\n").unwrap();
        set_executable(&bin.join("claude"));

        let results = kernel_checks("native", Some(bin.as_os_str()));
        assert_eq!(results[0].level, CheckLevel::Ok, "{:?}", results[0]);
        assert!(
            results[0].detail.contains("已安装：codex-cli 9.9.9"),
            "{:?}",
            results[0]
        );
        assert_eq!(results[1].level, CheckLevel::Warn, "{:?}", results[1]);
        assert!(
            results[1].detail.contains("claude CLI 不可用"),
            "{:?}",
            results[1]
        );
        assert!(results[1].detail.contains("退出码 3"), "{:?}", results[1]);

        // 选中坏掉的那个：升级为 ✗
        let results = kernel_checks("claude", Some(bin.as_os_str()));
        assert_eq!(results[1].level, CheckLevel::Fail, "{:?}", results[1]);
    }
}
