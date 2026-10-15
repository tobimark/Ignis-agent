//! Shell 工具：在工作区内执行命令。
//!
//! `shell_run` 是 v0.1 里唯一会 spawn 进程的工具，因此「超时/取消必须真的杀掉进程」也落在
//! 这里：子进程自成进程组（`process_group(0)`，组号 == 子进程 pid），超时或
//! [`CancellationToken`] 触发时对整组 SIGKILL，`sh -c` 派生出的后台任务无法逃逸。
//! 密钥类环境变量（名匹配 `(?i)(_KEY|_TOKEN|_SECRET|_PASSWORD)$`）不进子进程，
//! 避免 `env`/`echo $…` 把中转密钥写回工具输出与日志。
//!
//! [`CancellationToken`]: tokio_util::sync::CancellationToken

use std::path::PathBuf;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use ignis_runtime::ToolSpec;
use regex::Regex;
use serde_json::{Value, json};
use tokio::process::Command;

use super::util::truncate_output;
use super::{Tool, ToolCtx, ToolError};
use crate::security::{SecurityError, resolve_in_workspace};

/// 密钥环境变量名黑名单（brief 钉死的模式）。
static SECRET_ENV_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(_KEY|_TOKEN|_SECRET|_PASSWORD)$").expect("密钥环境变量正则必须可编译")
});

/// 在工作区内执行一条 shell 命令。
pub(super) struct ShellRun;

#[async_trait]
impl Tool for ShellRun {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell_run".into(),
            description: "Run a non-interactive shell command (`sh -c`) inside the workspace with a timeout. The result starts with the exit code and merges stdout then stderr. Secret-looking environment variables are not passed to the command.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command line executed by `sh -c` inside the workspace"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory relative to the workspace root (default: the workspace root)"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
        let command = required_str(args, "command")?;
        let cwd = resolve_cwd(ctx, optional_str(args, "cwd")?)?;

        let mut builder = Command::new("sh");
        builder
            .arg("-c")
            .arg(command)
            .current_dir(&cwd)
            // 命令绝不能吃掉 REPL 的 stdin
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // 子进程自成进程组：组号 == 子进程 pid，超时/取消时对整组发信号
        #[cfg(unix)]
        builder.process_group(0);
        for (name, _) in std::env::vars() {
            if is_secret_env_name(&name) {
                builder.env_remove(name);
            }
        }

        let child = builder
            .spawn()
            .map_err(|err| ToolError::message(format!("启动命令失败：{command}：{err}")))?;
        // `wait_with_output` 会消费 child，收尸前先取出 pid（即进程组号）
        let pid = child
            .id()
            .ok_or_else(|| ToolError::message(format!("无法获取子进程 pid：{command}")))?;
        let wait = child.wait_with_output();
        tokio::pin!(wait);
        let output = tokio::select! {
            output = &mut wait => output,
            _ = tokio::time::sleep(Duration::from_secs(ctx.limits.shell_timeout_secs)) => {
                kill_process_group(pid);
                let _ = wait.await;
                return Err(ToolError::message(format!(
                    "命令超时（timeout {} 秒），已终止整个进程组：{command}",
                    ctx.limits.shell_timeout_secs
                )));
            }
            _ = ctx.cancel.cancelled() => {
                kill_process_group(pid);
                let _ = wait.await;
                return Err(ToolError::message(format!(
                    "命令已取消（cancelled），已终止整个进程组：{command}"
                )));
            }
        };
        let output =
            output.map_err(|err| ToolError::message(format!("执行命令失败：{command}：{err}")))?;
        Ok(format_output(&output, ctx.limits.tool_output_limit))
    }
}

/// 变量名是否命中密钥黑名单：`(?i)(_KEY|_TOKEN|_SECRET|_PASSWORD)$`。
fn is_secret_env_name(name: &str) -> bool {
    SECRET_ENV_PATTERN.is_match(name)
}

/// 对子进程所在进程组发 SIGKILL（负 pid 即「该组全部进程」）；进程已退出时失败无副作用。
fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    // SAFETY: pid 取自本进程刚 spawn 的子进程，`kill` 只发信号、不触碰内存，
    // 目标不存在或已退出时也只是返回错误码。
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid; // 非 Unix 没有进程组语义，仅由 `kill_on_drop(true)` 兜底
}

/// 结果头：正常退出给 `exit code: {n}`；被信号杀死时 [`ExitStatus::code`] 为 `None`，
/// 退化成等价的 `exit signal: {n}`，模型仍能判断命令为何中止。
fn status_header(status: &ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code: {code}");
    }
    match signal_of(status) {
        Some(signal) => format!("exit signal: {signal}"),
        None => "exit status: unknown".to_string(),
    }
}

