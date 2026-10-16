//! 内核选择与发现：`--kernel`/配置的生效解析、按名构造执行内核，以及 PATH 上的 CLI 探测
//! （`mk_runtime` 的可用性判定与 `ignis doctor` 的外部内核探测共用同一份实现）。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignis_core::config::Config;
use ignis_runtime::Runtime;
use ignis_runtime::claude::ClaudeRuntime;
use ignis_runtime::codex::CodexRuntime;
use ignis_runtime::native::NativeRuntime;
use ignis_runtime::openai::OpenAiClient;

/// 三个执行内核：`--kernel` 的值域、`/kernel` 的校验与 [`mk_runtime`] 共用一份口径。
pub const KERNELS: [&str; 3] = ["native", "codex", "claude"];

/// 主路径内核：`--kernel` 与配置 `default_kernel` 都没给时回落的默认值（完整工具与审批，
/// 见设计文档 §6.3）。
pub const NATIVE_KERNEL: &str = KERNELS[0];

/// 未知内核的人话错误：可选值就是 `--kernel` 的 clap 值域（同一份 [`KERNELS`]），不另抄一遍。
fn unknown_kernel(kernel: &str) -> String {
    format!("未知内核 '{kernel}'：可选 {}", KERNELS.join(" / "))
}

/// 内核名白名单校验：`--kernel` 的值域（clap）是同一份口径，`/kernel` 用它给人话错误。
pub fn known_kernel(kernel: &str) -> Result<&'static str, String> {
    KERNELS
        .iter()
        .copied()
        .find(|known| *known == kernel)
        .ok_or_else(|| unknown_kernel(kernel))
}

/// 生效内核名的解析（spec §6.1「默认取配置」）：`--kernel` > 配置 `default_kernel` > `native`。
///
/// 两处来源都过同一份白名单 [`known_kernel`]：配置文件里的未知内核（手改或拼错）给人话错误，
/// 不静默回落到 native——用户以为切过去了、实际没有，比直接报错更难查。
pub fn resolve_kernel(flag: Option<&str>, configured: &str) -> Result<&'static str, String> {
    known_kernel(flag.unwrap_or(configured))
}

/// 非 native 内核的能力提示行（决策 #4：工具跑在内核自己的沙箱里，记忆写与技能全文加载不可用）；
/// native 没有降级可提示，返回 `None`。
///
/// 调用方传的都是过了 [`known_kernel`] 的名字，这里只区分 native 与两个外部内核。
pub fn kernel_note(kernel: &str) -> Option<String> {
    if kernel == NATIVE_KERNEL {
        return None;
    }
    Some(format!(
        "note: kernel={kernel} — tools run inside {kernel}'s sandbox; \
         remember/load_skill unavailable; usage may be missing"
    ))
}

/// native 缺少生效密钥时的人话错误：点名该设哪个环境变量。
pub(crate) fn missing_api_key(config: &Config) -> String {
    format!(
        "未配置 API key（设置 {} 或 config.json 的 api_key）",
        config.model.api_key_env
    )
}

/// 按内核名构造执行内核；构造失败（外部 CLI 没装、native 没凭据）以人话 `Err` 返回。
///
/// 名字不在值域内也走同一条人话错误（`--kernel` 由 clap 把关，`/kernel` 落到这里）。
pub fn mk_runtime(
    kernel: &str,
    config: &Config,
    workspace: &Path,
) -> Result<Arc<dyn Runtime>, String> {
    mk_runtime_with_path(
        kernel,
        config,
        workspace,
        std::env::var_os("PATH").as_deref(),
    )
}

/// 带 PATH 注入的变体：可用性判定只看传进来的这份 PATH，用例因此不必改进程环境（测试可并行）。
pub(crate) fn mk_runtime_with_path(
    kernel: &str,
    config: &Config,
    workspace: &Path,
    path: Option<&OsStr>,
) -> Result<Arc<dyn Runtime>, String> {
    let kernel = known_kernel(kernel)?;
    match kernel {
        NATIVE_KERNEL => {
            // 没凭据的 native 根本跑不动：`Session` 装配期与 `/kernel` 切换都已提前挡过，
            // 这里兜底报同一句人话——静默拿一把空 key 构造运行器只会在第一次请求时 401
            let api_key = config
                .api_key
                .clone()
                .ok_or_else(|| missing_api_key(config))?;
            let client = OpenAiClient::new(&config.model, api_key)
                .map_err(|error| error.to_string())?
                .with_max_retries(config.limits.max_retries);
            Ok(Arc::new(NativeRuntime::new(client)))
        }
        // 两个外部内核都用它们自己的默认模型：`config.model` 是给 OpenAI 兼容 relay 的模型名，
        // 塞给 codex/claude 只会让它们去请求一个自家 provider 不认识的模型（模型名各自的配置里管）
        "codex" => {
            ensure_installed("codex", path)?;
            Ok(Arc::new(CodexRuntime::new(workspace.to_path_buf(), None)))
        }
        "claude" => {
            ensure_installed("claude", path)?;
            Ok(Arc::new(ClaudeRuntime::new(workspace.to_path_buf(), None)))
        }
        // 值域与分支同源：将来加了新内核却忘了在这里接上时，报人话而不是静默落到别的实现
        other => Err(format!("内核 '{other}' 尚未接入")),
    }
}

