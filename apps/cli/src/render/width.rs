//! 显示宽度与折行：终端渲染共用的宽度口径。
//!
//! 宽度是「终端列数」的近似：控制字符不占列，东亚宽/全角字符占 2 列，其余占 1 列。
//! 不引入 `unicode-width`（零新增依赖）：宽区间是一张手写的常用表，组合字符按 1 近似。

/// 东亚宽/全角区间近似表（闭区间，用 `char as u32` 比较）。
///
/// 覆盖常用块：谚文字母与音节、CJK 统一表意文字及其扩展、平假名/片假名与拼音扩展、
/// CJK 兼容形式、全角 ASCII 与全角符号。表外的块（emoji 等）按 1 列近似——宁少不虚，
/// 也不能凭空多占列把已经排好的行挤炸。
const WIDE_RANGES: &[(u32, u32)] = &[
    (0x1100, 0x115F),   // 谚文字母（首声）
    (0x2E80, 0x303E),   // CJK 部首补充 … CJK 符号与标点（含 U+3000 全角空格）
    (0x3041, 0x33FF),   // 平假名 … CJK 兼容（含片假名、拼音扩展、注音符号）
    (0x3400, 0x4DBF),   // CJK 统一表意文字扩展 A
    (0x4E00, 0x9FFF),   // CJK 统一表意文字
    (0xA000, 0xA4CF),   // 彝文音节 … 彝文部首
    (0xAC00, 0xD7A3),   // 谚文音节
    (0xF900, 0xFAFF),   // CJK 兼容表意文字
    (0xFE30, 0xFE4F),   // CJK 兼容形式（竖排标点等）
    (0xFF00, 0xFF60),   // 全角 ASCII、全角标点
    (0xFFE0, 0xFFE6),   // 全角符号（含全角货币符号）
    (0x20000, 0x2FFFD), // CJK 扩展 B 及以后的表意文字
    (0x30000, 0x3FFFD), // CJK 扩展 G/H
];

/// 字符占几列：控制字符（含 `\n`、`\t`）0，东亚宽/全角 2，其余（含组合字符）1。
pub fn char_width(c: char) -> usize {
    if c.is_control() {
        return 0;
    }
    if is_wide(c) { 2 } else { 1 }
}

/// 字符串的显示宽度：各字符宽度之和。
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 按显示宽度折行。
///
/// 规则（`width` 是每行的目标显示宽度）：
/// - 优先在空白处断行，断点处的分隔空白被吞掉，续行不以空白开头；
/// - 宽字符（CJK）可在任意字符间断——中文没有词边界，只能逐字排；
/// - 超长的无空白串按宽度硬断（英文长单词、URL、base64 都会被切开）；
/// - `usize::MAX` 表示不折行：整段原样一行（非 tty 的调用方用它保住「每行可 grep」）；
/// - 文本里原有的换行不算硬断，它只是宽度 0 的分隔空白（只在断点处被吞掉）：要按物理行
///   分开断行是调用方的事；
/// - `width == 0` 无法折行，原样一整行返回（防御）；空串与纯空白串不产出任何行。
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if width == 0 {
        return vec![text.to_string()];
    }

    let mut wrapper = Wrapper::new(width);
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let after = start + c.len_utf8();
        if c.is_whitespace() {
            // 整段空白是一个「分隔空白」原子：只在断点处被吞掉，不参与硬断
            let end = take_while(&mut chars, after, char::is_whitespace);
            let space = &text[start..end];
            wrapper.push_space(space, display_width(space));
        } else if is_wide(c) {
            // 宽字符各自成段：CJK 字符之间就是断点
            let single = &text[start..after];
            wrapper.push_atom(single, char_width(c));
        } else {
            // 连续窄字符段：段内没有断点，只有整段塞不下时才硬断
            let end = take_while(&mut chars, after, |nc| !nc.is_whitespace() && !is_wide(nc));
            let word = &text[start..end];
            wrapper.push_atom(word, display_width(word));
        }
    }

    wrapper.finish()
}

/// 是否落在东亚宽区间表里。
fn is_wide(c: char) -> bool {
    let code = c as u32;
    WIDE_RANGES
        .iter()
        .any(|&(lo, hi)| (lo..=hi).contains(&code))
}

/// 从 `chars` 里吃掉满足 `keep` 的连续字符，返回当前段的结束字节下标（`end` 为它之前的起点）。
fn take_while(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    mut end: usize,
    keep: impl Fn(char) -> bool,
) -> usize {
    while let Some(&(i, c)) = chars.peek() {
        if !keep(c) {
            break;
        }
        end = i + c.len_utf8();
        chars.next();
    }
    end
}

/// 折行器：单趟扫过字符，往当前行塞原子，塞不下就定稿换行。
struct Wrapper {
    /// 目标显示宽度（调用方保证 > 0）。
    width: usize,
    /// 已定稿的行。
    lines: Vec<String>,
    /// 当前行（未定稿）。
    line: String,
    /// 当前行的显示宽度，与 `line` 同步维护（避免每步重算）。
    line_width: usize,
}

impl Wrapper {
    /// 建一个目标宽度为 `width` 的折行器。
    fn new(width: usize) -> Self {
        Self {
            width,
            lines: Vec::new(),
            line: String::new(),
            line_width: 0,
        }
    }