/// 退出状态里的信号编号（非 Unix 平台拿不到）。
fn signal_of(status: &ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        std::os::unix::process::ExitStatusExt::signal(status)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// 合并 stdout、stderr（stdout 在前，必要时补一个换行）后按上限截断，
/// 前缀 `exit code: {n}` 必须活过截断（它在最前面）。
fn format_output(output: &Output, limit: usize) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut merged = stdout.into_owned();
    if !merged.is_empty() && !stderr.is_empty() && !merged.ends_with('\n') {
        merged.push('\n');
    }
    merged.push_str(&stderr);
    truncate_output(
        &format!("{}\n{merged}", status_header(&output.status)),
        limit,
    )
}

/// 解析 `cwd`：默认工作区根，其余相对工作区解析；越界与不可解析都转 [`ToolError`]。
fn resolve_cwd(ctx: &ToolCtx<'_>, cwd: Option<&str>) -> Result<PathBuf, ToolError> {
    let user_path = cwd.unwrap_or(".");
    resolve_in_workspace(ctx.workspace, user_path).map_err(|err| match &err {
        // 越界文案本身已含用户路径与工作区路径
        SecurityError::OutsideWorkspace { .. } => ToolError::message(err.to_string()),
        other @ (SecurityError::WorkspaceUnavailable { .. }
        | SecurityError::Unresolvable { .. }) => {
            ToolError::message(format!("cwd {user_path}：{other}"))
        }
    })
}

