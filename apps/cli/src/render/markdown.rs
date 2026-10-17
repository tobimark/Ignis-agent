//! Markdown 子集与流式装配：把模型输出的增量文本切成带样式的可渲染片段。
//!
//! 本模块只管「哪些字符是什么样式」：宽度与折行是 `width.rs` 的事，两者的消费者是 Task 8 的渲染器。
//! 子集（设计 §6.3 决策 #12）：围栏代码块、`#`–`####` 标题、`- `/`* ` 无序列表、`> ` 引用、
//! `---` 分隔线、行内 `` `code` `` 与 `**粗体**`；表格/链接/图片/斜体原样通过。
//!
//! **流式纪律**（设计 §6.3）：
//! - 普通文本逐字符即时输出，不做整行缓冲；
//! - 行首最多暂存 4 个字符判别块标记（围栏/标题/列表/引用）。两处例外都要等到行末才有证据，
//!   故按「候选前缀」暂存到本行结束（都不跨行、也不整行缓冲）：整行 `-` 的分隔线（≥3 个 `-`）、
//!   围栏体里的闭合候选行（≥3 个反引号 + 空白；出现别的字符就按原文回退）；
//! - 行内标记（`` ` ``、`**`）从标记起点暂存到配对成功或本行结束：未配对在行末**按字面输出**，
//!   不吞字符、不改写；因此长的 `**粗体**` 也正常工作（不设放弃窗口）；
//! - 同一输入按任意分片 `feed` + `finish`，输出拼接与整体投喂逐字节一致（不变量测试锁定）。
//!
//! 暂存缓冲一律按 `char` 操作，不做可能切坏 UTF-8 的字节下标。
//!
//! 有意的取舍（超出手集的形态一律按字面通过，不做二次猜测）：
//! - 换行永远是单独的 [`Style::Plain`] 段：它是行分隔而不是内容，渲染器据此断行；
//! - 行内标记在标题/引用行**照常生效**：行的块级样式（`Bold`/`Dim`）只是该行普通文本的默认样式；
//! - 围栏内是逐字符原文（[`Style::Dim`]），不做任何行内转换；围栏统一按 3 个反引号识别，
//!   不做长围栏配对；
//! - 相邻同风格片段会合并（`\n` 段除外）：同一输入分片方式不同可能得到不同的分段数，
//!   但拼接后的字节序相同。

/// 分隔线（整行 ≥3 个 `-`）的输出字面量：8 个 U+2500。
const HR_LINE: &str = "────────";

/// 围栏标记的反引号个数（起始与闭合都按 3 个识别，不做长围栏配对）。
const FENCE_TICKS: usize = 3;

/// 标题最多认的井号个数：`#`–`####`，第 5 个就说明是普通文本。
const MAX_HEADING_HASHES: usize = 4;

/// 分隔线至少要有的 `-` 个数。
const MIN_RULE_DASHES: usize = 3;

/// 分隔线允许的尾随空白长度上限（超过就按正文回放，避免无界暂存）。
const RULE_TRAILING_BLANKS: usize = 4;

/// 片段的样式。
///
/// 只有这四种：颜色/终端能力由渲染器决定（`color=false` 时零 ANSI），本模块不碰 ANSI。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// 正文。
    Plain,
    /// 加粗（`**…**` 与标题行）。
    Bold,
    /// 行内代码（`` `…` ``）。
    InlineCode,
    /// 暗色（围栏内容、引用行、分隔线）。
    Dim,
}

/// 一段可渲染文本。
///
/// 相邻且样式相同的字符会合并成一段（`\n` 段除外）：同一输入的分片方式不同，分段数可能不同，
/// 但拼接后的字节序相同（不变量契约）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// 片段文本。
    pub text: String,
    /// 片段样式。
    pub style: Style,
}

/// 块级状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    /// 普通行：先做行首判别，再做行内扫描。
    Plain,
    /// 围栏起始行：本行剩下的是语言标记（信息串），丢弃；行末进入围栏体。
    FenceOpen,
    /// 围栏体：逐字符原文（`Dim`），只在行首认闭合行。
    FenceBody,
    /// 疑似围栏闭合行：暂存 `≥3` 个反引号与随后的空白，行末确认、否则回退成正文。
    FenceClose,
}

