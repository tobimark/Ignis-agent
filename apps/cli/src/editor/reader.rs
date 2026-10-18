//! 编辑器读取层：把 raw mode 的字节流、按键解码、状态机与重绘接成一个可用的「读一行」。
//!
//! 这是分层里最外面的一层：只有它认识终端（`Read`/`Write`），解码器与状态机都不认识。注入 I/O 的
//! [`EditorReader`] 因此能在没有 pty 的单测里整条跑通（喂一段字节，断言写出的帧与返回值）；
//! [`EditorLineReader`] 只是把它接到进程的 stdin/stdout，满足 [`LineReader`] 的 `&self` 接口。
//!
//! 几条钉死的口径：
//!
//! - **提交**：`Event::Submit(text)` 先用事件里的文本（状态机手里那份已经被清空）画一帧「光标在
//!   末尾」，再写 `\r\n`，返回 `Ok(Some(text))`：输入行留在屏幕上，光标落到下一行。
//! - **EOF**：`Ctrl+D`（空缓冲）与「读到 0 字节」都返回 `Ok(None)` 且**不补换行**——REPL 自己
//!   `println!()`（终端里 Ctrl-D 不留换行）。
//! - **EINTR**：`ErrorKind::Interrupted` 是「被信号打断」，继续读，不是错误。
//! - **非法 UTF-8**：解码器用替换字符顶上并置位；读取层负责清掉输入行、打恰好一行告警、再重绘。
//! - **Tab**：不交给状态机（它不认识候选），走 [`slash::complete`]。
//! - **跨提交边界的按键**：一次读取可能同时装下「提交」与它后面的按键（快速输入、粘贴后紧跟
//!   回车），回车之后的按键存进队列，由下一次 `read_line` 处理——读进来的按键一个都不许丢。
//! - **按批重绘**：一次读取解出的按键在同一批里处理完，`Event::Redraw` 合并为**至多一帧**
//!   （粘贴 N 个字符不再写 N 帧）；提交与 EOF 仍当场落笔并返回。
//! - **raw mode 按读取区间启用**：[`EditorLineReader::read_line`] 进入时进 raw mode、返回前
//!   析构守卫还原（每一条路径都还原：提交、EOF、错误）——任务执行与交互审批（`InteractiveApproval`
//!   的按行读）期间终端必须回到 canonical/echo，否则审批提示永远等不到一行、输入也不回显。
//! - **宽度每次读取探一次**：进读取区间时用 [`width_for_stdout`] 探一次 stdout 的列数交给
//!   [`draw::frame_with`]（非 tty → `usize::MAX` → 交给终端软换行，与今日逐字相同）。折行与
//!   `\t` 展开都只改**画出来的字节**：提交的文本、历史与送进内核的 prompt 一个字节都不动。
//! - **折行块重锚**：`draw` 记着上一帧画完后光标落在块里的第几个物理行（[`draw::cursor_row`]），
//!   下一帧落笔前先写 `\r` + `\x1b[{n}A` 把光标提回**块首**，帧自己的 `\r\x1b[J` 因此从块首
//!   清掉整个折行块——少了这一步它只清光标所在行以下，块的上半截会被帧在自己的下面再画一遍
//!   （40 列下逐键输入 100 个字符会叠出好几份输入行）。提交、Tab 候选列表与非法 UTF-8 告警
//!   这三条「块没了 / 块从头来过」的路径把记账归零（见 [`EditorReader::clear_block`]）。

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use super::draw::{self, CONTINUATION};
use super::history::History;
use super::keys::{Decoder, Key};
use super::raw::RawMode;
use super::state::{EditorState, Event};
use crate::commands::slash::{self, Completion};
use crate::render::width_for_stdout;
use crate::repl::LineReader;

/// 非法 UTF-8 的一行告警（英文 chrome）：编辑器按读取区间至多打一次，v0.1 的回退路径
/// （[`crate::repl::StdinReader`]）共用同一份文案。
pub(crate) const INVALID_WARNING: &str = "warning: input contained invalid UTF-8 bytes; replaced with the replacement character (check your terminal encoding or input method)";

/// 清掉当前输入行并回到行首：`\r` 回列 0，`\x1b[J` 从光标清到屏幕底（与帧的开头同形）。
const CLEAR_LINE: &[u8] = b"\r\x1b[J";

/// 一次 `read` 最多收多少字节：够装一整段粘贴，又不至于让重绘攒太久。
const READ_CHUNK: usize = 256;

