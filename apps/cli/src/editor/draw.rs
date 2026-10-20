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
//! 显示层的两件事都在 [`frame_with`] 里：超过终端列数的逻辑行按显示宽度折成多个物理行（续行用
//! `continuation`），`\t` 按 8 列制表位补成空格。两者只改**画出来的字节**：提交的文本、历史、
//! 送进内核的 prompt 一个字节都不动，`width == usize::MAX`（非 tty）时连折行都不做。
//!
//! [`cursor_row`] 把同一套走行逻辑单独露出来（与 [`frame_with`] 共用 [`walk`]，算法只有一份）：
//! 读取层每画完一帧就记下光标落在块里的第几个物理行，下一帧落笔前先写 `\r` + `\x1b[{n}A` 回到
//! 块首，帧自己的 `\r\x1b[J` 才能从块首清掉**整个**折行块（重锚，见 `editor::reader`）。
//!
//! 已知限制：组合字符簇（emoji ZWJ 等）与 emoji 按字符近似（与 `render::width` 同口径）；
//! 折叠只看显示宽度，不认识「一个字符簇」——近似口径与滚动区一致；可用列的下限是 1，比它更宽
//! 的单个宽字符（只有 `available == 1` 这一种）整字放行，这一行的内容因此会多占一列。

use crate::render::width::{advance_column, char_width, display_width};

/// 续行提示：`\n` 之后的每一行都用它开头（首行用调用方给的提示符），列宽与 `❯ ` 一致。
pub const CONTINUATION: &str = "… ";

/// 清旧内容并回到行首：一帧的开头。
const CLEAR: &str = "\r\x1b[J";

/// 拼一帧重绘串（不折行）：[`frame_with`] 在 `width == usize::MAX` 上的特例。
///
/// 非 tty（管道、脚本、e2e）走的就是这条路：不折行、不展开 `\t`，交给终端软换行，输出与 v0.1
/// 逐字节相同。
pub fn frame(prefix: &str, continuation: &str, text: &str, cursor: usize) -> String {
    frame_with(prefix, continuation, text, cursor, usize::MAX)
}

/// 与 [`frame`] 同一个帧协议，但把每个逻辑行按**显示宽度**折成多个物理行（`width` = 终端总列数）。
///
/// - 每个物理行的可用列 = `width - 该行标记的显示宽度`，下限 1（保证每行至少消费一个字符，
///   宽字符放不下时整字放行、不死循环）；
/// - 折行路径里展开 `\t`：制表位从**该行内容的列 0** 起算（标记是行外的装饰，不进制表网格；
///   与 TUI 滚动区同一口径），补出来的空格只进这一帧。放不下就先折行；折到新行的列 0 仍放不下
///   （一格要 8 列、可用列更窄）就把这一格 clamp 到该行的可用列——制表位只是显示层的近似，
///   宁可短一格，也不画出一条超宽的行（列 0 上再折只会空转）；宽字符与它不同：整字放行；
/// - `width == usize::MAX` 时不折行、不展开 `\t`，输出与 [`frame`] 逐字节相等；
/// - 光标行/列按折行后的物理行算，帧尾的 `\x1b[{n}A` / `\x1b[{m}C` 口径不变。
pub fn frame_with(
    prefix: &str,
    continuation: &str,
    text: &str,
    cursor: usize,
    width: usize,
) -> String {
    let wrapped = walk(prefix, continuation, text, cursor, width);

    let mut out = String::from(CLEAR);
    for row in &wrapped.rows {
        out.push_str(row);
        out.push_str("\r\n");
    }

    // 每行写完都带 `\r\n`，此刻光标在最后一个物理行之后：上移回光标所在的那一行。
    // `cursor_row` 必然 < 行数（`saturating_sub` 只是兜住不可能发生的反向差）。
    let up = wrapped.rows.len().saturating_sub(wrapped.cursor_row);
    if up > 0 {
        out.push_str(&format!("\x1b[{up}A"));
    }
    out.push('\r');
    // 光标行内已显示多少列（含该行标记的宽度）：列数为 0 时不写右移序列
    if wrapped.cursor_columns > 0 {
        out.push_str(&format!("\x1b[{}C", wrapped.cursor_columns));
    }
    out
}

