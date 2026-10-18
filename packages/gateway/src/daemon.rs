//! 守护进程生命周期：自守护启动（`start`）、前台运行（`run_foreground`）、停止（`stop`）与
//! 状态探测（`status`）。
//!
//! 单实例靠 **pidfile 上的 `libc::flock` 独占非阻塞锁**：`{home}/ignis.pid` 由前台运行的那份
//! 进程持有到退出，`start`/`status`/`stop` 用「能不能拿到锁」判断「有没有在跑」——比只看 pid
//! 存活可靠（pid 会被复用），残留 pidfile 也不会被误判成运行中。
//!
//! `start` 是自守护：以 `setsid` 起一个脱离控制终端的新会话，re-exec 自己
//! `daemon run --foreground`，stdout/stderr 追加到 `{home}/logs/daemon.log`，环境原样继承
//! （`IGNIS_*` 覆盖因此跟着过去）；父进程等到 pidfile 里出现**本次拉起的那个子进程自己的 pid**
//! 才算成功——pidfile 里原有的活 pid 可能是复用来的无关进程，拿它报「已启动」是谎报。超时路径
//! 会把子进程杀干净（`kill` + `wait`）再报错，绝不留下无人管理的守护进程。
//!
//! 停止是幂等的：`stop` 对 SIGTERM 之后的进程最多等 [`STOP_TIMEOUT`]（12s，在调度循环 10s 的优雅
//! 收尾之上），仍不退出就升级 SIGKILL；本来就没在
//! 跑（锁空闲）时直接回 [`StopOutcome::NotRunning`]，不会对可能复用了同一 pid 的无关进程发信号；
//! 收尾只清「仍写着刚停掉那个 pid」的 pidfile，并发 `daemon start` 换上的新 pidfile 不动。
//!
//! 「锁被持有但 pid 读不出来」（文件被改坏、刚截断还没写完）只有一种口径：`status` 回
//! [`DaemonStatus::BadPidfile`]，`start`/`stop`/前台抢锁失败回 [`GatewayError::BadPidfile`]，
//! 措辞一致地点名 pidfile 不可读——不再各自编出 `Running { pid: 0 }` / `AlreadyRunning { pid: 0 }`。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
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
/// `stop` 等 SIGTERM 生效的上限，超时升级 SIGKILL。
///
/// 必须**大于**调度循环的 `SHUTDOWN_GRACE`（10s，见 [`crate::r#loop`]）：优雅退出的含义是「等在飞
/// 运行收尾」，`daemon stop` 若比它先到点，就会把「正在正常收尾的守护进程」SIGKILL 掉——在飞运行
/// 的任务行与运行行被留在 `running`（要靠下一次启动的清场收），长跑的运行白跑。12s 留了 2s 余量
/// （收尾轮询、进程退出与 pidfile 清理的时间）。这两个数是一对：改一个必须看另一个。
const STOP_TIMEOUT: Duration = Duration::from_secs(12);

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

