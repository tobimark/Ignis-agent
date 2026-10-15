//! `ignis config show|set`：打印生效配置、改写文件配置。
//!
//! `show` 走 `load_config`（含环境变量覆盖）；`set` 以**文件原文**为底稿
//! （`load_config_value`：缺失文件给 `{}`，不是 `Config::default()`），只改写被点名的
//! 那一项，再用 `save_config_value` 原样写回。因此既不会把 `IGNIS_MODEL` 之类的环境派生值
//! 持久化，也不会把 `workspace_root` 这类默认值物化进 `config.json`（否则一次 `config set`
//! 就会把当时的 cwd 钉死成工作区）。

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use ignis_core::config::{Config, ignis_home, load_config, load_config_value, save_config_value};
use serde_json::{Value, json};

use crate::cli::ConfigCommand;

/// 展示密钥的占位符：`config show` 的输出与 `config set api_key` 的回显都用它。
const REDACTED: &str = "***";

/// `ignis config`：`show` 打印生效配置，`set` 写回文件配置。
pub fn run(command: &ConfigCommand) -> Result<ExitCode> {
    match command {
        ConfigCommand::Show => show(),
        ConfigCommand::Set { key, value } => set(key, value),
    }
}

/// 打印生效配置（环境变量覆盖已应用）。
fn show() -> Result<ExitCode> {
    let config = load_config(&ignis_home())?;
    print!("{}", render(&config)?);
    Ok(ExitCode::SUCCESS)
}

/// 写回一个标量项并回显新值。
fn set(key: &str, value: &str) -> Result<ExitCode> {
    let echoed = set_value(&ignis_home(), key, value)?;
    println!("{key} = {echoed}");
    Ok(ExitCode::SUCCESS)
}

/// 生效配置的展示文本：每个标量叶子一行 `点路径 = 值`，`api_key` 恒为 `***`。
fn render(config: &Config) -> Result<String, serde_json::Error> {
    let mut leaves = Vec::new();
    flatten("", &serde_json::to_value(config)?, &mut leaves);
    // 输出顺序只取决于键名：与 serde_json 的 Map 实现（是否 preserve_order）无关
    leaves.sort_by(|left, right| left.0.cmp(&right.0));

    let mut out = String::new();
    for (path, value) in leaves {
        let shown = if path == "api_key" { REDACTED } else { &value };
        out.push_str(&format!("{path} = {shown}\n"));
    }
    Ok(out)
}

/// 把嵌套配置摊平成 `(点路径, 标量文本)`；字符串不加引号，其余用 JSON 字面量。
fn flatten(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, child, out);
            }
        }
        Value::String(text) => out.push((prefix.to_string(), text.clone())),
        other => out.push((prefix.to_string(), other.to_string())),
    }
}

/// `config set` 的白名单：可写的标量叶子（点路径 + 取值类型）。
///
/// 嵌套字段用 serde_json 的 JSON Pointer 读写（`limits.max_steps` → `/limits/max_steps`），
/// 不需要反射；白名单之外的点路径（容器、未知键、`model.protocol` 这类枚举）一律拒绝。
const FIELDS: &[(&str, Kind)] = &[
    ("workspace_root", Kind::Text),
    ("default_kernel", Kind::Text),
    ("api_key", Kind::Secret),
    ("model.base_url", Kind::Text),
    ("model.model", Kind::Text),
    ("model.api_key_env", Kind::Text),
    ("limits.max_steps", Kind::U32),
    ("limits.task_timeout_secs", Kind::U64),
    ("limits.token_budget", Kind::U64),
    ("limits.max_retries", Kind::U32),
    ("limits.shell_timeout_secs", Kind::U64),
    ("limits.tool_output_limit", Kind::Usize),
    ("limits.http_max_bytes", Kind::Usize),
    ("limits.context_char_budget", Kind::Usize),
];

/// 取值类型：决定 CLI 字符串怎么变成 JSON 标量。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 文本（路径、URL、模型名）。
    Text,
    /// 明文密钥：写进文件（0600），但回显打码。
    Secret,
    /// 无符号整数；按 `Config` 里字段的宽度逐项区分，越界即报错。
    U32,
    U64,
    Usize,
}

