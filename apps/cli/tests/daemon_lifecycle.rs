//! `ignis daemon start|stop|status` 的真实进程生命周期。
//!
//! 用 `CARGO_BIN_EXE_ignis` 拉起真二进制：自守护（`setsid` + re-exec）→ pidfile/flock → SIGTERM
//! 优雅退出都是生产路径。库侧的 `status`/`stop` 用来做类型化断言（CLI 输出只做粗断言，T6 会再
//! 打磨文案），进程与临时 home 都由用例自己收干净——失败时也不留后台守护进程与 pidfile。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use ignis_gateway::daemon::{DaemonStatus, status, stop};

/// 单个用例独占的临时目录，析构时清理。
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ignis-daemon-{}-{label}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("临时目录应可创建");
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

/// 用例结束（含断言失败/panic）时把守护进程收掉：`stop` 自身会走 SIGTERM → 5s → SIGKILL。
struct StopDaemon(PathBuf);

impl Drop for StopDaemon {
    fn drop(&mut self) {
        let _ = stop(&self.0);
    }
}

/// 跑一次 `ignis`：home 与工作区都钉在临时目录里，密钥走环境变量，stdin 是非 tty。
fn ignis(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ignis"))
        .args(args)
        .env("IGNIS_HOME", home)
        .env("IGNIS_WORKSPACE", home)
        .env("IGNIS_API_KEY", "test-key")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("ignis 二进制应可执行")
}

/// 进程输出汇总（断言失败时贴进消息里便于定位）。
fn text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// stdout 文本。
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// 从 `daemon started (pid N)` 里取出 N（start 的文案是计划钉死的）。
fn started_pid(output: &Output) -> i32 {
    let out = stdout(output);
    let pid = out
        .split("daemon started (pid ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_else(|| panic!("start 输出里没有 pid：{out}"));
    pid.trim().parse().expect("pid 应是数字")
}

/// 全流程：start（自守护）→ status Running → 二次 start 报错并点名 pid → stop → status Stopped
/// → 重复 stop 友好返回 0。
#[test]
fn daemon_lifecycle_start_status_stop() {
    let dir = TempDir::new("lifecycle");
    let home = dir.path().join("home");

    // 1) start：等 pidfile 就绪，父进程回 `daemon started (pid N)`
    let started = ignis(&home, &["daemon", "start"]);
    assert!(started.status.success(), "start 应成功：{}", text(&started));
    // 守护进程可能已经起来了：后面任何一步失败都要把它收掉
    let _guard = StopDaemon(home.clone());
    let pid = started_pid(&started);
    assert!(pid > 0);
    // pidfile / 日志都落在 home 下
    assert!(home.join("ignis.pid").is_file(), "pidfile 应存在");
    assert!(home.join("logs/daemon.log").is_file(), "daemon 日志应存在");
    // 库侧状态：Running { pid }（flock 探测，跨进程可见）
    assert_eq!(status(&home), DaemonStatus::Running { pid });

    // 2) status 子命令：报在跑并点名 pid
    let reported = ignis(&home, &["daemon", "status"]);
    assert!(reported.status.success(), "{}", text(&reported));
    let out = stdout(&reported);
    assert!(out.contains("running"), "status 应报在跑：{out}");
    assert!(out.contains(&pid.to_string()), "status 应点名 pid：{out}");

    // 3) 二次 start：必须失败，且错误里点名第一个守护进程的 pid
    let second = ignis(&home, &["daemon", "start"]);
    assert!(
        !second.status.success(),
        "已经有守护进程时 start 必须失败：{}",
        text(&second)
    );
    let err = String::from_utf8_lossy(&second.stderr).into_owned();
    assert!(
        err.starts_with("error: "),
        "错误应走 `error: {{人话}}`：{err}"
    );
    assert!(
        err.contains(&pid.to_string()),
        "错误里必须点名 pid {pid}：{err}"
    );
    // 第二个进程没有真的起来：还是原来那个 pid
    assert_eq!(status(&home), DaemonStatus::Running { pid });

    // 4) stop：SIGTERM 优雅退出，pidfile 清掉
    let stopped = ignis(&home, &["daemon", "stop"]);
    assert!(stopped.status.success(), "stop 应成功：{}", text(&stopped));
    assert!(
        stdout(&stopped).contains(&pid.to_string()),
        "stop 应点名 pid：{}",
        stdout(&stopped)
    );
    assert_eq!(status(&home), DaemonStatus::Stopped);
    assert!(!home.join("ignis.pid").exists(), "pidfile 应被清掉");

    // 5) 重复 stop：幂等、友好、退出 0
    let again = ignis(&home, &["daemon", "stop"]);
    assert!(
        again.status.success(),
        "没在跑时 stop 必须退出 0：{}",
        text(&again)
    );
    assert!(
        stdout(&again).contains("not running"),
        "应提示没在跑：{}",
        stdout(&again)
    );
    assert_eq!(status(&home), DaemonStatus::Stopped);
}

/// 没跑过的时候 `status` 是 Stopped（不创建 pidfile，也不报错）。
#[test]
fn daemon_status_without_pidfile_is_stopped() {
    let dir = TempDir::new("status-stopped");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let reported = ignis(&home, &["daemon", "status"]);

    assert!(reported.status.success(), "{}", text(&reported));
    assert!(
        stdout(&reported).contains("not running"),
        "应提示没在跑：{}",
        stdout(&reported)
    );
    assert!(!home.join("ignis.pid").exists(), "status 不该创建 pidfile");
    assert_eq!(status(&home), DaemonStatus::Stopped);
}
