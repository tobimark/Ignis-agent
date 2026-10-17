//! 重绘帧（纯函数）：把缓冲文本拼成一段终端串，光标按显示列数归位。
//!
//! 读取层要做的就是「写帧 + flush」；本模块不碰任何 I/O，纯粹是字符串拼接，所以单测可以逐字节
//! 断言。一帧的形状（`prefix` 是提示符、`continuation` 是续行提示，读取层传 `❯ ` 与
//! [`CONTINUATION`]）：
//!
//! ```text
//! \r\x1b[J              清掉上一帧（回到行首，从光标清到屏幕底）
//! {prefix}第一行\r\n     首行带提示符
//! {continuation}第二行\r\n  其余行带续行提示
//! \x1b[{n}A              光标上移 `行数 - 光标行`
//! \r                     回到列 0
//! \x1b[{m}C              光标右移 `display_width(光标行的标记) + 光标行内已显示宽度`
//! ```
//!
//! 三条钉死的口径：
//!
//! - **列数一律按显示宽度**（`render::width` 的 `display_width` / `char_width`）：`你` 占 2 列，
//!   按字节数或字符数算都会让中日韩文本的光标错位；光标行的标记宽度取该行实际用的那个
//!   （首行 `prefix`、续行 `continuation`，两者宽度可以不同）。
//! - **不写行尾填充空格**：旧内容由 `\r\x1b[J` 清掉，每行只画「标记 + 原文」，光标靠相对移动归位。
//! - **上移/右移次数为 0 时省略整条序列**（防御性：提示符为空且光标在段首）。
//!
//! 已知限制：组合字符簇（emoji ZWJ 等）与 emoji 按字符近似（与 `render::width` 同口径）；
//! 超过终端列数的一行交给终端软换行，这里不折行——光标在长行上可能因此偏移（设计已记录）。

use crate::render::width::{char_width, display_width};

/// 续行提示：`\n` 之后的每一行都用它开头（首行用调用方给的提示符），列宽与 `❯ ` 一致。
pub const CONTINUATION: &str = "… ";

/// 清旧内容并回到行首：一帧的开头。
const CLEAR: &str = "\r\x1b[J";