impl Kind {
    /// CLI 字符串 → JSON 标量；类型不符返回 `None`。
    fn parse(self, raw: &str) -> Option<Value> {
        match self {
            Kind::Text | Kind::Secret => Some(Value::String(raw.to_string())),
            Kind::U32 => raw.parse::<u32>().ok().map(|value| json!(value)),
            Kind::U64 => raw.parse::<u64>().ok().map(|value| json!(value)),
            Kind::Usize => raw.parse::<usize>().ok().map(|value| json!(value)),
        }
    }

    /// 报错文案里的取值说明。
    fn expected(self) -> &'static str {
        match self {
            Kind::Text | Kind::Secret => "文本",
            Kind::U32 => "0-4294967295 的整数",
            Kind::U64 | Kind::Usize => "非负整数",
        }
    }
}

/// 把白名单内的标量点路径写回 `config.json`，返回回显值（密钥打码）。
///
/// 底稿是文件原文而非 `Config::default()`：文件因此保持「只写覆盖项」，
/// 一次 `config set` 不会顺手把其它字段（尤其 `workspace_root`）写死成当时的默认值。
fn set_value(home: &Path, key: &str, value: &str) -> Result<String> {
    let Some((_, kind)) = FIELDS.iter().find(|(path, _)| *path == key).copied() else {
        return Err(anyhow!("未知配置项：{key}"));
    };
    let scalar = kind
        .parse(value)
        .ok_or_else(|| anyhow!("取值不合法：{value}（{key} 需要{}）", kind.expected()))?;

    let mut json = load_config_value(home)?;
    write_scalar(&mut json, key, scalar)?;
    // 校验：改完的文件仍须能反序列化成 `Config`（类型不符、枚举非法在这里报错）
    serde_json::from_value::<Config>(json.clone())?;
    save_config_value(home, &json)?;

    Ok(if kind == Kind::Secret {
        REDACTED.to_string()
    } else {
        value.to_string()
    })
}