/// 暂存原因：暂存期间字符不输出，等证据齐了再落定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// 不在暂存：字符即时处理。
    None,
    /// 行首：最多 4 个字符判别块标记（`---` 连续段除外）。
    LineStart,
    /// 已见一个 `*`：再来一个才是 `**`。
    Star,
    /// 已见 `` ` ``，等配对的反引号。
    Code,
    /// 已见 `**`，等配对的 `**`。
    Bold,
    /// `**` 内已见一个 `*`：再来一个才配对。
    BoldStar,
    /// 围栏体行首：暂存 1–2 个反引号（够 3 个就是闭合行）。
    FenceTicks,
}

/// 行首判别的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// 证据不足：继续暂存。
    Hold,
    /// 命中块标记：标记字符不再作为正文输出。
    Hit(BlockMark),
    /// 不是块标记：暂存的字符按行内规则回放。
    Reject,
}

/// 行首能命中的块标记。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockMark {
    /// ```` ``` ```` 围栏。
    Fence,
    /// `#`–`####` 标题。
    Heading,
    /// `- ` / `* ` 列表。
    List,
    /// `> ` 引用。
    Quote,
}

/// 流式 Markdown 装配器：`feed` 收增量、`finish` 收尾。
///
/// 只做「行级 + 行内」的样式装配，不做宽度与折行（那是 `width.rs` 与渲染器的事）。
pub struct MdStream {
    /// 块级状态。
    block: Block,
    /// 暂存原因。
    pending: Pending,
    /// 暂存缓冲：一律按 `char` 存，避开会切坏 UTF-8 的字节下标。
    buf: Vec<char>,
    /// 当前行普通文本的样式（引用行 `Dim`、标题行 `Bold`，其余 `Plain`）。
    line_style: Style,
}

impl MdStream {
    /// 建一个空的装配器：处于「普通行、行首」的初始状态。
    pub fn new() -> Self {
        Self {
            block: Block::Plain,
            pending: Pending::LineStart,
            buf: Vec::new(),
            line_style: Style::Plain,
        }
    }

    /// 投喂一段增量文本，返回本次新生成的可渲染片段。
    ///
    /// 暂存中的字符不算「可渲染」，会在后续 `feed` 或 [`MdStream::finish`] 里落定。
    pub fn feed(&mut self, text: &str) -> Vec<Segment> {
        let mut out = Vec::new();
        for c in text.chars() {
            self.step(c, &mut out);
        }
        out
    }

    /// 收尾：把暂存按契约落定（未配对的行内标记按字面输出、未闭合围栏不回吐标记），并复位状态。
    ///
    /// 不补换行：收尾处的换行由渲染器负责（Task 8），本模块只保证「不吞字符」。
    /// 复位后可再次 `feed` 下一段流；已吐出的片段不受影响。
    pub fn finish(&mut self) -> Vec<Segment> {
        let mut out = Vec::new();
        self.flush(&mut out);
        *self = Self::new();
        out
    }

    /// 处理一个字符。
    fn step(&mut self, c: char, out: &mut Vec<Segment>) {
        // 换行先走行末结算：任何暂存都在行末按契约（配对或字面）落定
        if c == '\n' {
            self.end_line(out);
            return;
        }
        match self.block {
            Block::Plain => {
                if self.pending == Pending::LineStart {
                    self.step_line_start(c, out);
                } else {
                    self.step_inline(c, out);
                }
            }
            // 起始行的语言标记不输出
            Block::FenceOpen => {}
            Block::FenceBody => self.step_fence(c, out),
            Block::FenceClose => self.step_fence_close(c, out),
        }
    }

    /// 行首：暂存字符判别块标记；判别落定后本行不再走这条路径。
    fn step_line_start(&mut self, c: char, out: &mut Vec<Segment>) {
        self.buf.push(c);
        match classify(&self.buf) {
            Verdict::Hold => {}
            Verdict::Hit(mark) => {
                let buf = std::mem::take(&mut self.buf);
                self.pending = Pending::None;
                self.apply(mark, &buf, out);
            }
            // 不是块标记：暂存的字符按行内规则回放（可能重新挂起）
            Verdict::Reject => {
                let buf = std::mem::take(&mut self.buf);
                self.pending = Pending::None;
                for ch in buf {
                    self.step_inline(ch, out);
                }
            }
        }
    }

    /// 命中块标记：`buf` 是已判别的标记字符。
    fn apply(&mut self, mark: BlockMark, buf: &[char], out: &mut Vec<Segment>) {
        match mark {
            // 起始行与语言标记都不输出
            BlockMark::Fence => self.block = Block::FenceOpen,
            // 标题去标记、整行加粗
            BlockMark::Heading => self.line_style = Style::Bold,
            // 引用去标记、整行暗色
            BlockMark::Quote => self.line_style = Style::Dim,
            // 列表保留 `- ` 前缀（续行缩进由渲染器负责）
            BlockMark::List => {
                let text: String = buf.iter().collect();
                push(out, &text, self.line_style);
            }
        }
    }

