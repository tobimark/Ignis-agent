//! 环境自检：`ignis doctor` 的六项检查（外部内核探测由 CLI 追加在它们之后）。
//!
//! 检查项与顺序就是 CLI 的输出契约：home → config → api_key → base_url → workspace → db。
//! 每项有 [`CheckLevel`]：只有 `Fail` 让 `ignis doctor` 以非零退出码收场，`Warn` 只逐行提示
//! （「缺外部内核 CLI」「选了外部内核时缺 Ignis 密钥」「外部内核下 `base_url` 不可达」都不该
//! 拦住整套自检）。`api_key` 与 `base_url` 都按生效内核分级：只有 [`NATIVE_KERNEL`] 需要
//! Ignis 自己的端点与密钥。
//! 除 `base_url` 的 TCP 连接外都是本地读取，因此整个模块保持同步；会按需创建数据目录与
//! 数据库文件，这与 `Db::open` 的既有行为一致。

use std::fs;
use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use ignis_storage::Db;

use crate::config::{Config, ConfigError, config_path};

/// `base_url` 检查的 TCP 连接超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 自检里唯一需要 Ignis 自己的模型端点与 Ignis 密钥的执行内核（两个外部内核的模型调用与凭据
/// 都由各自 CLI 负责）。
pub const NATIVE_KERNEL: &str = "native";

/// `api_key` 检查项名：CLI 按这个名字定位这一行做内核相关的降级，两侧共用同一份字面量——
/// core 侧改名时 CLI 的匹配不会悄悄失效。
pub const CHECK_API_KEY: &str = "api_key";

/// 单项自检的级别：决定报告行首的记号与 `ignis doctor` 的退出码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
    /// 通过（报告行 `✓`）。
    Ok,
    /// 警告：只提示、不影响退出码（报告行 `!`）。
    Warn,
    /// 失败：需要处理，`ignis doctor` 以退出码 1 收场（报告行 `✗`）。
    Fail,
}

/// 单项自检的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// 检查项名（六项基本检查；外部内核项与 `kernel` 由 CLI 追加）。
    pub name: &'static str,
    /// 级别：`Warn` 不改变退出码。
    pub level: CheckLevel,
    /// 人话结论；失败时给出可操作的原因。
    pub detail: String,
}

/// 依次执行六项检查：home → config → api_key → base_url → workspace → db。
///
/// `loaded` 是调用方加载的**生效**配置（含环境变量覆盖）：配置解析失败本身也是一个结论，
/// 所以这里收 `Result` 而不是 `&Config`——解析失败时相关检查如实报 ✗，而不是让 doctor
/// 直接以 `error:` 退出。
///
/// `kernel` 是生效内核名（CLI 侧 `--kernel` > 配置 `default_kernel` > `native` 的解析结果）：
/// 它只决定两项的内核口径——Ignis 自己的密钥与 `base_url` 端点只有 [`NATIVE_KERNEL`] 需要，
/// 两个外部内核用各自 CLI 的凭据与网络，同一条缺失结论降级为 `Warn`（`--kernel codex` 照跑）。
pub fn run_checks(
    home: &Path,
    loaded: &Result<Config, ConfigError>,
    kernel: &str,
) -> Vec<CheckResult> {
    vec![
        check_home(home),
        check_config(home, loaded),
        check_api_key(loaded, kernel == NATIVE_KERNEL),
        check_base_url(loaded, kernel),
        check_workspace(loaded),
        check_db(home),
    ]
}

/// `home`：数据目录存在且可写（缺失就先创建）。
fn check_home(home: &Path) -> CheckResult {
    const NAME: &str = "home";
    if let Err(source) = fs::create_dir_all(home) {
        return fail(NAME, format!("无法创建目录 {}：{source}", home.display()));
    }
    // 真写一个探针文件：只看 `is_dir()` 会把只读目录误判成通过
    let probe = home.join(format!(".doctor-{}.tmp", std::process::id()));
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            ok(NAME, format!("目录可用：{}", home.display()))
        }
        Err(source) => fail(NAME, format!("目录不可写 {}：{source}", home.display())),
    }
}

