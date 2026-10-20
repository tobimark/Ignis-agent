//! 编辑器状态机（纯逻辑）：按键进、事件出，不碰终端、不做 I/O。
//!
//! 分工：解码层（[`super::keys`]）把字节流翻成 [`Key`]，本模块维护编辑状态并回一个 [`Event`]，
//! 读取层（Task 7）按事件决定要不要重绘、提交或退出，重绘的字节由 [`super::draw`] 拼。状态机
//! 自己只认识四样东西：字符缓冲、光标（字符下标）、历史浏览位置（`view`）与进入浏览前留下的
//! 草稿（`draft`）。
//!
//! 几条钉死的口径：
//!
//! - **缓冲与光标都是字符**：`buffer: Vec<char>`、光标是字符下标——既不是字节，也不是显示列。
//!   `Left`/`Right` 一步一个字符，中日韩宽字符同样只算一格；列数只在 [`super::draw`] 里算。
//! - **多行是一段文本**：`Newline`（Ctrl+J / Alt+Enter / 粘贴里的换行）只往缓冲里插 `\n`，
//!   永不提交；只有 `Enter` 提交，提交的是**整段**文本（内部换行保留）。多行时 `Home`/`End`
//!   到的是整段的首尾，不是当前行的。
//! - **`Ctrl+D`**：缓冲为空才是 EOF（退出语义与 v0.1 一致）；有内容时等价 `Delete`。
//! - **历史导航**：`Up` 往更旧走、`Down` 往更新走；到两头（最旧一条之后 / 回到草稿之后）只有
//!   `Idle`。除 `Up`/`Down` 之外的**任何**按键都脱离浏览：缓冲保留被召回的那一条、光标落到
//!   末尾、草稿丢弃——「召回之后接着打字 = 接在末尾」。
//! - **只在内容变化时 `Redraw`**：空操作（行首退格、末尾删除、到头的光标移动、已经是空的
//!   `Ctrl+U`）一律 `Idle`，读取层因此不会白写一帧。
//! - `Tab`/粘贴标记/`CtrlC`/`Other`/`PageUp`/`PageDown` 都是 `Idle`：Tab 由读取层走补全（本模块
//!   不认识候选），粘贴标记只是区间记号，空闲 Ctrl+C 的退出语义在 REPL 侧；`PageUp`/`PageDown`
//!   在 TUI 里由应用层接手（翻滚动区），经典路径与它们此前当 `Other` 时逐字一致。

use super::history::History;
use super::keys::Key;

/// 一次按键处理的结果：读取层按它决定写什么、要不要继续。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// 什么都没变：不重绘。
    Idle,
    /// 缓冲或光标变了：按新的 `text`/`cursor` 重绘一帧。
    Redraw,
    /// 提交：整段文本（内部换行保留），缓冲已清空。
    Submit(String),
    /// 空缓冲上按了 `Ctrl+D`：读取层收摊（EOF）。
    Eof,
}

/// 行编辑器状态机。
pub struct EditorState {
    /// 输入缓冲：字符序列（`\n` 也在里面，多行就是一段带换行的文本）。
    buffer: Vec<char>,
    /// 光标：`buffer` 的字符下标（`0` = 段首，`buffer.len()` = 段尾）。
    cursor: usize,
    /// 输入历史：提交时 `push`、导航时 `get`。
    history: History,
    /// 正在浏览的历史条目下标（`0` = 最新一条）；`None` = 不在浏览态。
    view: Option<usize>,
    /// 进入浏览态之前的缓冲：`Down` 回到最新之后要原样还回来。
    draft: Vec<char>,
}

impl EditorState {
    /// 空状态：历史由调用方注入（单测给内存历史，生产给 `~/.ignis/history` 载入的那份）。
    pub fn new(history: History) -> Self {
        Self {
            buffer: Vec::new(),
            cursor: 0,
            history,
            view: None,
            draft: Vec::new(),
        }
    }

    /// 当前缓冲的文本（提交与 Tab 补全都以它为准）。
    pub fn text(&self) -> String {
        self.buffer.iter().collect()
    }