/// 取必填字符串参数；缺失或类型不对都给出可操作的错误。
fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    optional_str(args, key)?
        .ok_or_else(|| ToolError::message(format!("缺少必需的参数 `{key}`（字符串）")))
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| ToolError::message(format!("参数 `{key}` 必须是字符串"))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use ignis_runtime::Limits;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// 单个测试独占的临时目录，析构时清理（本 crate 不引入 tempfile 依赖）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-core-shell-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 临时工作区 + 运行上限；`limits` 与工作区同生命周期，供 `ToolCtx` 借用。
    struct Env {
        ws: TempDir,
        limits: Limits,
    }

    impl Env {
        fn new() -> Self {
            Self {
                ws: TempDir::new(),
                limits: Limits::default(),
            }
        }

        /// 只把 shell 超时压到 `secs` 秒（超时相关用例专用）。
        fn with_timeout(secs: u64) -> Self {
            let mut env = Self::new();
            env.limits.shell_timeout_secs = secs;
            env
        }

        fn path(&self) -> &Path {
            self.ws.path()
        }

        async fn run(&self, args: Value) -> Result<String, ToolError> {
            self.run_with_cancel(args, CancellationToken::new()).await
        }

        /// 用调用方给的令牌执行：取消路径要能在运行途中触发。
        async fn run_with_cancel(
            &self,
            args: Value,
            cancel: CancellationToken,
        ) -> Result<String, ToolError> {
            let ctx = ToolCtx {
                workspace: self.ws.path(),
                cancel,
                limits: &self.limits,
            };
            ShellRun.run(&args, &ctx).await
        }
    }

    /// `pgrep -f <pattern>` 命中的 pid 列表（空串即已没有任何进程匹配）。
    fn pgrep(pattern: &str) -> String {
        let output = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(pattern)
            .output()
            .expect("pgrep 应可用");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn runs_command_in_workspace() {
        let env = Env::new();
        let root = env.path().canonicalize().unwrap();

        // 默认 cwd 即工作区根
        let output = env.run(json!({"command": "pwd"})).await.unwrap();
        assert_eq!(output, format!("exit code: 0\n{}\n", root.display()));

        // cwd 相对工作区解析
        std::fs::create_dir(env.path().join("sub")).unwrap();
        let output = env
            .run(json!({"command": "pwd", "cwd": "sub"}))
            .await
            .unwrap();
        assert_eq!(
            output,
            format!("exit code: 0\n{}\n", root.join("sub").display())
        );
    }

    #[tokio::test]
    async fn kills_process_group_on_timeout() {
        let env = Env::with_timeout(1);
        let started = Instant::now();
        let err = env
            .run(json!({"command": "sleep 12345 & sleep 12346"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "1 秒超时应很快返回，实际 {:?}",
            started.elapsed()
        );
        // 后台任务与前台任务同属一个进程组：SIGKILL 打到组上，两个 sleep 都必须消失
        assert_eq!(pgrep("12345"), "", "后台任务逃出了进程组");
        assert_eq!(pgrep("12346"), "", "前台任务未被终止");
    }

    #[tokio::test]
    async fn kills_process_group_on_cancel() {
        let env = Env::new();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });

        let started = Instant::now();
        let err = env
            .run_with_cancel(json!({"command": "sleep 12347 & sleep 12348"}), cancel)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "取消应立即返回，实际 {:?}",
            started.elapsed()
        );
        // 取消同样按进程组终止：Task 11 的中止路径依赖这一点
        assert_eq!(pgrep("12347"), "", "后台任务逃出了进程组");
        assert_eq!(pgrep("12348"), "", "前台任务未被终止");
    }

    /// edition 2024 下 `set_var` 是 unsafe（多线程里改父进程环境本身不安全）；只在这一个
    /// 测试里写环境——真实路径必须先让父进程带上密钥变量，才能验证子进程不再继承它。
    #[tokio::test]
    async fn strips_secret_env_vars() {
        // SAFETY: 只写本测试自用的三个变量；本 crate 的子进程一律剔除匹配名，
        // 其余测试不读环境变量，观测不到这次写入。
        unsafe {
            std::env::set_var("FOO_KEY", "secret");
            std::env::set_var("ignis_lower_token", "lower-secret");
            std::env::set_var("IGNIS_PLAIN", "visible");
        }

        let env = Env::new();
        let output = env
            .run(json!({"command": r#"echo "[$FOO_KEY][$ignis_lower_token][$IGNIS_PLAIN]""#}))
            .await
            .unwrap();
        // `_KEY`/`_TOKEN` 后缀（大小写无关）被剔除，普通变量照常传入
        assert_eq!(output, "exit code: 0\n[][][visible]\n");
        assert!(!output.contains("secret"), "{output}");
    }

    /// 密钥后缀规则本身：`(?i)(_KEY|_TOKEN|_SECRET|_PASSWORD)$`。
    #[test]
    fn secret_env_name_filter_follows_the_pinned_pattern() {
        for name in [
            "FOO_KEY",
            "foo_key",
            "OPENAI_TOKEN",
            "_token",
            "X_SECRET",
            "MY_PASSWORD",
            "_KEY",
        ] {
            assert!(is_secret_env_name(name), "{name} 应被剔除");
        }
        for name in [
            "KEY",
            "KEYS",
            "FOO_KEYRING",
            "TOKENIZER",
            "PASSPHRASE",
            "IGNIS_PLAIN",
            "",
        ] {
            assert!(!is_secret_env_name(name), "{name} 不应被剔除");
        }
    }

    #[tokio::test]
    async fn truncates_noisy_output() {
        let env = Env::new();
        // 300KB 稳定输出：`yes` 被 head 关掉管道后自行退出，退出码取 head 的 0
        let output = env
            .run(json!({"command": "yes ignis | head -c 300000"}))
            .await
            .unwrap();
        assert!(output.starts_with("exit code: 0\n"), "{output}");
        let limit = env.limits.tool_output_limit;
        let marker = format!("\n… [truncated: output exceeded {limit} bytes]");
        assert_eq!(output.len(), limit + marker.len());
        assert!(output.ends_with(&marker), "{output}");
    }

    #[tokio::test]
    async fn reports_exit_code_and_merges_output() {
        let env = Env::new();
        let output = env
            .run(json!({"command": "echo out; echo err >&2; exit 3"}))
            .await
            .unwrap();
        // 非零退出码仍是 Ok（模型需要看到输出），stderr 接在 stdout 之后
        assert!(output.starts_with("exit code: 3\n"), "{output}");
        assert!(output.ends_with("out\nerr\n"), "{output}");
    }

    #[tokio::test]
    async fn reports_signal_death_without_an_exit_code() {
        let env = Env::new();
        // 被信号杀死时没有退出码：给出等价的信号编号，模型仍能判断命令为何中止
        let output = env.run(json!({"command": "kill -9 $$"})).await.unwrap();
        assert!(output.starts_with("exit signal: 9"), "{output}");
    }

    /// 命令绝不能吃掉 REPL 的 stdin：子进程 stdin 接 /dev/null，`read` 立即见到 EOF。
    #[tokio::test]
    async fn never_reads_the_parent_stdin() {
        let env = Env::new();
        let output = env
            .run(json!({"command": r#"read line; echo "[$line]""#}))
            .await
            .unwrap();
        assert_eq!(output, "exit code: 0\n[]\n");
    }

    #[tokio::test]
    async fn reports_missing_command_argument() {
        let env = Env::new();
        let err = env.run(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("command"), "{err}");
        let err = env.run(json!({"command": 3})).await.unwrap_err();
        assert!(err.to_string().contains("command"), "{err}");
    }

    #[tokio::test]
    async fn rejects_cwd_escape() {
        let env = Env::new();
        for bad in ["..", "/etc"] {
            let err = env
                .run(json!({"command": "pwd", "cwd": bad}))
                .await
                .unwrap_err();
            assert!(err.to_string().contains(bad), "cwd = {bad}：{err}");
        }

        // 越界 cwd 在解析阶段就被拒：命令根本不执行，工作区外不产生副作用
        let probe = format!("ignis-escaped-{}", std::process::id());
        let err = env
            .run(json!({"command": format!("mkdir {probe}"), "cwd": ".."}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
        assert!(
            !env.path().parent().unwrap().join(&probe).exists(),
            "{probe} 不该被创建"
        );
    }
}