/// 光标在折行块里落在第几个物理行（**行序数从 0 起算**）。
///
/// 与 [`frame_with`] 共用同一套走行逻辑（[`walk`]，算法只有一份）：读取层每画完一帧就用它记下
/// 光标在块里的位置，下一帧落笔前先写 `\r` + `\x1b[{n}A` 回到**块首**，帧自己的 `\r\x1b[J`
/// 因此从块首清掉整个折行块——少了这一步，它只清光标所在行以下，块的上半截会被帧在自己的下面
/// 再画一遍（40 列下逐键输入 100 个字符就是这样叠出好几份输入行的）。
///
/// 行序数与帧尾 `\x1b[{n}A` 的「上移次数」同一口径：`width == usize::MAX`（不折行）时交回的是
/// **逻辑行**的序数——那种口径下物理行由终端的软换行决定，帧自己也只按逻辑行数上移。
pub fn cursor_row(
    prefix: &str,
    continuation: &str,
    text: &str,
    cursor: usize,
    width: usize,
) -> usize {
    walk(prefix, continuation, text, cursor, width).cursor_row
}

/// TUI 输入块复用的折行入口：返回物理行（含各自的 `prefix`/`continuation` 标记）、
/// 光标所在物理行（0 基）与光标列数（含该行标记）。
///
/// 与 [`frame_with`] / [`cursor_row`] 共用 [`walk`]：TUI 与经典 REPL 的折行、制表位和
/// 显示宽度口径因此只有一份实现。
pub(crate) fn wrapped(
    prefix: &str,
    continuation: &str,
    text: &str,
    cursor: usize,
    width: usize,
) -> (Vec<String>, usize, usize) {
    let wrapped = walk(prefix, continuation, text, cursor, width);
    (wrapped.rows, wrapped.cursor_row, wrapped.cursor_columns)
}

/// 折行走行的产物：物理行与光标落点。
struct Wrapped {
    /// 已经定稿的物理行（标记 + 正文），[`frame_with`] 逐行拼进帧。
    rows: Vec<String>,
    /// 光标所在的物理行序数（0 起算）。
    cursor_row: usize,
    /// 光标在这一行里已显示的列数（含标记宽度）。
    cursor_columns: usize,
}

/// 把文本折成物理行并算出光标落点：[`frame_with`] 与 [`cursor_row`] 唯一的走行实现。
///
/// 规则见 [`frame_with`] 的文档；`width == usize::MAX` 时不折行、不展开 `\t`。
fn walk(prefix: &str, continuation: &str, text: &str, cursor: usize, width: usize) -> Wrapped {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    // 只有真终端宽度才折行、才展开 `\t`（`usize::MAX` 是「交给终端软换行」的既有口径）
    let wrapping = width != usize::MAX;

    // 已经定稿的物理行（标记 + 正文）
    let mut rows: Vec<String> = Vec::new();
    // 光标位置：落在第几个物理行、这一行里已显示多少列（含标记——帧尾的 `\x1b[{m}C` 从列 0 数）
    let mut cursor_at: Option<(usize, usize)> = None;

    // 当前正在拼的物理行：`row` 里已经有标记，`used` 是正文已占的显示列（不含标记）
    let mut row = String::new();
    let (mut marker_width, mut available) = open_row(&mut row, true, prefix, continuation, width);
    let mut used = 0usize;

    for (index, c) in chars.iter().enumerate() {
        // 光标是字符之间的缝：处理第 `cursor` 个字符**之前**就是它的位置（放不下要折行时，
        // 光标留在换出去的那一行末尾，正是「这个字符之前」）
        if index == cursor {
            cursor_at = Some((rows.len(), marker_width + used));
        }
        if *c == '\n' {
            // 逻辑行的边界：定稿当前物理行，再开一行（标记一律 `continuation`）
            rows.push(std::mem::take(&mut row));
            (marker_width, available) = open_row(&mut row, false, prefix, continuation, width);
            used = 0;
            continue;
        }

        // 这个字符占几列：`\t` 补到本行内容的下一个制表位（不折行时不展开，按既有口径 0 列算）
        let tab = wrapping && *c == '\t';
        let mut columns = if tab {
            advance_column(used, '\t') - used
        } else {
            char_width(*c)
        };
        // 这一行放不下就折到下一行：行内已经有内容时才折（保证每行至少消费一个字符），宽字符
        // 放不下时整字放行——宁可这一行多占一列，也不切开它、也不空转
        if wrapping && used > 0 && used + columns > available {
            rows.push(std::mem::take(&mut row));
            (marker_width, available) = open_row(&mut row, false, prefix, continuation, width);
            used = 0;
            if tab {
                // 换了行，制表位要从新行的列 0 重新算
                columns = advance_column(0, '\t');
            }
        }
        // `\t` 在新行的列 0 仍放不下（整段要 8 列、可用列更窄）就把这一格 clamp 到可用列：
        // 制表位只是显示层的近似，宁可短一格，也不画出超宽的行；列 0 上再折只会空转，所以是
        // clamp。宽字符不走这条路（它整字放行，见模块顶部的「已知限制」）。
        if tab {
            columns = columns.min(available);
        }
        if tab {
            // `\t` 在屏幕上就是补到制表位的那些空格（显示层的展开，帧外一切照旧）
            row.extend(std::iter::repeat_n(' ', columns));
        } else {
            row.push(*c);
        }
        used += columns;
    }
    // 光标在文本末尾（含空缓冲）时：它就在这个还没定稿的物理行里
    let (cursor_row, cursor_columns) = cursor_at.unwrap_or((rows.len(), marker_width + used));
    rows.push(std::mem::take(&mut row));

    Wrapped {
        rows,
        cursor_row,
        cursor_columns,
    }
}

