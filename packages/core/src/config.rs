//! 配置：`~/.ignis/config.json` 的加载/保存、环境变量覆盖与密钥脱敏。
//!
//! 生效值的组装顺序是「默认值 → 文件（存在则解析）→ 环境变量覆盖」，环境变量只影响
//! 生效值、不写回文件。`Config` 的 `Debug` 恒定把 `api_key` 渲染成 `***`。

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ignis_runtime::{Limits, ModelConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 配置文件名，位于 home 目录下。
const CONFIG_FILE: &str = "config.json";
/// 未设置 `IGNIS_HOME` 时使用的子目录名。
const DOT_IGNIS: &str = ".ignis";

const ENV_HOME: &str = "IGNIS_HOME";
const ENV_WORKSPACE: &str = "IGNIS_WORKSPACE";
const ENV_BASE_URL: &str = "IGNIS_BASE_URL";
const ENV_MODEL: &str = "IGNIS_MODEL";
const ENV_API_KEY: &str = "IGNIS_API_KEY";

/// 生效配置：配置文件只需写要覆盖的字段，其余回落到 [`Default`]。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 工具操作的项目根目录。
    pub workspace_root: PathBuf,
    /// 默认执行内核，v0.1 只有 `native`。
    pub default_kernel: String,
    /// 模型接入：协议、baseUrl、模型名与密钥来源环境变量。
    pub model: ModelConfig,
    /// 直接写入配置文件的密钥（亦可由 `IGNIS_API_KEY` 覆盖）。
    pub api_key: Option<String>,
    /// 运行上限。
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 未配置工作区时以启动目录为根，避免悄悄指向别处
            workspace_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            default_kernel: "native".into(),
            model: ModelConfig::default(),
            api_key: None,
            limits: Limits::default(),
        }
    }
}

/// `api_key` 恒定打印 `***`，保证密钥不会随调试输出进入日志或终端。
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("workspace_root", &self.workspace_root)
            .field("default_kernel", &self.default_kernel)
            .field("model", &self.model)
            .field("api_key", &"***")
            .field("limits", &self.limits)
            .finish()
    }
}

/// 配置读写的失败原因；涉及文件时错误信息都带上出问题的路径。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// 文件存在但读不出来（权限、类型不符等）。
    #[error("读取配置失败：{}：{source}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// 文件内容不是合法 JSON 或字段类型不符。
    #[error("配置文件无法解析：{}：{source}", .path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// 建目录、写临时文件或 rename 失败。
    #[error("写入配置失败：{}：{source}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// 配置序列化失败（例如路径含无法编码的字节）。
    #[error("配置序列化失败：{source}")]
    Serialize {
        #[source]
        source: serde_json::Error,
    },
}

/// Ignis 数据目录：`IGNIS_HOME` 优先，否则 `~/.ignis`。
pub fn ignis_home() -> PathBuf {
    home_from(|key| std::env::var(key).ok())
}

/// 与 [`ignis_home`] 相同，但环境读取走 `get`，便于单测不碰真实环境变量。
fn home_from(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(value) = non_empty(get(ENV_HOME)) {
        return PathBuf::from(value);
    }
    // 极端环境下 HOME 缺失时退回当前目录，避免调用方不得不处理 None
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(DOT_IGNIS)
}

/// 配置文件路径：`{home}/{config.json}`。
pub fn config_path(home: &Path) -> PathBuf {
    home.join(CONFIG_FILE)
}

/// 加载配置：缺失文件（含 home 目录尚未创建）用默认值，随后应用环境变量覆盖。
pub fn load_config(home: &Path) -> Result<Config, ConfigError> {
    load_config_with(home, |key| std::env::var(key).ok())
}

/// 只读配置文件（加默认值），**不**应用环境变量覆盖。
///
/// `ignis config set` 必须走这条路径：把生效值写回文件会把 `IGNIS_MODEL` 之类的环境变量
/// 派生值持久化进 `config.json`。
pub fn load_config_file(home: &Path) -> Result<Config, ConfigError> {
    load_file(home)
}

