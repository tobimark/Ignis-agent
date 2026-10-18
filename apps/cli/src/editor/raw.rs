//! raw mode 守卫：把终端从「行规范 + 回显」切到「按字节读」，并负责原样还原。
//!
//! 这个模块只做终端层的进出，不解读按键、不持有编辑状态、不画屏幕：
//! - [`RawMode::enable`] / [`RawMode::enable_on`] 进入 raw mode 并开 bracketed paste，
//!   返回的守卫在 `Drop` 里恢复进入前的 termios 并关掉 bracketed paste；
//! - [`RawMode::supported`] 只做「这台终端能不能承载转义重绘」的字符串判定，
//!   调用方据此在进不去 raw mode 时回退 v0.1 的按行输入；
//! - [`restore_terminal`] 是退出路径的兜底：守卫活在「读取区间」里，而读取跑在脱离线程上，
//!   退出时那条线程可能仍握着守卫阻塞在 `read` 里、进程退出不会析构它——REPL 退出前按
//!   这里记录下的现场做一次 best-effort 还原：复原 termios **并**补上 bracketed paste 的关闭
//!   序列（`Drop` 永远跑不到时，那 6 个字节只能由它写；没有活跃守卫时是无操作）。
//!
//! 两条刻意保留的语义：
//! - **`ISIG` 不动**：Ctrl+C 仍然是 SIGINT，空闲退出与任务取消的 watcher 都挂在信号上，
//!   不能改成「编辑器自己解释的字节」；
//! - **`OPOST` 不动**：输出后处理照旧，重绘自己写显式 `\r\n`，不靠终端帮忙。

use std::ffi::OsStr;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// bracketed paste 开/关（DECSET / DECRST 2004）：开了之后终端会用
/// `\x1b[200~` ... `\x1b[201~` 把整段粘贴包起来，区间的换行不会被当成提交。
const PASTE_ENABLE: &[u8] = b"\x1b[?2004h";
const PASTE_DISABLE: &[u8] = b"\x1b[?2004l";

/// 进程里「当前活跃守卫」的现场：读取发生在 `repl::next_line` 起的脱离线程里，退出路径拿不到
/// 那份守卫，只能按这条记录还原终端（见 [`restore_terminal`]）。
static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

/// 守卫身份序号：`Drop` 只清掉属于自己的那条记录（旧守卫不误伤新守卫）。
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// 活跃守卫的现场：目标 fd 与进入前的 termios。
struct Active {
    /// 认领用的身份序号（见 [`NEXT_ID`]）。
    id: u64,
    /// 进入 raw mode 的那个 fd。
    fd: i32,
    /// 进入前的 termios。
    original: libc::termios,
}

/// raw mode 的 RAII 守卫：活着的期间终端处于编辑器要求的形态，析构时还原。
///
/// 守卫由「读取区间」持有（`EditorLineReader::read_line` 里进出，见 `editor::reader`），
/// 不放进共享状态、也不由 REPL 主循环跨读取持有：任务执行与交互审批期间终端必须回到
/// canonical/echo，否则审批提示的按行读等不到回车、输入也不回显。
pub struct RawMode {
    /// 身份序号：`Drop` 只认领自己的那条全局记录。
    id: u64,
    /// 进入 raw mode 的那个 fd（stdin）。
    fd: i32,
    /// 进入前的 termios，`Drop` 时原样写回。
    original: libc::termios,
}

impl RawMode {
    /// 对 stdin（fd 0）进入 raw mode。
    pub fn enable() -> io::Result<RawMode> {
        RawMode::enable_on(libc::STDIN_FILENO)
    }

