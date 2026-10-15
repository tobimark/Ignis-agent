//! 守护进程生命周期：自守护启动（`start`）、前台运行（`run_foreground`）、停止（`stop`）与
//! 状态探测（`status`）。
//!
//! 单实例靠 **pidfile 上的 `libc::flock` 独占非阻塞锁**：`{home}/ignis.pid` 由前台运行的那份
//! 进程持有到退出，`start`/`status`/`stop` 用「能不能拿到锁」判断「有没有在跑」——比只看 pid
//! 存活可靠（pid 会被复用），残留 pidfile 也不会被误判成运行中。
//!
//! `start` 是自守护：以 `setsid` 起一个脱离控制终端的新会话，re-exec 自己
//! `daemon run --foreground`，stdout/stderr 追加到 `{home}/logs/daemon.log`，环境原样继承
//! （`IGNIS_*` 覆盖因此跟着过去）；父进程等到 pidfile 里出现活着的 pid 才算成功。
//!
//! 停止是幂等的：`stop` 对 SIGTERM 之后的进程最多等 5 秒，仍不退出就升级 SIGKILL；本来就没在
//! 跑（锁空闲）时直接回 [`StopOutcome::NotRunning`]，不会对可能复用了同一 pid 的无关进程发信号。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ignis_core::alerts::{MultiNotifier, Notifier};
use ignis_core::config::{ensure_home, load_config};
use ignis_core::memory::MemoryStore;
use ignis_core::skills::SkillCatalog;
use ignis_runtime::Runtime;
use ignis_runtime::native::NativeRuntime;
use ignis_runtime::openai::OpenAiClient;
use ignis_storage::Db;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::r#loop::SchedulerLoop;
use crate::notify::{DesktopNotifier, LogNotifier};
use crate::{GatewayError, Result};

/// pidfile 名：`{home}/ignis.pid`。
const PID_FILE: &str = "ignis.pid";
/// 守护进程日志（自守护时 stdout/stderr 都追加到这里）：`{home}/logs/daemon.log`。
const DAEMON_LOG: &str = "daemon.log";
/// 告警的日志渠道（[`LogNotifier`]）：`{home}/logs/alerts.log`。
const ALERTS_LOG: &str = "alerts.log";
/// 数据目录下的日志目录名。
const LOG_DIR: &str = "logs";
/// `start` 等 pidfile 的上限（brief 钉死 2s）与轮询步长。
const START_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// pidfile 出现后再给子进程的稳定窗口：装配期（配置/密钥/库）立刻失败时不要报「已启动」。
const SETTLE: Duration = Duration::from_millis(100);
/// `stop` 等 SIGTERM 生效的上限（brief 钉死 5s），超时升级 SIGKILL。
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// `start` 的结果：成功只有一种——守护进程已经带着 pid 就绪。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// 守护进程已在后台运行（`pid` 来自 pidfile）。
    Started { pid: i32 },
}

/// `stop` 的结果：真停掉了一个进程，还是本来就没在跑（幂等）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// 向 `pid` 发过信号并等到它退出。
    Stopped { pid: i32 },
    /// 没有在跑的守护进程（pidfile 缺失、或锁空闲）。
    NotRunning,
}

/// `status` 的三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStatus {
    /// pidfile 的锁被持有 = 守护进程在跑。
    Running { pid: i32 },
    /// 没有 pidfile，也没有锁。
    Stopped,
    /// pidfile 还在，但锁已经空闲：进程死了没清理（下次 `start` 会覆盖它）。
    StalePidfile,
}

/// 守护进程 pidfile 路径：`{home}/ignis.pid`。
pub fn pid_file(home: &Path) -> PathBuf {
    home.join(PID_FILE)
}

/// `{home}/logs`。
fn log_dir(home: &Path) -> PathBuf {
    home.join(LOG_DIR)
}

/// 当前守护进程状态（只读探测，不会创建 pidfile）。
pub fn status(home: &Path) -> DaemonStatus {
    let path = pid_file(home);
    if !lock_is_free(&path) {
        // 锁被持有 = 守护进程活着；pid 读不出来只可能是「刚拿到锁、还没写 pid」的窗口
        return DaemonStatus::Running {
            pid: read_pid(&path).unwrap_or(0),
        };
    }
    if path.exists() {
        DaemonStatus::StalePidfile
    } else {
        DaemonStatus::Stopped
    }
}