/// `status` 的四种结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    /// pidfile 的锁被持有 = 守护进程在跑。
    Running { pid: i32 },
    /// 没有 pidfile，也没有锁。
    Stopped,
    /// pidfile 还在，但锁已经空闲：进程死了没清理（下次 `start` 会覆盖它）。
    StalePidfile,
    /// 锁被持有，但 pidfile 内容不是合法 pid（被改坏、写入未完成）：认不出是谁在跑。
    ///
    /// 这与 `start`/`stop` 的 [`GatewayError::BadPidfile`] 是同一个情形、同一套措辞：宁可说
    /// 「读不出来」，也不编一个 `pid: 0` 出来。
    BadPidfile { path: PathBuf },
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
        // 锁被持有 = 守护进程活着；pid 读不出来（刚拿到锁还没写完 / 文件被改坏）时统一报
        // BadPidfile，而不是谎报一个 `pid: 0`
        return match locked_pid(&path) {
            LockedPid::Known(pid) => DaemonStatus::Running { pid },
            LockedPid::Unreadable => DaemonStatus::BadPidfile { path },
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
/// 已经在跑时返回 [`GatewayError::AlreadyRunning`]（带 pid，不再起第二个）；锁被持有但 pid 读不
/// 出来时返回 [`GatewayError::BadPidfile`]——与 `status`/`stop` 同一个口径。
pub fn start(home: &Path) -> Result<StartOutcome> {
    ensure_home(home)?;
    let pid_path = pid_file(home);
    // 「锁被持有」的统一判定：pid 可读 → AlreadyRunning{pid}；不可读 → BadPidfile
    if !lock_is_free(&pid_path) {
        return Err(locked_pid(&pid_path).into_error(&pid_path));
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

    // `child.id()` 就是守护进程的 pid：直接 spawn/exec，中间没有包一层。就绪判定必须看到这个
    // pid——pidfile 里原有的活 pid（SIGKILL 之后被复用到无关进程上的）不是我们起的守护进程。
    let child_pid = child.id() as i32;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if is_ready(&pid_path, child_pid) {
            // pidfile 出现之后子进程仍可能因为装配失败（密钥、库、配置）立刻退出：给一个稳定窗口
            std::thread::sleep(SETTLE);
            if let Some(status) = poll_child(&mut child)? {
                return Err(GatewayError::ChildExited {
                    status: status.to_string(),
                    log: log_path,
                });
            }
            return Ok(StartOutcome::Started { pid: child_pid });
        }
        if let Some(status) = poll_child(&mut child)? {
            return Err(GatewayError::ChildExited {
                status: status.to_string(),
                log: log_path,
            });
        }
        if Instant::now() >= deadline {
            // 超时也要把子进程收干净：`Child` 的 `Drop` 只放手柄，不会杀进程，慢启动的守护进程
            // 会留在后台无人管理（`start` 已经报失败，谁都不知道它还在跑）
            reap_child(&mut child);
            return Err(GatewayError::StartTimeout {
                secs: START_TIMEOUT.as_secs(),
                path: pid_path,
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 停止守护进程：SIGTERM → 最多等 [`STOP_TIMEOUT`]（12s，在调度循环的 10s 优雅收尾之上）→ 仍不
/// 退就 SIGKILL，最后清掉 pidfile。
///
/// 「在跑」以 pidfile 的锁为准（pid 可能被复用，锁空闲时绝不对无关进程发信号）；本来就没在跑时
/// 直接回 [`StopOutcome::NotRunning`]，调用方按幂等成功处理。锁被持有但 pid 读不出来时回
/// [`GatewayError::BadPidfile`]（与 `status`/`start` 同一个口径），不会凭猜的 pid 发信号。
pub fn stop(home: &Path) -> Result<StopOutcome> {
    let path = pid_file(home);
    if lock_is_free(&path) {
        return Ok(StopOutcome::NotRunning);
    }
    let pid = match locked_pid(&path) {
        LockedPid::Known(pid) => pid,
        LockedPid::Unreadable => return Err(GatewayError::bad_pidfile(&path)),
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
    // 进程退出时自己会删 pidfile；这里兜底清理（SIGKILL 路径不会跑到 Drop），但只删「仍是刚停掉
    // 那个 pid」的文件——并发 `daemon start` 抢到锁换上的新 pidfile 是它的，抢删会让 `status`
    // 在守护进程活着时谎报「没在跑」
    clear_pidfile(&path, pid);
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
#[derive(Debug)]
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
                // 锁被持有：与 `start`/`status`/`stop` 同一个口径（pid 可读点名 pid，不可读报坏文件）
                return Err(locked_pid(path).into_error(path));
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

/// 「锁被持有」时 pidfile 的读数：能读出合法 pid，还是不可读（被改坏、刚截断还没写完）。
enum LockedPid {
    Known(i32),
    Unreadable,
}

impl LockedPid {
    /// 锁被持有导致的失败原因：pid 可读 → [`GatewayError::AlreadyRunning`]（点名 pid），
    /// 不可读 → [`GatewayError::BadPidfile`]（说清文件不可读/被改坏）。
    fn into_error(self, path: &Path) -> GatewayError {
        match self {
            LockedPid::Known(pid) => GatewayError::AlreadyRunning { pid },
            LockedPid::Unreadable => GatewayError::bad_pidfile(path),
        }
    }
}

/// 「锁被持有但 pid 读不出来」的唯一判定。
///
/// `status` / `start` / `stop` 以及前台抢锁失败（[`PidFile::acquire`]）都从这里取结论，不再各自
/// 编一个 `pid: 0` 之类的说法出来。
fn locked_pid(path: &Path) -> LockedPid {
    match read_pid(path) {
        Some(pid) => LockedPid::Known(pid),
        None => LockedPid::Unreadable,
    }
}

/// pidfile 里的活 pid，且必须**正是** `expected`（`start` 刚拉起的那个子进程）。
///
/// 只看「有个活 pid」会被 pid 复用骗过：上一次守护进程被 SIGKILL 后，同一 pid 落到无关进程身上，
/// `start` 就会拿它的 pid 报「已启动」，而守护进程其实没起来。
fn is_ready(path: &Path, expected: i32) -> bool {
    live_pid(path) == Some(expected)
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

/// `stop` 的兜底收尾：只在 pidfile 仍写着「刚停掉的那个 pid」时删它。
///
/// 并发 `daemon start` 可能在这段窗口里抢到锁、把文件换成新守护进程的 pid：那已经是它的 pidfile
/// （它自己退出时会摘），抢删会让 `status` 在新守护进程活着时谎报「没在跑」——它锁着的是一个
/// 已经不存在的路径，谁也看不见。
fn clear_pidfile(path: &Path, stopped_pid: i32) {
    if read_pid(path) == Some(stopped_pid) {
        let _ = fs::remove_file(path);
    }
}

/// 轮询子进程是否已经退出（`try_wait` 顺带收尸）。
///
/// 查询本身失败时也先把子进程收干净再报错：`start` 一旦返回错误，就不能再留一个没人管的守护进程。
fn poll_child(child: &mut Child) -> Result<Option<ExitStatus>> {
    match child.try_wait() {
        Ok(status) => Ok(status),
        Err(source) => {
            reap_child(child);
            Err(GatewayError::io("查看守护进程子进程状态失败", source))
        }
    }
}

/// 收尾一个还没起完的子进程：`kill()` + `wait()`。
///
/// `Child` 的 `Drop` 只关句柄、不杀进程，所以超时（以及上面查状态失败）的路径必须显式杀干净并
/// 收尸：否则「慢启动的守护进程」会留在后台无人管理，而 `start` 已经报了失败。
fn reap_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
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

    /// 锁相关等待点（拿到锁 / 读到终态）共用的 deadline：抖动只改等待方式，门槛与断言不动。
    const LOCK_WAIT: Duration = Duration::from_secs(5);

    /// 到 deadline 为止轮询：拿到 pidfile 上的独占锁为止。
    ///
    /// 「能不能拿到锁」在这个测试二进制里是个等待点：同进程并行跑的用例会 spawn 子进程，`fork`
    /// 之后、`exec` 之前子进程会短暂继承本进程打开着的 fd——`lock_is_free` 探针刚放掉的锁可能还
    /// 没在子进程里被 `O_CLOEXEC` 关掉，这时拿锁会瞬时拿到 `EWOULDBLOCK`（P2/P4 各记录过一次）。
    fn wait_for_lock(file: &File) {
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match flock_exclusive(file) {
                Ok(()) => return,
                Err(error) => assert!(
                    Instant::now() < deadline,
                    "等待 pidfile 的独占锁超时（{}s）：{error}",
                    LOCK_WAIT.as_secs()
                ),
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// 到 deadline 为止轮询 `status` 读到 `want` 为止（`drop(holder)` 之后的重读）。
    ///
    /// 锁的释放与「下一次读到的状态」之间同样隔着一个等待点（同上：被 fork 出去的子进程可能还
    /// 捏着继承来的那份 fd，锁要到它 `exec` 时才真正消失），故重读到终态再断言。
    fn wait_for_status(home: &Path, want: &DaemonStatus) {
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            let seen = status(home);
            if seen == *want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "等待状态 {want:?} 超时（{}s）：现在是 {seen:?}",
                LOCK_WAIT.as_secs()
            );
            std::thread::sleep(POLL_INTERVAL);
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

        // 有人持锁（模拟在跑的守护进程）：Running，pid 取自文件。
        // 拿锁这一步是等待点（见 `wait_for_lock`）：探针刚放掉的锁可能还没从 fork 出去的子进程里
        // 消失，有界轮询到拿到为止——断言逐字不变。
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        wait_for_lock(&holder);
        assert_eq!(
            status(home.path()),
            DaemonStatus::Running {
                pid: std::process::id() as i32
            }
        );
        // 同一个读数：前台抢锁失败也点名这个 pid（不是编出来的 `pid: 0`）
        let error = PidFile::acquire(&path).unwrap_err();
        assert!(
            matches!(error, GatewayError::AlreadyRunning { pid } if pid == std::process::id() as i32),
            "{error}"
        );

        // 锁释放（守护进程退出）→ 回到 Stopped/Stale 判定：锁的去与下一次读之间也是等待点
        // （被 fork 的子进程要到 exec 才交回继承来的 fd），有界轮询重读到终态再断言。
        drop(holder);
        wait_for_status(home.path(), &DaemonStatus::StalePidfile);
        assert_eq!(status(home.path()), DaemonStatus::StalePidfile);
        fs::remove_file(&path).unwrap();
        assert_eq!(status(home.path()), DaemonStatus::Stopped);
    }

    /// 「锁被持有但 pid 读不出来」只有一个口径：`status` 报 [`DaemonStatus::BadPidfile`]，
    /// `stop` 与前台抢锁失败报同一条 [`GatewayError::BadPidfile`]——不再各自编出
    /// `Running { pid: 0 }` / `AlreadyRunning { pid: 0 }` 与第三种说法。
    #[test]
    fn a_locked_but_unreadable_pidfile_is_one_state_everywhere() {
        let home = TempHome::new("bad-pidfile");
        let path = pid_file(home.path());
        // 被改坏（手工清空、写了一半）的 pidfile，同时有人持锁 = 有守护进程在跑但认不出是谁
        fs::write(&path, "not-a-pid\n").unwrap();
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        flock_exclusive(&holder).unwrap();

        assert_eq!(
            status(home.path()),
            DaemonStatus::BadPidfile { path: path.clone() },
            "status 应报 pidfile 不可读，而不是 Running {{ pid: 0 }}"
        );

        let error = stop(home.path()).unwrap_err();
        assert!(
            matches!(error, GatewayError::BadPidfile { .. }),
            "stop 应报同一个状态（不猜 pid、不发信号）：{error}"
        );
        // 措辞要能指导下一步：点名 pidfile、说清它坏在哪、让人检查/清理后再来
        let message = error.to_string();
        assert!(message.contains("pidfile"), "{message}");
        assert!(message.contains("合法 pid"), "{message}");
        assert!(
            message.contains(path.to_string_lossy().as_ref()),
            "{message}"
        );

        let error = PidFile::acquire(&path).unwrap_err();
        assert!(
            matches!(error, GatewayError::BadPidfile { .. }),
            "前台抢锁失败也走同一个状态：{error}"
        );
        assert_eq!(status(home.path()), DaemonStatus::BadPidfile { path });
    }

    /// `start` 的就绪判定不能被「pidfile 里有个活 pid」骗过：必须是刚拉起的那个子进程自己的 pid。
    /// 用 pid 1（一定活着）冒充「SIGKILL 之后复用来的无关进程」。
    #[test]
    fn readiness_requires_the_spawned_childs_own_pid() {
        let home = TempHome::new("readiness");
        let path = pid_file(home.path());
        let me = std::process::id() as i32;
        assert!(process_alive(1), "用例前提：pid 1 是活进程");

        fs::write(&path, "1\n").unwrap();
        assert!(
            !is_ready(&path, me),
            "pidfile 里别人的活 pid 不算就绪（老实现会拿它谎报「已启动」）"
        );

        fs::write(&path, format!("{me}\n")).unwrap();
        assert!(is_ready(&path, me), "子进程自己写下的 pid 才算就绪");
    }

    /// `stop` 的兜底清理不能动「别人的」pidfile：并发 `daemon start` 抢到锁、把文件换成新守护进程
    /// 的 pid 时，抢删会让 `status` 在新守护进程活着时谎报「没在跑」。
    #[test]
    fn stop_cleanup_spares_a_newer_daemons_pidfile() {
        let home = TempHome::new("stop-cleanup");
        let path = pid_file(home.path());
        // 老守护进程（`stop` 刚 SIGTERM 掉的那个）的 pid：用一个「肯定不是本进程」的假 pid，
        // 才能把「我们停掉的 pid」与「新守护进程的 pid」区分开
        let stopped = 4242;

        // 并发的 `daemon start` 赢了锁：新守护进程写下自己的 pid 并持有它（真的 PidFile 句柄）
        let new_daemon = PidFile::acquire(&path).unwrap();
        clear_pidfile(&path, stopped);
        assert!(path.exists(), "新守护进程的 pidfile 不能被老 stop 抢删");
        assert_eq!(read_pid(&path), Some(std::process::id() as i32));

        // 反过来：文件仍是刚停掉的那个 pid（没有新守护进程接手）时，兜底清理照旧
        drop(new_daemon); // 新守护进程退出时自己摘文件
        assert!(!path.exists(), "PidFile 的 Drop 会摘掉文件");
        fs::write(&path, format!("{stopped}\n")).unwrap();
        clear_pidfile(&path, stopped);
        assert!(!path.exists(), "仍写着刚停掉的 pid 时兜底删除照旧");
    }

    /// 超时收尾 `reap_child`：起一个长睡的子进程，收尾之后它必须真的没了（SIGKILL + 收尸）——
    /// `start` 超时绝不能把慢启动的守护进程留在后台无人管理。僵尸进程在 `kill(pid, 0)` 眼里仍是
    /// 「活着」，所以这条断言同时证明「杀了」和「收了」。
    #[test]
    fn reap_child_kills_and_reaps_the_child() {
        let mut child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let pid = child.id() as i32;
        assert!(process_alive(pid), "用例前提：子进程刚起来还活着");

        reap_child(&mut child);

        assert!(!process_alive(pid), "收尾之后不该再有这个进程（含僵尸）");
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