    /// 收一个「原子」：空白串、单个宽字符，或一段连续窄字符。
    ///
    /// 原子是折行的最小单位；只有它本身比整行还宽时才逐字符硬断（保证每步都前进）。
    fn push_atom(&mut self, atom: &str, atom_width: usize) {
        if self.line_width + atom_width <= self.width {
            self.line.push_str(atom);
            self.line_width += atom_width;
            return;
        }

        // 当前行放不下：先定稿（行尾的分隔空白随之吞掉），再拿空行重新安排这个原子
        self.flush_line();
        if atom_width <= self.width {
            self.line.push_str(atom);
            self.line_width = atom_width;
            return;
        }

        // 原子自身就超宽（超长无空白串）：按显示宽度硬断
        for c in atom.chars() {
            let cw = char_width(c);
            if self.line_width > 0 && self.line_width + cw > self.width {
                self.flush_line();
            }
            self.line.push(c);
            self.line_width += cw;
        }
    }

    /// 收一段分隔空白：放得下就留在行尾（它是后续断点的候选），放不下就吞掉。
    ///
    /// 吞掉而不是挪到下一行：断点处的空白属于上一行的尾巴，续行必须以实字符开头。
    fn push_space(&mut self, space: &str, space_width: usize) {
        if self.line_width + space_width <= self.width {
            self.line.push_str(space);
            self.line_width += space_width;
        } else {
            self.flush_line();
        }
    }

    /// 定稿当前行：行尾空白在此吞掉；整行都是空白（例如前导缩进没挤下任何字符）就不产出。
    fn flush_line(&mut self) {
        let line = self.line.trim_end();
        if !line.is_empty() {
            self.lines.push(line.to_string());
        }
        self.line.clear();
        self.line_width = 0;
    }

    /// 收尾：定稿最后一行。
    fn finish(mut self) -> Vec<String> {
        self.flush_line();
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::{char_width, display_width, wrap};

    /// 去掉全部空白后的字符序列：折行不变量锚在它上面（不丢、不增、不重排）。
    fn non_whitespace(text: &str) -> Vec<char> {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn char_width_table() {
        assert_eq!(char_width('你'), 2);
        assert_eq!(char_width('，'), 2);
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('\n'), 0);
        // 其余控制字符同样不占列；组合字符按 1 近似；普通空格算 1
        assert_eq!(char_width('\t'), 0);
        assert_eq!(char_width('\u{7}'), 0);
        assert_eq!(char_width('\u{301}'), 1);
        assert_eq!(char_width(' '), 1);
    }

    #[test]
    fn display_width_sums_char_widths() {
        assert_eq!(display_width("你好abc"), 7);
        assert_eq!(display_width(""), 0);
        assert_eq!(display_width("，。"), 4);
        // 控制字符不占列（`\n` 既不换行也不占宽）
        assert_eq!(display_width("a\nb"), 2);
    }

    #[test]
    fn wrap_breaks_between_cjk_chars() {
        assert_eq!(wrap("你好世界", 4), ["你好", "世界"]);
    }

    #[test]
    fn wrap_prefers_whitespace_and_swallows_the_separator() {
        assert_eq!(wrap("hello world", 8), ["hello", "world"]);
        // 断点处的整段分隔空白都被吞掉，续行不以空白开头
        assert_eq!(wrap("hello   world", 8), ["hello", "world"]);
        assert_eq!(wrap("ab cd ef", 5), ["ab cd", "ef"]);
    }

    #[test]
    fn wrap_hard_breaks_overlong_runs() {
        assert_eq!(wrap("aaaaaa", 3), ["aaa", "aaa"]);
        // 断点优先空白：长串后面还有词时，先把空白之前的放满再折断
        assert_eq!(wrap("aaaaa bbbb", 5), ["aaaaa", "bbbb"]);
    }

    #[test]
    fn wrap_gives_an_overwide_char_its_own_line() {
        // 单个宽字符比整行还宽时只能独占一行（不丢字符、不空转）
        assert_eq!(wrap("你好", 1), ["你", "好"]);
        assert_eq!(wrap("ab你好", 4), ["ab你", "好"]);
    }

    #[test]
    fn wrap_is_defensive_without_width_or_content() {
        // 宽度 0 无法折行：原样一整行返回，由调用方兜底
        assert_eq!(wrap("你好", 0), ["你好"]);
        // 空串（以及纯空白串）没有内容，不产出空行
        assert!(wrap("", 5).is_empty());
        assert!(wrap("", 0).is_empty());
        assert!(wrap("   \t ", 4).is_empty());
    }

    #[test]
    fn wrap_with_huge_width_never_breaks() {
        // 任务层的「不折行」模式（非 tty）用 `usize::MAX`：整段一行，保持每行可 grep
        assert_eq!(wrap("你好 world 们", usize::MAX), ["你好 world 们"]);
    }

    #[test]
    fn wrap_keeps_every_non_whitespace_char() {
        let corpus = [
            "the quick brown fox jumps over the lazy dog",
            "supercalifragilisticexpialidocious",
            "你好世界，这是一个混合了英文 word 与标点的句子。",
            "混合\t制表\n与换行 以及  多个空格",
            "  前导空格和结尾换行\n",
            "短",
            "",
            "   \t ",
        ];

        for text in corpus {
            for width in 1..=12 {
                let lines = wrap(text, width);
                assert_eq!(
                    non_whitespace(&lines.join("")),
                    non_whitespace(text),
                    "非空白字符序列必须一字不差：{text:?} @ width={width} → {lines:?}"
                );
                for line in &lines {
                    // 行是「实」的：不产出空行，也不留行尾的悬挂空白
                    assert!(!line.is_empty(), "{text:?} @ width={width} → {lines:?}");
                    assert!(
                        !line.ends_with(char::is_whitespace),
                        "{text:?} @ width={width} → {lines:?}"
                    );
                    // 整行宽度不超过目标（单个宽字符比整行宽时例外：它只能独占一行）
                    assert!(
                        display_width(line) <= width.max(2),
                        "{text:?} @ width={width} → {lines:?}"
                    );
                }
            }
        }
    }
}