/// 读取层：`R` 是字节来源（终端）、`W` 是重绘与告警的落点。
///
/// 注入 I/O 让整条链路（解码 → 状态机 → 重绘 → 提交）可以在内存里跑，不需要 pty。
pub struct EditorReader<R: Read, W: Write> {
    /// 字节来源：raw mode 下的 stdin（读到几个字节就返回）。
    input: R,
    /// 写屏幕：帧、候选列表与告警都从这里出去。
    output: W,
    /// 字节 → 按键：跨读取缓存解不完的尾巴。
    decoder: Decoder,
    /// 编辑状态：缓冲、光标与历史都在它手里，活过每一次 `read_line`。
    state: EditorState,
    /// 已解出但还没交给状态机的按键（见模块文档「跨提交边界的按键」）。
    pending: VecDeque<Key>,
    /// 本次读取的终端列数：进读取区间时由 [`width_for_stdout`] 探一次（非 tty → `usize::MAX`，
    /// 也就是「交给终端软换行」的既有口径），`draw` / `finish_submit` 都拿它去折行。
    width: usize,
    /// 上一帧画完后光标落在折行块里的第几个物理行（0 起算，见 [`draw::cursor_row`]）：
    /// 下一帧落笔前据此把光标提回块首，让帧自己的 `\r\x1b[J` 清掉整个块（重锚，见 [`draw_sequence`]）。
    /// 块从当前行重新开始时归零：每次读取开始时、`finish_submit` 写完 `\r\n` 之后，
    /// 以及 [`EditorReader::clear_block`] 清掉整块之后。
    prev_cursor_row: usize,
}

impl<R: Read, W: Write> EditorReader<R, W> {
    /// 装配读取层：历史从 `history_path` 载入（缺失 → 空历史，见 [`History::load`]），
    /// 解码器与状态机都从零开始。
    pub fn new(input: R, output: W, history_path: PathBuf) -> Self {
        Self {
            input,
            output,
            decoder: Decoder::new(),
            state: EditorState::new(History::load(history_path)),
            pending: VecDeque::new(),
            // 读取前先按「不折行」当底：真有终端时 `read_line` 会重探
            width: usize::MAX,
            // 还没有画过任何一帧：光标与「块首」同位置
            prev_cursor_row: 0,
        }
    }

    /// 读一行：先画提示符，然后按「取一批按键 → 交给状态机 → 至多重绘一帧」的事件循环走。
    ///
    /// - 提交：把整段文本留在屏幕上（光标在末尾的一帧）再补 `\r\n`，返回 `Ok(Some(text))`；
    /// - `Ctrl+D`（空缓冲）与「读到 0 字节」：返回 `Ok(None)`，且不补换行；
    /// - `ErrorKind::Interrupted`：继续读，不是错误（raw mode 下的阻塞读会被信号打断）；
    /// - 非法 UTF-8：清掉输入行 → 一行告警 → 照常重绘；
    /// - 一批里的多个重绘请求只落一帧（见模块文档「按批重绘」），提交/EOF 仍当场落笔。
    pub fn read_line(&mut self, prompt: &str) -> io::Result<Option<String>> {
        // 终端宽度每次读取探一次（与渲染器同一口径）：非 tty → `usize::MAX`，帧与今日逐字相同。
        // 探在画第一帧之前：整条读取区间里的每一帧都用同一份宽度（中途改窗口不重排已经画出的行）。
        self.width = width_for_stdout(io::stdout().is_terminal());
        // 新的一块从光标当前所在的那一行开始：上一次读取留下的块已经在它下面结束（不会再有东西
        // 需要从块首清），重锚记账从零起。
        self.prev_cursor_row = 0;
        self.draw(prompt)?;
        loop {
            // 这一批要处理的按键：上一次读取跨过提交边界留下的先来（提示符已经画好），
            // 队列空了才去读一段新字节。
            let (keys, invalid) = if self.pending.is_empty() {
                let mut bytes = [0u8; READ_CHUNK];
                let read = match self.input.read(&mut bytes) {
                    // EOF / 对端关闭：与 Ctrl+D 同一条路（REPL 自己补换行）
                    Ok(0) => return Ok(None),
                    Ok(read) => read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                };

                let keys = self.decoder.feed(&bytes[..read]);
                let invalid = self.decoder.take_invalid();
                if invalid {
                    // 告警要落在输入行原来的位置上，所以先清掉它；替换字符已经由解码器插进
                    // 按键流，批末重绘时会把它一起画出来。
                    self.warn_invalid()?;
                }
                (keys, invalid)
            } else {
                (self.pending.drain(..).collect(), false)
            };

            // 按批合并重绘：批里有一个按键请求重绘，就在批末画一帧（不是每个按键一帧）。
            let mut redraw = false;
            // Tab 的多候选列表已经当场把输入行画回来了：它之后的合并重绘从此刻重新计数。
            let mut listed = false;
            for (index, key) in keys.iter().enumerate() {
                match self.key_step(prompt, *key)? {
                    Done::Quiet => {}
                    Done::Redraw => redraw = true,
                    Done::Listed => {
                        redraw = false;
                        listed = true;
                    }
                    Done::Submit(text) => {
                        // 这次读取里回车之后的按键留给下一次 `read_line`（读进来的键一个都不许丢）
                        self.pending.extend(keys[index + 1..].iter().copied());
                        return Ok(Some(text));
                    }
                    Done::Eof => {
                        self.pending.extend(keys[index + 1..].iter().copied());
                        return Ok(None);
                    }
                }
            }
            if redraw || (invalid && !listed) {
                // 后半句是兜底：告警把输入行清掉了，而这一批按键一个重绘都没触发——补一帧，
                // 否则输入行会一直空着等下一个按键（替换字符的插入本来就会请求重绘）。
                self.draw(prompt)?;
            }
        }
    }

