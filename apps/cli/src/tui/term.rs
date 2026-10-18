//! 终端层：备用屏守卫、尺寸探测与 panic 兜底。
//!
//! 备用屏 + 藏光标 + 清屏这几件事的字节只在这里产生（帧里的定位序列由 `view::compose` 产生），
//! raw mode 复用 [`crate::editor::raw::RawMode`]（`ISIG` 保持开着：Ctrl+C 仍是 SIGINT）。
//!
//! 三条钉死的语义：
//! - **先 raw、后写屏**：`enable(fd)` 失败时屏幕零改动（降级 note 打在经典路径上）；
//! - **先退屏、后析构 raw**：`Drop` 里先写 `SHOW_CURSOR + LEAVE_ALT_SCREEN`、再析构 raw 守卫，
//!   否则 bracketed paste 的关闭序列会落在备用屏之外；
//! - **每条退出路径都靠守卫收尾**：正常返回、`?` 早退、panic 展开（见 [`install_panic_hook`]）。

use std::io::{self, Write};
use std::sync::Once;

use crate::editor::raw::{RawMode, restore_terminal};

use super::Size;

/// 进入备用屏（DECSET 1049）。
pub const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";

/// 退出备用屏（DECRST 1049）。
pub const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";

/// 藏光标（DECRST 25）。
pub const HIDE_CURSOR: &str = "\x1b[?25l";

/// 亮光标（DECSET 25）。
pub const SHOW_CURSOR: &str = "\x1b[?25h";

/// 清屏（ED 2）。
pub const CLEAR_SCREEN: &str = "\x1b[2J";

/// 光标归位（CUP）。
pub const CURSOR_HOME: &str = "\x1b[H";

/// 进入备用屏写出的序列，顺序即字节序（备用屏 → 藏光标 → 清屏 → 归位）。
const ENTER_SEQUENCES: [&str; 4] = [ENTER_ALT_SCREEN, HIDE_CURSOR, CLEAR_SCREEN, CURSOR_HOME];

/// 退出时写出的序列，顺序即字节序（先亮光标、再退出备用屏）。
const LEAVE_SEQUENCES: [&str; 2] = [SHOW_CURSOR, LEAVE_ALT_SCREEN];

/// 探不到尺寸时的回退：80×24（非 tty / ioctl 失败 / 终端报 0）。
const DEFAULT_SIZE: Size = Size { cols: 80, rows: 24 };

/// `TIOCGWINSZ` 探测：非 tty / ioctl 失败 / 报 0 都给 `None`（调用方回退 80×24）。
pub fn probe_size(fd: i32) -> Option<Size> {
    // SAFETY: `size` 是本函数独占的一块 `winsize`；`TIOCGWINSZ` 成功时写满它，失败时返回非 0，
    // 此时不读它的内容（`ok` 为 false 直接给 None）。
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_col > 0 && size.ws_row > 0).then_some(Size {
        cols: size.ws_col as usize,
        rows: size.ws_row as usize,
    })
}

/// 全屏终端守卫：进入时进 raw mode 并写 `ENTER_ALT_SCREEN + HIDE_CURSOR + CLEAR_SCREEN +
/// CURSOR_HOME`；`Drop` 写 `SHOW_CURSOR + LEAVE_ALT_SCREEN` 并析构 raw 守卫（termios 复原 +
/// 关 bracketed paste）。**每一条退出路径**（含 panic 展开与早退）都靠它收尾。
pub struct TerminalGuard {
    /// raw 守卫：`Drop` 里在写完退出序列之后再析构（顺序见模块文档）。
    raw: Option<RawMode>,
    /// 帧/note 的输出落点：生产入口是 stdout，用例可注入（pty 上跑同一条路径）。
    out: Box<dyn Write + Send>,
    /// 进入时探一次、[`TerminalGuard::refresh_size`] 时更新。
    size: Size,
    /// raw mode 与尺寸探测共用的 fd。
    fd: i32,
}