/// 自守护启动：re-exec 自己 `daemon run --foreground`，等 pidfile 就绪后回 pid。
///
/// 已经在跑（pidfile 的锁被持有）时返回 [`GatewayError::AlreadyRunning`]（带 pid），不再起第二个。
pub fn start(home: &Path) -> Result<StartOutcome> {
    ensure_home(home)?;
    if let DaemonStatus::Running { pid } = status(home) {
        return Err(GatewayError::AlreadyRunning { pid });
    }

    let log_path = log_dir(home).join(DAEMON_LOG);
    fs::create_dir_all(log_dir(home)).map_err(|source| {
        GatewayError::io(
            format!("创建日志目录失败（{}）", log_dir(home).display()),
            source,
        )
    })?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|source| {
            GatewayError::io(
                format!("打开守护进程日志失败（{}）", log_path.display()),
                source,
            )
        })?;
    let stderr = stdout.try_clone().map_err(|source| {
        GatewayError::io(
            format!("复制守护进程日志句柄失败（{}）", log_path.display()),
            source,
        )
    })?;

    let exe = std::env::current_exe()
        .map_err(|source| GatewayError::io("无法定位自身的可执行文件", source))?;
    let mut command = Command::new(&exe);
    command
        .args(["daemon", "run", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // SAFETY: `pre_exec` 在 fork 之后、exec 之前运行，只调用 async-signal-safe 的 `setsid`；
    // 失败时把 errno 包成 io::Error 交回父进程（`spawn` 会报错）。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|source| {
        GatewayError::io(format!("拉起守护进程失败（{}）", exe.display()), source)
    })?;

    let pid_path = pid_file(home);
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(pid) = live_pid(&pid_path) {
            // pidfile 出现之后子进程仍可能因为装配失败（密钥、库、配置）立刻退出：给一个稳定窗口
            std::thread::sleep(SETTLE);
            if let Some(status) = child
                .try_wait()
                .map_err(|source| GatewayError::io("查看守护进程子进程状态失败", source))?
            {
                return Err(GatewayError::ChildExited {
                    status: status.to_string(),
                    log: log_path,
                });
            }
            return Ok(StartOutcome::Started { pid });
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|source| GatewayError::io("查看守护进程子进程状态失败", source))?
        {
            return Err(GatewayError::ChildExited {
                status: status.to_string(),
                log: log_path,
            });
        }
        if Instant::now() >= deadline {
            return Err(GatewayError::StartTimeout {
                secs: START_TIMEOUT.as_secs(),
                path: pid_path,
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 停止守护进程：SIGTERM → 最多等 5 秒 → 仍不退就 SIGKILL，最后清掉 pidfile。
///
/// 「在跑」以 pidfile 的锁为准（pid 可能被复用，锁空闲时绝不对无关进程发信号）；本来就没在跑时
/// 直接回 [`StopOutcome::NotRunning`]，调用方按幂等成功处理。
pub fn stop(home: &Path) -> Result<StopOutcome> {
    let path = pid_file(home);
    if lock_is_free(&path) {
        return Ok(StopOutcome::NotRunning);
    }
    let Some(pid) = read_pid(&path) else {
        return Err(GatewayError::BadPidfile { path });
    };

    match signal_process(pid, libc::SIGTERM) {
        Ok(()) => {
            if !wait_for_exit(pid, STOP_TIMEOUT) {
                // 升级：SIGKILL 不可阻塞，发完就收尾
                let _ = signal_process(pid, libc::SIGKILL);
            }
        }
        // 竞态：锁还没释放但进程已经没了（刚退出）
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
        Err(source) => {
            return Err(GatewayError::io(
                format!("向守护进程 pid {pid} 发送 SIGTERM 失败"),
                source,
            ));
        }
    }
    // 进程退出时自己会删 pidfile；这里兜底清理（SIGKILL 路径不会跑到 Drop）
    let _ = fs::remove_file(&path);
    Ok(StopOutcome::Stopped { pid })
}

/// 前台运行：抢锁写 pid → 装信号 → 装配调度循环 → 跑到取消。
///
/// 由 `ignis daemon run --foreground`（以及 `start` 的 re-exec）调用；`home` 取自 `IGNIS_HOME` /
/// `~/.ignis`。返回时 pidfile 一定被清掉（持锁的 [`PidFile`] 的 `Drop`）。
pub async fn run_foreground(home: &Path) -> Result<()> {
    ensure_home(home)?;
    fs::create_dir_all(log_dir(home)).map_err(|source| {
        GatewayError::io(
            format!("创建日志目录失败（{}）", log_dir(home).display()),
            source,
        )
    })?;

    // 单实例锁 + 写 pid：拿不到锁说明已经有守护进程（`start` 的前置探测之外的兜底）
    let pidfile = PidFile::acquire(&pid_file(home))?;
    println!(
        "daemon: ready (pid {}, home {})",
        std::process::id(),
        home.display()
    );

    // SIGTERM（`daemon stop` / systemd）与 SIGINT（前台 Ctrl-C）都走优雅退出
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|source| GatewayError::io("安装 SIGTERM 处理器失败", source))?;
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|source| GatewayError::io("安装 SIGINT 处理器失败", source))?;
    let cancel = CancellationToken::new();
    let watcher = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            tokio::select! {
                _ = sigterm.recv() => println!("daemon: received SIGTERM, shutting down"),
                _ = sigint.recv() => println!("daemon: received SIGINT, shutting down"),
            }
            cancel.cancel();
        }
    });

    // 装配在最靠后的位置：失败（缺密钥、库坏了）时持锁的 pidfile 由守卫清掉，错误原样返回
    // （自守护时写进 daemon 日志，`start` 的稳定窗口会把它报成启动失败）
    let result = match build_loop(home) {
        Ok(scheduler) => scheduler.run(cancel.clone()).await,
        Err(error) => Err(error),
    };
    cancel.cancel();
    watcher.abort();
    drop(pidfile);
    result
}