    /// 处理一个按键，返回它把这次读取带向哪里：Tab 走补全，其余交给状态机。
    ///
    /// 这里只**记录**重绘请求（[`Done::Redraw`]）：落笔由 `read_line` 在批末合并，保证一次
    /// 读取里的 N 个字符至多写一帧。
    fn key_step(&mut self, prompt: &str, key: Key) -> io::Result<Done> {
        // Tab 不交给状态机（它不认识候选）：补全的结果由这一层落地，所以状态机里那条
        // 「Tab 是 Idle」的分支只在它自己的单测里走到。
        if key == Key::Tab {
            return match slash::complete(&self.state.text()) {
                // 唯一候选：整行换成它、光标落到末尾（重绘合并到批末）
                Completion::Accepted(text) => {
                    self.state.replace(&text);
                    Ok(Done::Redraw)
                }
                // 多候选：清掉输入行、逐行列出、再把输入行画回来（缓冲不变）
                Completion::Listing(lines) => {
                    self.write_listing(prompt, &lines)?;
                    Ok(Done::Listed)
                }
                // 没有候选：什么都不写
                Completion::None => Ok(Done::Quiet),
            };
        }
        Ok(match self.state.handle(key) {
            Event::Idle => Done::Quiet,
            Event::Redraw => Done::Redraw,
            // 缓冲已经清了：「光标在末尾」的那一帧要用事件里的文本画
            Event::Submit(text) => {
                self.finish_submit(prompt, &text)?;
                Done::Submit(text)
            }
            Event::Eof => Done::Eof,
        })
    }

    /// 按当前状态画一帧（提示符 + 缓冲 + 光标归位）并立刻冲出去。
    ///
    /// 帧按本次读取探得的宽度折行（`\t` 也在这条路上展开成空格）：缓冲区里那份文本原样不动。
    /// 落笔前先按上一帧记下的光标行回到块首（见 [`draw_sequence`]），再记下这一帧的新位置。
    fn draw(&mut self, prompt: &str) -> io::Result<()> {
        let text = self.state.text();
        let cursor = self.state.cursor();
        let (bytes, row) = draw_sequence(self.prev_cursor_row, prompt, &text, cursor, self.width);
        self.prev_cursor_row = row;
        self.write_bytes(bytes.as_bytes())
    }

    /// 提交：先把「光标在末尾」的一帧留在屏幕上（文本从事件里拿：状态机手里那份已经被清空），
    /// 再写 `\r\n` 把光标送到下一行——输入行原样留在屏幕上，与 readline 的观感一致。
    ///
    /// 这一帧同样先重锚：否则光标落在折行块的下半截时，帧自己的 `\r\x1b[J` 只从那里往下清，
    /// 块的上半截会被再画一遍。`\r\n` 之后块结束在光标上一行，记账归零。
    fn finish_submit(&mut self, prompt: &str, text: &str) -> io::Result<()> {
        let (bytes, _) = draw_sequence(
            self.prev_cursor_row,
            prompt,
            text,
            text.chars().count(),
            self.width,
        );
        self.write_bytes(bytes.as_bytes())?;
        self.write_bytes(b"\r\n")?;
        self.prev_cursor_row = 0;
        Ok(())
    }

    /// 清掉当前的输入块：`prev_cursor_row > 0`（块不止一行、光标不在块首）时先写 `\r` +
    /// `\x1b[{n}A` 回到块首，再 `\r\x1b[J`——只从光标那一行往下清会留下一截旧块在上头。
    /// 清完块就没了，记账归零（调用方要重绘输入行时，它是从当前这一行重新开始的）。
    fn clear_block(&mut self) -> io::Result<()> {
        if self.prev_cursor_row > 0 {
            self.output
                .write_all(format!("\r\x1b[{}A", self.prev_cursor_row).as_bytes())?;
        }
        self.prev_cursor_row = 0;
        self.output.write_all(CLEAR_LINE)
    }

    /// 非法 UTF-8 的告警：清掉输入行 → 一行告警（调用方随后照常重绘输入行）。
    fn warn_invalid(&mut self) -> io::Result<()> {
        self.clear_block()?;
        self.output.write_all(INVALID_WARNING.as_bytes())?;
        self.write_bytes(b"\r\n")
    }

    /// 候选列表：清掉输入行 → 逐行列出 → 把输入行画回来（缓冲与光标都原样）。
    fn write_listing(&mut self, prompt: &str, lines: &[String]) -> io::Result<()> {
        self.clear_block()?;
        for line in lines {
            self.output.write_all(line.as_bytes())?;
            self.output.write_all(b"\r\n")?;
        }
        self.draw(prompt)
    }

    /// 写一段字节并立刻冲刷：raw mode 下终端不会帮忙回显，帧与告警都不该在缓冲区里等。
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.output.write_all(bytes)?;
        self.output.flush()
    }
}