    /// 光标位置：`buffer` 的字符下标。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 整段替换缓冲并退出浏览态（补全接受候选时用）：光标落到末尾。
    pub fn replace(&mut self, text: &str) {
        self.buffer = text.chars().collect();
        self.cursor = self.buffer.len();
        self.leave_browse();
    }

    /// 处理一个按键，返回读取层要执行的动作。
    pub fn handle(&mut self, key: Key) -> Event {
        match key {
            Key::Char(c) => self.insert(c),
            Key::Newline => self.insert('\n'),
            Key::Enter => self.submit(),
            Key::Up => self.recall_older(),
            Key::Down => self.recall_newer(),
            Key::Left => self.step_left(),
            Key::Right => self.step_right(),
            // Ctrl+A / Ctrl+E 在解码层就归到 Home / End（Task 2），这里只有这一条路。
            Key::Home => self.move_to(0),
            Key::End => self.move_to(self.buffer.len()),
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete_at_cursor(),
            Key::CtrlD => self.ctrl_d(),
            Key::CtrlU => self.clear(),
            // 都不改内容，所以是 Idle；但它们同样算「非导航键」，一样脱离浏览。
            // `Esc` 只由 TUI 的 Esc 消歧合成（解码器不产生它）：状态机忽略，语义归选择器那一层。
            // `PageUp`/`PageDown`（`\x1b[5~`/`\x1b[6~`）在经典路径里与 `Other` 逐字同一条处理：
            // 它们此前就是 `Other`（增量新增只给变体起了名字），字节级行为一字不改；翻页的落点
            // 只有 TUI 的编辑器态认识（`TuiApp::on_bytes`），状态机这里仍是空操作。
            Key::Tab
            | Key::PasteStart
            | Key::PasteEnd
            | Key::CtrlP
            | Key::CtrlC
            | Key::Esc
            | Key::PageUp
            | Key::PageDown
            | Key::Other => {
                self.leave_browse();
                Event::Idle
            }
        }
    }

    /// 在光标处插入一个字符（光标右边的字符右移）。
    fn insert(&mut self, c: char) -> Event {
        self.leave_browse();
        self.buffer.insert(self.cursor, c);
        self.cursor += 1;
        Event::Redraw
    }

    /// 提交整段文本：入史、清空缓冲与浏览态。
    fn submit(&mut self) -> Event {
        let text = self.text();
        // 空行与相邻重复由 `History` 负责（它才认识「最新一条」）；状态机只管把手里的文本交出去。
        self.history.push(&text);
        self.buffer.clear();
        self.cursor = 0;
        self.leave_browse();
        Event::Submit(text)
    }

    /// `Up`：往更旧的一条走；到最旧（或历史为空）只有 `Idle`。
    ///
    /// 第一次离开自己的输入时先把它记成草稿，`Down` 回到最新之后要原样还回来。
    fn recall_older(&mut self) -> Event {
        let next = match self.view {
            None => 0,
            Some(index) => index.saturating_add(1),
        };
        let Some(entry) = self.history.get(next) else {
            return Event::Idle;
        };
        if self.view.is_none() {
            self.draft = std::mem::take(&mut self.buffer);
        }
        self.view = Some(next);
        self.buffer = entry.chars().collect();
        self.cursor = self.buffer.len();
        Event::Redraw
    }

    /// `Down`：往更新的一条走；走到最新之后再按一次就回到进历史前的草稿，再按只有 `Idle`。
    fn recall_newer(&mut self) -> Event {
        let Some(index) = self.view else {
            return Event::Idle;
        };
        if index == 0 {
            // 回到草稿（进历史前的那份输入）
            self.buffer = std::mem::take(&mut self.draft);
            self.view = None;
            self.cursor = self.buffer.len();
            return Event::Redraw;
        }
        match self.history.get(index - 1) {
            Some(entry) => {
                self.buffer = entry.chars().collect();
                self.cursor = self.buffer.len();
                self.view = Some(index - 1);
                Event::Redraw
            }
            // 条目不见了（理论上不会发生）：留在原地。
            None => Event::Idle,
        }
    }

