//! TUI 的编辑器输入态：复用 P2 的状态机与解码器，只多一条「瞬时提示行」。
//!
//! 分工与 `editor::reader` 同一份：解码层（[`crate::editor::keys`]）把字节翻成按键，状态机
//! （[`crate::editor::state`]）维护缓冲与光标，本模块只多做两件事——Tab 走
//! [`slash::complete`]、把编辑器窗口拼成 [`InputBlock`]。编辑逻辑一行都不在这里重写。
//!
//! 几条钉死的口径：
//! - **一批按键按顺序处理**：`Submit`（回车）与 `Exit`（空缓冲 `Ctrl+D`）之后这一批的剩余按键
//!   忽略（回车之后跟着的字节不许落到已经清空的缓冲里）；
//! - **Tab 的三种结果**：唯一命中整行替换、多候选把候选行用两个空格拼成一行提示（宽度截断交给
//!   帧合成器）、没有候选清空提示；
//! - **`Ctrl+C` 字节与 SIGINT 同义**（`ISIG` 关掉的终端上它就是那个字节）：这里只回
//!   [`Action::Exit`]，退出还是留在界内由外层按输入模式决定；
//! - **提示行只由 Tab 写**：其余按键不动它（T4/T5/T6 的输入态各自管自己那一条）；
//! - **审批态不走本模块的动作面**：[`InputState::keys`] 把按键原样交给调用方（审批按键与编辑器
//!   缓冲无关），编辑器的解码器仍复用这一份（跨批缓存不丢）。

use crate::commands::slash::{self, Completion};
use crate::editor::draw::CONTINUATION;
use crate::editor::history::History;
use crate::editor::keys::{Decoder, Key};
use crate::editor::state::{EditorState, Event};
use crate::render::width::{display_width, expand_tabs};
use crate::tui::app::Action;
use crate::tui::view::InputBlock;

/// 输入区首行的标记（与 `editor::draw`、`view::compose` 同一条字面量）。
const INPUT_MARK: &str = "❯ ";

/// TUI 的编辑器输入态：复用 P2 的状态机与解码器，只多一条「瞬时提示行」。
pub struct InputState {
    /// 编辑器状态机：缓冲、光标与历史都在它手里，活过整场会话。
    state: EditorState,
    /// 字节 → 按键的增量解码器（跨 `on_bytes` 缓存解不完的尾巴）。
    decoder: Decoder,
    /// 瞬时提示行（Tab 候选拼接；空串 = 没有）。
    hint: String,
}

impl InputState {
    /// 新建输入态：历史由调用方注入（生产给 `~/.ignis/history` 载入的那份）。
    pub fn new(history: History) -> Self {
        Self {
            state: EditorState::new(history),
            decoder: Decoder::new(),
            hint: String::new(),
        }
    }

    /// 解码一段字节（跨批缓存留在解码器里），按键交给调用方处置。
    ///
    /// 审批态（`TuiApp::approval_key`）走这条路：它要的是按键而不是编辑器动作——审批按键绝不
    /// 进编辑器缓冲，`a`/`s`/`r`/`d` 的语义只归审批面。
    pub(crate) fn keys(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.decoder.feed(bytes)
    }

    /// 喂一段原始字节，返回要外层执行的动作（顺序即处理顺序）。
    ///
    /// `Submit`/`Exit` 之后这一批的剩余按键忽略：回车提交之后跟着的字节（快速输入、粘贴后紧跟
    /// 回车）不许落到已经清空的缓冲里——与忙碌期禁用输入是同一条规则。
    pub fn on_bytes(&mut self, bytes: &[u8]) -> Vec<Action> {
        self.on_bytes_with_keys(bytes)
            .into_iter()
            .map(|(_, action)| action)
            .collect()
    }

    /// 与 [`InputState::on_bytes`] 同一套处理，但把 `(按键, 动作)` 成对交回：TUI 的编辑器态要靠
    /// 它认出翻页键——`PageUp`/`PageDown` 在状态机里是空操作（`leave_browse` + `Idle`），而翻页
    /// 的落点（滚动区）只有应用层认识。
    pub(crate) fn on_bytes_with_keys(&mut self, bytes: &[u8]) -> Vec<(Key, Action)> {
        let keys = self.keys(bytes);
        let mut steps = Vec::new();
        for key in keys {
            let action = self.on_key(key);
            let stop = matches!(action, Action::Submit(_) | Action::Exit);
            steps.push((key, action));
            if stop {
                break;
            }
        }
        steps
    }