/// 重绘一帧的完整字节（重锚 + 帧），以及这一帧画完后光标落在折行块里的物理行。
///
/// `prev_cursor_row > 0`（上一帧的光标不在块首）时，先写 `\r` + `\x1b[{prev_cursor_row}A` 把光标
/// 从块内提回**块首**；跟在后面的帧自己以 `\r\x1b[J` 开头，于是从块首清掉**整个**折行块。少了
/// 这一提，`\r\x1b[J` 只清光标所在行以下，块的上半截会被帧在自己的下面再画一遍——40 列里逐键
/// 输入 100 个字符，屏幕上就会堆出好几份输入行（光标被留在块内低处，一帧比一帧低）。
///
/// 帧本身的字节一个字都不动（[`draw::frame_with`]）：多出来的这几个字节在帧**之前**。
/// `prev_cursor_row == 0`（光标本来就在块首/块只有一行）时不写移动序列——`\x1b[0A` 没有意义。
fn draw_sequence(
    prev_cursor_row: usize,
    prompt: &str,
    text: &str,
    cursor: usize,
    width: usize,
) -> (String, usize) {
    let frame = draw::frame_with(prompt, CONTINUATION, text, cursor, width);
    // 这一帧自己会画在哪一行：与帧同一套折行（`walk`），读取层据此记账
    let row = draw::cursor_row(prompt, CONTINUATION, text, cursor, width);
    if prev_cursor_row == 0 {
        return (frame, row);
    }
    (format!("\r\x1b[{prev_cursor_row}A{frame}"), row)
}

/// 一次按键处理完之后，这次读取的去路。
enum Done {
    /// 继续读：这个按键没有改屏幕（空操作、没有候选的 Tab）。
    Quiet,
    /// 继续读：这个按键请求重绘（合并到这一批的最后一次绘制，见 `read_line`）。
    Redraw,
    /// 继续读：Tab 的多候选列表已经当场写出（含输入行一帧），本批的合并重绘重新计数。
    Listed,
    /// 提交：`read_line` 返回 `Ok(Some(text))`。
    Submit(String),
    /// EOF：`read_line` 返回 `Ok(None)`。
    Eof,
}

/// 面向 REPL 的读取层：把编辑器接到进程的 stdin/stdout。
///
/// `LineReader` 只给 `&self`，所以编辑器套一层 [`Mutex`]；每次读取都在 `repl::next_line` 起的
/// 脱离线程里跑，锁的持有者因此只有那一条线程。
pub struct EditorLineReader {
    /// 编辑器：`Mutex` 只为了满足 `LineReader::read_line(&self)`。
    inner: Mutex<EditorReader<io::Stdin, io::Stdout>>,
}

impl EditorLineReader {
    /// 用真实终端与给定的历史文件路径装配（raw mode 由 [`LineReader::read_line`] 按读取
    /// 区间自己进出，见 `editor::raw`）。
    pub fn new(history_path: PathBuf) -> EditorLineReader {
        EditorLineReader {
            inner: Mutex::new(EditorReader::new(io::stdin(), io::stdout(), history_path)),
        }
    }
}

