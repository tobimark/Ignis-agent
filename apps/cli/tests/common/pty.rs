//! pty 夹具：`libc::openpty` 拿一对 fd，把 `ignis` 二进制接到从端上，主端当终端模拟器使。
//!
//! 与 `common/mod.rs` 的其他助手同一条纪律：零新增依赖（std + `libc`）、绝不碰真实的 `~/.ignis`
//! （home/workspace 一律由用例的 `TempDir` 给）、不依赖网络（`IGNIS_BASE_URL` 指向用例里的
//! `ScriptedModel`）。只服务 REPL 的交互用例：别的 e2e 走 `ignis_cmd` 那条「stdin 不是 tty」的路。
//!
//! ## 从端配方（Step 1 用 `sh -c 'echo hi'` 实测选定）
//!
//! **三次 `Stdio::from(dup(slave))` + `pre_exec` 里 `setsid` + `TIOCSCTTY`**：
//!
//! 1. 父进程用 `OwnedFd::try_clone`（`F_DUPFD_CLOEXEC`）复制三份从端，分别交给子进程的
//!    stdin/stdout/stderr——std 的 `do_exec` 先 `dup2` 这三个 fd、再跑 `pre_exec` 闭包，
//!    所以闭包里 `STDIN_FILENO` 已经是从端；
//! 2. `pre_exec`（fork 之后、exec 之前）里先 `setsid()`：新会话 + 新进程组，子进程成为会话
//!    领头进程；再对 `STDIN_FILENO` `TIOCSCTTY`——从端只能是「没有控制终端的会话领头进程」
//!    的控制终端，这一步之后终端产生的 SIGINT（Ctrl+C）才会送到子进程而不是测试进程，
//!    空闲 Ctrl-C 退出的用例才测的是真信号；
//! 3. 父进程留住 `openpty` 给的原始从端 fd（不交给子进程）：用例退出后用它 `tcgetattr` 验
//!    「raw mode 已还原」。
//!
//! 备选配方（先按常规设好 stdio，再在 `pre_exec` 里 `libc::login_tty(slave)`）没有采用：
//! `login_tty` 自己会再 `dup2` 一遍 0/1/2（等于把 stdio 装配做两遍），链路多一层没有收益。
//!
//! ## 读法
//!
//! 主端放进 `Arc<File>`：读线程阻塞读它、用例往它写（模拟键盘与粘贴）。读线程把字节**原文**
//! 攒进 `Arc<Mutex<Vec<u8>>>`，`expect` 每 20ms 按字节复查一次（只查上一轮之后新增的一小段，
//! 代价不随累积输出膨胀）——输入行的回显与 CLI 的输出去向都是这一个从端，因此观察窗口是完整的
//! 一条流。

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `expect` 的总超时：够真二进制启动 + 跑完一轮假模型，又不至于把挂死的用例拖成几分钟。
pub const PTY_TIMEOUT: Duration = Duration::from_secs(10);

/// `expect` 的复查间隔（主线程只能轮询读线程攒下的字节）。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// `Drop` 里等被杀的子进程收尸的上限：SIGKILL 递出去后它几毫秒内就退，2 秒足够。
///
/// `Drop` 不能 panic（展开过程中 panic 会直接 abort），所以这里超时只是放弃收尸，不阻塞收尾。
const DROP_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// 从端的窗口列数：给得够宽，渲染器按列折行就不会把断言用的短行拆开。
///
/// 不设窗口尺寸的话 `ws_col` 初值是 0，`render::width_for_stdout` 会走默认宽度的回退路径——
/// 那不是真终端会发生的事，显式给一个宽窗口让渲染走真实路径。
const WINDOW_COLUMNS: u16 = 200;

/// 从端的窗口行数（`render` 只按列折行，行数只是让终端形态像真的）。
const WINDOW_ROWS: u16 = 50;

/// 一次 pty 用例的装配参数。
pub struct PtyOptions<'a> {
    /// `IGNIS_HOME`：临时目录里的 home（历史文件、任务库都落在它下面）。
    pub home: &'a Path,
    /// `IGNIS_WORKSPACE`：临时目录里的工作区（**必须已存在**，CLI 启动时会校验）。
    pub workspace: &'a Path,
    /// `IGNIS_BASE_URL`：用例里的 `ScriptedModel::base_url()`。
    pub base_url: &'a str,
    /// 传给二进制的参数；REPL 用例传空（无子命令 = 交互模式）。
    pub args: &'a [&'a str],
    /// 覆盖默认环境里的同名变量（例如 `TERM=dumb` 走 v0.1 回退路径）。
    pub env: &'a [(&'a str, &'a str)],
    /// 从端的窗口尺寸覆盖（`TIOCSWINSZ`）：`Some((cols, rows))` 时用它（过小终端的降级用例要它），
    /// `None` 保持现有的 200×50。
    pub window: Option<(u16, u16)>,
}