    /// 行内扫描：普通字符即时输出，`` ` `` 与 `**` 从标记起点暂存到配对或行末。
    fn step_inline(&mut self, c: char, out: &mut Vec<Segment>) {
        match self.pending {
            Pending::None => match c {
                '*' => {
                    self.buf.push('*');
                    self.pending = Pending::Star;
                }
                '`' => {
                    self.buf.push('`');
                    self.pending = Pending::Code;
                }
                _ => push_char(out, c, self.line_style),
            },
            Pending::Star => {
                if c == '*' {
                    self.buf.push('*');
                    self.pending = Pending::Bold;
                } else {
                    // 单个 `*`（斜体）不在子集内：按字面吐出后照常处理本字符
                    self.release(out);
                    self.step_inline(c, out);
                }
            }
            Pending::Code => {
                if c == '`' {
                    // 配对成功：去掉两侧反引号，内文不做二次转换
                    let inner = self.take_inner(1, 0);
                    self.pending = Pending::None;
                    push(out, &inner, Style::InlineCode);
                } else {
                    self.buf.push(c);
                }
            }
            Pending::Bold => {
                if c == '*' {
                    self.buf.push('*');
                    self.pending = Pending::BoldStar;
                } else {
                    self.buf.push(c);
                }
            }
            Pending::BoldStar => {
                if c == '*' {
                    // 配对成功：去掉两侧的 `**`（闭标记只有第一个 `*` 进过缓冲）
                    let inner = self.take_inner(2, 1);
                    self.pending = Pending::None;
                    push(out, &inner, Style::Bold);
                } else {
                    self.buf.push(c);
                    self.pending = Pending::Bold;
                }
            }
            // 行首判别与围栏暂存不走行内扫描
            Pending::LineStart | Pending::FenceTicks => {}
        }
    }

    /// 围栏体：逐字符原文输出（`Dim`），只在行首暂存 1–2 个反引号以判别闭合行。
    fn step_fence(&mut self, c: char, out: &mut Vec<Segment>) {
        if self.pending == Pending::FenceTicks {
            if c == '`' {
                self.buf.push('`');
                if self.buf.len() == FENCE_TICKS {
                    self.block = Block::FenceClose;
                    self.pending = Pending::None;
                }
                return;
            }
            // 不是反引号：暂存的反引号是围栏正文，按原文吐出
            self.flush_fence_ticks(out);
        }
        push_char(out, c, Style::Dim);
    }

    /// 疑似闭合行：`≥3` 个反引号 +（可选）空白；出现别的字符就按原文回退成围栏正文。
    fn step_fence_close(&mut self, c: char, out: &mut Vec<Segment>) {
        if c == '`' || is_blank(c) {
            self.buf.push(c);
            return;
        }
        let text: String = std::mem::take(&mut self.buf).into_iter().collect();
        push(out, &text, Style::Dim);
        self.block = Block::FenceBody;
        self.pending = Pending::None;
        push_char(out, c, Style::Dim);
    }

    /// 行末结算：暂存按契约落定，然后输出换行。
    ///
    /// 围栏标记行的换行属于标记行本身，不输出（否则围栏前后会多出空行）。
    fn end_line(&mut self, out: &mut Vec<Segment>) {
        match self.block {
            Block::Plain => {
                if self.pending == Pending::LineStart {
                    let buf = std::mem::take(&mut self.buf);
                    self.pending = Pending::None;
                    // 整行都是 `-`（≥3 个）才算分隔线：这个证据只有行末才有
                    if is_rule(&buf) {
                        push(out, HR_LINE, Style::Dim);
                        newline(out);
                        self.start_line();
                        return;
                    }
                    for ch in buf {
                        self.step_inline(ch, out);
                    }
                }
                self.release(out);
                newline(out);
                self.start_line();
            }
            Block::FenceOpen => {
                self.block = Block::FenceBody;
                self.start_fence_line();
            }
            Block::FenceBody => {
                self.flush_fence_ticks(out);
                newline(out);
                self.start_fence_line();
            }
            // 闭合行确认：标记行与它的换行都不输出
            Block::FenceClose => {
                self.block = Block::Plain;
                self.start_line();
            }
        }
    }