impl LineReader for EditorLineReader {
    /// 提示符由编辑器自己画（REPL 把 `repl::PROMPT` 原样传进来）。
    ///
    /// **raw mode 只在本调用期间生效**：进入时 `RawMode::enable`、返回前守卫析构还原，提交、
    /// EOF、错误三条路径都一样。守卫绝不跨读取存活（也不能存进共享状态：每次读取都在脱离
    /// 线程里跑，守卫会被卡在阻塞读里永不析构）——任务执行与交互审批因此都在 canonical/echo
    /// 下运行，`InteractiveApproval` 的按行读等得到回车。
    fn read_line(&self, prompt: &str) -> io::Result<Option<String>> {
        let mut editor = match self.inner.lock() {
            // 锁中毒（上一次读取 panic 过）时不能让 REPL 卡死，也不该在这里 panic：
            // 按读取失败交回，主循环会把会话干净地结束掉。
            Ok(editor) => editor,
            Err(_) => return Err(io::Error::other("line editor lock poisoned")),
        };
        // 进不了 raw mode（终端被关掉等）照实上抛：REPL 带着错误结束会话，不硬撑一个
        // 「按键不回显、Enter 不提交」的编辑器。
        let _raw = RawMode::enable().map_err(|error| {
            io::Error::new(error.kind(), format!("failed to enable raw mode: {error}"))
        })?;
        editor.read_line(prompt)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::io::{self, Cursor, ErrorKind, Read, Write};
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{EditorReader, INVALID_WARNING, draw_sequence};
    use crate::editor::draw::{self, CONTINUATION};
    use crate::editor::keys::Key;
    use crate::editor::state::Event;
    use crate::render::width::char_width;

    /// 提示符：与 `repl::PROMPT` 同一条字面量（REPL 传进来的就是它）。
    const PROMPT: &str = "❯ ";

    /// 单个用例独占的临时目录（历史文件的落点），析构时清理：绝不碰真实的 `~/.ignis`。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-cli-reader-{}-{label}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// 历史文件路径：`<临时目录>/history`。
        fn history_path(&self) -> PathBuf {
            self.0.join("history")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 共享的输出记录器：读取层拿走所有权，用例仍能读到写出的字节。
    #[derive(Clone, Default)]
    struct Recorder(Rc<RefCell<Vec<u8>>>);

    impl Recorder {
        /// 录到的字节按 UTF-8 交回（帧与告警都是读取层自己写的，必然合法）。
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("读取层写出的必须是合法 UTF-8")
        }

        /// 取走自上次取走之后写出的字节：逐帧画屏幕的用例按帧喂给屏幕仿真器。
        fn take(&self) -> String {
            let mut written = self.0.borrow_mut();
            String::from_utf8(std::mem::take(&mut *written)).expect("读取层写出的必须是合法 UTF-8")
        }
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 输入一次性喂完的读取层：I/O 全在内存里，不需要 pty。
    fn editor(input: &[u8], history: &Path) -> (EditorReader<Cursor<Vec<u8>>, Recorder>, Recorder) {
        let out = Recorder::default();
        let reader = EditorReader::new(
            Cursor::new(input.to_vec()),
            out.clone(),
            history.to_path_buf(),
        );
        (reader, out)
    }

    /// 「提示符刚出现」的那一帧（空缓冲、光标在段首）。
    fn prompt_frame() -> String {
        draw::frame(PROMPT, CONTINUATION, "", 0)
    }

    /// 先报一次 EINTR，再把剩下的字节按块交出去。
    struct InterruptedOnce {
        rest: Vec<u8>,
        interrupted: bool,
    }

    impl InterruptedOnce {
        fn new(bytes: &[u8]) -> Self {
            Self {
                rest: bytes.to_vec(),
                interrupted: false,
            }
        }
    }

    impl Read for InterruptedOnce {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::Error::new(ErrorKind::Interrupted, "被打断"));
            }
            let take = self.rest.len().min(buf.len());
            buf[..take].copy_from_slice(&self.rest[..take]);
            self.rest.drain(..take);
            Ok(take)
        }
    }

    /// 一次真错误（不是 EINTR）的读取源。
    struct Failing;

    impl Read for Failing {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(ErrorKind::PermissionDenied, "读不了"))
        }
    }

    /// 敲一行回车：返回提交的文本，屏幕上留下「光标在末尾」的那一帧，光标落到下一行。
    #[test]
    fn submits_the_typed_line_and_leaves_it_on_screen() {
        let dir = TempDir::new("submit");
        let (mut reader, out) = editor("你好\r".as_bytes(), &dir.history_path());

        assert_eq!(reader.read_line(PROMPT).unwrap(), Some("你好".to_string()));

        let text = out.text();
        assert!(text.contains("❯ 你好"), "输入行必须留在屏幕上：{text:?}");
        assert!(
            text.ends_with(&format!(
                "{}\r\n",
                draw::frame(PROMPT, CONTINUATION, "你好", 2)
            )),
            "最后一帧是「光标在末尾」+ 换行：{text:?}"
        );
    }

    /// 空行上的 Ctrl+D 是 EOF：返回 `None`，且不补换行（换行由 REPL 自己打）。
    #[test]
    fn ctrl_d_on_an_empty_line_returns_none() {
        let dir = TempDir::new("ctrl-d");
        let (mut reader, out) = editor(b"\x04", &dir.history_path());

        assert_eq!(reader.read_line(PROMPT).unwrap(), None);
        assert_eq!(out.text(), prompt_frame(), "EOF 之后不该再写任何字节");
    }

    /// 一上来就是 EOF（对端关闭）：返回 `None`，也只画了那一帧提示符。
    #[test]
    fn eof_without_any_input_returns_none() {
        let dir = TempDir::new("eof");
        let (mut reader, out) = editor(b"", &dir.history_path());

        assert_eq!(reader.read_line(PROMPT).unwrap(), None);
        assert_eq!(out.text(), prompt_frame());
    }

    /// EINTR 不是错误：被打断的那次读取接着重试，行照常读进来。
    #[test]
    fn interrupted_reads_are_retried() {
        let dir = TempDir::new("eintr");
        let out = Recorder::default();
        let mut reader = EditorReader::new(
            InterruptedOnce::new(b"ok\r"),
            out.clone(),
            dir.history_path(),
        );

        assert_eq!(reader.read_line(PROMPT).unwrap(), Some("ok".to_string()));
        assert!(out.text().contains("❯ ok"), "{}", out.text());
    }

    /// 真正的读取错误照旧上抛：只有 EINTR 被吞掉，别的错误不许当成 EOF。
    #[test]
    fn real_read_errors_propagate() {
        let dir = TempDir::new("read-error");
        let out = Recorder::default();
        let mut reader = EditorReader::new(Failing, out, dir.history_path());

        let error = reader.read_line(PROMPT).expect_err("读取失败必须上抛");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    }

    /// 一次读取里的多个按键只落一帧：长粘贴不得按字符数写出 N 帧（按键重绘按读取区间合并）。
    #[test]
    fn one_read_batch_writes_at_most_one_redraw_frame() {
        let dir = TempDir::new("batch");
        // 50 个字符一次读完（没有回车：下一次读取是 EOF）
        let text = "a".repeat(50);
        let (mut reader, out) = editor(text.as_bytes(), &dir.history_path());

        assert_eq!(
            reader.read_line(PROMPT).unwrap(),
            None,
            "没有回车就没有提交"
        );

        let written = out.text();
        // 帧的开头都是 `\r\x1b[J`：提示符一帧 + 这批按键合并成一帧
        let frames = written.matches("\r\u{1b}[J").count();
        assert_eq!(frames, 2, "50 个字符只该写两帧：{written:?}");
        assert!(
            written.ends_with(&draw::frame(PROMPT, CONTINUATION, &text, 50)),
            "合并的那一帧要画全部 50 个字符：{written:?}"
        );
    }

    /// 粘贴区间里的换行只进缓冲：一段粘贴提交一次，内部换行原样保留（续行提示符画出来）。
    #[test]
    fn paste_never_submits_and_keeps_internal_newlines() {
        let dir = TempDir::new("paste");
        let (mut reader, out) = editor(b"\x1b[200~a\nb\x1b[201~\r", &dir.history_path());

        assert_eq!(reader.read_line(PROMPT).unwrap(), Some("a\nb".to_string()));

        let text = out.text();
        assert!(
            text.contains("❯ a\r\n… b"),
            "两行都要画（续行提示符）：{text:?}"
        );
    }

    /// 非法 UTF-8：解码器插替换字符、读取层打**恰好一行**告警，然后照常重绘。
    #[test]
    fn invalid_bytes_warn_once_and_insert_replacement() {
        let dir = TempDir::new("invalid");
        // `\xff` 是一个确定的坏序列，后面跟着 `你好` 与回车
        let (mut reader, out) = editor(b"\xff\xe4\xbd\xa0\xe5\xa5\xbd\r", &dir.history_path());

        assert_eq!(
            reader.read_line(PROMPT).unwrap(),
            Some("\u{FFFD}你好".to_string())
        );

        let text = out.text();
        assert_eq!(
            text.matches("warning:").count(),
            1,
            "告警恰好一行：{text:?}"
        );
        assert!(text.contains(INVALID_WARNING), "{text:?}");
        assert!(
            text.contains("❯ \u{FFFD}你好"),
            "重绘要把替换字符画出来：{text:?}"
        );
    }

    /// Tab 唯一命中：整行换成候选，提交的就是补全后的文本。
    #[test]
    fn tab_accepts_a_unique_command_prefix() {
        let dir = TempDir::new("tab-accept");
        let (mut reader, out) = editor(b"/he\t\r", &dir.history_path());

        assert_eq!(reader.read_line(PROMPT).unwrap(), Some("/help".to_string()));
        assert!(
            out.text().contains("❯ /help"),
            "补全后要重绘输入行：{}",
            out.text()
        );
    }

    /// Tab 多候选：逐行列出候选，输入行与缓冲原样（后面的 EOF 只说明输入到此为止）。
    #[test]
    fn tab_lists_candidates_without_changing_the_input() {
        let dir = TempDir::new("tab-list");
        let (mut reader, out) = editor(b"/\t", &dir.history_path());

        assert_eq!(reader.read_line(PROMPT).unwrap(), None);
        let text = out.text();
        assert!(text.contains("/memory"), "{text:?}");
        assert!(text.contains("/status"), "{text:?}");
        assert!(
            text.ends_with(&draw::frame(PROMPT, CONTINUATION, "/", 1)),
            "列完候选要把 `/` 的输入行画回来：{text:?}"
        );
    }

    /// 两次读取共用一个编辑器：历史（上一次提交）与 ↑ 召回都在第二次生效。两次输入落在同一批
    /// 字节里（一次 `read` 全拿到），回车之后的按键由队列留给下一次 `read_line`。
    #[test]
    fn up_arrow_recalls_the_previous_submission() {
        let dir = TempDir::new("recall");
        let (mut reader, _out) = editor("第一次\r\x1b[A\r".as_bytes(), &dir.history_path());

        assert_eq!(
            reader.read_line(PROMPT).unwrap(),
            Some("第一次".to_string())
        );
        assert_eq!(
            reader.read_line(PROMPT).unwrap(),
            Some("第一次".to_string()),
            "↑ 召回上一次提交，回车把它再交回一次"
        );
    }

    /// P6 T2 修复：折行块里的连续两帧之间，读取层先把光标提回块首（`\r` + `\x1b[{n}A`），
    /// 帧自己的字节一字不改；上一帧的光标本来就在块首（`n == 0`）时不写移动序列。
    #[test]
    fn redraw_reanchors_at_the_block_top_with_the_previous_cursor_row() {
        let dir = TempDir::new("reanchor");
        let (mut reader, out) = editor(b"", &dir.history_path());
        // 单测进程的 stdout 不是 tty：`read_line` 会探出 `usize::MAX`（不折行）。这条用例要的是
        // 真终端口径的 5 列（`❯ ` 2 列 → 每行 3 列可用），所以把宽度钉死再走绘制的两条路。
        reader.width = 5;
        reader.state.replace("abcdef");

        // 绘制助手本身的口径：光标在块首（0）时交回的就是帧自己，一个字都不多写
        let (plain, row) = draw_sequence(0, PROMPT, "abcdef", 6, 5);
        assert_eq!(
            plain,
            draw::frame_with(PROMPT, CONTINUATION, "abcdef", 6, 5)
        );
        assert_eq!(row, 1, "`abcdef` 在 5 列里折成两行，末尾的光标在第 1 行");

        reader.draw(PROMPT).unwrap();
        let first = out.take();
        assert_eq!(
            first,
            draw::frame_with(PROMPT, CONTINUATION, "abcdef", 6, 5),
            "第一帧：上一帧的光标在块首"
        );
        assert!(
            !first.contains("\u{1b}[0A"),
            "`n == 0` 不写移动序列：{first:?}"
        );

        // 上一帧把光标留在了块的第 1 行：这一帧前面就多一条回块首的移动（帧本身逐字节不变）
        reader.draw(PROMPT).unwrap();
        assert_eq!(
            out.take(),
            format!(
                "\r\u{1b}[1A{}",
                draw::frame_with(PROMPT, CONTINUATION, "abcdef", 6, 5)
            )
        );

        // 换成三行的文本：数跟着**上一帧**留下的行（还是 1）走，之后才是这一帧自己的行（2）
        reader.state.replace("abcdefghi");
        reader.draw(PROMPT).unwrap();
        assert!(
            out.take().starts_with("\r\u{1b}[1A"),
            "重锚按上一帧的光标行走，不是这一帧的"
        );
        reader.draw(PROMPT).unwrap();
        assert_eq!(
            out.take(),
            format!(
                "\r\u{1b}[2A{}",
                draw::frame_with(PROMPT, CONTINUATION, "abcdefghi", 9, 5)
            ),
            "三行块的光标在最后一行：这一帧的回提是 2"
        );
    }

    /// 测试用的极简 ANSI 屏幕：只认读取层会写的那几条序列，够用来数「屏幕上到底留了几份输入行」。
    ///
    /// 认 `\r`、`\n`（带滚动）、`\x1b[J`（清到屏幕底）、`\x1b[K`（清到行尾）、`\x1b[{n}A`（上移）、
    /// `\x1b[{n}C`（右移）、宽字符（占两格）与**待折行**语义（写满最后一列后光标停在该列，下一个
    /// 字符才折行——终端不会把「正好写满」当成换行）。其余序列一律 panic：读取层不该写。
    struct Screen {
        /// 终端列数 / 行数。
        cols: usize,
        rows: usize,
        /// 屏幕格子：`cells[行][列]`，宽字符的第二格是 [`WIDE_TAIL`]。
        cells: Vec<Vec<char>>,
        /// 光标的位置与「待折行」标记。
        row: usize,
        col: usize,
        wrap_pending: bool,
    }

    /// 宽字符占的第二格：渲染可见文本时写成空格（保持列对齐）。
    const WIDE_TAIL: char = '\u{0}';

    impl Screen {
        fn new(cols: usize, rows: usize) -> Self {
            assert!(cols >= 2, "屏幕仿真器至少要有 2 列（宽字符占两格）");
            Self {
                cols,
                rows,
                cells: vec![vec![' '; cols]; rows],
                row: 0,
                col: 0,
                wrap_pending: false,
            }
        }

        /// 喂一段读取层写出的字节。
        fn feed(&mut self, bytes: &str) {
            let mut chars = bytes.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\r' => {
                        self.col = 0;
                        self.wrap_pending = false;
                    }
                    // LF：只往下走一行（读取层的换行都是 `\r\n`，列由 `\r` 归零）
                    '\n' => self.down_one_row(),
                    '\u{1b}' => {
                        assert_eq!(chars.next(), Some('['), "读取层只写 CSI 序列：{bytes:?}");
                        let mut params = String::new();
                        while let Some(&next) = chars.peek() {
                            if ('@'..='~').contains(&next) {
                                break;
                            }
                            params.push(chars.next().expect("peek 过"));
                        }
                        let final_byte = chars.next().expect("CSI 有终结字节");
                        self.csi(&params, final_byte, bytes);
                    }
                    c => self.put(c),
                }
            }
        }

        /// 一条 CSI：`J` 清到屏幕底、`K` 清到行尾、`A` 上移、`C` 右移（参数缺省 1 次）。
        fn csi(&mut self, params: &str, final_byte: char, bytes: &str) {
            let count = if params.is_empty() {
                1
            } else {
                params.parse().unwrap_or(1)
            };
            match final_byte {
                'J' => {
                    self.clear_to_end_of_line();
                    for row in self.row + 1..self.rows {
                        self.cells[row].fill(' ');
                    }
                }
                'K' => self.clear_to_end_of_line(),
                'A' => {
                    self.row = self.row.saturating_sub(count);
                    self.wrap_pending = false;
                }
                'C' => {
                    self.col = (self.col + count).min(self.cols - 1);
                    self.wrap_pending = false;
                }
                other => panic!("屏幕仿真器不认识的序列 \\x1b[{params}{other}：{bytes:?}"),
            }
        }

        /// 写一个可见字符：待折行先落地，放不下的宽字符整字挪到下一行（终端不切开它）。
        fn put(&mut self, c: char) {
            let width = char_width(c);
            if self.wrap_pending || self.col + width > self.cols {
                self.col = 0;
                self.down_one_row();
            }
            for offset in 0..width {
                self.cells[self.row][self.col + offset] = if offset == 0 { c } else { WIDE_TAIL };
            }
            self.col += width;
            if self.col >= self.cols {
                // 正好写满最后一列：光标停在该列，下一个字符才折行
                self.col = self.cols - 1;
                self.wrap_pending = true;
            }
        }

        /// 光标下移一行；已经在最后一行就滚动（整屏上移一行，底部补一空行）。
        fn down_one_row(&mut self) {
            self.wrap_pending = false;
            if self.row + 1 >= self.rows {
                self.cells.remove(0);
                self.cells.push(vec![' '; self.cols]);
            } else {
                self.row += 1;
            }
        }

        /// 从光标清到行尾。
        fn clear_to_end_of_line(&mut self) {
            self.cells[self.row][self.col..].fill(' ');
            self.wrap_pending = false;
        }

        /// 屏幕上的可见文本：每行去掉右侧空白，宽字符的第二格渲染成空格（保持列对齐）。
        fn visible(&self) -> Vec<String> {
            self.cells
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|&c| if c == WIDE_TAIL { ' ' } else { c })
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect()
        }
    }

    /// 屏幕上剩下的输入字符个数：一份 100 个字符的输入行 = 100，堆了 N 份就会数出更多。
    fn input_chars_on_screen(screen: &[String]) -> usize {
        screen.iter().map(|row| row.matches('x').count()).sum()
    }

    /// P6 T2 修复的回归：超宽输入行在**逐键重绘**下不再一份一份往下堆。
    ///
    /// 走读取层真实的绘制步骤（提示符一帧 → 每敲一个字符画一帧 → 提交一帧），只是绕开 `read_line`
    /// 的 tty 探测与按批合并：单测的 stdout 不是 tty，`read_line` 会探出 `usize::MAX`（不折行），
    /// 而这条回归要的是真终端的 40 列；交互式输入本来也是一次读一个按键、一个按键一次重绘。
    /// 屏幕仿真器照终端语义重放这些字节，于是能直接问「屏幕上到底有几份输入行」。
    #[test]
    fn wrapped_input_line_is_redrawn_in_place_keystroke_by_keystroke() {
        let dir = TempDir::new("stack");
        let (mut reader, out) = editor(b"", &dir.history_path());
        reader.width = 40;
        let mut screen = Screen::new(40, 12);

        reader.draw(PROMPT).unwrap();
        screen.feed(&out.take());

        let line = "x".repeat(100);
        for c in line.chars() {
            assert_eq!(reader.state.handle(Key::Char(c)), Event::Redraw);
            reader.draw(PROMPT).unwrap();
            screen.feed(&out.take());
        }

        // 40 列、标记 2 列 → 每行 38 个字符：100 个字符正好折成 38 + 38 + 24 三行，一份都不多。
        // 修复前每一帧只从光标那一行往下清，块被自己再画一遍、光标又被留在更低处，屏幕上会叠出
        // 好几份（越高越往下沉，直到滚出屏幕）。
        let typed = screen.visible();
        assert_eq!(typed[0], format!("❯ {}", "x".repeat(38)));
        assert_eq!(typed[1], format!("… {}", "x".repeat(38)));
        assert_eq!(typed[2], format!("… {}", "x".repeat(24)));
        assert!(
            typed[3..].iter().all(String::is_empty),
            "块下面不该还有内容：{typed:#?}"
        );
        assert_eq!(
            input_chars_on_screen(&typed),
            100,
            "100 个字符在屏幕上只该是一份：{typed:#?}"
        );

        // 提交帧同样先重锚：定稿的输入行留在屏幕上，还是一份（光标落到块的下一行）
        reader.finish_submit(PROMPT, &line).unwrap();
        screen.feed(&out.take());
        let submitted = screen.visible();
        assert_eq!(submitted[..3], typed[..3], "定稿的输入行留在屏幕上");
        assert!(
            submitted[3..].iter().all(String::is_empty),
            "提交不该再多出一份：{submitted:#?}"
        );
        assert_eq!(
            input_chars_on_screen(&submitted),
            100,
            "提交之后仍然只有一份：{submitted:#?}"
        );
    }
}