/// 装配调度循环：配置 → 密钥 → 库 → 内核 → 通知渠道 → 记忆/技能目录。
///
/// 与 CLI 的 `Session::new` 同一套口径（密钥优先取配置文件里的 `api_key`，其次配置指定的环境
/// 变量），这样守护进程与交互运行看到的是同一份模型配置。
fn build_loop(home: &Path) -> Result<SchedulerLoop> {
    let mut config = load_config(home)?;
    let env = config.model.api_key_env.clone();
    let api_key = config.api_key.clone().or_else(|| {
        std::env::var(&env)
            .ok()
            .filter(|key| !key.trim().is_empty())
    });
    let Some(api_key) = api_key else {
        return Err(GatewayError::MissingApiKey { env });
    };
    // 生效密钥回填进 config：每轮任务的 ToolRuntime 与脱敏器都从 `config.api_key` 取它
    config.api_key = Some(api_key.clone());

    let db = Arc::new(Db::open(&home.join("ignis.db"))?);
    let client = OpenAiClient::new(&config.model, api_key)
        .map_err(|error| GatewayError::Model(error.to_string()))?
        .with_max_retries(config.limits.max_retries);
    let runtime: Arc<dyn Runtime> = Arc::new(NativeRuntime::new(client));
    // 渠道：日志文件 + 桌面通知（`notify-send` 不在只警告一次后降级）
    let notifier: Arc<dyn Notifier> = Arc::new(MultiNotifier::new(vec![
        Arc::new(LogNotifier::new(log_dir(home).join(ALERTS_LOG))),
        Arc::new(DesktopNotifier::new()),
    ]));
    let memory = MemoryStore::new(Arc::clone(&db), home.join("memory"));
    let skills = SkillCatalog::load(&home.join("skills"));
    Ok(SchedulerLoop::new(
        db,
        Arc::new(config),
        runtime,
        notifier,
        memory,
        skills,
        log_dir(home),
    ))
}

/// 持锁的 pidfile：打开（必要时创建）→ 独占非阻塞 `flock` → 写入自己的 pid；析构删文件。
///
/// 锁随 `_lock` 字段一起释放；先删文件再释放锁也无妨——`status`/`stop` 判定「在跑」看的是锁。
struct PidFile {
    /// 持有 `flock` 的句柄：本结构体活着期间锁就被它持有（名字带下划线正是因为它只负责「一直
    /// 开着」，没有任何读取点）。
    _lock: File,
    path: PathBuf,
}

impl PidFile {
    fn acquire(path: &Path) -> Result<PidFile> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|source| {
                GatewayError::io(format!("打开 pidfile 失败（{}）", path.display()), source)
            })?;
        match flock_exclusive(&file) {
            Ok(()) => {}
            Err(error) if is_would_block(&error) => {
                return Err(GatewayError::AlreadyRunning {
                    pid: read_pid(path).unwrap_or(0),
                });
            }
            Err(source) => {
                return Err(GatewayError::io(
                    format!("锁定 pidfile 失败（{}）", path.display()),
                    source,
                ));
            }
        }
        file.set_len(0)
            .and_then(|()| (&file).write_all(format!("{}\n", std::process::id()).as_bytes()))
            .and_then(|()| file.sync_all())
            .map_err(|source| {
                GatewayError::io(format!("写 pidfile 失败（{}）", path.display()), source)
            })?;
        Ok(PidFile {
            _lock: file,
            path: path.to_path_buf(),
        })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        // 先摘文件再放锁：持锁的 `file` 字段随后析构时锁自然释放
        let _ = fs::remove_file(&self.path);
    }
}