/// 开一个物理行：把标记写进 `row`，交回 `(标记宽度, 这一行的可用正文列)`。
///
/// 标记只看「是不是全帧第一行」：[`frame`] 的 `lines == 0` 口径不变——`prefix` 只给第一行，
/// 逻辑行的续行与后续逻辑行都用 `continuation`。可用列 = `width - 标记宽度`，下限 1（宽度比标记
/// 还窄时仍保证每行消费一个字符）；`usize::MAX` 是「不折行」，可用列直接给满。
fn open_row(
    row: &mut String,
    first: bool,
    prefix: &str,
    continuation: &str,
    width: usize,
) -> (usize, usize) {
    let marker = if first { prefix } else { continuation };
    row.push_str(marker);
    let marker_width = display_width(marker);
    if width == usize::MAX {
        (marker_width, usize::MAX)
    } else {
        (marker_width, width.saturating_sub(marker_width).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::{CONTINUATION, cursor_row, display_width, frame, frame_with};

    /// 从一帧里取出物理行（`\r\n` 分隔的段）：最后一截是帧尾的光标归位序列，不算行。
    fn rows_of(frame: &str) -> Vec<&str> {
        let mut rows: Vec<&str> = frame.split("\r\n").collect();
        rows.pop();
        // 第一行带着帧首的清屏序列 `\r\x1b[J`
        rows[0] = rows[0].trim_start_matches("\r\u{1b}[J");
        rows
    }

    /// 一个物理行的**内容**（去掉该行自己的标记）。
    fn content_of(row: &str) -> &str {
        row.strip_prefix("❯ ")
            .or_else(|| row.strip_prefix("… "))
            .unwrap_or_else(|| panic!("每行都以自己的标记开头：{row:?}"))
    }

    /// 折行与不折行是同一个帧协议：`width == usize::MAX`（非 tty）那一份必须与 `frame` 逐字节相等。
    #[test]
    fn frame_and_frame_with_agree_when_unwrapped() {
        let corpus = [
            "",
            "a",
            "你好",
            "a\nbc",
            "abc\n",
            "第一行\n第二行",
            "含\t制表符的一行",
            "很宽的一行，宽到真终端里一定会被软换行，但非 tty 的帧不许折",
        ];
        for text in corpus {
            for cursor in 0..=text.chars().count() {
                assert_eq!(
                    frame_with("❯ ", CONTINUATION, text, cursor, usize::MAX),
                    frame("❯ ", CONTINUATION, text, cursor),
                    "{text:?} @ cursor={cursor}"
                );
                // 空标记（列数为 0 时不写右移序列）也逐字节相等
                assert_eq!(
                    frame_with("", "", text, cursor, usize::MAX),
                    frame("", "", text, cursor),
                    "{text:?} @ cursor={cursor}（空标记）"
                );
            }
        }
    }

    /// 按显示宽度折行：终端 5 列、标记 2 列 → 每个物理行 3 列可用，续行用续行提示。
    #[test]
    fn frame_with_wraps_at_the_terminal_width() {
        // 逐字：两行都带自己的标记，光标在末尾（第二行 `… ` + `def` = 5 列）
        assert_eq!(
            frame_with("❯ ", CONTINUATION, "abcdef", 6, 5),
            "\r\u{1b}[J❯ abc\r\n… def\r\n\u{1b}[1A\r\u{1b}[5C"
        );
        // 光标在第一行里：列按第一行的标记算，帧尾仍上移「行数 − 光标行」次
        assert_eq!(
            frame_with("❯ ", CONTINUATION, "abcdef", 3, 5),
            "\r\u{1b}[J❯ abc\r\n… def\r\n\u{1b}[2A\r\u{1b}[5C"
        );
        // `\t` 只在折行路径里展开（显示层）：`a` 之后补到第 8 列 → 7 个空格
        assert_eq!(
            frame_with("❯ ", CONTINUATION, "a\tb", 3, 40),
            "\r\u{1b}[J❯ a       b\r\n\u{1b}[1A\r\u{1b}[11C"
        );
        // 逻辑行的续行也吃续行标记的宽度：两行之间按 `\n` 拆，各自按宽度折
        let out = frame_with("❯ ", CONTINUATION, "abcd\nef", 7, 5);
        assert_eq!(
            out, "\r\u{1b}[J❯ abc\r\n… d\r\n… ef\r\n\u{1b}[1A\r\u{1b}[4C",
            "{out:?}"
        );
        // 每行都不留行尾填充空格（清旧内容靠帧首的 `\r\x1b[J`）
        assert!(!out.contains(" \r\n"), "{out:?}");
    }

    /// 宽字符不切开：放不下就整字挪到下一个物理行；连一个都放不下时整字放行（不死循环）。
    #[test]
    fn frame_with_never_splits_a_wide_char() {
        // 3 列可用放不下第二对 2 列：`你你` 就是 `❯ 你` 与 `… 你` 两行
        let two = frame_with("❯ ", CONTINUATION, "你你", 2, 5);
        let rows: Vec<&str> = two.split("\r\n").collect();
        assert_eq!(rows[0], "\r\u{1b}[J❯ 你");
        assert_eq!(rows[1], "… 你");
        assert_eq!(
            rows[2], "\u{1b}[1A\r\u{1b}[4C",
            "光标在末尾：`… ` 2 列 + `你` 2 列"
        );

        // 三个 `你`：每个整字各占一行（逐行消费一个宽字符，绝不切开）
        let three = frame_with("❯ ", CONTINUATION, "你你你", 3, 5);
        let rows: Vec<&str> = three.split("\r\n").collect();
        assert_eq!(rows[0], "\r\u{1b}[J❯ 你");
        assert_eq!(rows[1], "… 你");
        assert_eq!(rows[2], "… 你", "{three:?}");
    }

    /// 可用列下限为 1：宽度 1（连标记都放不下）时每行仍消费一个字符，不死循环。
    #[test]
    fn frame_with_always_consumes_at_least_one_char() {
        assert_eq!(
            frame_with("❯ ", CONTINUATION, "ab", 2, 1),
            "\r\u{1b}[J❯ a\r\n… b\r\n\u{1b}[1A\r\u{1b}[3C"
        );
        // 宽字符比可用列还宽：整字放行，每行一个字符
        let wide = frame_with("❯ ", CONTINUATION, "你好", 2, 1);
        let rows: Vec<&str> = wide.split("\r\n").collect();
        assert_eq!(rows[0], "\r\u{1b}[J❯ 你");
        assert_eq!(rows[1], "… 好");
        assert_eq!(rows[2], "\u{1b}[1A\r\u{1b}[4C", "{wide:?}");
    }

    /// 窄终端里的 `\t` 不再画出一条超宽的行：这一格（从列 0 起就是 8 列）放不下时先折行，
    /// 折到新行的列 0 仍放不下就 clamp 到该行的可用列——列 0 上再折只会空转，宁可短一格。
    #[test]
    fn tabs_never_emit_a_row_wider_than_the_available_columns() {
        // 5 列、标记 2 列 → 可用 3 列：`a` 之后的 `\t` 从列 1 要补 7 格，先折行；新行列 0 仍要
        // 8 格，clamp 到 3 格（`… ` 之后 3 个空格，整行正好 5 列），`b` 再折到第三行
        let clamped = format!("… {}", " ".repeat(3));
        assert_eq!(
            frame_with("❯ ", CONTINUATION, "a\tb", 3, 5),
            format!("\r\u{1b}[J❯ a\r\n{clamped}\r\n… b\r\n\u{1b}[1A\r\u{1b}[3C")
        );
        // 行首的 `\t` 不折（列 0 上折不动）：整格 clamp 到可用列 2
        let padded = format!("❯ {}", " ".repeat(2));
        assert_eq!(
            frame_with("❯ ", CONTINUATION, "\t", 1, 4),
            format!("\r\u{1b}[J{padded}\r\n\u{1b}[1A\r\u{1b}[4C")
        );

        // 性质：窄宽度 × 带 `\t` 的语料，每个物理行的内容都不超过这一行的可用列。唯一的例外是
        // 模块顶部记着的那条已知限制——可用列只有 1 列时，单个宽字符整字放行（多占一列）。
        for width in 1..=9 {
            for text in [
                "a\tb", "\t", "a\t\tb", "你\tb", "ab\tc", "\t\t", "a\tbcdef", "你",
            ] {
                let frame = frame_with("❯ ", CONTINUATION, text, text.chars().count(), width);
                // `❯ ` 与 `… ` 都是 2 列标记：可用列 = 宽度 − 2，下限 1
                let available = width.saturating_sub(2).max(1);
                for row in rows_of(&frame) {
                    let content = content_of(row);
                    let wide_char_limit = available == 1 && content.chars().count() == 1;
                    assert!(
                        display_width(content) <= available || wide_char_limit,
                        "{text:?} @ width={width}：行 {row:?} 的内容占 {} 列，超出可用列 {available}",
                        display_width(content)
                    );
                }
            }
        }
    }

    /// 光标行：与 [`frame_with`] 同一套走行（帧尾的上移次数也按它算），读取层拿它做重锚记账。
    #[test]
    fn cursor_row_follows_the_wrapped_layout() {
        // 5 列、标记 2 列 → 每行 3 列可用：`abcdef` 折成两行，光标在末尾 → 第 1 行（0 起算）
        assert_eq!(cursor_row("❯ ", CONTINUATION, "abcdef", 6, 5), 1);
        assert_eq!(
            cursor_row("❯ ", CONTINUATION, "abcdef", 3, 5),
            0,
            "光标在第一行里"
        );
        assert_eq!(cursor_row("❯ ", CONTINUATION, "abcdef", 0, 5), 0);
        // 逻辑行的续行同样是物理行：`\n` 之后的行序数接着往下数
        assert_eq!(cursor_row("❯ ", CONTINUATION, "abcd\nef", 7, 5), 2);
        // 不折行（非 tty）时交回的是逻辑行序数，与帧尾 `\x1b[{n}A` 的上移次数同一口径
        assert_eq!(cursor_row("❯ ", CONTINUATION, "a\nb", 3, usize::MAX), 1);
        // 与帧对得上：帧里上移「行数 − 光标行」次（每一行都以 `\r\n` 收尾）
        for (text, cursor, width) in [("abcdef", 6, 5), ("abcdef", 3, 5), ("a\nbc", 4, 5)] {
            let frame = frame_with("❯ ", CONTINUATION, text, cursor, width);
            let rows = rows_of(&frame).len();
            let up = rows - cursor_row("❯ ", CONTINUATION, text, cursor, width);
            assert!(up > 0, "这一组用例的光标都不在最后一行：{frame:?}");
            assert!(frame.contains(&format!("\u{1b}[{up}A")), "{frame:?}");
        }
    }

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