    /// 缓冲文本（补全、提交都读它）。
    pub fn text(&self) -> String {
        self.state.text()
    }

    /// 瞬时提示行（Tab 候选拼接；空串 = 没有）。
    pub fn hint(&self) -> &str {
        &self.hint
    }

    /// 换一条提示行（T4/T5/T6 的输入态各自写自己那一条）。
    pub fn set_hint(&mut self, hint: String) {
        self.hint = hint;
    }

    /// 编辑器区（`❯ `/`… ` 前缀 + 光标显示列，按显示宽度算）。
    ///
    /// 窗口取「光标所在行起的两行」（`window_start = cursor_line.min(text_lines - 2)`）：光标
    /// 因此在窗口里（末行也不越界），多行输入只看得到光标附近的两行。光标列按显示宽度算——`你`
    /// 占 2 列，`❯ `/`… ` 各 2 列。
    ///
    /// `\t` 只在显示层展开（8 列制表位，从行内容的列 0 起算，与滚动区同一口径）：落进
    /// [`InputBlock::lines`] 的两个显示行里不再有制表符，光标列也在**展开后的**文本上算——
    /// 缓冲里那份文本（提交时交给内核的）一个字节不动。
    pub fn block(&self) -> InputBlock {
        let text = self.state.text();
        let chars: Vec<char> = text.chars().collect();
        let cursor = self.state.cursor().min(chars.len());
        let lines: Vec<&str> = text.split('\n').collect();

        // 光标在第几行、这一行内光标之前有多少显示列
        let mut cursor_line = 0usize;
        let mut line_start = 0usize;
        for (index, ch) in chars.iter().enumerate().take(cursor) {
            if *ch == '\n' {
                cursor_line += 1;
                line_start = index + 1;
            }
        }
        let before: String = chars[line_start..cursor].iter().collect();
        // 光标列按展开后的行内文本算（`\t` 占几列只有展开之后才知道）
        let before = expand_tabs(&before, 0);

        let window_start = cursor_line.min(lines.len().saturating_sub(2));
        let mut rows = [String::new(), String::new(), self.hint.clone()];
        for (offset, row) in rows.iter_mut().take(2).enumerate() {
            // 文本没有这一行时留空（不画孤零零的标记）
            let Some(content) = lines.get(window_start + offset) else {
                continue;
            };
            let marker = if offset == 0 {
                INPUT_MARK
            } else {
                CONTINUATION
            };
            // `\t` 只在这一层展开：提交层与历史里仍是字面制表符
            *row = format!("{marker}{}", expand_tabs(content, 0));
        }
        // 光标行在窗口内的相对位置（0 或 1），列 = 该行标记的显示宽度 + 光标前文本的显示宽度
        let cursor_row = cursor_line - window_start;
        let marker_width = if cursor_row == 0 {
            display_width(INPUT_MARK)
        } else {
            display_width(CONTINUATION)
        };
        InputBlock {
            lines: rows,
            cursor: Some((cursor_row, marker_width + display_width(&before))),
        }
    }

    /// 非法 UTF-8 的告警位（读一次就清零）：解码器宽松解码时置位，调用方据此打一行告警。
    pub(crate) fn take_invalid(&mut self) -> bool {
        self.decoder.take_invalid()
    }

    /// 吃掉「一个孤零零的 ESC」（Esc 消歧窗口 = 一个 tick）；返回真表示吃到了。
    pub(crate) fn take_lone_escape(&mut self) -> bool {
        self.decoder.take_lone_escape()
    }

    /// 处理一个按键：Tab 走补全，`Ctrl+C` 字节与 SIGINT 同义，其余交给状态机。
    fn on_key(&mut self, key: Key) -> Action {
        match key {
            Key::Tab => self.on_tab(),
            // `ISIG` 关掉的终端上 `Ctrl+C` 只是一个字节：与 SIGINT 同义（外层决定退出/留在界内）
            Key::CtrlC => Action::Exit,
            key => match self.state.handle(key) {
                Event::Idle => Action::None,
                Event::Redraw => Action::Redraw,
                Event::Submit(text) => Action::Submit(text),
                // 空缓冲上的 `Ctrl+D`（EOF）在 TUI 里就是「退出」
                Event::Eof => Action::Exit,
            },
        }
    }