    /// 对 `fd` 进入 raw mode：读回当前 termios → 改成编辑器形态 → 立刻生效 → 开 bracketed paste。
    ///
    /// `fd` 不是 tty（管道、文件、已关闭）或任何一步 termios 调用报错时返回 `Err`，
    /// 由调用方回退按行输入——这里不 panic、不静默降级。
    pub fn enable_on(fd: i32) -> io::Result<RawMode> {
        // SAFETY: `termios` 全是标量与定长数组，全零是合法初值；随后 `tcgetattr` 会写满它。
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `original` 是本函数独占的一块 termios；`tcgetattr` 成功时写满它，
        // 失败时返回非 0，此时直接上抛错误、不读它的内容。
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let mut raw = original;
        apply_raw(&mut raw);
        // SAFETY: `raw` 是本函数独占的一份 termios，`tcsetattr` 只读它；`TCSANOW` 表示立刻生效
        // （不等输出的排空），失败时返回非 0。
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut active) = ACTIVE.lock() {
            *active = Some(Active { id, fd, original });
        }
        write_bytes(fd, PASTE_ENABLE);
        Ok(RawMode { id, fd, original })
    }

    /// 终端能否承载这套编辑器：`TERM` 缺失、为空或 `dumb` 时回退按行输入
    /// （哑终端上转义重绘会把屏幕打花，宁可少功能）；别的取值一律放行。
    pub fn supported(term: Option<&OsStr>) -> bool {
        // 非 UTF-8 的 TERM 取值不可信，同样按不可用处理（回退总是安全的那个方向）。
        term.and_then(OsStr::to_str)
            .is_some_and(|term| !term.is_empty() && term != "dumb")
    }
}

impl Drop for RawMode {
    /// 恢复进入时的 termios 并关掉 bracketed paste，全部 best-effort：
    /// 这里的失败只影响终端观感，绝不能从 `Drop` 里 panic，也不能盖掉更早的错误。
    fn drop(&mut self) {
        // SAFETY: `self.original` 由本结构独占，`tcsetattr` 只读它。
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
        write_bytes(self.fd, PASTE_DISABLE);
        // 只清掉属于自己的那条记录：锁中毒时跳过（还原已经按 `self` 做过了）
        if let Ok(mut active) = ACTIVE.lock()
            && active.as_ref().is_some_and(|entry| entry.id == self.id)
        {
            *active = None;
        }
    }
}

/// 退出路径的 best-effort 还原：除按 [`ACTIVE`] 记录复原 termios 外，**还要关掉 bracketed
/// paste**（`\x1b[?2004l`）。空闲 Ctrl+C 退出时读取线程仍握着守卫阻塞在 `read` 里，进程退出不会
/// 析构它，`Drop` 的关闭序列因此永远不会写出——用户的 shell 会残留粘贴模式。
///
/// 没有活跃记录时不写任何字节（既有的「无操作」语义保留）；锁中毒一律不动终端。
///
/// 由 REPL 在退出前调用（见 `repl::run`）——「退出时终端必须回到行规范 + 回显」的兜底。
/// 锁中毒等任何异常都不 panic：退出路径只许做减法。
pub fn restore_terminal() {
    let Ok(active) = ACTIVE.lock() else {
        return; // 锁中毒：记录不可信，宁可不动终端
    };
    if let Some(entry) = active.as_ref() {
        // SAFETY: `entry.original` 由这条记录独占，`tcsetattr` 只读它。
        let _ = unsafe { libc::tcsetattr(entry.fd, libc::TCSANOW, &entry.original) };
        // 守卫的 `Drop` 跑不到（读取线程握着它、进程不会析构它）：关闭序列在这里补上，
        // 用户的 shell 不会残留粘贴模式。没有活跃记录时走不到这一行——零字节。
        write_bytes(entry.fd, PASTE_DISABLE);
    }
}

/// 把一份 termios 改成编辑器要求的形态（保留 `ISIG` 与 `OPOST`）。
///
/// - `ICANON` 关：不在行尾才交付，读到几个字节就回来，编辑器自己攒多行；
/// - `ECHO` 关：回显由重绘负责，不然每个按键都会多打一遍；
/// - `ICRNL` 关：回车不再被译成换行（Enter 与 Ctrl+J 是两件事，见设计 §6.4）；
/// - `IXON` 关：Ctrl+S / Ctrl+Q 不再冻结输入流；
/// - `VMIN=1`、`VTIME=0`：有字节就返回，不做读超时。
fn apply_raw(t: &mut libc::termios) {
    t.c_lflag &= !(libc::ICANON | libc::ECHO);
    t.c_iflag &= !(libc::ICRNL | libc::IXON);
    t.c_cc[libc::VMIN] = 1;
    t.c_cc[libc::VTIME] = 0;
}