/// `config`：配置文件可解析。环境变量覆盖发生在解析之后，因此与生效配置的解析结论等价。
fn check_config(home: &Path, loaded: &Result<Config, ConfigError>) -> CheckResult {
    const NAME: &str = "config";
    match loaded {
        Ok(_) => ok(NAME, format!("配置可解析：{}", config_path(home).display())),
        Err(error) => fail(NAME, error.to_string()),
    }
}

/// `api_key`：Ignis 自己的密钥只有 native 内核需要（外部内核用各自 CLI 的凭据）。
///
/// `ignis_key_required` 为真（选中 native）时缺它是 `Fail`：第一次请求就会 401；为假时降级为
/// `Warn`——只配了外部内核的用户不该被这一条挡在门外。
fn check_api_key(loaded: &Result<Config, ConfigError>, ignis_key_required: bool) -> CheckResult {
    const NAME: &str = CHECK_API_KEY;
    let Ok(config) = loaded else {
        return unavailable(NAME);
    };
    // `IGNIS_API_KEY` 在加载时已并入 `api_key`，这里再查模型自己的环境变量
    if config
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
    {
        return ok(NAME, "api_key 已配置".to_string());
    }
    let env = &config.model.api_key_env;
    if std::env::var(env).is_ok_and(|key| !key.trim().is_empty()) {
        return ok(NAME, format!("环境变量 {env} 已配置"));
    }
    if ignis_key_required {
        fail(
            NAME,
            format!("未配置 API key（native 内核需要；设置 {env} 或 config.json 的 api_key）"),
        )
    } else {
        warn(
            NAME,
            "未配置 API key（仅 native 内核需要；当前外部内核用自身 CLI 的凭据）".to_string(),
        )
    }
}

/// `base_url`：能 TCP 连上 `model.base_url` 的主机与端口。
///
/// 只有 native 内核会访问这个端点：外部内核的模型调用由各自 CLI 负责，端点不可达（或地址不成形）
/// 一律降级为 `Warn`，措辞点明当前内核不访问它；native 仍是硬检查——第一次请求就要用它。
fn check_base_url(loaded: &Result<Config, ConfigError>, kernel: &str) -> CheckResult {
    const NAME: &str = "base_url";
    let Ok(config) = loaded else {
        return unavailable(NAME);
    };
    let native = kernel == NATIVE_KERNEL;
    let address = match host_port(&config.model.base_url) {
        Ok(address) => address,
        Err(reason) => return base_url_failure(NAME, native, kernel, reason),
    };
    match connect(&address, CONNECT_TIMEOUT) {
        Ok(()) => ok(NAME, format!("已连接 {address}")),
        Err(error) => {
            base_url_failure(NAME, native, kernel, format!("无法连接 {address}：{error}"))
        }
    }
}

/// `base_url` 的失败口径：native → `Fail`（第一次请求就要用它）；外部内核 → `Warn` + 点明
/// 当前内核不访问该端点。
fn base_url_failure(name: &'static str, native: bool, kernel: &str, detail: String) -> CheckResult {
    if native {
        return fail(name, detail);
    }
    warn(
        name,
        format!("{detail}（仅 native 内核需要；当前内核 {kernel} 不访问该端点）"),
    )
}

/// `workspace`：工作区目录存在。
fn check_workspace(loaded: &Result<Config, ConfigError>) -> CheckResult {
    const NAME: &str = "workspace";
    let Ok(config) = loaded else {
        return unavailable(NAME);
    };
    let path = &config.workspace_root;
    if path.is_dir() {
        ok(NAME, format!("目录存在：{}", path.display()))
    } else {
        fail(NAME, format!("目录不存在或不是目录：{}", path.display()))
    }
}

