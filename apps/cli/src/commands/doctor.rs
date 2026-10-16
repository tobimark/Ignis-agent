//! `ignis doctor`：环境自检，逐行 `✓/!/✗ {name}: {detail}`。
//!
//! 检查项与顺序即输出契约：core 六项（home → config → api_key → base_url → workspace → db）之后是
//! 配置里的内核名非法时的 `kernel` 行（紧跟六项、其余检查按 native 兜底），再是两个外部内核探测行
//! （codex → claude）。退出码只看 `Fail`：任一 ✗ → 1；`!` 警告照常 0，出现在四处——没装/跑不通的
//! 外部 CLI（未选中它时）、选了外部内核时缺 Ignis 密钥、配置里的内核名非法且缺钥匙（这一条把
//! api_key 降级为 `!`，退出码已由 `✗ kernel` 顶上）、外部内核下 `base_url` 不可达（该内核不访问
//! 这个端点）——自检不该因为一个没用上的内核没装或没配就变红。

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use ignis_core::config::{Config, ConfigError, ignis_home, load_config};
use ignis_core::doctor::{CHECK_API_KEY, CheckLevel, CheckResult, run_checks};

use crate::cli::Cli;
use crate::commands::kernel::{KERNELS, NATIVE_KERNEL, find_program, resolve_kernel};

/// 执行全部检查并打印结论。
pub fn run(cli: &Cli) -> Result<ExitCode> {
    let home = ignis_home();
    // 配置解析失败是自检要报告的结论之一：不让它变成 `error:` 直接退出
    let loaded = load_config(&home);
    let results = collect(
        &home,
        &loaded,
        cli.kernel.as_deref(),
        std::env::var_os("PATH").as_deref(),
    );

    let mut out = std::io::stdout();
    report(&mut out, &results)
}

/// 组合全部检查项（打印与退出码交给 [`report`]）：core 六项 → 内核名非法时的 `kernel` 行 →
/// codex / claude 两个外部内核探测行。
///
/// 生效内核与 `run`/REPL 走同一条解析链（`--kernel` > 配置 default_kernel > native）：它决定缺
/// CLI、缺 Ignis 密钥、`base_url` 不可达是 ✗ 还是 `!`。`home` 与 `path` 可注入，用例因此不必碰
/// 真实 `~/.ignis` 与进程环境（测试可并行）。
fn collect(
    home: &Path,
    loaded: &Result<Config, ConfigError>,
    kernel_flag: Option<&str>,
    path: Option<&OsStr>,
) -> Vec<CheckResult> {
    // 配置里的内核名只在解析不出来时兜底 native：其余检查先按这个口径跑
    let configured = loaded
        .as_ref()
        .map(|config| config.default_kernel.as_str())
        .unwrap_or(NATIVE_KERNEL);
    let (selected, kernel_error) = resolve_selection(kernel_flag, configured);

    let mut results = run_checks(home, loaded, selected);
    if kernel_error.is_some() {
        soften_api_key_for_unresolved_kernel(&mut results);
    }
    results.extend(kernel_error);
    results.extend(kernel_checks(selected, path));
    results
}

/// 配置里的内核名非法（`✗ kernel` 已让退出码非零）：其余检查按 native 兜底，此时缺 key 的
/// `✗ api_key … native 内核需要` 会误导只是拼错名字、其实只打算用 codex/claude 的用户——
/// 降级为 `!`，并在文案里点明这一行是兜底口径（顺序与退出码都不变）。
///
/// 行名用 core 的 [`CHECK_API_KEY`]：字面量同源，core 侧改名时这里不会悄悄匹配不上。
fn soften_api_key_for_unresolved_kernel(results: &mut [CheckResult]) {
    for result in results.iter_mut() {
        if result.name == CHECK_API_KEY && result.level == CheckLevel::Fail {
            result.level = CheckLevel::Warn;
            result.detail = format!(
                "{}（配置的内核名非法，其余检查按 native 兜底）",
                result.detail
            );
        }
    }
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

/// `--version` 探测的等待上限：卡住的 CLI 只该让这一行变 ✗/`!`，不该把 `ignis doctor` 拖住。
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// 上限内的轮询间隔：探测是一条短命令，不值得为它引入通知机制。
const VERSION_PROBE_POLL: Duration = Duration::from_millis(50);

/// 跑 `{binary} --version`，取首个非空输出行作为版本短串。
///
/// stdout 优先，它空时退到 stderr：有的 CLI 把版本打到 stderr，不该因此被判「不可用」；接受的
/// 假 ✓ 模式是 stderr 上的启动噪声（如 `warning: ...`）会被读成版本。stdin 接空设备、整轮上限
/// [`VERSION_PROBE_TIMEOUT`]，超时就杀掉子进程并按「不可用」如实报（与退出码非零/没有输出同形）。
fn probe_version(binary: &Path) -> Result<String, String> {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "--version 超过 {}s 未返回（已终止）",
                        VERSION_PROBE_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(VERSION_PROBE_POLL);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    // 子进程已经退出：`wait_with_output` 只是把管道里读全
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "信号终止".to_string());
        return Err(format!("--version 退出码 {code}"));
    }
    first_line(&output.stdout)
        .or_else(|| first_line(&output.stderr))
        .ok_or_else(|| "--version 没有输出".to_string())
}