/// 一个跑在 pty 上的子进程 + 它的主端流。
pub struct Pty {
    /// 子进程：`Drop` 里 kill + wait，绝不留下跑飞的 ignis。
    child: Child,
    /// 主端：读线程与本进程写共用一份。
    master: Arc<File>,
    /// 从端（父进程这份）：子进程退出后验 termios 用。
    slave: Option<OwnedFd>,
    /// 读线程攒下的主端字节（原文，按需 lossy 解码）。
    output: Arc<Mutex<Vec<u8>>>,
}

impl Pty {
    /// 在 pty 上启动 `ignis`：默认环境见 [`Pty::spawn`] 里的注入，`options.env` 逐项覆盖。
    ///
    /// 启动失败直接 panic（用例失败），不给「没有 pty 就跳过」的软路径：这条路线是设计钉死的。
    pub fn spawn(options: &PtyOptions<'_>) -> Pty {
        // 先放一份空目录缓存：TUI 启动会刷新模型能力，测试不该依赖 models.dev 网络。
        seed_empty_model_catalog(options.home);
        let (master, slave) = openpty();
        set_window_size(master.as_raw_fd(), WINDOW_COLUMNS, WINDOW_ROWS);
        // 用例要的尺寸（过小终端的降级）：在子进程起来之前按从端设好；`None` 时上面那份 200×50
        // 一个字节都不动
        if let Some((columns, rows)) = options.window {
            set_window_size(slave.as_raw_fd(), columns, rows);
        }

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ignis"));
        cmd.args(options.args)
            .env("IGNIS_HOME", options.home)
            .env("IGNIS_WORKSPACE", options.workspace)
            .env("IGNIS_BASE_URL", options.base_url)
            .env("IGNIS_MODEL", "test-model")
            .env("IGNIS_API_KEY", "test-key")
            // 颜色与终端形态都钉死：断言只看文案，`PATH` 一律不动（子进程要能跑 npm 之类的工具）
            .env("NO_COLOR", "1")
            .env("TERM", "xterm-256color");
        for (key, value) in options.env {
            cmd.env(key, value);
        }
        // 三次 dup：std 只接管「传进去的那份」，openpty 给的原始从端留在父进程手里验 termios
        cmd.stdin(Stdio::from(dup_slave(&slave)))
            .stdout(Stdio::from(dup_slave(&slave)))
            .stderr(Stdio::from(dup_slave(&slave)));

        // SAFETY: 闭包在 fork 之后、exec 之前跑，只调 `setsid` / `ioctl` 两个异步信号安全的
        // syscall（连错误路径也只读 `errno`），不分配、不碰可能被 fork 卡住的锁；按上面的顺序，
        // `STDIN_FILENO` 此刻已经是本会话的从端，`setsid` 又保证子进程是没有控制终端的会话领头。
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn().expect("ignis 二进制应可在 pty 从端上启动");

        // `Arc<File>`：读线程阻塞读主端，用例往同一份 fd 写（pty 的两个方向互不干扰）
        let master = Arc::new(File::from(master));
        let output = Arc::new(Mutex::new(Vec::new()));
        {
            let master = Arc::clone(&master);
            let output = Arc::clone(&output);
            std::thread::spawn(move || {
                let mut file: &File = &master;
                let mut buf = [0u8; 4096];
                loop {
                    match file.read(&mut buf) {
                        // 从端全关（子进程退出 + 父进程这份也关掉）后主端读到 EOF，收工
                        Ok(0) => break,
                        Ok(n) => output
                            .lock()
                            .expect("主端输出锁不该中毒")
                            .extend_from_slice(&buf[..n]),
                        // EINTR 只是被打断，不是终态：一次杂散信号不该让读线程提前收工，
                        // 否则后面的 `expect` 会变成「流里什么都没有」的假超时。从端全关时
                        // 主端读到的是 EIO，不走这一支。
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        // 其余错误（从端全关时的 EIO 等）是终态，收工
                        Err(_) => break,
                    }
                }
            });
        }

        Pty {
            child,
            master,
            slave: Some(slave),
            output,
        }
    }

    /// 改变从端窗口尺寸（模拟用户拖拽窗口）：内核会给前台进程组发 SIGWINCH。
    pub fn resize(&self, columns: u16, rows: u16) {
        set_window_size(self.master.as_raw_fd(), columns, rows);
    }