    /// 收尾结算：暂存一律按契约落定。
    fn flush(&mut self, out: &mut Vec<Segment>) {
        match self.block {
            Block::Plain => {
                if self.pending == Pending::LineStart {
                    let buf = std::mem::take(&mut self.buf);
                    self.pending = Pending::None;
                    // 行末确认的分隔线（`finish` 前没有换行的 `---`）
                    if is_rule(&buf) {
                        push(out, HR_LINE, Style::Dim);
                        return;
                    }
                    for ch in buf {
                        self.step_inline(ch, out);
                    }
                }
                // 未配对的行内标记按字面吐出（不吞字符、不改写）
                self.release(out);
            }
            // 行首只暂存了 1–2 个反引号：按围栏原文吐出
            Block::FenceBody => self.flush_fence_ticks(out),
            // 起始行未收尾：标记行不输出，也不进入围栏体
            Block::FenceOpen => {}
            // 闭合行未收尾：按已闭合处理（围栏标记不输出）
            Block::FenceClose => {}
        }
    }

    /// 把暂存缓冲按当前行样式原样吐出（未配对的标记、单个 `*`、行末结算都用它）。
    fn release(&mut self, out: &mut Vec<Segment>) {
        let buf = std::mem::take(&mut self.buf);
        self.pending = Pending::None;
        let text: String = buf.iter().collect();
        push(out, &text, self.line_style);
    }

    /// 围栏体行首暂存的反引号按围栏原文（`Dim`）吐出。
    fn flush_fence_ticks(&mut self, out: &mut Vec<Segment>) {
        let buf = std::mem::take(&mut self.buf);
        self.pending = Pending::None;
        let text: String = buf.iter().collect();
        push(out, &text, Style::Dim);
    }

    /// 取走暂存缓冲里被前后标记包住的内文（按字符下标取，不碰字节）。
    ///
    /// 闭标记的最后一个字符是当前字符、不进缓冲，故 `suffix` 按实际入缓冲的字符数给。
    fn take_inner(&mut self, prefix: usize, suffix: usize) -> String {
        let buf = std::mem::take(&mut self.buf);
        let end = buf.len().saturating_sub(suffix);
        buf.get(prefix..end).unwrap_or_default().iter().collect()
    }

    /// 开始新的一行：回到普通行的行首状态。
    fn start_line(&mut self) {
        self.block = Block::Plain;
        self.pending = Pending::LineStart;
        self.buf.clear();
        self.line_style = Style::Plain;
    }

    /// 开始围栏体的一行：行首暂存反引号，准备认闭合行。
    fn start_fence_line(&mut self) {
        self.pending = Pending::FenceTicks;
        self.buf.clear();
    }
}

impl Default for MdStream {
    fn default() -> Self {
        Self::new()
    }
}

/// 行首判别：只依赖本行开头的字符内容，不依赖分片边界（分片不变量的前提）。
///
/// 暂存上限按 4 个字符设计；`---` 是例外：要等本行结束才能确认整行都是 `-`，
/// 故按连续 `-` 段暂存（段本身可能比 4 长，但最短证据仍是行首 3 个 `-`）。
fn classify(buf: &[char]) -> Verdict {
    let Some(&first) = buf.first() else {
        // 空缓冲：还没字符，继续等（调用方保证非空，这里是防御）
        return Verdict::Hold;
    };
    match first {
        // 围栏：行首连续 3 个反引号
        '`' => {
            if buf.len() >= FENCE_TICKS && buf[..FENCE_TICKS].iter().all(|c| *c == '`') {
                Verdict::Hit(BlockMark::Fence)
            } else if buf.len() < FENCE_TICKS && buf.iter().all(|c| *c == '`') {
                Verdict::Hold
            } else {
                // 例如行首的 `code`：单个反引号是行内标记，回放给行内扫描器
                Verdict::Reject
            }
        }
        // 标题：`#`–`####` + 空格
        '#' => {
            let hashes = buf.iter().take_while(|c| **c == '#').count();
            if hashes > MAX_HEADING_HASHES {
                return Verdict::Reject;
            }
            match buf.get(hashes) {
                // 井号后的字符还没到：继续等（最多 4 个井号）
                None => Verdict::Hold,
                Some(' ') => Verdict::Hit(BlockMark::Heading),
                Some(_) => Verdict::Reject,
            }
        }
        // 列表（`- `）与分隔线（≥3 个 `-` 的整行）
        '-' => {
            let dashes = buf.iter().take_while(|c| **c == '-').count();
            let rest = &buf[dashes..];
            if rest.is_empty() {
                // 连续 `-` 段：列表要再看一个字符，分隔线要等到行末
                return Verdict::Hold;
            }
            if dashes == 1 && rest[0] == ' ' {
                return Verdict::Hit(BlockMark::List);
            }
            if dashes >= MIN_RULE_DASHES
                && rest.len() <= RULE_TRAILING_BLANKS
                && rest.iter().all(|c| is_blank(*c))
            {
                // 分隔线的尾随空白：仍要等行末
                return Verdict::Hold;
            }
            Verdict::Reject
        }
        // 列表：`* `（单个 `*` 的斜体不在子集内，按字面通过）
        '*' => {
            if buf.len() == 1 {
                Verdict::Hold
            } else if buf[1] == ' ' {
                Verdict::Hit(BlockMark::List)
            } else {
                Verdict::Reject
            }
        }
        // 引用：`> `
        '>' => {
            if buf.len() == 1 {
                Verdict::Hold
            } else if buf[1] == ' ' {
                Verdict::Hit(BlockMark::Quote)
            } else {
                Verdict::Reject
            }
        }
        _ => Verdict::Reject,
    }
}