impl TerminalGuard {
    /// 生产入口：stdout + `STDOUT_FILENO` + [`RawMode::enable_on`]。
    ///
    /// 调用方负责先做可用性判定（[`super::tui_unavailable_reason`]）：这里不重复判定，只把
    /// 「进不去」如实上抛。
    pub fn enter() -> io::Result<(TerminalGuard, Size)> {
        let out: Box<dyn Write + Send> = Box::new(io::stdout());
        TerminalGuard::enter_with(out, libc::STDOUT_FILENO, RawMode::enable_on)
    }

    /// 注入入口（用例在 pty 从端上跑同一条路径）：`fd` 同时用于 raw mode 与尺寸探测。
    ///
    /// 顺序钉死：**先 `enable(fd)`、成功后再写进入字节**——进不去时屏幕零改动，降级 note 打在
    /// 经典路径上；写进入字节失败时守卫已经构造出来，早退会走 `Drop` 把屏幕收干净。
    pub fn enter_with(
        out: Box<dyn Write + Send>,
        fd: i32,
        enable: impl FnOnce(i32) -> io::Result<RawMode>,
    ) -> io::Result<(TerminalGuard, Size)> {
        let raw = enable(fd)?;
        let size = probe_size(fd).unwrap_or(DEFAULT_SIZE);
        let mut guard = TerminalGuard {
            raw: Some(raw),
            out,
            size,
            fd,
        };
        for sequence in ENTER_SEQUENCES {
            guard.out.write_all(sequence.as_bytes())?;
        }
        guard.out.flush()?;
        Ok((guard, size))
    }

    /// 写一段字节（帧、note）并 flush；失败由调用方忽略（EPIPE 等）。
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.out.flush()
    }

    /// 缓存的尺寸（进入时探一次）。
    pub fn size(&self) -> Size {
        self.size
    }

    /// 重新探测（SIGWINCH）；探测失败保留旧值——窗口刚被拉大就报 0 是终端常态，把尺寸打回
    /// 0×0 会让帧合成退化到最小尺寸。
    pub fn refresh_size(&mut self) -> Size {
        if let Some(size) = probe_size(self.fd) {
            self.size = size;
        }
        self.size
    }
}

impl Drop for TerminalGuard {
    /// 退出路径只许做减法：先写退出序列（best-effort），再析构 raw 守卫还原 termios。
    ///
    /// 这里绝不能 panic：`Drop` 里的 panic 会盖掉更早的错误，也可能在展开过程中二次 panic。
    fn drop(&mut self) {
        for sequence in LEAVE_SEQUENCES {
            let _ = self.out.write_all(sequence.as_bytes());
        }
        let _ = self.out.flush();
        // 显式 take：退出序列必须落在备用屏之内，raw 守卫随后析构（关 bracketed paste + 复原 termios）
        drop(self.raw.take());
    }
}

/// panic hook 只装一次（进程内）。
static PANIC_HOOK: Once = Once::new();

/// 安装 panic hook：先按备用屏退出序列与 [`restore_terminal`] 的记录做 best-effort 还原，再把
/// panic 转交给原来的 hook。TUI 启动时调用一次，重复调用是 no-op。
///
/// hook 体本身不 panic、不分配（panic 现场只许做常量与 syscall）：写失败、锁中毒、没有活跃
/// 守卫都只是「还原不完整」，绝不能让第二条 panic 盖掉原始信息。
pub fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            for sequence in LEAVE_SEQUENCES {
                write_stdout(sequence.as_bytes());
            }
            restore_terminal();
            previous(info);
        }));
    });
}