/// 往 `fd` 写一段字节，best-effort：调用方不靠它的成败判断任何事。
fn write_bytes(fd: i32, bytes: &[u8]) {
    // SAFETY: 指针与长度都来自同一块 `bytes`，`write` 只读这段内存，不持有它。
    let _ = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::{Mutex, MutexGuard};

    use super::{RawMode, apply_raw};

    /// 触碰进程级记录的用例必须串行：`ACTIVE` 是进程全局的，而同一个测试二进制里的用例默认
    /// 并行跑——并行会让「把守卫 forget 掉再 restore」这类用例的现场被别的用例覆盖。
    static SLOT_LOCK: Mutex<()> = Mutex::new(());

    /// 拿串行锁；锁中毒（前一个用例 panic 过）也继续，不让失败级联成第二个 panic。
    fn slot_guard() -> MutexGuard<'static, ()> {
        SLOT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 一份「典型」终端 termios：行规范、回显、ISIG、ICRNL、IXON、OPOST 都置位，
    /// `VMIN`/`VTIME` 给非默认值（好看出 `apply_raw` 是否真的动过它们）。
    fn typical_termios() -> libc::termios {
        // SAFETY: `termios` 全是标量与定长数组，全零是合法初值；下面逐项显式置位。
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        t.c_lflag = libc::ICANON | libc::ECHO | libc::ISIG;
        t.c_iflag = libc::ICRNL | libc::IXON;
        t.c_oflag = libc::OPOST;
        t.c_cc[libc::VMIN] = 12;
        t.c_cc[libc::VTIME] = 7;
        t
    }

    /// 读一份 termios；失败即测试失败。
    fn get_termios(fd: RawFd) -> libc::termios {
        // SAFETY: `t` 是本函数独占的出参；`tcgetattr` 成功时写满它，失败时返回非 0。
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::tcgetattr(fd, &mut t) };
        assert_eq!(rc, 0, "tcgetattr 失败：{}", std::io::Error::last_os_error());
        t
    }

    /// 开一对 pty（主端 = 终端模拟器一侧，从端 = 程序看的那个 tty）。
    fn open_pty() -> (OwnedFd, OwnedFd) {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        // SAFETY: `openpty` 只往两个已初始化的出参里写；返回值非 0 表示失败，此时不读它们。
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0, "openpty 失败：{}", std::io::Error::last_os_error());
        // SAFETY: 上面 rc == 0，两个 fd 都是本进程新拿到的所有权，交给 `OwnedFd` 收尾关闭。
        unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
    }

    /// 非阻塞地读走主端上现有全部字节（守卫往终端写的转义序列落在这里）。
    fn read_master(master: &OwnedFd) -> Vec<u8> {
        let fd = master.as_raw_fd();
        // SAFETY: `fcntl` 只读写该 fd 的标志位；`F_SETFL` 的第三个参数是 int。
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(
            flags >= 0,
            "F_GETFL 失败：{}",
            std::io::Error::last_os_error()
        );
        // 非阻塞：万一守卫什么都没写，这里只是读到空，而不是把测试挂死。
        // SAFETY: 同上，`flags | O_NONBLOCK` 是合法的标志位组合。
        let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        assert_eq!(rc, 0, "F_SETFL 失败：{}", std::io::Error::last_os_error());

        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: `buf` 是本函数独占的缓冲区，`read` 至多写 `buf.len()` 字节并按返回值告知长度。
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break; // 读空（EAGAIN）或出错都到此为止
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        out
    }

    #[test]
    fn supported_falls_back_on_dumb_or_missing_term() {
        // 哑终端上转义重绘会把屏幕打花，宁可回退 v0.1 行模式。
        assert!(!RawMode::supported(None), "TERM 缺失必须判为不可用");
        assert!(
            !RawMode::supported(Some(OsStr::new("dumb"))),
            "TERM=dumb 必须判为不可用"
        );
        assert!(
            !RawMode::supported(Some(OsStr::new(""))),
            "空 TERM 必须判为不可用"
        );
        // 正常终端照旧可用。
        assert!(RawMode::supported(Some(OsStr::new("xterm-256color"))));
        assert!(RawMode::supported(Some(OsStr::new("screen"))));
        assert!(RawMode::supported(Some(OsStr::new("tmux-256color"))));
    }

    #[test]
    fn apply_raw_keeps_sigint_and_drops_line_discipline() {
        let mut t = typical_termios();
        apply_raw(&mut t);

        assert_eq!(
            t.c_lflag & libc::ICANON,
            0,
            "ICANON 必须关掉（行规范交给编辑器）"
        );
        assert_eq!(t.c_lflag & libc::ECHO, 0, "ECHO 必须关掉（回显由重绘负责）");
        assert_ne!(
            t.c_lflag & libc::ISIG,
            0,
            "ISIG 必须保留：Ctrl+C 仍是 SIGINT（空闲退出与任务取消 watcher 挂在信号上）"
        );
        assert_eq!(
            t.c_iflag & libc::ICRNL,
            0,
            "ICRNL 必须关掉（回车不再被译成换行）"
        );
        assert_eq!(
            t.c_iflag & libc::IXON,
            0,
            "IXON 必须关掉（Ctrl+S/Ctrl+Q 不该冻住输入流）"
        );
        assert_eq!(
            t.c_oflag & libc::OPOST,
            libc::OPOST,
            "OPOST 不动（重绘自己写显式 \\r\\n）"
        );
        assert_eq!(t.c_cc[libc::VMIN], 1, "VMIN=1：一读到字节就返回");
        assert_eq!(t.c_cc[libc::VTIME], 0, "VTIME=0：不做读超时");
    }

    #[test]
    fn enable_on_then_drop_restores_the_original_termios() {
        let _serial = slot_guard();
        let (master, slave) = open_pty();
        let slave_fd = slave.as_raw_fd();
        let baseline = get_termios(slave_fd);
        assert_ne!(
            baseline.c_lflag & libc::ISIG,
            0,
            "pty 从端默认开着 ISIG（本用例的基线假设；不成立说明夹具变了）"
        );

        {
            let _guard =
                RawMode::enable_on(slave_fd).expect("pty 从端是真 tty，进入 raw mode 必须成功");
            let raw = get_termios(slave_fd);
            assert_eq!(raw.c_lflag & libc::ECHO, 0, "raw 期间 ECHO 必须为 0");
            assert_eq!(raw.c_lflag & libc::ICANON, 0, "raw 期间 ICANON 必须为 0");
            assert_eq!(
                raw.c_lflag & libc::ISIG,
                baseline.c_lflag & libc::ISIG,
                "ISIG 与进入前一致（Ctrl+C 的语义不许被改动）"
            );
            assert_eq!(raw.c_cc[libc::VMIN], 1);
            assert_eq!(raw.c_cc[libc::VTIME], 0);
            assert_eq!(
                read_master(&master),
                b"\x1b[?2004h".to_vec(),
                "进入时必须向终端写 bracketed paste 开"
            );
        }

        let restored = get_termios(slave_fd);
        assert_eq!(restored.c_iflag, baseline.c_iflag, "c_iflag 必须逐字段复原");
        assert_eq!(restored.c_oflag, baseline.c_oflag, "c_oflag 必须逐字段复原");
        assert_eq!(restored.c_cflag, baseline.c_cflag, "c_cflag 必须逐字段复原");
        assert_eq!(restored.c_lflag, baseline.c_lflag, "c_lflag 必须逐字段复原");
        assert_eq!(restored.c_cc, baseline.c_cc, "c_cc 必须逐字段复原");
        assert_eq!(
            read_master(&master),
            b"\x1b[?2004l".to_vec(),
            "Drop 时必须向终端写 bracketed paste 关"
        );

        // 非 tty（管道读端）拿不到 termios → Err，调用方据此回退行模式。
        let mut fds: [RawFd; 2] = [-1, -1];
        // SAFETY: `pipe` 只往两个已初始化的出参里写；返回 0 表示成功。
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: 上面 rc == 0，两个 fd 都是本进程新拿到的所有权。
        let (read_end, _write_end) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        assert!(
            RawMode::enable_on(read_end.as_raw_fd()).is_err(),
            "非 tty 必须返回 Err（不许 panic，也不许假装成功）"
        );
    }

    /// 退出路径的兜底：守卫被读取线程握着（`Drop` 不会跑）时，[`super::restore_terminal`]
    /// 按进程记录把终端还原——用户回到 shell 不会留在 raw mode。
    #[test]
    fn restore_terminal_puts_the_tty_back_when_the_guard_is_still_held() {
        let _serial = slot_guard();
        let (_master, slave) = open_pty();
        let slave_fd = slave.as_raw_fd();
        let baseline = get_termios(slave_fd);

        let guard = RawMode::enable_on(slave_fd).expect("pty 从端是真 tty，进入 raw mode 必须成功");
        assert_eq!(
            get_termios(slave_fd).c_lflag & libc::ECHO,
            0,
            "raw 期间 ECHO 必须为 0（还原断言的前提）"
        );
        // 模拟「退出时守卫还活着、进程不会析构它」：forget 掉，Drop 不再跑
        std::mem::forget(guard);

        super::restore_terminal();

        let restored = get_termios(slave_fd);
        assert_eq!(restored.c_lflag, baseline.c_lflag, "c_lflag 必须逐字段复原");
        assert_eq!(restored.c_iflag, baseline.c_iflag, "c_iflag 必须逐字段复原");
        assert_eq!(restored.c_cc, baseline.c_cc, "c_cc 必须逐字段复原");

        // 现场清干净：被 forget 的守卫留下的记录不再指向别的用例
        *super::ACTIVE.lock().expect("记录锁不该中毒") = None;
    }

    /// 退出路径还要关掉 bracketed paste：空闲 Ctrl-C 退出时读取线程仍握着守卫阻塞在 `read` 里，
    /// 进程退出不会析构它，`Drop` 的关闭序列因此永远不会写出——用户的 shell 会残留粘贴模式。
    ///
    /// 写入面就是终端（pty 主端读到的字节）：进入时那一段先读掉，之后的窗口只可能来自
    /// `restore_terminal` 自己；守卫随后析构并清掉记录，再调一次还原就该是零字节。
    #[test]
    fn restore_terminal_closes_bracketed_paste_on_a_pty() {
        let _serial = slot_guard();
        let (master, slave) = open_pty();
        let slave_fd = slave.as_raw_fd();
        let baseline = get_termios(slave_fd);

        let guard = RawMode::enable_on(slave_fd).expect("pty 从端是真 tty，进入 raw mode 必须成功");
        assert_eq!(
            read_master(&master),
            b"\x1b[?2004h".to_vec(),
            "进入时先写 bracketed paste 开（下面的窗口从它之后开始）"
        );

        // 模拟空闲 Ctrl-C 的退出现场：守卫还活着（读取线程握着它），只有进程记录可用
        super::restore_terminal();

        assert_eq!(
            read_master(&master),
            b"\x1b[?2004l".to_vec(),
            "退出兜底必须补上 bracketed paste 关（shell 不该残留粘贴模式）"
        );
        let restored = get_termios(slave_fd);
        assert_eq!(restored.c_iflag, baseline.c_iflag, "c_iflag 必须逐字段复原");
        assert_eq!(restored.c_oflag, baseline.c_oflag, "c_oflag 必须逐字段复原");
        assert_eq!(restored.c_cflag, baseline.c_cflag, "c_cflag 必须逐字段复原");
        assert_eq!(restored.c_lflag, baseline.c_lflag, "c_lflag 必须逐字段复原");
        assert_eq!(restored.c_cc, baseline.c_cc, "c_cc 必须逐字段复原");

        // 守卫析构：它自己也写一遍关闭序列并清掉记录；此后没有活跃记录，还原一个字都不写
        drop(guard);
        let _ = read_master(&master);
        super::restore_terminal();
        assert!(
            read_master(&master).is_empty(),
            "没有活跃记录 → 零字节（既有的「无操作」语义保留）"
        );
    }
}