/// 拼一帧重绘串：`text` 按 `\n` 拆行（首行 `prefix`、其余行 `continuation`），光标按列数归位。
///
/// `cursor` 是字符下标（与 `state::EditorState::cursor` 同口径）：它在哪一行，帧的最后就把光标
/// 移到那一行的列 `display_width(那一行的标记) + 光标行内已显示宽度`（续行用 `continuation` 的
/// 宽度算，不借前缀的）。`cursor` 超出 `text` 时按末尾处理（防御性；状态机不会给出这种下标）。
pub fn frame(prefix: &str, continuation: &str, text: &str, cursor: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());

    // 光标落在第几行、这一行里光标之前已显示多少列（逐字符累计：`你` 占 2 列）
    let mut row = 0usize;
    let mut columns_in_line = 0usize;
    for (index, c) in chars.iter().enumerate() {
        if index >= cursor {
            break;
        }
        if *c == '\n' {
            row += 1;
            columns_in_line = 0;
        } else {
            columns_in_line += char_width(*c);
        }
    }

    let mut out = String::from(CLEAR);
    let mut lines = 0usize;
    for line in text.split('\n') {
        let marker = if lines == 0 { prefix } else { continuation };
        out.push_str(marker);
        out.push_str(line);
        out.push_str("\r\n");
        lines += 1;
    }

    // 每行写完都带 `\r\n`，此刻光标在最后一行之后：上移回光标所在的那一行。
    // `row` 是光标之前的 `\n` 数，必然 < 行数（`saturating_sub` 只是兜住不可能发生的反向差）。
    let up = lines.saturating_sub(row);
    if up > 0 {
        out.push_str(&format!("\x1b[{up}A"));
    }
    out.push('\r');
    // 光标行的标记宽度：首行是提示符、其余行是续行提示（两者宽度可以不同，不能拿前缀凑）
    let marker_width = if row == 0 {
        display_width(prefix)
    } else {
        display_width(continuation)
    };
    let columns = marker_width + columns_in_line;
    if columns > 0 {
        out.push_str(&format!("\x1b[{columns}C"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{CONTINUATION, frame};

    /// 续行提示：与首行提示符一起由读取层传进来。
    #[test]
    fn continuation_marker_is_pinned() {
        assert_eq!(CONTINUATION, "… ");
    }

    #[test]
    fn empty_buffer_draws_the_prompt_and_puts_the_cursor_after_it() {
        assert_eq!(
            frame("❯ ", CONTINUATION, "", 0),
            "\r\u{1b}[J❯ \r\n\u{1b}[1A\r\u{1b}[2C"
        );
    }

    #[test]
    fn cjk_columns_use_display_width() {
        // 光标在 `你` 之后：`❯ ` 2 列 + `你` 2 列 = 4
        assert_eq!(
            frame("❯ ", CONTINUATION, "你好", 1),
            "\r\u{1b}[J❯ 你好\r\n\u{1b}[1A\r\u{1b}[4C"
        );
        // 末尾：`你好` 4 列 → 2 + 4 = 6
        assert_eq!(
            frame("❯ ", CONTINUATION, "你好", 2),
            "\r\u{1b}[J❯ 你好\r\n\u{1b}[1A\r\u{1b}[6C"
        );
        // 前缀为空时列数只剩行内宽度；列数为 0 时不写右移序列
        assert_eq!(
            frame("", CONTINUATION, "ab", 1),
            "\r\u{1b}[Jab\r\n\u{1b}[1A\r\u{1b}[1C"
        );
        assert_eq!(
            frame("", CONTINUATION, "ab", 0),
            "\r\u{1b}[Jab\r\n\u{1b}[1A\r"
        );
    }

    #[test]
    fn multi_line_moves_up_to_the_cursor_row() {
        // 光标在第二行（`bc` 之后）：两行 + 上移 1 行 + 列 3
        assert_eq!(
            frame("❯ ", CONTINUATION, "a\nbc", 3),
            "\r\u{1b}[J❯ a\r\n… bc\r\n\u{1b}[1A\r\u{1b}[3C"
        );
        // 光标还在首行：两行都写完之后要上移两行
        assert_eq!(
            frame("❯ ", CONTINUATION, "a\nbc", 1),
            "\r\u{1b}[J❯ a\r\n… bc\r\n\u{1b}[2A\r\u{1b}[3C"
        );
        // 光标落在第二行行首：列数只有前缀宽度
        assert_eq!(
            frame("❯ ", CONTINUATION, "a\nbc", 2),
            "\r\u{1b}[J❯ a\r\n… bc\r\n\u{1b}[1A\r\u{1b}[2C"
        );
    }

    #[test]
    fn cursor_at_the_end_sits_after_the_last_char() {
        // `ab` 之后：2 + 2 = 4
        assert_eq!(
            frame("❯ ", CONTINUATION, "ab", 2),
            "\r\u{1b}[J❯ ab\r\n\u{1b}[1A\r\u{1b}[4C"
        );
    }

    /// 续行的光标列按**续行标记**的宽度算（P5 的 TUI 会传不同宽度的标记），首行仍用提示符宽度。
    #[test]
    fn continuation_rows_use_the_continuation_marker_width() {
        // 前缀 `❯ ` 2 列、续行 `→→ ` 3 列
        assert_eq!(
            frame("❯ ", "→→ ", "a\nbc", 2),
            "\r\u{1b}[J❯ a\r\n→→ bc\r\n\u{1b}[1A\r\u{1b}[3C",
            "续行行首的光标列 = 续行标记宽度（3），不是前缀宽度（2）"
        );
        // 光标在续行的 `b` 之后：3 + 1 = 4
        assert_eq!(
            frame("❯ ", "→→ ", "a\nbc", 3),
            "\r\u{1b}[J❯ a\r\n→→ bc\r\n\u{1b}[1A\r\u{1b}[4C"
        );
        // 首行不受续行宽度影响：2 + 1 = 3
        assert_eq!(
            frame("❯ ", "→→ ", "a\nbc", 1),
            "\r\u{1b}[J❯ a\r\n→→ bc\r\n\u{1b}[2A\r\u{1b}[3C"
        );
    }

    #[test]
    fn a_trailing_newline_draws_an_empty_continuation_line() {
        // 行尾的 `\n` 之后还有一条空行（光标停在那里：标记 2 列）
        assert_eq!(
            frame("❯ ", CONTINUATION, "abc\n", 4),
            "\r\u{1b}[J❯ abc\r\n… \r\n\u{1b}[1A\r\u{1b}[2C"
        );
    }

    #[test]
    fn frame_never_pads_line_ends() {
        // 行尾不留填充空格：每行就是「标记 + 原文」，清旧内容靠 `\r\x1b[J`
        let out = frame("❯ ", CONTINUATION, "ab\n中文", 0);
        assert_eq!(out, "\r\u{1b}[J❯ ab\r\n… 中文\r\n\u{1b}[2A\r\u{1b}[2C");
        assert!(!out.contains(" \r\n"), "行尾不该有填充空格：{out:?}");
    }
}