    /// Tab：唯一命中整行替换、多候选拼成一行提示、没有候选清空提示。
    fn on_tab(&mut self) -> Action {
        match slash::complete(&self.state.text()) {
            Completion::Accepted(text) => {
                self.state.replace(&text);
                self.set_hint(String::new());
                Action::Redraw
            }
            // 候选行用两个空格拼成一行（每行的宽度截断由帧合成器统一做）
            Completion::Listing(lines) => {
                self.set_hint(lines.join("  "));
                Action::Redraw
            }
            // 没有候选：提示本来非空才需要重绘（不白写一帧）
            Completion::None => {
                if self.hint.is_empty() {
                    Action::None
                } else {
                    self.set_hint(String::new());
                    Action::Redraw
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::history::History;
    use crate::render::width::{display_width, truncate_to_width};
    use crate::tui::app::Action;
    use crate::tui::{Submitted, classify};

    use super::InputState;

    /// 新建输入态：历史只在内存里（用例不碰 `~/.ignis/history`）。
    fn input() -> InputState {
        InputState::new(History::in_memory())
    }

    /// 键位路由复用编辑器状态机：字符、光标、删除、清空都照 P2 的口径回动作，不重写编辑逻辑。
    #[test]
    fn key_routing_reuses_the_editor_state_machine() {
        let mut state = input();
        assert_eq!(state.on_bytes(b"a"), [Action::Redraw]);
        assert_eq!(state.text(), "a");
        // 行首再左移：状态机给 `Idle`，这里就是「没有动作」
        assert_eq!(state.on_bytes(b"\x1b[D"), [Action::Redraw]);
        assert_eq!(state.on_bytes(b"\x1b[D"), [Action::None]);

        // 中文按字符移动：`你好ab` 里左移两格落到 `你好` 之后，退格删的是整个 `好`
        let mut cjk = input();
        cjk.on_bytes("你好ab".as_bytes());
        assert_eq!(cjk.text(), "你好ab");
        cjk.on_bytes(b"\x1b[D");
        cjk.on_bytes(b"\x1b[D");
        assert_eq!(
            cjk.on_bytes(b"\x7f"),
            [Action::Redraw],
            "Backspace 删一个字符"
        );
        assert_eq!(cjk.text(), "你ab");

        // Ctrl+U 清空；已经是空的就没有动作（不白写一帧）
        assert_eq!(cjk.on_bytes(b"\x15"), [Action::Redraw]);
        assert_eq!(cjk.text(), "");
        assert_eq!(cjk.on_bytes(b"\x15"), [Action::None]);

        // Ctrl+C 字节（`ISIG` 关掉的终端）与 SIGINT 同义
        assert_eq!(cjk.on_bytes(b"\x03"), [Action::Exit]);
    }

    /// 回车提交整段文本；空缓冲上的回车交回空串——主循环按 [`classify`] 的 `Empty` 什么都不做。
    #[test]
    fn enter_submits_and_enter_on_empty_is_noop() {
        let mut state = input();
        assert_eq!(
            state.on_bytes("你好\r".as_bytes()),
            [
                Action::Redraw,
                Action::Redraw,
                Action::Submit("你好".to_string())
            ]
        );
        assert_eq!(state.text(), "", "提交之后缓冲清空");

        // 空行：提交空串，由分类判成空操作（不送模型、不留行）
        assert_eq!(state.on_bytes(b"\r"), [Action::Submit(String::new())]);
        assert_eq!(
            classify(""),
            Submitted::Empty,
            "空提交在分类那一步变成空操作"
        );
    }

    /// `Ctrl+D`：只有空缓冲是退出（EOF）；有内容时等价 `Delete`，末尾没有可删的就是空操作。
    #[test]
    fn ctrl_d_exits_only_on_an_empty_buffer() {
        let mut state = input();
        assert_eq!(
            state.on_bytes(b"\x04"),
            [Action::Exit],
            "空缓冲 Ctrl+D = 退出"
        );

        state.on_bytes(b"ab");
        assert_eq!(
            state.on_bytes(b"\x04"),
            [Action::None],
            "光标在末尾：等价 Delete，没有可删的"
        );
        assert_eq!(state.text(), "ab");
        assert_eq!(state.on_bytes(b"\x01"), [Action::Redraw], "Ctrl+A = 行首");
        assert_eq!(
            state.on_bytes(b"\x04"),
            [Action::Redraw],
            "有内容时等价 Delete"
        );
        assert_eq!(state.text(), "b");
        assert_eq!(state.on_bytes(b"\x15"), [Action::Redraw], "清空");
        assert_eq!(state.on_bytes(b"\x04"), [Action::Exit], "又空了：还是退出");
    }

    /// `↑`/`↓` 走编辑器历史（提交入史、浏览与草稿还原都归状态机管）。
    #[test]
    fn up_and_down_walk_the_history() {
        let mut state = input();
        state.on_bytes("第一次\r".as_bytes());
        state.on_bytes("第二次\r".as_bytes());
        assert_eq!(state.text(), "");

        assert_eq!(state.on_bytes(b"\x1b[A"), [Action::Redraw], "↑ 是最新一条");
        assert_eq!(state.text(), "第二次");
        assert_eq!(state.on_bytes(b"\x1b[A"), [Action::Redraw]);
        assert_eq!(state.text(), "第一次");
        assert_eq!(state.on_bytes(b"\x1b[A"), [Action::None], "到最旧了");

        assert_eq!(state.on_bytes(b"\x1b[B"), [Action::Redraw]);
        assert_eq!(state.text(), "第二次");
        assert_eq!(state.on_bytes(b"\x1b[B"), [Action::Redraw]);
        assert_eq!(state.text(), "", "回到进历史前的草稿");
        assert_eq!(state.on_bytes(b"\x1b[B"), [Action::None]);
    }

    /// P6 T2：输入区的 `\t` 只在显示层展开（8 列制表位），光标列按**展开后的**文本算；
    /// 缓冲里的字面制表符一个字节都不动（提交时仍是 `a\tb`）。
    #[test]
    fn block_expands_tabs_and_keeps_the_cursor_aligned() {
        let mut state = input();
        // 直接按键的 0x09 是补全（见 `keys.rs`），要插字面制表符得走粘贴区间
        // （区间标记本身是空操作，三个字符各请求一次重绘）
        assert_eq!(
            state.on_bytes(b"\x1b[200~a\tb\x1b[201~"),
            [
                Action::None,
                Action::Redraw,
                Action::Redraw,
                Action::Redraw,
                Action::None
            ]
        );
        assert_eq!(state.text(), "a\tb", "缓冲里仍是字面制表符");

        let block = state.block();
        assert_eq!(block.lines[0], "❯ a       b");
        assert!(
            !block.lines[0].contains('\t'),
            "显示行里不该再有制表符：{:?}",
            block.lines[0]
        );
        // `a` 之后补到第 8 列（7 个空格）再放 `b` → 行内正文 9 列，光标在末尾
        assert_eq!(block.cursor, Some((0, 2 + 9)));
        assert_eq!(block.lines[1], "", "单行输入的第二条显示行是空的");
        assert_eq!(block.lines[2], "", "没有 Tab 候选提示");
    }

    /// Tab 走 `slash::complete`：唯一命中替换整行、多候选拼成一行提示、没有候选清空提示。
    #[test]
    fn tab_candidates_become_the_hint_line_and_a_unique_match_replaces_the_buffer() {
        // 唯一命中：整行换成候选
        let mut unique = input();
        unique.on_bytes(b"/he");
        assert_eq!(unique.on_bytes(b"\t"), [Action::Redraw]);
        assert_eq!(unique.text(), "/help");
        assert!(unique.hint().is_empty(), "接受候选不留提示");

        // 多候选：候选行用两个空格拼成一行提示，输入缓冲原样
        let mut many = input();
        many.on_bytes(b"/");
        assert_eq!(many.on_bytes(b"\t"), [Action::Redraw]);
        assert_eq!(many.text(), "/", "列候选不改缓冲");
        let hint = many.hint();
        assert!(hint.contains("/memory"), "{hint:?}");
        assert!(hint.contains("  "), "候选之间用两个空格拼接：{hint:?}");
        assert!(!hint.contains('\n'), "提示就是一行：{hint:?}");

        // 提示行是输入区第 3 行；宽度截断由帧合成器统一做（这里量给它看）
        let block = many.block();
        assert_eq!(block.lines[2], hint);
        let narrow = truncate_to_width(&block.lines[2], 20);
        assert_eq!(
            display_width(narrow),
            20,
            "提示行确实长到会被截断：{hint:?}"
        );
        assert!(hint.starts_with(narrow));

        // 没有候选：清空提示（提示本来非空 → 要重绘；再按一次是空操作）
        many.on_bytes(b"zz");
        assert_eq!(many.on_bytes(b"\t"), [Action::Redraw], "无候选：清空提示");
        assert!(many.hint().is_empty());
        assert_eq!(
            many.on_bytes(b"\t"),
            [Action::None],
            "已经是空的：不白写一帧"
        );
    }
}