/// 在 JSON 对象上按点路径写入标量；父级对象缺失时按需创建（新文件从 `{}` 起步）。
fn write_scalar(root: &mut Value, key: &str, scalar: Value) -> Result<()> {
    let segments: Vec<&str> = key.split('.').filter(|part| !part.is_empty()).collect();
    let Some((leaf, parents)) = segments.split_last() else {
        return Err(anyhow!("未知配置项：{key}"));
    };

    let mut target = root;
    for parent in parents {
        target = target
            .as_object_mut()
            .ok_or_else(|| anyhow!("配置项缺失：{key}"))?
            .entry((*parent).to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
    target
        .as_object_mut()
        .ok_or_else(|| anyhow!("配置项缺失：{key}"))?
        .insert((*leaf).to_string(), scalar);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_core::config::{load_config, load_config_file};
    use ignis_runtime::ModelConfig;

    use super::*;

    /// 单个测试独占的临时 home，析构时清理。
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-cli-config-{}-{label}-{}",
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

    #[test]
    fn config_show_redacts_key() {
        let config = Config {
            api_key: Some("sk-test-secret".into()),
            model: ModelConfig {
                model: "local-llm".into(),
                ..ModelConfig::default()
            },
            ..Config::default()
        };

        let out = render(&config).unwrap();

        assert!(!out.contains("sk-test-secret"), "{out}");
        assert!(out.contains("api_key = ***\n"), "{out}");
        // 其余叶子逐项可见，一行一个点路径
        assert!(out.contains("model.model = local-llm\n"), "{out}");
        assert!(
            out.contains("model.api_key_env = OPENAI_API_KEY\n"),
            "{out}"
        );
        assert!(out.contains("limits.max_steps = 30\n"), "{out}");
        assert!(out.contains("default_kernel = native\n"), "{out}");
        assert!(
            out.contains(&format!(
                "workspace_root = {}\n",
                std::env::current_dir().unwrap().display()
            )),
            "{out}"
        );
    }

    #[test]
    fn config_set_writes_file_values_only() {
        let home = TempHome::new("set");

        assert_eq!(
            set_value(home.path(), "model.model", "local-llm").unwrap(),
            "local-llm"
        );
        assert_eq!(
            set_value(home.path(), "limits.max_steps", "7").unwrap(),
            "7"
        );

        // 读文件配置（不叠加环境变量）：两次修改都在，其余字段仍是默认值
        let saved = load_config_file(home.path()).unwrap();
        assert_eq!(saved.model.model, "local-llm");
        assert_eq!(saved.limits.max_steps, 7);
        assert_eq!(saved.model.base_url, ModelConfig::default().base_url);
        // `save_config` 的 0600 不能被破坏
        #[cfg(unix)]
        assert_eq!(mode(&home.path().join("config.json")), 0o600);
    }

    #[test]
    fn config_set_on_fresh_home_keeps_file_overrides_only() {
        let home = TempHome::new("fresh");
        assert!(!home.path().join("config.json").exists());

        assert_eq!(
            set_value(home.path(), "limits.max_steps", "7").unwrap(),
            "7"
        );

        // 文件里只有被点名的那一项：默认值（尤其 workspace_root）一个都没物化
        let raw = fs::read_to_string(home.path().join("config.json")).unwrap();
        let json: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json, json!({"limits": {"max_steps": 7}}), "{raw}");
        assert!(json.get("workspace_root").is_none(), "{raw}");
        assert!(json.get("default_kernel").is_none(), "{raw}");

        // 生效配置的 workspace_root 仍是默认（进程启动目录），没有被钉死在某个项目
        let effective = load_config(home.path()).unwrap();
        assert_eq!(effective.workspace_root, std::env::current_dir().unwrap());
        assert_eq!(effective.limits.max_steps, 7);
    }

    #[test]
    fn config_set_preserves_existing_file_keys() {
        let home = TempHome::new("preserve");
        fs::write(
            home.path().join("config.json"),
            r#"{
  "model": { "model": "file-model" },
  "api_key": "sk-file-secret",
  "limits": { "max_steps": 3 }
}"#,
        )
        .unwrap();

        set_value(home.path(), "model.base_url", "http://localhost:11434/v1").unwrap();

        // 无键丢失、无默认值补齐：改了 base_url，其余原样，没多出任何键
        let raw = fs::read_to_string(home.path().join("config.json")).unwrap();
        let json: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            json,
            json!({
                "model": {
                    "model": "file-model",
                    "base_url": "http://localhost:11434/v1",
                },
                "api_key": "sk-file-secret",
                "limits": { "max_steps": 3 },
            }),
            "{raw}"
        );
        #[cfg(unix)]
        assert_eq!(mode(&home.path().join("config.json")), 0o600);
    }

    #[test]
    fn config_set_refuses_to_overwrite_unparsable_file() {
        let home = TempHome::new("corrupt");
        let path = home.path().join("config.json");
        fs::write(&path, "{oops").unwrap();

        let error = set_value(home.path(), "limits.max_steps", "7")
            .unwrap_err()
            .to_string();

        assert!(error.contains("配置文件无法解析"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{oops");
    }

    #[test]
    fn config_set_redacts_key_echo() {
        let home = TempHome::new("secret");

        assert_eq!(
            set_value(home.path(), "api_key", "sk-test-secret").unwrap(),
            "***"
        );

        // 文件里确实写上了（0600），只是终端回显不得泄露
        let saved = load_config_file(home.path()).unwrap();
        assert_eq!(saved.api_key.as_deref(), Some("sk-test-secret"));
    }

    #[test]
    fn config_set_rejects_unknown_path_and_unparsable_value() {
        let home = TempHome::new("reject");

        // 白名单之外（含容器路径）一律拒绝
        for key in ["model.nope", "model", "limits", "nope"] {
            let error = set_value(home.path(), key, "x").unwrap_err().to_string();
            assert!(error.contains("未知配置项"), "{key} → {error}");
        }
        assert!(
            !home.path().join("config.json").exists(),
            "非法路径不该写出配置文件"
        );

        let error = set_value(home.path(), "limits.max_steps", "abc")
            .unwrap_err()
            .to_string();
        assert!(error.contains("取值不合法"), "{error}");
        let error = set_value(home.path(), "limits.max_steps", "-1")
            .unwrap_err()
            .to_string();
        assert!(error.contains("取值不合法"), "{error}");
    }
}