    /// `←`：光标左移一个字符；已经在段首就只有 `Idle`。
    fn step_left(&mut self) -> Event {
        self.leave_browse();
        if self.cursor == 0 {
            return Event::Idle;
        }
        self.cursor -= 1;
        Event::Redraw
    }

    /// `→`：光标右移一个字符；已经在段尾就只有 `Idle`。
    fn step_right(&mut self) -> Event {
        self.leave_browse();
        if self.cursor >= self.buffer.len() {
            return Event::Idle;
        }
        self.cursor += 1;
        Event::Redraw
    }

    /// `Home`/`End`：把光标落到给定下标；位置没变就只有 `Idle`。
    fn move_to(&mut self, position: usize) -> Event {
        self.leave_browse();
        let position = position.min(self.buffer.len());
        if self.cursor == position {
            return Event::Idle;
        }
        self.cursor = position;
        Event::Redraw
    }

    /// `Backspace`：删光标前一个字符；行首（或空缓冲）只有 `Idle`。
    fn backspace(&mut self) -> Event {
        self.leave_browse();
        if self.cursor == 0 {
            return Event::Idle;
        }
        self.cursor -= 1;
        self.buffer.remove(self.cursor);
        Event::Redraw
    }

    /// `Delete`：删光标处字符；缓冲末尾只有 `Idle`。
    fn delete_at_cursor(&mut self) -> Event {
        self.leave_browse();
        if self.cursor >= self.buffer.len() {
            return Event::Idle;
        }
        self.buffer.remove(self.cursor);
        Event::Redraw
    }

    /// `Ctrl+D`：缓冲为空 = `Eof`（退出语义与 v0.1 一致）；否则等价 `Delete`。
    fn ctrl_d(&mut self) -> Event {
        if self.buffer.is_empty() {
            return Event::Eof;
        }
        self.delete_at_cursor()
    }

    /// `Ctrl+U`：清空整段缓冲（光标回到段首）；已经是空的就只有 `Idle`。
    fn clear(&mut self) -> Event {
        self.leave_browse();
        if self.buffer.is_empty() {
            return Event::Idle;
        }
        self.buffer.clear();
        self.cursor = 0;
        Event::Redraw
    }