/// 与 [`load_config`] 相同，但环境读取走 `get`，便于单测覆盖而不碰真实环境变量。
fn load_config_with(
    home: &Path,
    get: impl Fn(&str) -> Option<String>,
) -> Result<Config, ConfigError> {
    let mut cfg = load_file(home)?;
    apply_env_overrides(&mut cfg, get);
    Ok(cfg)
}

/// 文件原文的 JSON 表示：文件缺失给 `{}`，内容坏掉仍报 [`ConfigError::Parse`]。
///
/// `ignis config set` 用它当底稿（而不是 `Config::default()`）：一次 `set` 只落地被改写的
/// 那一项，`workspace_root` 之类的默认值不会被物化进 `config.json`。
pub fn load_config_value(home: &Path) -> Result<Value, ConfigError> {
    match read_raw(home)? {
        Some(raw) => serde_json::from_str(&raw).map_err(|source| parse_error(home, source)),
        None => Ok(Value::Object(serde_json::Map::new())),
    }
}

/// 默认值 + 文件（存在则解析）。
fn load_file(home: &Path) -> Result<Config, ConfigError> {
    match read_raw(home)? {
        Some(raw) => serde_json::from_str(&raw).map_err(|source| parse_error(home, source)),
        None => Ok(Config::default()),
    }
}

/// 读配置文件原文：文件缺失给 `None`，其它读取失败带上路径报错。
fn read_raw(home: &Path) -> Result<Option<String>, ConfigError> {
    let path = config_path(home);
    match fs::read_to_string(&path) {
        Ok(raw) => Ok(Some(raw)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read { path, source }),
    }
}

/// 解析失败一律带上配置文件路径。
fn parse_error(home: &Path, source: serde_json::Error) -> ConfigError {
    ConfigError::Parse {
        path: config_path(home),
        source,
    }
}

/// 保存配置：home 目录按需创建，先写同目录临时文件再 rename，落到 0600。
pub fn save_config(home: &Path, cfg: &Config) -> Result<(), ConfigError> {
    write_config(home, json_bytes(cfg)?)
}

/// 保存一份原始 JSON 文档，排版与 [`save_config`] 一致。
///
/// `ignis config set` 用它把「只改了一处」的文件原文写回：走 `Config` 的序列化会把
/// `Config::default()` 的所有默认值（尤其 `workspace_root`）物化进配置文件。
pub fn save_config_value(home: &Path, value: &Value) -> Result<(), ConfigError> {
    write_config(home, json_bytes(value)?)
}

/// 配置文件排版：pretty JSON + 末尾换行。
fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>, ConfigError> {
    let mut data =
        serde_json::to_vec_pretty(value).map_err(|source| ConfigError::Serialize { source })?;
    data.push(b'\n');
    Ok(data)
}

/// 原子落盘：home 按需创建，同目录临时文件 + rename（0600，不留半截 JSON）。
fn write_config(home: &Path, data: Vec<u8>) -> Result<(), ConfigError> {
    let path = config_path(home);
    fs::create_dir_all(home).map_err(|source| ConfigError::Write {
        path: home.to_path_buf(),
        source,
    })?;

    let tmp = home.join(format!(".{CONFIG_FILE}.{}.tmp", std::process::id()));
    write_private(&tmp, &data).map_err(|source| ConfigError::Write {
        path: tmp.clone(),
        source,
    })?;
    if let Err(source) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(ConfigError::Write { path, source });
    }
    Ok(())
}

/// 以 0600 写入：`mode` 只在新建时生效（umask 无法放宽它），已存在的旧临时文件再收紧一次。
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(data)?;
    file.sync_all()
}