/// 直接往 fd 1 写一段字节：panic 现场不碰 `io::Stdout`（避免锁与分配）。
fn write_stdout(bytes: &[u8]) {
    // SAFETY: 指针与长度都来自同一块 `bytes`，`write` 只读这段内存、不持有它。
    let _ = unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::{Arc, Mutex};

    use crate::editor::raw::RawMode;
    use crate::tui::Size;

    use super::{TerminalGuard, install_panic_hook, probe_size};

    /// 记录写出字节的写入器：守卫的进入/退出序列落在它里面（raw mode 的 bracketed paste 字节
    /// 由 `RawMode` 直接写 fd，不经过它）。
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("记录锁不该中毒")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Recorder {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("记录锁不该中毒").clone())
                .expect("守卫只写 ASCII 转义序列")
        }
    }

    /// 每次 `write` 时顺手记下「此刻终端还在 raw mode 吗」的写入器：用来钉死 `Drop` 的次序
    /// （先退出备用屏、再析构 raw 守卫）。
    #[derive(Clone)]
    struct RawProbe {
        fd: RawFd,
        /// 每次写的时刻 ECHO 是否开着（raw mode 下 ECHO 是关的）。
        echo: Arc<Mutex<Vec<bool>>>,
    }

    impl Write for RawProbe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            // SAFETY: `t` 是本函数独占的出参；`tcgetattr` 成功时写满它，失败时返回非 0。
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            let echo =
                unsafe { libc::tcgetattr(self.fd, &mut t) } == 0 && t.c_lflag & libc::ECHO != 0;
            self.echo.lock().expect("探针锁不该中毒").push(echo);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
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

    /// 给 pty 设窗口尺寸（终端模拟器那一侧的 `TIOCSWINSZ`）。
    fn set_winsize(fd: RawFd, cols: u16, rows: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `size` 是本函数独占的一份 `winsize`；`TIOCSWINSZ` 只读它。
        let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
        assert_eq!(
            rc,
            0,
            "TIOCSWINSZ 失败：{}",
            std::io::Error::last_os_error()
        );
    }

    /// 开一根管道，返回（读端，写端）。
    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        let mut fds: [RawFd; 2] = [-1, -1];
        // SAFETY: `pipe` 只往两个已初始化的出参里写；返回 0 表示成功。
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe 失败");
        // SAFETY: 上面返回 0，两个 fd 都是本进程新拿到的所有权。
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    /// 守卫在 pty 从端上走同一条生产路径：进入字节逐字、raw mode 生效、尺寸来自 `TIOCSWINSZ`，
    /// `Drop` 字节逐字、termios 逐字段复原。
    #[test]
    fn guard_enters_and_leaves_the_alt_screen_on_a_pty() {
        let (_master, slave) = open_pty();
        let slave_fd = slave.as_raw_fd();
        set_winsize(slave_fd, 100, 30);
        let baseline = get_termios(slave_fd);

        let recorder = Recorder::default();
        let (mut guard, size) =
            TerminalGuard::enter_with(Box::new(recorder.clone()), slave_fd, RawMode::enable_on)
                .expect("pty 从端是真 tty，进入守卫必须成功");

        assert_eq!(
            recorder.text(),
            "\u{1b}[?1049h\u{1b}[?25l\u{1b}[2J\u{1b}[H",
            "进入字节逐字：备用屏 + 藏光标 + 清屏 + 归位"
        );
        assert_eq!(
            size,
            Size {
                cols: 100,
                rows: 30
            },
            "尺寸来自 TIOCSWINSZ"
        );
        assert_eq!(guard.size(), size, "size() 返回进入时探到的尺寸");
        assert_eq!(
            get_termios(slave_fd).c_lflag & libc::ECHO,
            0,
            "raw mode 必须已生效（进入时 ECHO 关掉）"
        );

        // SIGWINCH 那条路：重新探测取到新尺寸；探测失败（终端报 0）保留旧值。
        set_winsize(slave_fd, 40, 12);
        assert_eq!(guard.refresh_size(), Size { cols: 40, rows: 12 });
        set_winsize(slave_fd, 0, 0);
        assert_eq!(
            guard.refresh_size(),
            Size { cols: 40, rows: 12 },
            "探测失败保留旧值（不许把尺寸打回 0×0）"
        );

        guard.write(b"frame").expect("注入的写入器不会失败");
        assert_eq!(
            recorder.text(),
            "\u{1b}[?1049h\u{1b}[?25l\u{1b}[2J\u{1b}[Hframe"
        );

        drop(guard);
        assert_eq!(
            recorder.text(),
            "\u{1b}[?1049h\u{1b}[?25l\u{1b}[2J\u{1b}[Hframe\u{1b}[?25h\u{1b}[?1049l",
            "Drop 字节逐字：亮光标 + 退出备用屏"
        );
        let restored = get_termios(slave_fd);
        assert_eq!(restored.c_iflag, baseline.c_iflag, "c_iflag 必须逐字段复原");
        assert_eq!(restored.c_oflag, baseline.c_oflag, "c_oflag 必须逐字段复原");
        assert_eq!(restored.c_cflag, baseline.c_cflag, "c_cflag 必须逐字段复原");
        assert_eq!(restored.c_lflag, baseline.c_lflag, "c_lflag 必须逐字段复原");
        assert_eq!(restored.c_cc, baseline.c_cc, "c_cc 必须逐字段复原");
    }

    /// 备用屏退出序列必须先于 raw 守卫析构：反过来的话 bracketed paste 的关闭序列会落在备用屏
    /// 之外，用户看到的 shell 会残留一半的终端状态。
    #[test]
    fn drop_leaves_the_alt_screen_before_restoring_raw_mode() {
        let (_master, slave) = open_pty();
        let slave_fd = slave.as_raw_fd();
        let echo = Arc::new(Mutex::new(Vec::new()));
        let probe = RawProbe {
            fd: slave_fd,
            echo: Arc::clone(&echo),
        };

        let (mut guard, _size) =
            TerminalGuard::enter_with(Box::new(probe), slave_fd, RawMode::enable_on)
                .expect("pty 从端是真 tty，进入守卫必须成功");
        guard.write(b"x").expect("探针不会失败");
        drop(guard);

        let seen = echo.lock().expect("探针锁不该中毒").clone();
        assert!(!seen.is_empty(), "守卫每个阶段都得真的写过字节");
        assert!(
            seen.iter().all(|echo_on| !echo_on),
            "守卫写出的每一笔（含 Drop 的退出序列）都发生在 raw mode 之内：{seen:?}"
        );
        // 守卫析构之后终端回到行规范 + 回显。
        assert_ne!(
            get_termios(slave_fd).c_lflag & libc::ECHO,
            0,
            "Drop 之后 raw 守卫也已析构：ECHO 复原"
        );
    }

    /// raw mode 进不去时：上抛错误、屏幕零改动（一个字节都不写、termios 不动）。
    #[test]
    fn enter_with_leaves_the_screen_untouched_when_raw_mode_fails() {
        let (_master, slave) = open_pty();
        let slave_fd = slave.as_raw_fd();
        let baseline = get_termios(slave_fd);
        let recorder = Recorder::default();

        let result: std::io::Result<(TerminalGuard, Size)> =
            TerminalGuard::enter_with(Box::new(recorder.clone()), slave_fd, |_| {
                Err(std::io::Error::other("注入的失败"))
            });
        assert!(result.is_err(), "进不去 raw mode 必须上抛错误");
        assert_eq!(recorder.text(), "", "屏幕零改动：进入序列一个字节都不许写");
        let after = get_termios(slave_fd);
        assert_eq!(after.c_lflag, baseline.c_lflag, "termios 也没被动过");
        assert_eq!(after.c_iflag, baseline.c_iflag, "termios 也没被动过");
    }

    /// 尺寸探测：pty 上读 `TIOCGWINSZ`，非 tty / 报 0 / 坏 fd 一律 `None`。
    #[test]
    fn probe_size_reads_tiocgwinsz_and_falls_back() {
        let (_master, slave) = open_pty();
        let slave_fd = slave.as_raw_fd();
        set_winsize(slave_fd, 100, 30);
        assert_eq!(
            probe_size(slave_fd),
            Some(Size {
                cols: 100,
                rows: 30
            })
        );

        // 终端报 0：当作探不到，由调用方回退 80×24。
        set_winsize(slave_fd, 0, 0);
        assert_eq!(probe_size(slave_fd), None, "报 0 的终端算探不到");

        // 管道读端不是 tty：ioctl 失败 → None。
        let (read_end, _write_end) = pipe_pair();
        assert_eq!(probe_size(read_end.as_raw_fd()), None, "非 tty 必须给 None");
        assert_eq!(probe_size(-1), None, "坏 fd 也给 None，不许 panic");
    }

    /// panic hook 装两次是 no-op（`Once`），两次调用都不 panic。
    #[test]
    fn install_panic_hook_is_idempotent() {
        install_panic_hook();
        install_panic_hook();
        // 装了 hook 就改了进程级的 panic 行为，这里不在用例里真的 panic：重复调用本身不炸即达标。
    }
}