/// 对已打开的 fd 加独占非阻塞 `flock`。
fn flock_exclusive(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: fd 来自仍然存活的 `File`；`flock` 只操作内核锁表，不触碰进程内存。
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 是否是「锁被占用」：不同平台把这一族错误映射成同一族错误码。
fn is_would_block(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock)
        || error.raw_os_error() == Some(libc::EWOULDBLOCK)
}

/// 探测 pidfile 上的独占锁是否空闲：文件不存在/打不开都算「没人在跑」。
///
/// 探针自己拿到的锁随 `file` 析构立刻释放，不会影响真正的持有者。
fn lock_is_free(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return true;
    };
    match flock_exclusive(&file) {
        Ok(()) => true,
        Err(error) => !is_would_block(&error),
    }
}

/// 读 pidfile 里的 pid；文件不在或内容不是正整数返回 `None`。
fn read_pid(path: &Path) -> Option<i32> {
    let mut raw = String::new();
    File::open(path).ok()?.read_to_string(&mut raw).ok()?;
    let pid: i32 = raw.trim().parse().ok()?;
    (pid > 0).then_some(pid)
}

/// pidfile 里的 pid，且该 pid 确实是活着的进程。
fn live_pid(path: &Path) -> Option<i32> {
    let pid = read_pid(path)?;
    process_alive(pid).then_some(pid)
}

/// `kill(pid, 0)` 的存活探测：`0` 表示在，`EPERM` 表示在（只是没权限发信号），`ESRCH` 表示没了。
fn process_alive(pid: i32) -> bool {
    // SAFETY: `kill(pid, 0)` 只做存在性与权限探测，不发送信号、不触碰内存。
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// 给进程发一个信号。
fn signal_process(pid: i32, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: `kill` 只对给定 pid 发信号，不触碰内存。
    if unsafe { libc::kill(pid, signal) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 轮询等进程退出；`timeout` 内没等到返回 `false`。
fn wait_for_exit(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// 单个测试独占的临时 home，析构时清理。
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-gateway-daemon-{}-{label}-{}",
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

    /// 三态：没有 pidfile → `Stopped`；残留 pidfile（锁空闲）→ `StalePidfile`；
    /// 有人持锁 → `Running { pid }`。锁的争夺在同一个进程里换一个 fd 同样成立。
    #[test]
    fn status_reports_stopped_stale_and_running() {
        let home = TempHome::new("status");
        let path = pid_file(home.path());

        // 没跑过：什么都没有
        assert_eq!(status(home.path()), DaemonStatus::Stopped);
        assert!(!path.exists(), "探测不该创建 pidfile");

        // 残留 pidfile（进程死了没清）：锁空闲 → StalePidfile
        fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        assert_eq!(status(home.path()), DaemonStatus::StalePidfile);

        // 有人持锁（模拟在跑的守护进程）：Running，pid 取自文件
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        flock_exclusive(&holder).unwrap();
        assert_eq!(
            status(home.path()),
            DaemonStatus::Running {
                pid: std::process::id() as i32
            }
        );

        // 锁释放（守护进程退出）→ 回到 Stopped/Stale 判定
        drop(holder);
        assert_eq!(status(home.path()), DaemonStatus::StalePidfile);
        fs::remove_file(&path).unwrap();
        assert_eq!(status(home.path()), DaemonStatus::Stopped);
    }

    /// pid 读写的边界：空文件、坏内容、非正整数都不算「活着的 pid」。
    #[test]
    fn read_pid_rejects_empty_and_invalid_content() {
        let home = TempHome::new("read-pid");
        let path = pid_file(home.path());

        assert_eq!(read_pid(&path), None, "文件不存在");
        fs::write(&path, "").unwrap();
        assert_eq!(read_pid(&path), None, "空文件（刚拿到锁还没写）");
        fs::write(&path, "not-a-pid\n").unwrap();
        assert_eq!(read_pid(&path), None);
        fs::write(&path, "0\n").unwrap();
        assert_eq!(read_pid(&path), None, "0 不是合法 pid");
        fs::write(&path, "-3\n").unwrap();
        assert_eq!(read_pid(&path), None);
        fs::write(&path, " 4242 \n").unwrap();
        assert_eq!(read_pid(&path), Some(4242), "首尾空白容忍");
    }

    /// 存活探测：自己一定活着；一个几乎不可能存在的 pid 一定死了。
    #[test]
    fn process_alive_sees_self_and_a_missing_pid() {
        assert!(process_alive(std::process::id() as i32));
        assert!(!process_alive(i32::MAX - 1));
    }
}
