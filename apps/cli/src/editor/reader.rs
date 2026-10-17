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

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use super::draw::{self, CONTINUATION};
use super::history::History;
use super::keys::{Decoder, Key};
use super::raw::RawMode;
use super::state::{EditorState, Event};
use crate::commands::slash::{self, Completion};
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
    fn draw(&mut self, prompt: &str) -> io::Result<()> {
        let text = self.state.text();
        let cursor = self.state.cursor();
        let frame = draw::frame(prompt, CONTINUATION, &text, cursor);
        self.write_bytes(frame.as_bytes())
    }

    /// 提交：先把「光标在末尾」的一帧留在屏幕上（文本从事件里拿：状态机手里那份已经被清空），
    /// 再写 `\r\n` 把光标送到下一行——输入行原样留在屏幕上，与 readline 的观感一致。
    fn finish_submit(&mut self, prompt: &str, text: &str) -> io::Result<()> {
        let frame = draw::frame(prompt, CONTINUATION, text, text.chars().count());
        self.write_bytes(frame.as_bytes())?;
        self.write_bytes(b"\r\n")
    }

    /// 非法 UTF-8 的告警：清掉输入行 → 一行告警（调用方随后照常重绘输入行）。
    fn warn_invalid(&mut self) -> io::Result<()> {
        self.output.write_all(CLEAR_LINE)?;
        self.output.write_all(INVALID_WARNING.as_bytes())?;
        self.write_bytes(b"\r\n")
    }

    /// 候选列表：清掉输入行 → 逐行列出 → 把输入行画回来（缓冲与光标都原样）。
    fn write_listing(&mut self, prompt: &str, lines: &[String]) -> io::Result<()> {
        self.output.write_all(CLEAR_LINE)?;
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

    use super::{EditorReader, INVALID_WARNING};
    use crate::editor::draw::{self, CONTINUATION};

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
}