/// 外部内核的可用性检查：PATH 上找不到这个 CLI 就是「没装」。
fn ensure_installed(program: &str, path: Option<&OsStr>) -> Result<(), String> {
    if find_program(program, path).is_some() {
        return Ok(());
    }
    Err(format!(
        "找不到 {program} CLI：请先安装 {program} 并确保它在 PATH 中"
    ))
}

/// PATH 里按目录顺序找可执行文件；`path` 为 `None`（环境变量缺失）时找不到任何东西。
///
/// `pub(crate)`：`ignis doctor` 的外部内核探测复用同一份查找逻辑。
pub(crate) fn find_program(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// 可执行文件判定：普通文件，且（Unix 下）至少有一位执行位。
fn is_executable(candidate: &Path) -> bool {
    let Ok(metadata) = candidate.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

/// 用例夹具：PATH 注入用的临时目录与可执行位设置（`commands/mod.rs`、`commands/doctor.rs`
/// 的内联用例共用）。
#[cfg(test)]
pub(crate) mod fixtures {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 单个测试独占的临时目录（PATH 夹具），析构时清理。
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-cli-kernel-{}-{label}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 给夹具脚本补上执行位（Unix 之外没有这个概念，测试里另按存在性判定）。
    pub(crate) fn set_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::fixtures::{TempDir, set_executable};
    use super::*;

    /// 内核白名单与能力提示文案逐字钉住（`--kernel` 的 clap 值域是同一份口径）。
    #[test]
    fn kernel_whitelist_and_capability_note_are_pinned() {
        // 值域与 `/kernel` 共用一份口径：三个名字逐字钉住，native 是默认值
        assert_eq!(KERNELS, ["native", "codex", "claude"]);
        assert_eq!(NATIVE_KERNEL, KERNELS[0]);

        for kernel in KERNELS {
            assert_eq!(known_kernel(kernel).unwrap(), kernel);
        }
        for bad in ["", "Native", "gemini", "codex "] {
            let error = known_kernel(bad).unwrap_err();
            assert!(error.contains(&format!("未知内核 '{bad}'")), "{error}");
            assert!(error.contains("native / codex / claude"), "{error}");
        }

        // native 没有降级可提示；两个外部内核同一模板，逐字
        assert_eq!(kernel_note("native"), None);
        assert_eq!(
            kernel_note("codex").as_deref(),
            Some(
                "note: kernel=codex — tools run inside codex's sandbox; \
                 remember/load_skill unavailable; usage may be missing"
            )
        );
        assert_eq!(
            kernel_note("claude").as_deref(),
            Some(
                "note: kernel=claude — tools run inside claude's sandbox; \
                 remember/load_skill unavailable; usage may be missing"
            )
        );
    }

    /// 生效内核的解析真值表（spec §6.1「默认取配置」）：`--kernel` > 配置 `default_kernel` >
    /// `native`；两处来源共用同一条白名单，配置里的未知名不静默回落。
    #[test]
    fn resolve_kernel_prefers_the_flag_then_the_config_then_native() {
        // 显式旗标压过配置（`config set default_kernel claude` 之后仍能用 --kernel 临时改）
        assert_eq!(resolve_kernel(Some("codex"), "claude").unwrap(), "codex");
        // 不给旗标：配置说了算——这正是 `config set default_kernel codex` 之后的下一轮运行
        assert_eq!(resolve_kernel(None, "codex").unwrap(), "codex");
        assert_eq!(resolve_kernel(None, "native").unwrap(), NATIVE_KERNEL);

        // 配置里的未知名（手改或拼错）：同一条人话错误，不静默回落到 native
        for bad in ["gemini", "", "Native"] {
            let error = resolve_kernel(None, bad).unwrap_err();
            assert!(error.contains(&format!("未知内核 '{bad}'")), "{error}");
            assert!(error.contains("native / codex / claude"), "{error}");
            // 旗标一侧是同一份口径：显式给了就轮不到配置，错误也逐字同形
            assert_eq!(resolve_kernel(Some(bad), "codex").unwrap_err(), error);
        }
    }

    /// `mk_runtime` 的选择真值表（`/kernel` 与 `--kernel` 同一条路径）：名字合法且可用才构造出内核。
    ///
    /// 可用性走 PATH 注入的变体——不能为测试改进程环境（用例并行，`set_var` 会互相打架）。
    #[test]
    fn mk_runtime_selects_by_name_and_reports_unavailable_external_kernels() {
        let dir = TempDir::new("select");
        let config = Config {
            api_key: Some("test-key".into()),
            ..Config::default()
        };

        // native 永远可（不查 PATH），能力是全套
        let runtime = mk_runtime("native", &config, Path::new("/tmp")).unwrap();
        assert_eq!(runtime.id(), "native");
        assert!(runtime.capabilities().tools);

        // 未知名：人话错误，可选值列全
        let error = mk_runtime("gemini", &config, Path::new("/tmp"))
            .err()
            .expect("未知内核名应报错");
        assert!(error.contains("未知内核 'gemini'"), "{error}");
        assert!(error.contains("native / codex / claude"), "{error}");

        // 已知但 PATH 上没有（目录此刻还是空的）：不上天，给人话错误
        for program in ["codex", "claude"] {
            let error =
                mk_runtime_with_path(program, &config, dir.path(), Some(dir.path().as_os_str()))
                    .err()
                    .expect("PATH 上没有这个 CLI 时应报错");
            assert_eq!(
                error,
                format!("找不到 {program} CLI：请先安装 {program} 并确保它在 PATH 中")
            );
        }

        // 装上就能切：id 与能力声明逐项钉住（外部内核三个能力都是 false）
        for program in ["codex", "claude"] {
            let bin = dir.path().join(program);
            fs::write(&bin, "#!/bin/sh\n").unwrap();
            set_executable(&bin);
        }
        let path = Some(dir.path().as_os_str());
        assert_eq!(
            mk_runtime_with_path("codex", &config, dir.path(), path)
                .unwrap()
                .id(),
            "codex"
        );
        let claude = mk_runtime_with_path("claude", &config, dir.path(), path).unwrap();
        assert_eq!(claude.id(), "claude");
        assert!(!claude.capabilities().tools);
        assert!(!claude.capabilities().approvals);
        assert!(!claude.capabilities().usage);
    }

    /// native 分支的凭据兜底：没凭据时不静默拿一把空 key 构造运行器（那要到第一次请求才 401），
    /// 而是报与 `Session` 装配期同一句人话。
    #[test]
    fn mk_runtime_refuses_native_without_an_ignis_key() {
        let dir = TempDir::new("native-keyless");
        // 环境变量名取一个不存在的（不走默认的 `OPENAI_API_KEY`），断言与运行者环境无关
        let config = Config {
            model: ignis_runtime::ModelConfig {
                api_key_env: "IGNIS_T4_ABSENT_KEY".into(),
                ..ignis_runtime::ModelConfig::default()
            },
            ..Config::default()
        };

        let error = mk_runtime("native", &config, dir.path())
            .err()
            .expect("缺凭据的 native 不该构造出运行器");
        assert!(error.contains("未配置 API key"), "{error}");
        assert!(error.contains("IGNIS_T4_ABSENT_KEY"), "{error}");
    }

    /// PATH 扫描：命中第一个可执行文件；非可执行文件、缺失的 PATH 都不算「装上了」。
    #[test]
    fn find_program_scans_path_entries_for_an_executable() {
        let dir = TempDir::new("path-scan");
        let program = "ignis-fake-codex";
        let fixture = dir.path().join(program);
        fs::write(&fixture, "#!/bin/sh\n").unwrap();

        // PATH 缺失（环境变量没设）：找不到任何东西
        assert_eq!(find_program(program, None), None);

        // 存在但没有执行位：不算可执行（非 Unix 平台只看存在性）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&fixture, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(find_program(program, Some(dir.path().as_os_str())), None);
        }
        set_executable(&fixture);
        assert_eq!(
            find_program(program, Some(dir.path().as_os_str())),
            Some(fixture.clone())
        );

        // 多目录按顺序扫描，前面的目录不存在也不影响后面的命中
        let joined =
            std::env::join_paths([dir.path().join("nope"), dir.path().to_path_buf()]).unwrap();
        assert_eq!(find_program(program, Some(&joined)), Some(fixture));
    }

    /// 缺二进制的错误含 CLI 名与安装提示；装上（PATH 里可执行）即返回 `Ok`。
    #[test]
    fn external_kernel_availability_reports_a_missing_binary() {
        let dir = TempDir::new("missing-cli");

        let error = ensure_installed("codex", Some(dir.path().as_os_str())).unwrap_err();
        assert_eq!(error, "找不到 codex CLI：请先安装 codex 并确保它在 PATH 中");

        fs::write(dir.path().join("codex"), "#!/bin/sh\n").unwrap();
        set_executable(&dir.path().join("codex"));
        assert_eq!(
            ensure_installed("codex", Some(dir.path().as_os_str())),
            Ok(())
        );
    }
}