/// 输出字节里的首个非空行（lossy 解码，与内核适配器同一宽容度）。
fn first_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
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

    /// 组合顺序与退出码接线：core 六项 → （配置内核名非法时）`kernel` → codex → claude；
    /// 退出码只看 ✗（`!` 照常 0，`✗ kernel` 也照样变红）。
    #[test]
    fn collect_composes_checks_in_the_documented_order_and_wires_the_exit_code() {
        let dir = TempDir::new("collect");
        let workspace = dir.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        // 本地 listener 顶替真实端点：TCP 握手能完成即可（与 core 的用例同一手法）
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let config = Config {
            workspace_root: workspace,
            api_key: Some("test-key".into()),
            model: ignis_runtime::ModelConfig {
                base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
                // 环境变量名取一个不存在的（不走默认的 `OPENAI_API_KEY`），断言与运行者环境无关
                api_key_env: "IGNIS_T7_ABSENT_KEY".into(),
                ..ignis_runtime::ModelConfig::default()
            },
            ..Config::default()
        };
        // 空目录当 PATH：两个外部内核都探不到（未选中 → `!`，不改变退出码）
        let path = Some(dir.path().as_os_str());

        let results = collect(dir.path(), &Ok(config.clone()), None, path);
        assert_eq!(
            results
                .iter()
                .map(|result| (result.name, result.level))
                .collect::<Vec<_>>(),
            [
                ("home", CheckLevel::Ok),
                ("config", CheckLevel::Ok),
                ("api_key", CheckLevel::Ok),
                ("base_url", CheckLevel::Ok),
                ("workspace", CheckLevel::Ok),
                ("db", CheckLevel::Ok),
                ("codex", CheckLevel::Warn),
                ("claude", CheckLevel::Warn),
            ],
            "core 六项之后是 codex → claude"
        );
        let mut out = Vec::new();
        assert_eq!(
            report(&mut out, &results).unwrap(),
            ExitCode::SUCCESS,
            "只有 ! 警告时退出码照常 0"
        );

        // 配置里的内核名非法：`kernel` 行紧跟 core 六项（不是排在探测行之后）；缺 key 的
        // api_key 降级为 `!`（只是拼错名字、本来要用外部内核的用户不该被那条 ✗ 带偏）；
        // 退出码仍由 `✗ kernel` 顶上
        let bad = Config {
            default_kernel: "gemini".into(),
            api_key: None,
            ..config.clone()
        };
        let results = collect(dir.path(), &Ok(bad), None, path);
        assert_eq!(
            results.iter().map(|result| result.name).collect::<Vec<_>>(),
            [
                "home",
                "config",
                "api_key",
                "base_url",
                "workspace",
                "db",
                "kernel",
                "codex",
                "claude"
            ]
        );
        let kernel = &results[6];
        assert_eq!(kernel.level, CheckLevel::Fail, "{kernel:?}");
        assert!(kernel.detail.contains("未知内核 'gemini'"), "{kernel:?}");
        let api_key = results
            .iter()
            .find(|result| result.name == CHECK_API_KEY)
            .expect("api_key 行必在");
        assert_eq!(
            api_key.level,
            CheckLevel::Warn,
            "拼错内核名时缺 key 的 ✗ 会误导：{api_key:?}"
        );
        assert!(
            api_key.detail.contains("配置的内核名非法"),
            "降级后的文案要点明兜底口径：{api_key:?}"
        );

        let mut out = Vec::new();
        assert_eq!(
            report(&mut out, &results).unwrap(),
            ExitCode::FAILURE,
            "✗ kernel 仍让 doctor 变红"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("✗ kernel:"), "{text}");
        assert!(text.contains("! api_key:"), "{text}");

        // 对照：配置正常但缺 key（选中 native）时同一条 api_key 结论仍是 ✗——只有内核名非法
        // 这一条路径才降级
        let keyless = Config {
            api_key: None,
            ..config.clone()
        };
        let results = collect(dir.path(), &Ok(keyless), None, path);
        let api_key = results
            .iter()
            .find(|result| result.name == CHECK_API_KEY)
            .expect("api_key 行必在");
        assert_eq!(api_key.level, CheckLevel::Fail, "{api_key:?}");
        assert!(!api_key.detail.contains("配置的内核名非法"), "{api_key:?}");

        drop(listener);
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

    /// `--version` 的读数：stdout 优先，它空时退到 stderr（版本打到 stderr 的 CLI 不该被判
    /// 「不可用」）；两边都没有非空输出时如实报「没有输出」。
    #[test]
    fn probe_version_prefers_stdout_and_falls_back_to_stderr() {
        let dir = TempDir::new("probe-version");
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            fs::write(&path, body).unwrap();
            set_executable(&path);
            path
        };

        // 版本只打到 stderr：仍读出同一个版本短串（修复前这一条会得到「--version 没有输出」）
        let stderr_only = script(
            "stderr-only",
            "#!/bin/sh\nprintf '%s\\n' 'codex 9.9.9' >&2\n",
        );
        assert_eq!(probe_version(&stderr_only).unwrap(), "codex 9.9.9");

        // stdout 有输出时仍是 stdout 说了算
        let both = script(
            "both",
            "#!/bin/sh\nprintf '%s\\n' 'stdout-version'\nprintf '%s\\n' 'stderr-noise' >&2\n",
        );
        assert_eq!(probe_version(&both).unwrap(), "stdout-version");

        // 两边都空：如实报「没有输出」，不拿空串冒充版本
        let quiet = script("quiet", "#!/bin/sh\nexit 0\n");
        assert_eq!(probe_version(&quiet).unwrap_err(), "--version 没有输出");

        // 首个非空行：前导空行跳过
        let padded = script(
            "padded",
            "#!/bin/sh\nprintf '\\n\\n%s\\n' 'codex-cli 1.2.3' >&2\n",
        );
        assert_eq!(probe_version(&padded).unwrap(), "codex-cli 1.2.3");
    }

    /// `base_url` 不可达按内核分级：选中外部内核（Ignis 不访问该端点）只给 `!`、doctor 退出 0；
    /// 同一份配置选中 native 仍是 ✗。
    #[test]
    fn unreachable_base_url_only_warns_for_external_kernels() {
        let dir = TempDir::new("doctor-base-url");
        let workspace = dir.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        // PATH 夹具里装上 codex：选中它时探测行 ✓，只有 base_url 一条 `!`
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            bin.join("codex"),
            "#!/bin/sh\nprintf '%s\\n' 'codex-cli 9.9.9'\n",
        )
        .unwrap();
        set_executable(&bin.join("codex"));
        let path = Some(bin.as_os_str());

        let config = Config {
            workspace_root: workspace,
            api_key: Some("test-key".into()),
            model: ignis_runtime::ModelConfig {
                // 端口 1 上不会有服务：连接会被立刻拒绝（不需要等超时）
                base_url: "127.0.0.1:1".into(),
                // 环境变量名取一个不存在的（不走默认的 `OPENAI_API_KEY`），断言与运行者环境无关
                api_key_env: "IGNIS_T8_ABSENT_KEY".into(),
                ..ignis_runtime::ModelConfig::default()
            },
            ..Config::default()
        };

        let results = collect(dir.path(), &Ok(config.clone()), Some("codex"), path);
        let base_url = &results[3];
        assert_eq!(base_url.name, "base_url");
        assert_eq!(base_url.level, CheckLevel::Warn, "{base_url:?}");
        assert!(base_url.detail.contains("127.0.0.1:1"), "{base_url:?}");
        assert!(
            base_url.detail.contains("仅 native 内核需要"),
            "{base_url:?}"
        );
        assert!(base_url.detail.contains("当前内核 codex"), "{base_url:?}");
        let mut out = Vec::new();
        assert_eq!(
            report(&mut out, &results).unwrap(),
            ExitCode::SUCCESS,
            "外部内核下端点不可达不该让 doctor 变红：{results:?}"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("! base_url:"), "{text}");
        assert!(!text.contains('✗'), "{text}");

        // 对照：同一份配置选中 native → `✗ base_url`，退出码 1
        let results = collect(dir.path(), &Ok(config), Some("native"), path);
        assert_eq!(results[3].level, CheckLevel::Fail, "{:?}", results[3]);
        let mut out = Vec::new();
        assert_eq!(report(&mut out, &results).unwrap(), ExitCode::FAILURE);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("✗ base_url:"), "{text}");
    }

    /// 卡住的外部 CLI 不会把 `ignis doctor` 拖住：`--version` 只等 [`VERSION_PROBE_TIMEOUT`]，
    /// 超时就杀掉子进程并按「不可用」如实报（与退出码非零/没有输出同形，级别照旧由是否选中决定）。
    #[test]
    fn probe_version_bounds_a_wedged_cli() {
        let dir = TempDir::new("probe-timeout");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        // `exec`：子进程被 kill 时不会留下一个孤儿 `sleep`
        fs::write(bin.join("codex"), "#!/bin/sh\nexec sleep 600\n").unwrap();
        set_executable(&bin.join("codex"));

        let started = Instant::now();
        let results = kernel_checks("native", Some(bin.as_os_str()));
        let elapsed = started.elapsed();

        assert_eq!(results[0].name, "codex");
        assert_eq!(results[0].level, CheckLevel::Warn, "{:?}", results[0]);
        assert!(
            results[0].detail.contains("codex CLI 不可用") && results[0].detail.contains("未返回"),
            "超时要按「CLI 不可用」如实报：{:?}",
            results[0]
        );
        assert!(
            elapsed < VERSION_PROBE_TIMEOUT + Duration::from_secs(1),
            "探测必须在有界时间内收场，实测 {elapsed:?}"
        );
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