/// 整行分隔线的判据：≥3 个 `-`，后面只允许空白。
fn is_rule(buf: &[char]) -> bool {
    let dashes = buf.iter().take_while(|c| **c == '-').count();
    dashes >= MIN_RULE_DASHES && buf[dashes..].iter().all(|c| is_blank(*c))
}

/// 块标记后面的空白：只认半角空格与制表符（全角空格按普通字符处理）。
fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// 追加一个片段：与上一段同风格时合并（`\n` 段两侧都不合并，保证换行单独成段）。
fn push(out: &mut Vec<Segment>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        let mergeable = last.style == style && last.text != "\n" && text != "\n";
        if mergeable {
            last.text.push_str(text);
            return;
        }
    }
    out.push(Segment {
        text: text.to_string(),
        style,
    });
}

/// 追加一个字符：与上一段同风格时合并。
fn push_char(out: &mut Vec<Segment>, c: char, style: Style) {
    if let Some(last) = out.last_mut() {
        let mergeable = last.style == style && last.text != "\n";
        if mergeable {
            last.text.push(c);
            return;
        }
    }
    out.push(Segment {
        text: c.to_string(),
        style,
    });
}

/// 输出换行段：永远单独成段，渲染器据此断行。
fn newline(out: &mut Vec<Segment>) {
    out.push(Segment {
        text: "\n".to_string(),
        style: Style::Plain,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 预期片段（测试里逐段断言用）。
    fn seg(text: &str, style: Style) -> Segment {
        Segment {
            text: text.to_string(),
            style,
        }
    }

    /// 片段拼接：不变量只比较拼接后的字节序。
    fn join(segs: Vec<Segment>) -> String {
        segs.into_iter().map(|s| s.text).collect()
    }

    /// 整体投喂 + 收尾：行为表格用例的默认入口。
    fn render(text: &str) -> Vec<Segment> {
        let mut md = MdStream::new();
        let mut out = md.feed(text);
        out.extend(md.finish());
        out
    }

    /// 分片不变量的语料：覆盖标题/列表/引用/分隔线/围栏/行内/中文/空行/未收尾行。
    const CORPUS: &str = r"# 一级标题
正文里有 **粗体** 与 `行内代码`，还有单个 * 星号。
## 二级标题
- 列表项 **加粗**
* 另一种列表
> 引用一行 **内联**
---

```
let x = **不转换**; // 中文注释
```

##### 五个井号不是标题
收尾不带换行";

    /// 未收尾语料：未配对的标记、被切断的块标记、未闭合的围栏、结尾无换行。
    const LOOSE: &str = r"未配对 **粗体
半截 `代码 与 * 单星号
> 引用 **未配对
- 列表
####
--- 
##### 五井号
**长
```
围栏未闭合 **不做行内** ` 反引号
收尾无换行";

    #[test]
    fn plain_text_is_emitted_immediately_and_newlines_are_their_own_segment() {
        let mut md = MdStream::new();
        assert_eq!(join(md.feed("ab")), "ab", "普通文本即时输出，不整行缓冲");
        assert_eq!(
            md.feed("\n"),
            vec![seg("\n", Style::Plain)],
            "换行以 \\n 明文段单独输出"
        );
        assert!(md.finish().is_empty(), "没有暂存时收尾不吐东西");
    }

    #[test]
    fn line_start_holds_at_most_four_chars_for_marker_detection() {
        let mut md = MdStream::new();
        assert!(md.feed("####").is_empty(), "块标记证据不足时暂存");
        assert_eq!(
            join(md.feed("x")),
            "####x",
            "第 5 个字符排除标题后按字面回放"
        );

        let mut md = MdStream::new();
        assert_eq!(join(md.feed("plain")), "plain", "非标记行首直接输出");
    }

    #[test]
    fn headings_strip_the_marker_and_bold_the_whole_line() {
        assert_eq!(
            render("# Title\n"),
            vec![seg("Title", Style::Bold), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render("#### Deep\n"),
            vec![seg("Deep", Style::Bold), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render("#\n"),
            vec![seg("#", Style::Plain), seg("\n", Style::Plain)],
            "只有井号、后面没有空格：不判标题"
        );
        assert_eq!(
            render("#NoSpace\n"),
            vec![seg("#NoSpace", Style::Plain), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render("##### Five\n"),
            vec![seg("##### Five", Style::Plain), seg("\n", Style::Plain)],
            "5 个井号不是标题"
        );
        assert_eq!(
            render("a # b\n"),
            vec![seg("a # b", Style::Plain), seg("\n", Style::Plain)],
            "行中的 # 不判标题"
        );
    }

    #[test]
    fn fences_hide_their_markers_and_keep_the_body_raw_and_dim() {
        // ```rust 语言标记行不输出；围栏内的 **x** 不做行内转换
        assert_eq!(
            render("```rust\nlet x = **y**;\n```\n"),
            vec![seg("let x = **y**;", Style::Dim), seg("\n", Style::Plain)]
        );
        // 围栏结束后正文恢复
        assert_eq!(
            render("```\na\n```\nafter\n"),
            vec![
                seg("a", Style::Dim),
                seg("\n", Style::Plain),
                seg("after", Style::Plain),
                seg("\n", Style::Plain),
            ]
        );
        // 未闭合围栏到 finish 仍按已进入处理：不回吐标记
        assert_eq!(
            render("```\nraw **x**\n"),
            vec![seg("raw **x**", Style::Dim), seg("\n", Style::Plain)]
        );
        // 只有围栏标记行：什么都不输出
        assert!(render("```rust\n").is_empty());
        // 围栏内以反引号开头、但不是闭合行的内容照常原文输出
        assert_eq!(
            render("```\n```rust\nx\n"),
            vec![
                seg("```rust", Style::Dim),
                seg("\n", Style::Plain),
                seg("x", Style::Dim),
                seg("\n", Style::Plain),
            ]
        );
    }

    #[test]
    fn lists_keep_their_prefix_and_still_convert_inline_markers() {
        assert_eq!(
            render("- a\n"),
            vec![seg("- a", Style::Plain), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render("* b\n"),
            vec![seg("* b", Style::Plain), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render("- **b**\n"),
            vec![
                seg("- ", Style::Plain),
                seg("b", Style::Bold),
                seg("\n", Style::Plain),
            ]
        );
        // 有序列表不在子集内：原样通过
        assert_eq!(
            render("1. x\n"),
            vec![seg("1. x", Style::Plain), seg("\n", Style::Plain)]
        );
    }

    #[test]
    fn quotes_strip_the_marker_and_dim_the_line() {
        assert_eq!(
            render("> hi\n"),
            vec![seg("hi", Style::Dim), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render(">hi\n"),
            vec![seg(">hi", Style::Plain), seg("\n", Style::Plain)],
            "引用标记要求 > 后跟空格"
        );
        // 行内标记在引用行照常生效，未标记的文本按 Dim
        assert_eq!(
            render("> hi **b**\n"),
            vec![
                seg("hi ", Style::Dim),
                seg("b", Style::Bold),
                seg("\n", Style::Plain),
            ]
        );
    }

    #[test]
    fn dash_lines_become_a_dim_rule() {
        let rule = seg("────────", Style::Dim);
        assert_eq!(HR_LINE, "────────", "分隔线字面量（8 个 U+2500）");
        assert_eq!(render("---\n"), vec![rule.clone(), seg("\n", Style::Plain)]);
        assert_eq!(
            render("-----\n"),
            vec![rule.clone(), seg("\n", Style::Plain)],
            "3 个以上都是分隔线"
        );
        assert_eq!(
            render("--- \n"),
            vec![rule, seg("\n", Style::Plain)],
            "尾随空白被吞掉"
        );
        // 不足以构成分隔线：按字面输出
        assert_eq!(
            render("--\n"),
            vec![seg("--", Style::Plain), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render("--- x\n"),
            vec![seg("--- x", Style::Plain), seg("\n", Style::Plain)]
        );
    }

    #[test]
    fn inline_code_and_bold_are_converted_with_markers_stripped() {
        assert_eq!(
            render("a `code` b\n"),
            vec![
                seg("a ", Style::Plain),
                seg("code", Style::InlineCode),
                seg(" b", Style::Plain),
                seg("\n", Style::Plain),
            ]
        );
        assert_eq!(
            render("a **b** c\n"),
            vec![
                seg("a ", Style::Plain),
                seg("b", Style::Bold),
                seg(" c", Style::Plain),
                seg("\n", Style::Plain),
            ]
        );
        assert_eq!(
            render("**a**b**c**\n"),
            vec![
                seg("a", Style::Bold),
                seg("b", Style::Plain),
                seg("c", Style::Bold),
                seg("\n", Style::Plain),
            ]
        );
        assert_eq!(
            render("中文**粗体**。\n"),
            vec![
                seg("中文", Style::Plain),
                seg("粗体", Style::Bold),
                seg("。", Style::Plain),
                seg("\n", Style::Plain),
            ]
        );
        // 斜体/表格/链接/图片不在子集内：原样通过
        assert_eq!(
            render("a * b\n"),
            vec![seg("a * b", Style::Plain), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render("| a | b |\n"),
            vec![seg("| a | b |", Style::Plain), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render("[x](y) ![z](w)\n"),
            vec![seg("[x](y) ![z](w)", Style::Plain), seg("\n", Style::Plain)]
        );
    }

    #[test]
    fn unclosed_inline_markers_are_emitted_literally_at_line_end() {
        assert_eq!(
            render("a **b\n"),
            vec![seg("a **b", Style::Plain), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render("a `b\n"),
            vec![seg("a `b", Style::Plain), seg("\n", Style::Plain)]
        );
        assert_eq!(
            render("**x"),
            vec![seg("**x", Style::Plain)],
            "收尾时未配对按字面吐出"
        );
        // 前半段已在 feed 里吐出、后半段在收尾时吐出：分属两次调用，段落不会跨调用合并
        assert_eq!(join(render("半截 `代码")), "半截 `代码");
        assert_eq!(
            render("半截 `代码")
                .iter()
                .map(|s| s.style)
                .collect::<Vec<_>>(),
            vec![Style::Plain, Style::Plain]
        );
    }

    #[test]
    fn long_bold_spans_wait_without_a_give_up_window() {
        let body = "很长".repeat(150); // 300 个字符
        let mut md = MdStream::new();
        assert!(md.feed("**").is_empty(), "开标记先暂存");
        assert!(md.feed(&body).is_empty(), "长内文不设放弃窗口");
        let mut out = md.feed("**\n");
        out.extend(md.finish());
        assert_eq!(out, vec![seg(&body, Style::Bold), seg("\n", Style::Plain)]);
    }

    #[test]
    fn markers_split_across_feeds_classify_the_same() {
        let mut md = MdStream::new();
        let mut out = Vec::new();
        for chunk in [
            "#", " Ti", "tle", "\n", "-", " ", "item", "\n", "`", "`", "`", "rust", "\n", "in",
            "\n", "`", "`", "`", "\n",
        ] {
            out.extend(md.feed(chunk));
        }
        out.extend(md.finish());

        // 拼接与整体投喂一致（分段数不跨调用合并，故逐段比较没有意义）
        assert_eq!(
            join(out.clone()),
            join(render("# Title\n- item\n```rust\nin\n```\n"))
        );
        // 被切开的标记照样判对：标题整行 Bold、围栏内容 Dim、列表保留前缀
        let by_style = |style: Style| -> String {
            out.iter()
                .filter(|s| s.style == style)
                .map(|s| s.text.as_str())
                .collect()
        };
        assert_eq!(by_style(Style::Bold), "Title", "被切开的标题仍整行 Bold");
        assert_eq!(by_style(Style::Dim), "in", "被切开的围栏仍按原文 Dim");
        assert_eq!(
            by_style(Style::Plain),
            // 标题行末的换行 + 列表行（保留前缀）+ 围栏正文行末的换行；标记行的换行不输出
            "\n- item\n\n",
            "列表前缀保留，围栏标记行（含其换行）不输出"
        );
    }

    #[test]
    fn finish_resets_state_for_the_next_stream() {
        let mut md = MdStream::new();
        let mut out = md.feed("```\n**未闭合");
        out.extend(md.finish());
        assert_eq!(join(out), "**未闭合", "围栏内容按原文吐出、不回吐标记");
        assert_eq!(join(md.feed("# 新\n")), "新\n", "收尾后状态复位");
        assert!(md.finish().is_empty());
    }

    /// 确定性 xorshift64：分片不变量要可复现，不引 `rand`（零新增依赖）。
    struct XorShift(u64);

    impl XorShift {
        /// 固定种子的伪随机源（种子置位，xorshift 不会卡死在 0）。
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }

        /// 下一个 64 位状态。
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// `0..bound` 的取值。
        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// 按随机分片投喂（分片长度 0..=5，含退化成逐字符的情形），返回拼接后的文本。
    fn render_split(text: &str, rng: &mut XorShift) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut md = MdStream::new();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            let end = (i + rng.below(6) as usize).min(chars.len()).max(i + 1);
            let chunk: String = chars[i..end].iter().collect();
            out.push_str(&join(md.feed(&chunk)));
            i = end;
        }
        out.push_str(&join(md.finish()));
        out
    }

    #[test]
    fn random_split_feeding_matches_whole_feeding_byte_for_byte() {
        // 语料本身要真的碰到子集，否则不变量测试是空转
        let whole = join(render(CORPUS));
        assert!(whole.contains("一级标题"), "标题未被渲染：{whole}");
        assert!(whole.contains("**不转换**"), "围栏内容未原样输出：{whole}");
        let loose = join(render(LOOSE));
        assert!(loose.contains("####"), "整行井号应按字面输出：{loose}");
        assert!(
            loose.contains("围栏未闭合"),
            "未闭合围栏内容未输出：{loose}"
        );

        let mut rng = XorShift::new(0x2545_F491_4F6C_DD1D);
        for (name, corpus, expected) in [("well-formed", CORPUS, whole), ("loose", LOOSE, loose)] {
            for round in 0..100 {
                let split = render_split(corpus, &mut rng);
                assert_eq!(
                    split, expected,
                    "{name} 第 {round} 轮随机分片与整体投喂不一致"
                );
            }
        }
    }

    #[test]
    fn char_by_char_feeding_matches_whole_feeding() {
        for (name, corpus) in [("well-formed", CORPUS), ("loose", LOOSE)] {
            let whole = join(render(corpus));
            let mut md = MdStream::new();
            let mut out = String::new();
            // 逐字符投喂：分片边界落在每个字符之间（中文/全角同理，不切坏 UTF-8）
            for c in corpus.chars() {
                out.push_str(&join(md.feed(&c.to_string())));
            }
            out.push_str(&join(md.finish()));
            assert_eq!(out, whole, "{name}：逐字符投喂与整体投喂不一致");
        }
    }

    /// 把片段展开成「字符 + 样式」序列：样式粒度也要与整体投喂一致。
    fn styled_chars(segs: &[Segment]) -> Vec<(char, Style)> {
        segs.iter()
            .flat_map(|s| s.text.chars().map(move |c| (c, s.style)))
            .collect()
    }

    #[test]
    fn every_single_split_point_matches_whole_feeding() {
        for (name, corpus) in [("well-formed", CORPUS), ("loose", LOOSE)] {
            let whole = render(corpus);
            let whole_bytes = join(whole.clone());
            let whole_styles = styled_chars(&whole);
            let chars: Vec<char> = corpus.chars().collect();

            // 穷举二分点：随机的 100 轮之外，保证没有「单个边界」上的丢字/重字
            for at in 0..=chars.len() {
                let head: String = chars[..at].iter().collect();
                let tail: String = chars[at..].iter().collect();
                let mut md = MdStream::new();
                let mut out = md.feed(&head);
                out.extend(md.feed(&tail));
                out.extend(md.finish());

                assert_eq!(
                    join(out.clone()),
                    whole_bytes,
                    "{name}：在 {at} 处切开后字节不一致"
                );
                assert_eq!(
                    styled_chars(&out),
                    whole_styles,
                    "{name}：在 {at} 处切开后样式不一致"
                );
            }
        }
    }

    #[test]
    fn dash_runs_hold_until_the_line_ends_and_otherwise_replay_literally() {
        let mut md = MdStream::new();
        assert!(
            md.feed("-------").is_empty(),
            "连续 - 段要等行末才能确认分隔线"
        );
        assert_eq!(join(md.feed("\n")), "────────\n");

        // 段里出现非 `-` 字符：立刻按字面回放，不留到行末
        let mut md = MdStream::new();
        assert_eq!(join(md.feed("--x")), "--x");
    }

    #[test]
    fn closing_fence_without_a_trailing_newline_still_closes() {
        // 收尾时碰到闭合行（无换行）：按已闭合处理，不回吐标记
        assert_eq!(
            render("```\na\n```"),
            vec![seg("a", Style::Dim), seg("\n", Style::Plain)]
        );
        assert_eq!(render("```"), Vec::new(), "只有起始标记行：不输出");
    }
}