/// `db`：数据库可打开且迁移成功。
fn check_db(home: &Path) -> CheckResult {
    const NAME: &str = "db";
    let path = home.join("ignis.db");
    match Db::open(&path) {
        Ok(_) => ok(NAME, format!("数据库可用（迁移完成）：{}", path.display())),
        Err(error) => fail(NAME, format!("数据库不可用 {}：{error}", path.display())),
    }
}

fn ok(name: &'static str, detail: String) -> CheckResult {
    CheckResult {
        name,
        level: CheckLevel::Ok,
        detail,
    }
}

fn warn(name: &'static str, detail: String) -> CheckResult {
    CheckResult {
        name,
        level: CheckLevel::Warn,
        detail,
    }
}

fn fail(name: &'static str, detail: String) -> CheckResult {
    CheckResult {
        name,
        level: CheckLevel::Fail,
        detail,
    }
}

/// 配置解析失败时依赖配置的检查无法进行：如实报 ✗。
fn unavailable(name: &'static str) -> CheckResult {
    fail(name, "配置解析失败，无法检查".to_string())
}

/// 从 `model.base_url` 取 `host:port`：只做字面量解析（不引入 URL 库），丢弃路径与查询串，
/// `http`/`https` 缺端口时补 80/443。
fn host_port(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim();
    let (rest, scheme_port) = match trimmed.split_once("://") {
        Some(("http", rest)) => (rest, Some(80)),
        Some(("https", rest)) => (rest, Some(443)),
        // 没有 scheme 的 `host:port` 也接受（本地服务地址常这么写）
        None => (trimmed, None),
        Some((scheme, _)) => {
            return Err(format!(
                "base_url 的协议不受支持：{scheme}://（{base_url}）"
            ));
        }
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err(format!("base_url 缺少主机：{base_url}"));
    }
    // IPv6 字面量：`[::1]` 或 `[::1]:443`
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("base_url 的 IPv6 地址不完整：{base_url}"))?;
        let host = format!("[{host}]");
        return match tail.strip_prefix(':') {
            Some(port) if !port.is_empty() => Ok(format!("{host}:{port}")),
            _ => with_default_port(&host, scheme_port, base_url),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() => Ok(format!("{host}:{port}")),
        _ => with_default_port(authority, scheme_port, base_url),
    }
}

/// 补协议默认端口；没有默认端口（无 scheme 又没写端口）就报错。
fn with_default_port(host: &str, port: Option<u16>, base_url: &str) -> Result<String, String> {
    port.map(|port| format!("{host}:{port}"))
        .ok_or_else(|| format!("base_url 缺少端口：{base_url}"))
}