    /// 往主端写一段字节并立刻冲刷：等同于在终端上按键或粘贴。
    ///
    /// 调用前用例应先等到一个只有 CLI 写出过才会出现的串（例如提示符）：在 raw mode 生效之前
    /// 打字会被行规范改写（`\r` 被 `ICRNL` 译成 `\n`，于是 Enter 变成「插换行」）。
    pub fn send(&mut self, bytes: &str) {
        let mut master: &File = &self.master;
        master.write_all(bytes.as_bytes()).expect("主端应可写");
        master.flush().expect("主端应可冲刷");
    }

    /// 等 `needle` 出现在主端流里：每 [`POLL_INTERVAL`] 复查一次，超过 [`PTY_TIMEOUT`] 就
    /// panic 并贴出已读到的全部输出（贴现场比只报超时好定位）。
    ///
    /// 轮询在锁内按字节查（不整段拷贝）：每轮只查上一次之后新增的一小段，起点往前退
    /// `needle` 长度减一，跨轮到达的串（前一轮只收到前半截）也不会漏；因此长等待的复查代价
    /// 不随累积输出膨胀。偏移只活在本次调用里，不做跨调用复用——换个 `needle` 时旧数据必须
    /// 重新扫，否则可能在旧区间里漏掉新串。命中判定先于超时判定，卡在边界上的串不会误判。
    pub fn expect(&mut self, needle: &str) {
        if needle.is_empty() {
            return; // 空串恒在，与 `str::contains("")` 同义
        }
        let needle_bytes = needle.as_bytes();
        let overlap = needle_bytes.len() - 1;
        let deadline = Instant::now() + PTY_TIMEOUT;
        // 本次调用已经查过的字节数
        let mut scanned: usize = 0;
        loop {
            let found = {
                let output = self.output.lock().expect("主端输出锁不该中毒");
                let from = scanned.saturating_sub(overlap);
                scanned = output.len();
                output[from..]
                    .windows(needle_bytes.len())
                    .any(|window| window == needle_bytes)
            };
            if found {
                return;
            }
            if Instant::now() >= deadline {
                // 只有失败路径整段拷贝（现场可定位优先）
                let output = self.output();
                panic!(
                    "等待 {needle:?} 超时（{}s 内没出现）：\n{output}",
                    PTY_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// 主端到目前为止收到的全部字节（lossy 解码）。
    ///
    /// 尾巴上刚好半截的多字节序列会显示成替换字符；`expect` 不看这条展示路径（它按字节查），
    /// 这里的解码只为失败时的现场可读。
    pub fn output(&self) -> String {
        let bytes = self.output.lock().expect("主端输出锁不该中毒").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// 等子进程退出并给出退出码；信号终止给 `-1`（用例只断言 `0`，非零一律是失败）。
    ///
    /// 有界轮询：`try_wait()` 每 [`POLL_INTERVAL`] 探一次，超过 [`PTY_TIMEOUT`] 就 panic 并
    /// 贴出已读输出——`Child::wait()` 会无限期阻塞，挂死的 REPL 只会把用例拖成几分钟的空等。
    pub fn wait(&mut self) -> i32 {
        match self.reap(PTY_TIMEOUT) {
            Ok(Some(code)) => code,
            Ok(None) => panic!(
                "等待子进程退出超时（{}s 内没退出）：\n{}",
                PTY_TIMEOUT.as_secs(),
                self.output()
            ),
            Err(error) => panic!("子进程状态应可查：{error}"),
        }
    }

    /// 从端的 `(ECHO, ICANON)`：`raw mode` 还原后两个都该回到置位状态。
    ///
    /// 读的是父进程手里那份从端 fd 的 termios——它与子进程读写的终端是同一个对象。
    pub fn echo_and_canonical(&self) -> (bool, bool) {
        let fd = self
            .slave
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .expect("从端在 Drop 之前一直由父进程持有");
        // SAFETY: `termios` 全是标量与定长数组，全零是合法初值；`tcgetattr` 成功时写满它，
        // 失败时返回非 0——此时直接失败，不读它写坏的内容。
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: 同上；`t` 是本函数独占的一块 termios。
        let rc = unsafe { libc::tcgetattr(fd, &mut t) };
        assert_eq!(rc, 0, "从端应可读 termios：{}", io::Error::last_os_error());
        (t.c_lflag & libc::ECHO != 0, t.c_lflag & libc::ICANON != 0)
    }

    /// 有界回收：`try_wait()` 每 [`POLL_INTERVAL`] 探一次，等满 `timeout`；拿到退出码给 `Some`
    /// （信号终止 `-1`），超时给 `None`。
    ///
    /// `Child::wait()` 没有超时，`wait()` 与 `Drop` 都走这条有界路径；std 会缓存 exit status，
    /// 重复调用拿到同一个码。
    fn reap(&mut self, timeout: Duration) -> io::Result<Option<i32>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status.code().unwrap_or(-1)));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for Pty {
    /// 收尾：kill + wait + 关掉父进程手里两份 fd。**绝不留下孤儿进程**。
    ///
    /// 读线程不 join（它阻塞在主端 `read` 上，只有从端全关才醒）：从端一关，它下一次读就得到
    /// EIO、自己退出；万一没醒，脱离线程随测试进程结束消失，不会把用例挂住。
    fn drop(&mut self) {
        let _ = self.child.kill();
        // 有界收尸：SIGKILL 已经递出去，正常几毫秒内就退；万一没退也不在 `Drop` 里挂住
        // （展开过程中 panic 会直接 abort），放弃收尸即可。
        let _ = self.reap(DROP_REAP_TIMEOUT);
        // 子进程已死，这份一关就再没人持有从端 → 主端的阻塞读拿到 EIO
        drop(self.slave.take());
        // `master` 字段随结构体析构，但读线程手里还有一份 `Arc`：那份随线程消失，
        // fd 由内核在进程退出时回收（测试进程里的几条 pty 不构成泄漏）
    }
}

/// 在测试 home 里放一份「刚拉取」的空模型目录缓存：避免测试启动时访问 models.dev。
///
/// 空目录不会命中任何模型，能力解析继续走内置 Registry；用例的断言都是终端行为，
/// 不依赖目录内容。真实运行会写同一份缓存（`~/.ignis/model_catalog.json`）。
fn seed_empty_model_catalog(home: &Path) {
    if std::fs::create_dir_all(home).is_err() {
        return;
    }
    let _ = ignis_runtime::catalog::ModelCatalog::write_cache(
        &home.join("model_catalog.json"),
        &ignis_runtime::catalog::ModelCatalog::empty(),
        ignis_runtime::catalog::unix_now(),
    );
}

/// 开一对 pty：主端（终端模拟器一侧）+ 从端（子进程看到的那个 tty）。
///
/// `termios` / `winsize` 传 NULL 表示用系统默认：从端初值是行规范 + 回显 + `ISIG` 的常规形态，
/// 正是「raw mode 有没有被还原」这条断言要的基线。
fn openpty() -> (OwnedFd, OwnedFd) {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // SAFETY: `openpty` 只往两个已初始化的出参里写 fd，其余两个参数传 NULL 表示用默认值；
    // 返回值非 0 表示失败，此时不读两个出参。
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty 失败：{}", io::Error::last_os_error());
    // SAFETY: 上一行 rc == 0，两个 fd 都是本进程刚拿到的所有权，交给 `OwnedFd` 收尾关闭。
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // `openpty` 给的原始 fd 不带 `FD_CLOEXEC`：不补的话子进程 fork + exec 后还会白白继承这两份
    // （子进程 exec 前就用不上了）。补在原始 fd 上不影响子进程的 stdio：三份 `Stdio` 走的是
    // `F_DUPFD_CLOEXEC` 的 dup，而 exec 前 `dup2` 到 0/1/2 会把目标 fd 的 `FD_CLOEXEC` 清掉。
    set_cloexec(&master);
    set_cloexec(&slave);
    (master, slave)
}

/// 给 `openpty` 出来的原始 fd 补上 `FD_CLOEXEC`（`F_GETFD` 读出来，置位后 `F_SETFD` 写回去）。
fn set_cloexec(fd: &OwnedFd) {
    let raw = fd.as_raw_fd();
    // SAFETY: `F_GETFD` 只读 `raw` 自己的 fd 标志，不碰内存。
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    assert_ne!(flags, -1, "F_GETFD 失败：{}", io::Error::last_os_error());
    // SAFETY: 同上；`F_SETFD` 只写 `raw` 的标志位，`flags | FD_CLOEXEC` 是它的合法入参。
    let rc = unsafe { libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    assert_eq!(rc, 0, "F_SETFD 失败：{}", io::Error::last_os_error());
}

/// 给从端定窗口尺寸（主端与从端共享同一份 winsize）。
fn set_window_size(fd: RawFd, columns: u16, rows: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `size` 是本函数独占的一块 `winsize`；`TIOCSWINSZ` 只读它，失败时返回非 0。
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
    assert_eq!(rc, 0, "TIOCSWINSZ 失败：{}", io::Error::last_os_error());
}

/// 复制一份从端（`F_DUPFD_CLOEXEC`：子进程 exec 后不留多余副本）。
fn dup_slave(slave: &OwnedFd) -> OwnedFd {
    slave.try_clone().expect("从端应可 dup")
}
