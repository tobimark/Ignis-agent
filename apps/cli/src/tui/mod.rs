//! TUI（Beta）的终端层与捕获基础件：备用屏守卫、尺寸探测、输出净化、宽度截断。
//!
//! 本模块只做「进得去、退得干净、模型输出只能显示不能驱动终端」这三件事，不合成帧、不跑主循环
//! （那是后续任务的 `view`/`app`）。分层：
//! - [`term`]：备用屏/光标/尺寸/panic 兜底（raw mode 复用 [`crate::editor::raw::RawMode`]，
//!   `ISIG` 保持开着）；
//! - [`capture`]：渲染器的输出落点（[`capture::CaptureSink`]）与净化（[`capture::sanitize`]）；
//! - 本文件：可用性判定（[`tui_unavailable_reason`]）与降级 note（[`fallback_note`]）。
//!
//! 可用性判定是**纯函数**：tty / `TERM` / 尺寸三件事都由调用方探好后传进来，用例不必造终端也不
//! 必改环境变量（`repl::run` 与 `tui::start` 各自把现场探好）。

pub mod capture;
pub mod term;

/// 终端尺寸（列 × 行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// 列数。
    pub cols: usize,
    /// 行数。
    pub rows: usize,
}

/// TUI 可用的最小列数：小于它就降级经典 REPL（设计 §6.10「尺寸过小」）。
pub const MIN_COLS: usize = 20;

/// TUI 可用的最小行数：小于它就降级经典 REPL（设计 §6.10「尺寸过小」）。
pub const MIN_ROWS: usize = 8;

/// 进不去 TUI 的理由（`None` = 可用）。
///
/// 纯函数：tty / `TERM` / 尺寸三件事都由调用方探好后传进来，用例不必造终端也不必改环境变量。
/// 判定顺序即理由链——stdin → stdout → `TERM` → 尺寸，一条不过就报它那句，后面的不再看：
/// 用户看到的第一句永远是最靠前的那个硬条件。
pub fn tui_unavailable_reason(
    stdin_tty: bool,
    stdout_tty: bool,
    term: Option<&std::ffi::OsStr>,
    size: Option<Size>,
) -> Option<String> {
    if !stdin_tty {
        return Some("stdin is not a terminal".to_string());
    }
    if !stdout_tty {
        return Some("stdout is not a terminal".to_string());
    }
    if !crate::editor::raw::RawMode::supported(term) {
        return Some("TERM is dumb".to_string());
    }
    // 探不到尺寸按 0×0 处理：降级总是安全的那个方向，也让理由句保持同一个形状。
    let size = size.unwrap_or(Size { cols: 0, rows: 0 });
    if size.cols < MIN_COLS || size.rows < MIN_ROWS {
        return Some(format!("terminal too small ({}x{})", size.cols, size.rows));
    }
    None
}

/// 降级时的一行 note（逐字，英文 chrome）。
///
/// `reason` 来自 [`tui_unavailable_reason`] 或终端守卫的 io 错误；调用方把它整行打到经典路径
/// 上（`--tui` 不可用时只多这一行，其余一切照旧）。
pub fn fallback_note(reason: &str) -> String {
    format!("note: TUI unavailable ({reason}); falling back to the classic REPL")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{MIN_COLS, MIN_ROWS, Size, fallback_note, tui_unavailable_reason};

    /// 「够大」的常规终端：表驱动用例的可用基线。
    fn ok_size() -> Size {
        Size { cols: 80, rows: 24 }
    }

    /// 常规终端名：`TERM` 这一格在多数用例里都不是判据。
    fn ok_term() -> Option<&'static OsStr> {
        Some(OsStr::new("xterm-256color"))
    }

    /// 六格判定链的逐字口径：stdin → stdout → `TERM` → 尺寸，一条不过就报它那句。
    #[test]
    fn tui_unavailable_reason_covers_every_guard() {
        // 全绿：可用。
        assert_eq!(
            tui_unavailable_reason(true, true, ok_term(), Some(ok_size())),
            None
        );

        // 第一句：stdin 不是 tty（比 stdout 那条更靠前）。
        assert_eq!(
            tui_unavailable_reason(false, true, ok_term(), Some(ok_size())).as_deref(),
            Some("stdin is not a terminal")
        );
        assert_eq!(
            tui_unavailable_reason(false, false, ok_term(), Some(ok_size())).as_deref(),
            Some("stdin is not a terminal"),
            "两条都不过时先报 stdin 那句（判定顺序即理由链）"
        );

        // 第二句：stdout 不是 tty。
        assert_eq!(
            tui_unavailable_reason(true, false, ok_term(), Some(ok_size())).as_deref(),
            Some("stdout is not a terminal")
        );

        // 第三句：`TERM` 不可用（dumb / 缺失 / 空串都算）。
        for dumb in [Some(OsStr::new("dumb")), None, Some(OsStr::new(""))] {
            assert_eq!(
                tui_unavailable_reason(true, true, dumb, Some(ok_size())).as_deref(),
                Some("TERM is dumb"),
                "{dumb:?} 必须判为不可用"
            );
        }

        // 第四句：尺寸过小（逐字带上实际尺寸）或探不到尺寸。
        for (size, reason) in [
            (Size { cols: 19, rows: 24 }, "terminal too small (19x24)"),
            (Size { cols: 20, rows: 7 }, "terminal too small (20x7)"),
            (Size { cols: 80, rows: 7 }, "terminal too small (80x7)"),
            (Size { cols: 0, rows: 0 }, "terminal too small (0x0)"),
        ] {
            assert_eq!(
                tui_unavailable_reason(true, true, ok_term(), Some(size)).as_deref(),
                Some(reason),
                "{size:?}"
            );
        }
        assert_eq!(
            tui_unavailable_reason(true, true, ok_term(), None).as_deref(),
            Some("terminal too small (0x0)"),
            "探不到尺寸按「太小」处理：降级总是安全的那个方向"
        );

        // 边界属于可用一侧：恰好 20×8 放行。
        assert_eq!(
            tui_unavailable_reason(true, true, ok_term(), Some(Size { cols: 20, rows: 8 })),
            None
        );
        assert_eq!((MIN_COLS, MIN_ROWS), (20, 8), "最小尺寸常量逐字钉住");
    }

    /// 降级 note 逐字（英文 chrome；Task 7 的 pty 用例按它 expect）。
    #[test]
    fn fallback_note_is_pinned() {
        assert_eq!(
            fallback_note("TERM is dumb"),
            "note: TUI unavailable (TERM is dumb); falling back to the classic REPL"
        );
        assert_eq!(
            fallback_note("terminal too small (10x4)"),
            "note: TUI unavailable (terminal too small (10x4)); falling back to the classic REPL"
        );
    }

    /// `Size` 是 `Copy + Eq + Debug`：帧合成按值传它，用例直接比较。
    #[test]
    fn size_is_copy_eq_and_debug() {
        fn round_trip<T: Copy + Eq + std::fmt::Debug>(value: T) -> T {
            value
        }
        let size = Size { cols: 80, rows: 24 };
        assert_eq!(round_trip(size), size, "Copy/Eq/Debug 是接口契约：{size:?}");
        assert_ne!(
            Size { cols: 80, rows: 24 },
            Size { cols: 80, rows: 25 },
            "行数参与相等判定"
        );
    }
}