    /// 非导航键一律脱离浏览：缓冲保留被召回的那一条、光标落到末尾、草稿丢弃。
    ///
    /// 光标落到末尾是「召回之后接着打字 = 接在末尾」的口径；调用方随后做的移动/删除照常在
    /// 这个位置上生效（例如召回后按 `←` 就是在末尾的基础上左移一格）。
    fn leave_browse(&mut self) {
        if self.view.take().is_some() {
            self.cursor = self.buffer.len();
        }
        self.draft.clear();
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::history::History;
    use crate::editor::keys::{Decoder, Key};

    use super::{EditorState, Event};

    /// 新建状态机：历史只在内存里（单测不碰 `~/.ignis/history`）。
    fn editor() -> EditorState {
        EditorState::new(History::in_memory())
    }

    /// 逐个字符敲进去（与真实键盘路径一致：一次一个 `Key::Char`）。
    fn type_text(state: &mut EditorState, text: &str) {
        for c in text.chars() {
            state.handle(Key::Char(c));
        }
    }

    /// 敲入并回车提交，返回提交事件。
    fn submit(state: &mut EditorState, text: &str) -> Event {
        type_text(state, text);
        state.handle(Key::Enter)
    }

    /// `Event` 是 `Debug + Clone + PartialEq + Eq`（读取层按值比较与传递）。
    fn round_trip<T: Clone + Eq + std::fmt::Debug>(value: T) -> T {
        value
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut state = editor();
        assert_eq!(state.text(), "");
        assert_eq!(state.cursor(), 0);

        assert_eq!(state.handle(Key::Char('a')), Event::Redraw);
        assert_eq!(state.handle(Key::Char('你')), Event::Redraw);
        assert_eq!(state.text(), "a你");
        assert_eq!(state.cursor(), 2, "光标是字符下标：`你` 只算一格");

        // ← 回一格后从中间插入：插在光标处，光标右边的 `你` 右移
        assert_eq!(state.handle(Key::Left), Event::Redraw);
        assert_eq!(state.cursor(), 1);
        assert_eq!(state.handle(Key::Char('好')), Event::Redraw);
        assert_eq!(state.text(), "a好你");
        assert_eq!(state.cursor(), 2);

        // 接口上的 derive 约束也在这一条里过一遍
        assert_eq!(
            round_trip(Event::Submit("草稿".to_string())),
            Event::Submit("草稿".to_string())
        );
    }

    #[test]
    fn home_end_and_arrows_move_by_char() {
        let mut state = editor();
        // 中文按「一个字符」算一步：不是字节数（6 字节），也不是显示宽度（4 列）
        type_text(&mut state, "你好ab");
        assert_eq!(state.cursor(), 4);

        assert_eq!(state.handle(Key::Home), Event::Redraw);
        assert_eq!(state.cursor(), 0);
        assert_eq!(state.handle(Key::Left), Event::Idle, "行首再左移是空操作");
        assert_eq!(state.handle(Key::Right), Event::Redraw);
        assert_eq!(state.cursor(), 1);
        assert_eq!(state.handle(Key::Right), Event::Redraw);
        assert_eq!(state.cursor(), 2, "`你好` 是两步");
        assert_eq!(state.handle(Key::End), Event::Redraw);
        assert_eq!(state.cursor(), 4);
        assert_eq!(state.handle(Key::Right), Event::Idle, "行尾再右移是空操作");
        assert_eq!(state.handle(Key::End), Event::Idle, "已经在行尾");

        // Ctrl+A / Ctrl+E 在解码层就归到 Home/End（Task 2），状态机只有这一条路
        assert_eq!(state.handle(Key::Home), Event::Redraw);
        assert_eq!(state.handle(Key::Right), Event::Redraw);
        assert_eq!(state.handle(Key::Char('你')), Event::Redraw);
        assert_eq!(state.text(), "你你好ab");
        assert_eq!(state.cursor(), 2);
    }

    #[test]
    fn backspace_and_delete_edit_around_the_cursor() {
        let mut state = editor();
        type_text(&mut state, "abc");
        assert_eq!(state.handle(Key::Backspace), Event::Redraw);
        assert_eq!(state.text(), "ab");
        assert_eq!(state.cursor(), 2);

        assert_eq!(state.handle(Key::Home), Event::Redraw);
        assert_eq!(state.handle(Key::Backspace), Event::Idle, "行首没有可删的");
        assert_eq!(state.text(), "ab");
        assert_eq!(state.handle(Key::Delete), Event::Redraw);
        assert_eq!(state.text(), "b");
        assert_eq!(state.cursor(), 0);
        assert_eq!(state.handle(Key::End), Event::Redraw);
        assert_eq!(state.handle(Key::Delete), Event::Idle, "缓冲末尾没有可删的");
        assert_eq!(state.text(), "b");

        // 中文：删的是整个字符
        let mut cjk = editor();
        type_text(&mut cjk, "你好");
        assert_eq!(cjk.handle(Key::Left), Event::Redraw);
        assert_eq!(cjk.cursor(), 1);
        assert_eq!(cjk.handle(Key::Delete), Event::Redraw);
        assert_eq!(cjk.text(), "你");
        assert_eq!(cjk.cursor(), 1);
        assert_eq!(cjk.handle(Key::Backspace), Event::Redraw);
        assert_eq!(cjk.text(), "");
        assert_eq!(cjk.cursor(), 0);

        // 空缓冲：两个删除键都是空操作
        let mut empty = editor();
        assert_eq!(empty.handle(Key::Backspace), Event::Idle);
        assert_eq!(empty.handle(Key::Delete), Event::Idle);
        assert_eq!(empty.text(), "");
    }

    #[test]
    fn ctrl_u_clears_the_input() {
        let mut state = editor();
        // 多行也一并清掉
        type_text(&mut state, "第一行");
        assert_eq!(state.handle(Key::Newline), Event::Redraw);
        type_text(&mut state, "第二行");
        assert_eq!(state.handle(Key::Home), Event::Redraw);

        assert_eq!(state.handle(Key::CtrlU), Event::Redraw);
        assert_eq!(state.text(), "");
        assert_eq!(state.cursor(), 0);
        assert_eq!(
            state.handle(Key::CtrlU),
            Event::Idle,
            "已经是空的：不白写一帧"
        );
    }

    #[test]
    fn enter_submits_multi_line_text_and_clears_the_buffer() {
        let mut state = editor();
        type_text(&mut state, "第一行");
        assert_eq!(state.handle(Key::Newline), Event::Redraw);
        type_text(&mut state, "第二行");
        assert_eq!(
            state.handle(Key::Enter),
            Event::Submit("第一行\n第二行".to_string()),
            "提交的是整段文本：内部换行原样保留"
        );
        assert_eq!(state.text(), "");
        assert_eq!(state.cursor(), 0);
        // 缓冲已清空：再回车提交的是空串（空行怎么处理由 REPL 侧决定）
        assert_eq!(state.handle(Key::Enter), Event::Submit(String::new()));
    }

    #[test]
    fn ctrl_j_and_alt_enter_insert_newlines_never_submit() {
        // Ctrl+J（0x0A）与 Alt+Enter（`\x1b\r`）在解码层都是 `Key::Newline`：只插换行、永不提交
        let mut state = editor();
        type_text(&mut state, "a");
        assert_eq!(state.handle(Key::Newline), Event::Redraw);
        type_text(&mut state, "b");
        assert_eq!(state.handle(Key::Newline), Event::Redraw);
        assert_eq!(state.text(), "a\nb\n");
        assert_eq!(state.cursor(), 4);
        // 提交只认 Enter
        assert_eq!(
            state.handle(Key::Enter),
            Event::Submit("a\nb\n".to_string())
        );

        // 多行时的 Home/End 是整段的首尾（不是当前行的）
        let mut multi = editor();
        type_text(&mut multi, "a");
        assert_eq!(multi.handle(Key::Newline), Event::Redraw);
        type_text(&mut multi, "bc");
        assert_eq!(multi.cursor(), 4);
        assert_eq!(multi.handle(Key::Home), Event::Redraw);
        assert_eq!(multi.cursor(), 0, "Home 到整段首");
        assert_eq!(multi.handle(Key::End), Event::Redraw);
        assert_eq!(multi.cursor(), 4, "End 到整段尾");
    }

    #[test]
    fn up_and_down_walk_history_and_restore_the_draft() {
        // 空历史：两头都是 Idle
        let mut empty = editor();
        assert_eq!(empty.handle(Key::Up), Event::Idle);
        assert_eq!(empty.handle(Key::Down), Event::Idle);

        let mut state = editor();
        assert_eq!(
            submit(&mut state, "第一次"),
            Event::Submit("第一次".to_string())
        );
        assert_eq!(
            submit(&mut state, "第二次"),
            Event::Submit("第二次".to_string())
        );

        // 输入中的草稿：↑ 进历史之前先记下来
        type_text(&mut state, "草稿");
        assert_eq!(state.handle(Key::Up), Event::Redraw);
        assert_eq!(state.text(), "第二次", "↑ 是最新的一条");
        assert_eq!(state.cursor(), 3, "召回时光标落到末尾");
        assert_eq!(state.handle(Key::Up), Event::Redraw);
        assert_eq!(state.text(), "第一次");
        assert_eq!(state.handle(Key::Up), Event::Idle, "到最旧了");
        assert_eq!(state.text(), "第一次");

        // ↓ 往回走，到最新之后回到草稿，再按才是 Idle
        assert_eq!(state.handle(Key::Down), Event::Redraw);
        assert_eq!(state.text(), "第二次");
        assert_eq!(state.handle(Key::Down), Event::Redraw);
        assert_eq!(state.text(), "草稿");
        assert_eq!(state.cursor(), 2, "回到草稿时光标也落到末尾");
        assert_eq!(state.handle(Key::Down), Event::Idle, "已经回到草稿");
        assert_eq!(state.text(), "草稿");
    }

    #[test]
    fn editing_a_recalled_entry_detaches_from_history() {
        let mut state = editor();
        assert_eq!(
            submit(&mut state, "第一条"),
            Event::Submit("第一条".to_string())
        );
        assert_eq!(
            submit(&mut state, "第二条"),
            Event::Submit("第二条".to_string())
        );

        // 草稿 → ↑ 召回 → 打一个字：脱离浏览，插入接在被召回内容的末尾
        type_text(&mut state, "草稿");
        assert_eq!(state.handle(Key::Up), Event::Redraw);
        assert_eq!(state.text(), "第二条");
        assert_eq!(state.handle(Key::Char('!')), Event::Redraw);
        assert_eq!(state.text(), "第二条!");
        assert_eq!(state.cursor(), 4);
        assert_eq!(state.handle(Key::Down), Event::Idle, "脱离之后草稿不再回来");
        assert_eq!(state.text(), "第二条!");

        // 再召回一条（这次草稿是「第二条!」），退格：脱离时先落到末尾再删
        assert_eq!(state.handle(Key::Up), Event::Redraw);
        assert_eq!(state.text(), "第二条");
        assert_eq!(state.cursor(), 3);
        assert_eq!(state.handle(Key::Backspace), Event::Redraw);
        assert_eq!(state.text(), "第二");
        assert_eq!(state.cursor(), 2);
        assert_eq!(state.handle(Key::Down), Event::Idle, "草稿随脱离一起丢弃");
        assert_eq!(state.text(), "第二");
    }

    #[test]
    fn ctrl_d_is_eof_only_on_an_empty_buffer() {
        let mut state = editor();
        assert_eq!(state.handle(Key::CtrlD), Event::Eof, "空缓冲 = EOF（退出）");

        type_text(&mut state, "ab");
        assert_eq!(state.handle(Key::Home), Event::Redraw);
        assert_eq!(
            state.handle(Key::CtrlD),
            Event::Redraw,
            "有内容时等价 Delete"
        );
        assert_eq!(state.text(), "b");
        assert_eq!(state.cursor(), 0);
        assert_eq!(state.handle(Key::End), Event::Redraw);
        assert_eq!(state.handle(Key::CtrlD), Event::Idle, "末尾没有可删的");
        assert_eq!(state.text(), "b");
        assert_eq!(state.handle(Key::CtrlU), Event::Redraw);
        assert_eq!(state.handle(Key::CtrlD), Event::Eof, "又空了：还是 EOF");
    }

    #[test]
    fn tab_and_paste_markers_are_no_ops() {
        let mut state = editor();
        type_text(&mut state, "ab");
        for key in [
            Key::Tab,
            Key::PasteStart,
            Key::PasteEnd,
            Key::CtrlC,
            Key::PageUp,
            Key::PageDown,
            Key::Other,
        ] {
            assert_eq!(state.handle(key), Event::Idle, "{key:?} 不该触发重绘");
            assert_eq!(state.text(), "ab");
            assert_eq!(state.cursor(), 2);
        }

        // 「除 ↑/↓ 之外的按键都脱离浏览」：这些键同样把草稿丢掉（内容不变，所以还是 Idle）
        let mut browsing = editor();
        assert_eq!(
            submit(&mut browsing, "第一条"),
            Event::Submit("第一条".to_string())
        );
        type_text(&mut browsing, "草稿");
        assert_eq!(browsing.handle(Key::Up), Event::Redraw);
        assert_eq!(browsing.text(), "第一条");
        assert_eq!(browsing.handle(Key::PasteEnd), Event::Idle);
        assert_eq!(browsing.text(), "第一条", "标记键不改内容");
        assert_eq!(
            browsing.handle(Key::Down),
            Event::Idle,
            "草稿随脱离一起丢弃"
        );
    }

    /// F2（终审修复波）：`\x1b[5~`/`\x1b[6~` 在经典路径上逐字节不变——解码成 `PageUp`/`PageDown`
    /// 之后仍是 `Idle`（与它们此前当 `Key::Other` 时同一条处理），缓冲与光标一个字节都不动；
    /// 它们同样算「非导航键」，一样脱离浏览。
    #[test]
    fn page_keys_stay_no_ops_on_the_classic_path() {
        let mut state = editor();
        assert_eq!(
            submit(&mut state, "第一条"),
            Event::Submit("第一条".to_string())
        );
        type_text(&mut state, "草稿");
        assert_eq!(state.handle(Key::Up), Event::Redraw, "先召回历史");
        assert_eq!(state.text(), "第一条");

        let mut decoder = Decoder::new();
        for bytes in [b"\x1b[5~".as_slice(), b"\x1b[6~".as_slice()] {
            for key in decoder.feed(bytes) {
                assert_eq!(
                    state.handle(key),
                    Event::Idle,
                    "{bytes:?} 在经典路径上是空操作"
                );
            }
            assert_eq!(state.text(), "第一条", "缓冲一个字节都不动");
        }
        assert_eq!(state.handle(Key::Down), Event::Idle, "草稿随脱离一起丢弃");
    }

    #[test]
    fn submit_records_history_with_adjacent_dedupe() {
        let mut state = editor();
        // 相邻重复的提交交给 `History` 去重：历史里只留一条
        assert_eq!(
            submit(&mut state, "重复"),
            Event::Submit("重复".to_string())
        );
        assert_eq!(
            submit(&mut state, "重复"),
            Event::Submit("重复".to_string())
        );
        assert_eq!(state.handle(Key::Up), Event::Redraw);
        assert_eq!(state.text(), "重复");
        assert_eq!(state.handle(Key::Up), Event::Idle, "只有一条：再往上没有了");
        assert_eq!(state.handle(Key::Down), Event::Redraw, "回到进历史前的草稿");
        assert_eq!(state.text(), "");

        // 一次多行提交按行入史（`History` 的口径）：最后一行是最新的一条
        type_text(&mut state, "第一行");
        assert_eq!(state.handle(Key::Newline), Event::Redraw);
        type_text(&mut state, "第二行");
        assert_eq!(
            state.handle(Key::Enter),
            Event::Submit("第一行\n第二行".to_string())
        );
        assert_eq!(state.handle(Key::Up), Event::Redraw);
        assert_eq!(state.text(), "第二行");
        assert_eq!(state.handle(Key::Up), Event::Redraw);
        assert_eq!(state.text(), "第一行");
        assert_eq!(state.handle(Key::Up), Event::Redraw);
        assert_eq!(state.text(), "重复", "更旧的一条还是那次提交");
        assert_eq!(state.handle(Key::Up), Event::Idle);

        // 提交一并清掉浏览态：召回一条后直接回车，之后 ↓ 不再「回到草稿」
        let mut browsing = editor();
        assert_eq!(submit(&mut browsing, "旧"), Event::Submit("旧".to_string()));
        type_text(&mut browsing, "草稿");
        assert_eq!(browsing.handle(Key::Up), Event::Redraw);
        assert_eq!(browsing.text(), "旧");
        assert_eq!(browsing.handle(Key::Enter), Event::Submit("旧".to_string()));
        assert_eq!(browsing.text(), "");
        assert_eq!(
            browsing.handle(Key::Down),
            Event::Idle,
            "提交后没有草稿可回"
        );
    }

    #[test]
    fn replace_moves_the_cursor_to_the_end() {
        let mut state = editor();
        assert_eq!(submit(&mut state, "旧"), Event::Submit("旧".to_string()));
        type_text(&mut state, "草");
        assert_eq!(state.handle(Key::Up), Event::Redraw);
        assert_eq!(state.text(), "旧");

        // 补全接受：整段换成候选、光标到末尾、退出浏览态
        state.replace("/help");
        assert_eq!(state.text(), "/help");
        assert_eq!(state.cursor(), 5);
        assert_eq!(
            state.handle(Key::Down),
            Event::Idle,
            "replace 退出浏览态：草稿不再回来"
        );

        // 中文按字符算光标下标
        state.replace("你好");
        assert_eq!(state.text(), "你好");
        assert_eq!(state.cursor(), 2);
        assert_eq!(state.handle(Key::Char('!')), Event::Redraw);
        assert_eq!(state.text(), "你好!");
        assert_eq!(state.cursor(), 3);
    }
}