/// 逐个地址尝试连接（每个地址各自 [`CONNECT_TIMEOUT`]）；任一成功即通过，全失败报最后一个错误。
fn connect(address: &str, timeout: Duration) -> io::Result<()> {
    let resolved: Vec<SocketAddr> = address.to_socket_addrs()?.collect();
    let mut last = None;
    for addr in resolved {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(_stream) => return Ok(()),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("地址无法解析：{address}"),
        )
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_runtime::ModelConfig;

    use super::*;

    /// 单个测试独占的临时目录，析构时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-core-doctor-{}-{label}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(results: &[CheckResult]) -> Vec<&str> {
        results.iter().map(|result| result.name).collect()
    }

    /// 单项检查结论（六项的位置按 `run_checks` 的顺序钉住，这里按名字取用）。
    fn check<'a>(results: &'a [CheckResult], name: &str) -> &'a CheckResult {
        results
            .iter()
            .find(|result| result.name == name)
            .unwrap_or_else(|| panic!("缺少 {name} 检查：{results:?}"))
    }

    /// 检查项名的字面量是与 CLI（`ignis doctor` 的降级匹配）和文档的契约：改这里要连两侧一起改。
    #[test]
    fn check_names_are_stable() {
        assert_eq!(CHECK_API_KEY, "api_key");
        assert_eq!(NATIVE_KERNEL, "native");
    }

    /// `base_url` 的内核口径：native 是硬检查（不可达/地址不成形都 `✗`）；外部内核
    /// （Ignis 不访问这个端点）一律降级为 `!`，并在措辞里点明当前内核。
    #[test]
    fn base_url_is_hard_for_native_and_soft_for_external_kernels() {
        let home = TempDir::new("base-url-scope");
        let workspace = home.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        // 端口 1 上不会有服务：连接会被立刻拒绝（不需要等超时）
        let config = Config {
            workspace_root: workspace,
            // 环境变量名取一个不存在的（不走默认的 `OPENAI_API_KEY`），断言与运行者环境无关
            model: ModelConfig {
                base_url: "127.0.0.1:1".into(),
                api_key_env: "IGNIS_T8_ABSENT_KEY".into(),
                ..ModelConfig::default()
            },
            ..Config::default()
        };

        // native：不可达是 ✗（第一次请求就要用它）
        let results = run_checks(home.path(), &Ok(config.clone()), NATIVE_KERNEL);
        let base_url = check(&results, "base_url");
        assert_eq!(base_url.level, CheckLevel::Fail, "{base_url:?}");
        assert!(base_url.detail.contains("127.0.0.1:1"), "{base_url:?}");

        // 外部内核：同一条结论降级为 `!`（退出码不受影响），措辞点明当前内核不访问该端点
        for kernel in ["codex", "claude"] {
            let results = run_checks(home.path(), &Ok(config.clone()), kernel);
            let base_url = check(&results, "base_url");
            assert_eq!(base_url.level, CheckLevel::Warn, "{base_url:?}");
            assert!(base_url.detail.contains("127.0.0.1:1"), "{base_url:?}");
            assert!(
                base_url.detail.contains("仅 native 内核需要"),
                "{base_url:?}"
            );
            assert!(
                base_url.detail.contains(&format!("当前内核 {kernel}")),
                "{base_url:?}"
            );
        }

        // 地址不成形（既没端口也没 scheme 默认端口）同样是内核口径：native ✗、外部内核 `!`
        let malformed = Config {
            model: ModelConfig {
                base_url: "https:///v1".into(),
                ..config.model.clone()
            },
            ..config.clone()
        };
        let results = run_checks(home.path(), &Ok(malformed.clone()), NATIVE_KERNEL);
        assert_eq!(check(&results, "base_url").level, CheckLevel::Fail);
        let results = run_checks(home.path(), &Ok(malformed), "codex");
        let base_url = check(&results, "base_url");
        assert_eq!(base_url.level, CheckLevel::Warn, "{base_url:?}");
        assert!(base_url.detail.contains("缺少主机"), "{base_url:?}");
        assert!(
            base_url.detail.contains("仅 native 内核需要"),
            "{base_url:?}"
        );
    }

    #[test]
    fn doctor_reports_unreachable_base_url() {
        let home = TempDir::new("unreachable");
        // 端口 1 上不会有服务：连接会被立刻拒绝（不需要等超时）
        let config = Config {
            model: ModelConfig {
                base_url: "127.0.0.1:1".into(),
                ..ModelConfig::default()
            },
            ..Config::default()
        };

        let results = run_checks(home.path(), &Ok(config), NATIVE_KERNEL);

        assert_eq!(
            names(&results),
            ["home", "config", "api_key", "base_url", "workspace", "db"]
        );
        let api_key = check(&results, CHECK_API_KEY);
        assert_eq!(api_key.name, CHECK_API_KEY, "构造用的常量与行名同源");
        let base_url = check(&results, "base_url");
        assert_eq!(
            base_url.level,
            CheckLevel::Fail,
            "127.0.0.1:1 不该被判为可达：{base_url:?}"
        );
        assert!(
            base_url.detail.contains("127.0.0.1:1"),
            "失败详情要带上出问题的地址：{}",
            base_url.detail
        );
    }

    #[test]
    fn doctor_all_checks_pass_with_temp_home() {
        let dir = TempDir::new("all-pass");
        let home = dir.path().join("not-created"); // home 检查负责创建
        let workspace = dir.path().join("project");
        fs::create_dir_all(&workspace).unwrap();

        // 本地 listener 顶替真实模型服务：TCP 握手能完成即可
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let config = Config {
            workspace_root: workspace,
            api_key: Some("test-key".into()),
            model: ModelConfig {
                base_url: format!("http://{address}/v1"),
                ..ModelConfig::default()
            },
            ..Config::default()
        };

        let results = run_checks(&home, &Ok(config), NATIVE_KERNEL);

        assert_eq!(
            names(&results),
            ["home", "config", "api_key", "base_url", "workspace", "db"]
        );
        for result in &results {
            assert_eq!(
                result.level,
                CheckLevel::Ok,
                "{} 检查未通过：{}",
                result.name,
                result.detail
            );
        }
        // 检查本身把缺失的数据目录与数据库建了起来
        assert!(home.is_dir(), "{}", home.display());
        assert!(home.join("ignis.db").is_file(), "{}", home.display());
        drop(listener);
    }

    /// 缺 Ignis 密钥只对 native 是硬门槛：选中 native 时 `Fail`；选中外部内核（用各自 CLI 的
    /// 凭据）时降级为 `Warn`——只点了 codex/claude 的用户不该被这一条挡在门外。
    #[test]
    fn missing_key_fails_for_native_and_only_warns_for_external_kernels() {
        let home = TempDir::new("api-key");
        let workspace = home.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        // 环境变量名取一个不存在的（不走默认的 `OPENAI_API_KEY`），断言与运行者环境无关
        let config = Config {
            workspace_root: workspace,
            model: ModelConfig {
                base_url: "127.0.0.1:1".into(),
                api_key_env: "IGNIS_T4_ABSENT_KEY".into(),
                ..ModelConfig::default()
            },
            ..Config::default()
        };

        let results = run_checks(home.path(), &Ok(config.clone()), NATIVE_KERNEL);
        let key = check(&results, CHECK_API_KEY);
        assert_eq!(key.level, CheckLevel::Fail, "{key:?}");
        assert!(key.detail.contains("未配置 API key"), "{key:?}");
        assert!(key.detail.contains("native 内核需要"), "{key:?}");
        assert!(key.detail.contains("IGNIS_T4_ABSENT_KEY"), "{key:?}");

        // 空串/纯空白的密钥视为未配置（与 CLI 的 `resolve_api_key` 同一口径）：不拿空 key 去
        // 换一次注定 401 的请求
        let blank = Config {
            api_key: Some("   ".into()),
            ..config.clone()
        };
        let results = run_checks(home.path(), &Ok(blank), NATIVE_KERNEL);
        let key = check(&results, CHECK_API_KEY);
        assert_eq!(key.level, CheckLevel::Fail, "{key:?}");
        assert!(key.detail.contains("未配置 API key"), "{key:?}");

        // 外部内核：同一条缺密钥结论降级为警告（退出码不受影响），措辞说明这只与 native 有关
        let results = run_checks(home.path(), &Ok(config.clone()), "codex");
        let key = check(&results, CHECK_API_KEY);
        assert_eq!(key.level, CheckLevel::Warn, "{key:?}");
        assert!(key.detail.contains("仅 native 内核需要"), "{key:?}");

        // 有密钥时与内核无关：两种口径都是 ✓
        let with_key = Config {
            api_key: Some("test-key".into()),
            ..config
        };
        for kernel in [NATIVE_KERNEL, "codex"] {
            let results = run_checks(home.path(), &Ok(with_key.clone()), kernel);
            let key = check(&results, CHECK_API_KEY);
            assert_eq!(key.level, CheckLevel::Ok, "{key:?}");
            assert!(key.detail.contains("api_key 已配置"), "{key:?}");
        }
    }
}