/// 环境变量覆盖，只改生效值；空白字符串（如 `IGNIS_MODEL=`）视为未设置。
fn apply_env_overrides(cfg: &mut Config, get: impl Fn(&str) -> Option<String>) {
    if let Some(value) = non_empty(get(ENV_WORKSPACE)) {
        cfg.workspace_root = PathBuf::from(value);
    }
    if let Some(value) = non_empty(get(ENV_BASE_URL)) {
        cfg.model.base_url = value;
    }
    if let Some(value) = non_empty(get(ENV_MODEL)) {
        cfg.model.model = value;
    }
    if let Some(value) = non_empty(get(ENV_API_KEY)) {
        cfg.api_key = Some(value);
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_runtime::ModelProtocol;
    use serde_json::json;

    use super::*;

    const CONFIG_JSON: &str = "config.json";

    // 真实环境变量在 edition 2024 中是 unsafe 且会与并行测试竞争，因此除
    // `load_config`/`ignis_home` 的冒烟断言外，所有覆盖行为都通过注入的 lookup 验证。
    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// 单个测试独占的临时 home 目录，析构时清理。
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-core-{}-{label}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn config_path(&self) -> PathBuf {
            self.0.join(CONFIG_JSON)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn missing_file_yields_defaults() {
        let home = TempHome::new("missing");
        let cfg = load_config_with(home.path(), no_env).unwrap();
        assert_eq!(cfg.workspace_root, std::env::current_dir().unwrap());
        assert_eq!(cfg.default_kernel, "native");
        assert_eq!(cfg.model, ModelConfig::default());
        assert_eq!(cfg.limits, Limits::default());
        assert_eq!(cfg.api_key, None);
    }

    #[test]
    fn missing_home_dir_yields_defaults() {
        let base = TempHome::new("absent-home");
        let home = base.path().join("not-created");
        let cfg = load_config_with(&home, no_env).unwrap();
        assert_eq!(cfg.default_kernel, "native");
        assert_eq!(cfg.workspace_root, std::env::current_dir().unwrap());
    }

    #[test]
    fn partial_file_only_overrides_written_fields() {
        let home = TempHome::new("partial");
        fs::write(home.config_path(), r#"{"model":{"model":"local-llm"}}"#).unwrap();
        let cfg = load_config_with(home.path(), no_env).unwrap();
        assert_eq!(cfg.model.model, "local-llm");
        assert_eq!(cfg.model.base_url, ModelConfig::default().base_url);
        assert_eq!(cfg.model.api_key_env, "OPENAI_API_KEY");
        assert_eq!(cfg.model.protocol, ModelProtocol::OpenAi);
        assert_eq!(cfg.limits, Limits::default());
        assert_eq!(cfg.default_kernel, "native");
    }

    #[test]
    fn env_overrides_win_over_file() {
        let home = TempHome::new("env-wins");
        let raw = r#"{"model":{"model":"a"},"api_key":"file-key"}"#;
        fs::write(home.config_path(), raw).unwrap();

        let cfg = load_config_with(home.path(), |key| {
            (key == "IGNIS_MODEL").then(|| "b".to_string())
        })
        .unwrap();

        // 环境变量胜出；未被覆盖的字段仍来自文件
        assert_eq!(cfg.model.model, "b");
        assert_eq!(cfg.api_key.as_deref(), Some("file-key"));
        // 覆盖只存在于生效值，不写回文件
        assert_eq!(fs::read_to_string(home.config_path()).unwrap(), raw);
    }

    #[test]
    fn each_override_var_maps_to_its_field() {
        let mut cfg = Config::default();
        apply_env_overrides(&mut cfg, |key| match key {
            "IGNIS_WORKSPACE" => Some("/env/ws".into()),
            "IGNIS_BASE_URL" => Some("https://env/v1".into()),
            "IGNIS_MODEL" => Some("env-model".into()),
            "IGNIS_API_KEY" => Some("env-key".into()),
            // IGNIS_HOME 只由 ignis_home() 处理，这里必须忽略
            "IGNIS_HOME" => Some("/env/home".into()),
            _ => None,
        });
        assert_eq!(cfg.workspace_root, PathBuf::from("/env/ws"));
        assert_eq!(cfg.model.base_url, "https://env/v1");
        assert_eq!(cfg.model.model, "env-model");
        assert_eq!(cfg.api_key.as_deref(), Some("env-key"));
        assert_eq!(cfg.default_kernel, "native");
        assert_eq!(cfg.limits, Limits::default());
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let mut cfg = Config {
            api_key: Some("file-key".into()),
            ..Default::default()
        };
        apply_env_overrides(&mut cfg, |_| Some("  ".to_string()));
        assert_eq!(cfg.api_key.as_deref(), Some("file-key"));
        assert_eq!(cfg.model, ModelConfig::default());
        assert_eq!(cfg.workspace_root, Config::default().workspace_root);
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_0600_file() {
        let home = TempHome::new("mode");
        let cfg = Config::default();

        save_config(home.path(), &cfg).unwrap();
        assert_eq!(mode(&home.config_path()), 0o600);

        // 目标文件原本权限更宽时，原子替换后也不得放宽
        set_mode(&home.config_path(), 0o644);
        save_config(home.path(), &cfg).unwrap();
        assert_eq!(mode(&home.config_path()), 0o600);

        // 临时文件不留残渣
        let mut names: Vec<String> = fs::read_dir(home.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec![CONFIG_JSON.to_string()]);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let home = TempHome::new("roundtrip");
        let cfg = Config {
            workspace_root: PathBuf::from("/tmp/project"),
            default_kernel: "native".into(),
            model: ModelConfig {
                base_url: "http://localhost:11434/v1".into(),
                model: "qwen3".into(),
                api_key_env: "MY_KEY".into(),
                protocol: ModelProtocol::OpenAi,
            },
            api_key: Some("sk-roundtrip".into()),
            limits: Limits {
                max_steps: 7,
                ..Limits::default()
            },
        };

        save_config(home.path(), &cfg).unwrap();
        assert_eq!(load_config_with(home.path(), no_env).unwrap(), cfg);
    }

    #[test]
    fn save_config_creates_missing_home_dir() {
        let base = TempHome::new("nested");
        let home = base.path().join("not-created");
        assert!(!home.exists());

        save_config(&home, &Config::default()).unwrap();
        assert!(home.join(CONFIG_JSON).is_file());
    }

    #[test]
    fn load_config_value_missing_file_is_empty_object() {
        let home = TempHome::new("raw-missing");
        // 文件不存在时给 `{}` 而不是默认配置：`config set` 的底稿不能含默认值
        assert_eq!(load_config_value(home.path()).unwrap(), json!({}));
        assert_eq!(
            load_config_value(&home.path().join("absent")).unwrap(),
            json!({})
        );
    }

    #[test]
    fn load_config_value_reports_bad_json() {
        let home = TempHome::new("raw-invalid");
        let path = home.config_path();
        fs::write(&path, "{oops").unwrap();

        let err = load_config_value(home.path()).unwrap_err();
        assert!(
            matches!(&err, ConfigError::Parse { path: p, .. } if p == &path),
            "unexpected error: {err:?}"
        );
        assert!(err.to_string().contains(CONFIG_JSON), "err: {err}");
    }

    #[test]
    fn save_config_value_writes_raw_json_without_defaults() {
        let home = TempHome::new("raw-save");
        let value = json!({"limits": {"max_steps": 7}});

        save_config_value(home.path(), &value).unwrap();

        let raw = fs::read_to_string(home.config_path()).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        // 逐字保留：没有 workspace_root / default_kernel 之类的默认值
        assert_eq!(parsed, value);
        assert!(parsed.get("workspace_root").is_none(), "{raw}");
        // 落到文件的内容仍能按「默认值 + 文件」读回
        assert_eq!(
            load_config_with(home.path(), no_env)
                .unwrap()
                .limits
                .max_steps,
            7
        );
        #[cfg(unix)]
        assert_eq!(mode(&home.config_path()), 0o600);
        // 分支覆盖：文件不存在时用默认值装载，仍走同一套写盘代码
        let fresh = TempHome::new("raw-save-fresh");
        save_config_value(&fresh.path().join("not-created"), &json!({"api_key": null})).unwrap();
        assert!(fresh.path().join("not-created").join(CONFIG_JSON).is_file());
    }

    #[test]
    fn debug_never_prints_api_key() {
        let cfg = Config {
            api_key: Some("sk-test-secret".into()),
            ..Default::default()
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("sk-test-secret"));
        assert!(rendered.contains("***"));
        assert!(rendered.contains("workspace_root"));
    }

    #[test]
    fn invalid_json_reports_path() {
        let home = TempHome::new("invalid");
        let path = home.config_path();
        fs::write(&path, "{oops").unwrap();

        let err = load_config_with(home.path(), no_env).unwrap_err();
        assert!(
            matches!(&err, ConfigError::Parse { path: p, .. } if p == &path),
            "unexpected error: {err:?}"
        );
        assert!(err.to_string().contains(CONFIG_JSON), "err: {err}");
    }

    #[test]
    fn unreadable_config_reports_path() {
        let home = TempHome::new("unreadable");
        let path = home.config_path();
        fs::create_dir(&path).unwrap(); // 目录冒充配置文件

        let err = load_config_with(home.path(), no_env).unwrap_err();
        assert!(
            matches!(&err, ConfigError::Read { path: p, .. } if p == &path),
            "unexpected error: {err:?}"
        );
        assert!(err.to_string().contains(CONFIG_JSON), "err: {err}");
    }

    #[test]
    fn ignis_home_uses_env_or_home_dir() {
        assert_eq!(
            home_from(|key| (key == "IGNIS_HOME").then(|| "/tmp/ignis-env".to_string())),
            PathBuf::from("/tmp/ignis-env")
        );
        // 未设置 IGNIS_HOME → <home>/.ignis（HOME 缺失时才退回当前目录）
        assert!(home_from(no_env).ends_with(".ignis"));
        assert_eq!(ignis_home(), home_from(|key| std::env::var(key).ok()));
    }

    #[test]
    fn load_config_file_ignores_env_overrides() {
        let home = TempHome::new("file-only");
        let raw = r#"{"model":{"model":"file-model"},"api_key":"file-key"}"#;
        fs::write(home.config_path(), raw).unwrap();

        // 文件加载器完全不看环境；同一份文件走分层加载时会被 `IGNIS_MODEL` 覆盖
        let file_cfg = load_config_file(home.path()).unwrap();
        let effective = load_config_with(home.path(), |key| {
            (key == ENV_MODEL).then(|| "env-model".to_string())
        })
        .unwrap();

        assert_eq!(file_cfg.model.model, "file-model");
        assert_eq!(file_cfg.api_key.as_deref(), Some("file-key"));
        assert_eq!(effective.model.model, "env-model");
        // `config set` 只写文件加载器读到的值：覆盖不会被持久化
        assert_eq!(fs::read_to_string(home.config_path()).unwrap(), raw);
    }

    #[test]
    fn load_config_reads_file_through_public_api() {
        let home = TempHome::new("public");
        fs::write(home.config_path(), r#"{"model":{"api_key_env":"MY_KEY"}}"#).unwrap();

        // 只断言不受 IGNIS_* 覆盖影响的字段，避免依赖运行者环境
        let cfg = load_config(home.path()).unwrap();
        assert_eq!(cfg.model.api_key_env, "MY_KEY");
        assert_eq!(cfg.default_kernel, "native");
    }
}
