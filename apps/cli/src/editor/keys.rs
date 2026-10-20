//! 按键解码器：把 raw mode 读到的原始字节流解成 [`Key`]。
//!
//! 终端交付的是「字节」，编辑器要的是「按键」：这个模块只做这层翻译，不做 I/O、不持有编辑状态
//! （读取层把 [`Decoder::feed`] 的结果交给状态机）。三条刻意钉死的语义：
//!
//! - **跨读取缓存**：一次 `read` 可能停在半个转义序列或半个多字节 UTF-8 的中间，末尾那段
//!   「还可能是已知序列前缀」的字节留在 `pending` 里，下次 `feed` 拼上再解；攒过 [`MAX_PENDING`]
//!   字节还解不出来就认定是垃圾串（比如永远等不到终结字节的 CSI），整体丢弃并报一个 [`Key::Other`]，
//!   免得无限缓存。
//! - **粘贴区间**：`\x1b[200~` 之后不再解释转义序列，只在字节流里找 `\x1b[201~`；区间内的 Tab 按
//!   字面插入（`Char('\t')`，制表符缩进的代码粘进来不能丢缩进），其余控制字节（C0 含裸 `ESC`、以及
//!   DEL 0x7F）丢弃、非控制字节按字面插入、`\r` 与 `\n` 合成一个 `Newline`——长粘贴是一段任务文本，
//!   绝不能逐行提交（设计 §6.4、Review Focus 2）。
//! - **宽松解码**：非法 UTF-8 用替换字符顶上并置位，读取层用 [`Decoder::take_invalid`] 取走告警
//!   （取走即清零，所以一次读取最多打印一次）。
//!
//! `Enter`（0x0D）提交，`Ctrl+J`（0x0A）与 `Alt+Enter`（`\x1b\r`）插换行。raw mode 下 `ISIG` 仍
//! 开着（见 [`super::raw`]），所以 Ctrl+C 通常先变成 SIGINT；[`Key::CtrlC`] 是「终端没开 ISIG」时
//! 的兜底，语义由状态机决定。

/// 未决字节的上限：超过就认定是解不出来的垃圾串。
const MAX_PENDING: usize = 16;

/// ASCII 的 `ESC`（0x1B）：转义序列与粘贴结束标记都从它开始。
const ESC: u8 = 0x1b;

/// bracketed paste 的结束标记：粘贴区间里只在字节流里找它。
const PASTE_END: &[u8] = b"\x1b[201~";

/// 非法 UTF-8 字节序列顶上的替换字符。
const REPLACEMENT: char = '\u{fffd}';

/// 一个按键。`Copy + Eq + Debug` 让状态机与测试都好写。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    /// 一个字符（按 UTF-8 解出来的一个字符；粘贴区间里的 Tab 也是字面字符 `\t`）。
    Char(char),
    /// 回车（0x0D）：提交当前输入。
    Enter,
    /// 换行（0x0A / Alt+Enter / 粘贴区间里的 CR、LF）：插入换行，不提交。
    Newline,
    /// ↑。
    Up,
    /// ↓。
    Down,
    /// ←。
    Left,
    /// →。
    Right,
    /// Home 或 Ctrl+A。
    Home,
    /// End 或 Ctrl+E。
    End,
    /// Backspace（0x7F / 0x08）。
    Backspace,
    /// Delete（`\x1b[3~`）。
    Delete,
    /// Ctrl+D（0x04）。
    CtrlD,
    /// Ctrl+U（0x15）。
    CtrlU,
    /// Ctrl+P（0x10）：TUI 打开命令面板。
    CtrlP,
    /// Ctrl+C（0x03）。
    CtrlC,
    /// Tab（0x09；粘贴区间里不产生它，那里按字面出 `Char('\t')`）。
    Tab,
    /// Esc：**解码器不产生它**——裸 `ESC` 留在 `pending` 里等半条转义序列，由
    /// [`Decoder::take_lone_escape`] 在 tick 上交付（TUI 的 Esc 消歧），那里合成这一个按键。
    /// 经典编辑器走不到它。
    Esc,
    /// bracketed paste 开始（`\x1b[200~`）。
    PasteStart,
    /// bracketed paste 结束（`\x1b[201~`）。
    PasteEnd,
    /// PageUp（`\x1b[5~`）：**经典编辑器不绑定它**——状态机按 [`Key::Other`] 的既有处理
    /// （`leave_browse` + `Idle`），字节级行为与新增这个变体之前逐字一致；TUI 的编辑器态用它
    /// 翻一屏滚动区（落点只有应用层认识）。
    PageUp,
    /// PageDown（`\x1b[6~`）：与 [`Key::PageUp`] 同一条口径。
    PageDown,
    /// 没有绑定的按键（未识别的转义序列、没有映射的控制字节）：状态机忽略。
    Other,
}

/// 字节 → [`Key`] 的增量解码器：跨 `feed` 保留解不完的尾巴。
#[derive(Debug, Default)]
pub struct Decoder {
    /// 上次 `feed` 没解出来的尾巴（半个转义序列 / 半个多字节 UTF-8）。
    pending: Vec<u8>,
    /// 是否在 bracketed paste 区间里。
    pasting: bool,
    /// 上次 `feed` 里见过非法 UTF-8；由 [`Decoder::take_invalid`] 取走。
    lossy: bool,
}

impl Decoder {
    /// 新的解码器：缓存为空、不在粘贴区间、没有未读告警。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段刚读到的字节，返回这次能解出来的按键（可能为空：字节只是一条长序列的开头）。
    ///
    /// 解不出来的尾巴留在 `pending` 里等下次；`pending` 攒过 [`MAX_PENDING`] 就整体丢弃并报一个
    /// [`Key::Other`]。
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let mut keys = Vec::new();
        let mut consumed = 0usize;
        loop {
            match self.step(consumed) {
                Step::Key(key, len) => {
                    consumed += len;
                    // 粘贴区间的进出由区间的标记自己驱动。
                    match key {
                        Key::PasteStart => self.pasting = true,
                        Key::PasteEnd => self.pasting = false,
                        _ => {}
                    }
                    keys.push(key);
                }
                Step::Drop(len) => consumed += len,
                Step::Stuck => break,
            }
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        if self.pending.len() > MAX_PENDING {
            self.pending.clear();
            keys.push(Key::Other);
        }
        keys
    }

    /// 取走「读到过非法 UTF-8」的告警位；取走即清零（读取层据此一次读取只打印一次告警）。
    pub fn take_invalid(&mut self) -> bool {
        std::mem::take(&mut self.lossy)
    }

    /// 当前是否在 bracketed paste 区间里。
    pub fn pasting(&self) -> bool {
        self.pasting
    }

    /// 吃掉「一个孤零零的 ESC」并报 `true`（其余情况返回 `false` 且缓存原样不动）。
    ///
    /// TUI 每 tick 调它，把用户按的 Esc 从「可能是半条转义序列」里解救出来（Esc 消歧窗口 = 一个
    /// tick）：裸 `ESC` 本来会一直等在 `pending` 里凑后续字节，这里把它当按键交付。两种情形**不**
    /// 动缓存：
    /// - 半条 CSI（`\x1b[`）：可能是方向键的前半截，得等终结字节；
    /// - 粘贴区间里的 `\x1b`：可能是结束标记 `\x1b[201~` 的开头，吃掉就再也退不出粘贴了。
    ///
    /// 经典编辑器不调它，既有解码行为零变化。
    pub fn take_lone_escape(&mut self) -> bool {
        if self.pasting || self.pending.len() != 1 || self.pending[0] != ESC {
            return false;
        }
        self.pending.clear();
        true
    }

    /// 从 `pending[from..]` 尝试解出一个按键；解不出来说明字节还不够，等下次 `feed`。
    fn step(&mut self, from: usize) -> Step {
        if from >= self.pending.len() {
            return Step::Stuck;
        }
        if self.pasting {
            self.paste_step(from)
        } else {
            self.normal_step(from)
        }
    }

    /// 非粘贴区间：认转义序列 → 查控制字节表 → 解一个 UTF-8 字符。
    fn normal_step(&mut self, from: usize) -> Step {
        let Some(&first) = self.pending.get(from) else {
            return Step::Stuck;
        };
        // 1) `ESC` 打头一律交给转义序列分支（认不出也按「未绑定」把字节吃掉）。
        if first == ESC {
            return escape_step(&self.pending[from..]);
        }
        // 2) 控制字节表：Enter 与 Newline 是两件事，没绑定的控制字节一律 Other。
        if let Some(key) = control_key(first) {
            return Step::Key(key, 1);
        }
        // 3) 其余按 UTF-8 解一个字符（非法序列用替换字符顶上）。
        match utf8_step(&self.pending[from..]) {
            Utf8::Char { ch, len, lossy } => {
                self.lossy |= lossy;
                Step::Key(Key::Char(ch), len)
            }
            Utf8::Incomplete => Step::Stuck,
        }
    }

    /// 粘贴区间：不解释任何转义，只在字节流里找结束标记，其余按字面内容解。
    ///
    /// 区间内 `\r`/`\n` 合成一个 `Newline`、Tab 按字面插入（`Char('\t')`），其余控制字节
    /// （C0 含裸 `ESC`、以及 DEL 0x7F）丢弃，非法 UTF-8 用替换字符顶上。
    fn paste_step(&mut self, from: usize) -> Step {
        let rest = &self.pending[from..];
        let Some(&first) = rest.first() else {
            return Step::Stuck;
        };
        // 结束标记完整出现在最前面：区间到此为止。
        if rest.starts_with(PASTE_END) {
            return Step::Key(Key::PasteEnd, PASTE_END.len());
        }
        if first == ESC {
            // `ESC` 打头：可能还差几个字节才是结束标记，也可能只是个被丢弃的裸 `ESC`。
            return if PASTE_END.starts_with(rest) {
                Step::Stuck
            } else {
                Step::Drop(1)
            };
        }
        match first {
            // CR 与 LF 都只出 Newline；CRLF 合成一个，所以 CR 落在读取边界上时要等下一批字节。
            b'\r' => match rest.get(1) {
                Some(b'\n') => Step::Key(Key::Newline, 2),
                Some(_) => Step::Key(Key::Newline, 1),
                None => Step::Stuck,
            },
            b'\n' => Step::Key(Key::Newline, 1),
            // Tab 是唯一的例外：制表符缩进的代码粘进来必须保住缩进，所以按字面插入 `\t`。
            b'\t' => Step::Key(Key::Char('\t'), 1),
            // 其余控制字节（含裸 `ESC` 与 DEL 0x7F）在粘贴内容里只是噪音。
            byte if is_control(byte) => Step::Drop(1),
            _ => match utf8_step(rest) {
                Utf8::Char { ch, len, lossy } => {
                    self.lossy |= lossy;
                    Step::Key(Key::Char(ch), len)
                }
                // 粘贴结束在半个字符中间时，尾随的字节（或结束标记的 `ESC`）会让它变成
                // 一个「确定的坏序列」，所以这里只需等下一次 `feed`。
                Utf8::Incomplete => Step::Stuck,
            },
        }
    }
}

/// 一次解码尝试的结果。
enum Step {
    /// 解出一个按键，连同它消耗的字节数。
    Key(Key, usize),
    /// 消耗字节但不出按键（粘贴区间里被丢弃的控制字节）。
    Drop(usize),
    /// 现有字节还不够看清一个按键：留在 `pending` 里等下次 `feed`。
    Stuck,
}

/// `ESC` 打头的一段字节（非粘贴区间）：已知序列前缀匹配，认不出就按「未绑定」处理。
fn escape_step(buf: &[u8]) -> Step {
    let Some(&second) = buf.get(1) else {
        // 只有 `ESC` 一个字节：可能是任何序列的开头，等下一次 `feed`。
        return Step::Stuck;
    };
    match second {
        // Alt+Enter：readline 口径下就是「插入换行」。
        b'\r' => Step::Key(Key::Newline, 2),
        // CSI：`ESC [` 后跟参数/中间字节，遇终结字节收尾。
        b'[' => csi_step(buf),
        // SS3：`ESC O` + 终结字节。
        b'O' => ss3_step(buf),
        // `ESC` + 其它字节：两字节都消耗——Alt+字符在 readline 口径下就是「没有绑定」。
        _ => Step::Key(Key::Other, 2),
    }
}

/// CSI 序列（`ESC [` 打头）：读到终结字节（0x40..=0x7E）为止，再查解码表。
fn csi_step(buf: &[u8]) -> Step {
    let mut index = 2;
    while let Some(&byte) = buf.get(index) {
        if (0x40..=0x7e).contains(&byte) {
            return Step::Key(csi_key(&buf[2..index], byte), index + 1);
        }
        // 既不是参数/中间字节也不是终结字节：这条 CSI 是坏的，只吃掉 `ESC [`，
        // 其余的字节留给正常解码（多半是控制字节或 UTF-8 文本）。
        if !(0x20..=0x3f).contains(&byte) {
            return Step::Key(Key::Other, 2);
        }
        index += 1;
    }
    // 还没等到终结字节：整条序列留在缓存里等下次 `feed`。
    Step::Stuck
}

/// CSI 的参数（`ESC [` 与终结字节之间的部分）+ 终结字节 → 按键；没绑定的算 `Other`。
fn csi_key(params: &[u8], final_byte: u8) -> Key {
    match (params, final_byte) {
        (b"", b'A') => Key::Up,
        (b"", b'B') => Key::Down,
        (b"", b'C') => Key::Right,
        (b"", b'D') => Key::Left,
        (b"", b'H') => Key::Home,
        (b"", b'F') => Key::End,
        (b"1", b'~') => Key::Home,
        (b"3", b'~') => Key::Delete,
        (b"4", b'~') => Key::End,
        // 增量新增：`5~`/`6~` 此前按未绑定 CSI 解成 `Key::Other`，经典路径的处理逐字不变
        // （见 `editor::state` 的同一个 match 臂）；其余参数形状（`5;5~`、`15~` 之类）照旧 `Other`。
        (b"5", b'~') => Key::PageUp,
        (b"6", b'~') => Key::PageDown,
        (b"200", b'~') => Key::PasteStart,
        (b"201", b'~') => Key::PasteEnd,
        _ => Key::Other,
    }
}

/// SS3 序列（`ESC O` 打头）：必须再跟一个终结字节。
fn ss3_step(buf: &[u8]) -> Step {
    let Some(&third) = buf.get(2) else {
        // `ESC O` 还不完整。
        return Step::Stuck;
    };
    // 第三字节不是可打印的终结字节：这条 SS3 是残缺的，只吃掉 `ESC O`，
    // 后面的字节（多半是 UTF-8 文本）留给正常解码，别把用户的字吃了。
    if !(0x20..=0x7e).contains(&third) {
        return Step::Key(Key::Other, 2);
    }
    let key = match third {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        _ => Key::Other,
    };
    Step::Key(key, 3)
}

/// 单个控制字节 → 按键（解码表）；非控制字节返回 `None`，交给 UTF-8 分支。
fn control_key(byte: u8) -> Option<Key> {
    match byte {
        b'\r' => Some(Key::Enter),
        b'\n' => Some(Key::Newline),
        0x01 => Some(Key::Home),
        0x05 => Some(Key::End),
        0x04 => Some(Key::CtrlD),
        0x15 => Some(Key::CtrlU),
        0x10 => Some(Key::CtrlP),
        0x03 => Some(Key::CtrlC),
        0x09 => Some(Key::Tab),
        b'\x7f' | 0x08 => Some(Key::Backspace),
        _ if is_control(byte) => Some(Key::Other),
        _ => None,
    }
}

/// 控制字节：C0（0x00..=0x1F）与 DEL（0x7F）。0x80 以上是 UTF-8 序列的一部分，不算控制字节。
fn is_control(byte: u8) -> bool {
    byte < 0x20 || byte == 0x7f
}

/// 从字节串头部解一个 UTF-8 字符的结果。
enum Utf8 {
    /// 解出一个字符（`lossy` 为真表示这是非法序列顶上的替换字符）。
    Char { ch: char, len: usize, lossy: bool },
    /// 只是（可能是）一个字符的半个前缀，等下一次 `feed` 拼上。
    Incomplete,
}

/// 从 `bytes` 头部解出下一个 UTF-8 字符；非法序列按 `from_utf8_lossy` 口径出一个替换字符。
///
/// 只看前 4 个字节就够（UTF-8 字符最长 4 字节）：既避免每解一个字符就把整段校验一遍
/// （长粘贴会退化成 O(n²)），也让 `Incomplete` 一定意味着「尾巴就在缓冲区末尾」。
fn utf8_step(bytes: &[u8]) -> Utf8 {
    let head = &bytes[..bytes.len().min(4)];
    match std::str::from_utf8(head) {
        Ok(text) => first_char(text),
        Err(error) => {
            let valid = error.valid_up_to();
            if valid > 0 {
                // `valid_up_to` 之前一定是合法 UTF-8，取其中的第一个字符。
                return match std::str::from_utf8(&head[..valid]) {
                    Ok(text) => first_char(text),
                    // 上面那句按契约必成功；真失败了也只会「留待下次」，不 panic。
                    Err(_) => Utf8::Incomplete,
                };
            }
            match error.error_len() {
                // 一个确定的坏序列：吃掉它，出一个替换字符。
                Some(len) => Utf8::Char {
                    ch: REPLACEMENT,
                    len,
                    lossy: true,
                },
                // 只是半个（可能合法的）前缀：等下次 `feed`。
                None => Utf8::Incomplete,
            }
        }
    }
}

/// 合法 UTF-8 串里的第一个字符；空串当作「还没解出来」。
fn first_char(text: &str) -> Utf8 {
    match text.chars().next() {
        Some(ch) => Utf8::Char {
            ch,
            len: ch.len_utf8(),
            lossy: false,
        },
        None => Utf8::Incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoder, Key};

    /// 一次喂完整串的简化入口：多数用例只关心「这串字节解成什么按键」。
    fn decode(bytes: &[u8]) -> Vec<Key> {
        Decoder::new().feed(bytes)
    }

    /// `Key` 必须是 `Copy + Eq + Debug`（状态机按值传递、测试直接比较）。
    #[test]
    fn key_is_copy_eq_and_debug() {
        fn round_trip<T: Copy + Eq + std::fmt::Debug>(value: T) -> T {
            value
        }
        let key = Key::Char('你');
        assert_eq!(round_trip(key), key, "Copy/Eq/Debug 是接口契约：{key:?}");
    }

    #[test]
    fn enter_and_newline_are_distinct() {
        // raw mode 关掉了 ICRNL：0x0D 是 Enter（提交），0x0A 是 Newline（插入换行）。
        assert_eq!(decode(b"\r"), vec![Key::Enter], "0x0D 是 Enter");
        assert_eq!(decode(b"\n"), vec![Key::Newline], "0x0A 是 Newline");
        assert_eq!(
            decode(b"\r\n"),
            vec![Key::Enter, Key::Newline],
            "两个连在一起也各归各的"
        );
    }

    #[test]
    fn alt_enter_inserts_a_newline() {
        assert_eq!(
            decode(b"\x1b\r"),
            vec![Key::Newline],
            "Alt+Enter 是插入换行"
        );
    }

    #[test]
    fn csi_and_ss3_arrows_decode() {
        assert_eq!(decode(b"\x1b[A"), vec![Key::Up], "CSI 上");
        assert_eq!(decode(b"\x1bOA"), vec![Key::Up], "SS3 上");
        assert_eq!(decode(b"\x1b[B"), vec![Key::Down], "CSI 下");
        assert_eq!(decode(b"\x1bOB"), vec![Key::Down], "SS3 下");
        assert_eq!(decode(b"\x1b[C"), vec![Key::Right], "CSI 右");
        assert_eq!(decode(b"\x1bOC"), vec![Key::Right], "SS3 右");
        assert_eq!(decode(b"\x1b[D"), vec![Key::Left], "CSI 左");
        assert_eq!(decode(b"\x1bOD"), vec![Key::Left], "SS3 左");
        // 没绑定的 CSI（Ctrl+Up 之类）是 Other，而且整条都消耗掉。
        assert_eq!(
            decode(b"\x1b[1;5A"),
            vec![Key::Other],
            "未识别的 CSI 是 Other"
        );
    }

    #[test]
    fn home_end_and_delete_from_csi_and_ss3() {
        assert_eq!(decode(b"\x1b[H"), vec![Key::Home], "CSI H");
        assert_eq!(decode(b"\x1bOH"), vec![Key::Home], "SS3 H");
        assert_eq!(decode(b"\x1b[1~"), vec![Key::Home], "CSI 1~");
        assert_eq!(decode(b"\x1b[F"), vec![Key::End], "CSI F");
        assert_eq!(decode(b"\x1bOF"), vec![Key::End], "SS3 F");
        assert_eq!(decode(b"\x1b[4~"), vec![Key::End], "CSI 4~");
        assert_eq!(decode(b"\x1b[3~"), vec![Key::Delete], "CSI 3~");
    }

    /// F2（终审修复波）：PgUp/PgDn 是**增量新增**——`\x1b[5~`/`\x1b[6~` 此前解成 [`Key::Other`]，
    /// 现在各成一个变体（`state.rs` 里与 `Other` 逐字同一条处理，经典行为不变）；其余参数形状
    /// （带修饰的 `5;5~`、`15~` 之类）照旧是 `Other`。
    #[test]
    fn page_up_and_page_down_decode_from_csi_tilde() {
        assert_eq!(decode(b"\x1b[5~"), vec![Key::PageUp], "CSI 5~");
        assert_eq!(decode(b"\x1b[6~"), vec![Key::PageDown], "CSI 6~");
        assert_eq!(
            decode(b"\x1b[5;5~"),
            vec![Key::Other],
            "带修饰的 5~ 仍然未绑定（只做增量新增）"
        );
        assert_eq!(decode(b"\x1b[15~"), vec![Key::Other], "F5 之类照旧未绑定");

        // 跨读取缓存照旧：`\x1b[` + `5~` 拼上才出按键。
        let mut decoder = Decoder::new();
        assert!(decoder.feed(b"\x1b[5").is_empty(), "参数只到一半，先留着");
        assert_eq!(decoder.feed(b"~"), vec![Key::PageUp], "拼上后解成 PageUp");
    }

    #[test]
    fn control_keys_map_to_their_bindings() {
        assert_eq!(decode(b"\x01"), vec![Key::Home], "Ctrl+A = 行首");
        assert_eq!(decode(b"\x05"), vec![Key::End], "Ctrl+E = 行尾");
        assert_eq!(decode(b"\x04"), vec![Key::CtrlD], "Ctrl+D");
        assert_eq!(decode(b"\x15"), vec![Key::CtrlU], "Ctrl+U");
        assert_eq!(decode(b"\x10"), vec![Key::CtrlP], "Ctrl+P");
        assert_eq!(decode(b"\x03"), vec![Key::CtrlC], "Ctrl+C");
        assert_eq!(decode(b"\x09"), vec![Key::Tab], "Tab");
        assert_eq!(decode(b"\x7f"), vec![Key::Backspace], "0x7F 是 Backspace");
        assert_eq!(decode(b"\x08"), vec![Key::Backspace], "0x08 是 Backspace");
        // 其余控制字节没有绑定：readline 的 Ctrl+B/Ctrl+K 之类在这里都是 Other。
        assert_eq!(decode(b"\x02"), vec![Key::Other], "Ctrl+B 没有绑定");
        assert_eq!(decode(b"\x0b"), vec![Key::Other], "Ctrl+K 没有绑定");
        assert_eq!(decode(b"\x00"), vec![Key::Other], "NUL 没有绑定");
    }

    #[test]
    fn escape_sequences_split_across_feeds_stay_intact() {
        let mut decoder = Decoder::new();
        // 裸 `\x1b` 只是序列的开头，不能当按键吐出去。
        assert!(decoder.feed(b"\x1b").is_empty(), "半条序列必须留在缓存里");
        assert_eq!(decoder.feed(b"[A"), vec![Key::Up], "拼上后解成 Up");

        // 切在参数区之前（`\x1b[` + `A`）也一样。
        assert!(decoder.feed(b"\x1b[").is_empty(), "只有引子的 CSI 也得留着");
        assert_eq!(decoder.feed(b"A"), vec![Key::Up], "终结字节一到就是 Up");

        // `Alt+Enter` 被切在两个读取之间：`\x1b` 先到，仍然是插入换行而不是「未绑定」。
        assert!(decoder.feed(b"\x1b").is_empty(), "裸 ESC 先留着");
        assert_eq!(
            decoder.feed(b"\r"),
            vec![Key::Newline],
            "`\\r` 到齐后合成一个 Newline"
        );

        // `\x1b[20` + `0~`：跨读取拼成 PasteStart。
        assert!(decoder.feed(b"\x1b[20").is_empty(), "CSI 只到一半也得留着");
        assert_eq!(
            decoder.feed(b"0~"),
            vec![Key::PasteStart],
            "拼上后解成 PasteStart"
        );
        assert!(decoder.pasting(), "PasteStart 之后解码器进入粘贴区间");
    }

    #[test]
    fn multibyte_chars_split_across_feeds_stay_intact() {
        let mut decoder = Decoder::new();
        let bytes = "你".as_bytes();
        assert_eq!(bytes.len(), 3, "夹具假设：`你` 是 3 字节 UTF-8");
        assert!(decoder.feed(&bytes[..1]).is_empty(), "第一个字节不构成字符");
        assert!(
            decoder.feed(&bytes[1..2]).is_empty(),
            "前两个字节也不构成字符"
        );
        assert_eq!(
            decoder.feed(&bytes[2..]),
            vec![Key::Char('你')],
            "只在最后一个字节到齐后出字符"
        );
        assert_eq!(
            decoder.feed("好".as_bytes()),
            vec![Key::Char('好')],
            "后面紧跟的字符照常解"
        );

        // 4 字节字符（U+1F600）也要能劈成 `[F0 9F]` + `[98 80]`：解码窗口只有 4 字节，
        // 这种字符正好是窗口的上限。
        let mut decoder = Decoder::new();
        let emoji = "\u{1f600}".as_bytes();
        assert_eq!(
            emoji,
            &[0xf0, 0x9f, 0x98, 0x80][..],
            "夹具假设：U+1F600 是 4 字节 UTF-8"
        );
        assert!(decoder.feed(&emoji[..2]).is_empty(), "两个字节还不构成字符");
        assert_eq!(
            decoder.feed(&emoji[2..]),
            vec![Key::Char('\u{1f600}')],
            "后两个字节补齐 4 字节字符"
        );
        assert_eq!(
            decoder.feed(b"a"),
            vec![Key::Char('a')],
            "4 字节字符之后的字节照常解"
        );
    }

    #[test]
    fn alt_character_is_dropped_as_other() {
        // Alt+字符在 readline 口径下就是「没有绑定」：两个字节都消耗。
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(b"\x1bx"), vec![Key::Other], "Alt+x 没有绑定");
        assert_eq!(
            decoder.feed(b"y"),
            vec![Key::Char('y')],
            "`x` 已被吃掉，不会残留成字符"
        );
    }

    #[test]
    fn paste_mode_is_verbatim_and_crlf_collapses() {
        let mut decoder = Decoder::new();
        // 区间内 `\x1b[2m` 不是「设置颜色」：裸 ESC 丢弃，`[`、`2`、`m` 是三个字面字符。
        let keys = decoder.feed("\x1b[200~\x1b[2m\r\n你好\x1b[201~".as_bytes());
        assert_eq!(
            keys,
            vec![
                Key::PasteStart,
                Key::Char('['),
                Key::Char('2'),
                Key::Char('m'),
                Key::Newline,
                Key::Char('你'),
                Key::Char('好'),
                Key::PasteEnd,
            ],
            "粘贴区间内只有两个标记是按键，其余按字面走、CRLF 合成一个 Newline"
        );
        assert!(!decoder.pasting(), "PasteEnd 之后回到正常解码");
        assert_eq!(
            decoder.feed(b"\x1b[A"),
            vec![Key::Up],
            "区间结束后转义序列重新生效"
        );
        assert!(
            !decoder.take_invalid(),
            "粘贴区间内的字节都是合法 UTF-8，不该有告警"
        );

        // 同一次读取里跨过结束标记：`内容 + \x1b[201~ + \x1b[A` 必须让 `pasting` 在 feed 中途翻转，
        // 否则半条转义序列会被当成粘贴内容里的字面字符。
        assert_eq!(
            decoder.feed(b"\x1b[200~hi\x1b[201~\x1b[A"),
            vec![
                Key::PasteStart,
                Key::Char('h'),
                Key::Char('i'),
                Key::PasteEnd,
                Key::Up,
            ],
            "结束标记之后的 `\\x1b[A` 在同一个 feed 里就要按 Up 解"
        );
        assert!(!decoder.pasting(), "区间结束后回到正常解码");
    }

    /// F1（控制者裁定）：粘贴区间里 Tab 按字面插入（`Char('\t')`），其余 C0 控制字节与 DEL 仍然丢弃。
    #[test]
    fn paste_region_inserts_tab_literally_and_drops_other_controls() {
        let mut decoder = Decoder::new();
        // 制表符缩进的代码：Tab 必须活下来，否则粘进来的缩进会全部消失。
        let keys = decoder.feed("\x1b[200~if x {\n\tlet y = 1;\n}\x1b[201~".as_bytes());
        assert_eq!(
            keys,
            vec![
                Key::PasteStart,
                Key::Char('i'),
                Key::Char('f'),
                Key::Char(' '),
                Key::Char('x'),
                Key::Char(' '),
                Key::Char('{'),
                Key::Newline,
                Key::Char('\t'),
                Key::Char('l'),
                Key::Char('e'),
                Key::Char('t'),
                Key::Char(' '),
                Key::Char('y'),
                Key::Char(' '),
                Key::Char('='),
                Key::Char(' '),
                Key::Char('1'),
                Key::Char(';'),
                Key::Newline,
                Key::Char('}'),
                Key::PasteEnd,
            ],
            "粘贴里的 Tab 是字面制表符，不是被丢弃的控制字节"
        );
        assert!(!decoder.pasting(), "PasteEnd 之后回到正常解码");
        assert!(!decoder.take_invalid(), "全是合法 UTF-8，不该有告警");

        // Tab 是唯一的例外：裸 ESC 与 DEL 0x7F 照旧丢弃。
        assert_eq!(
            decoder.feed(b"\x1b[200~a\x1b\x7fb\tc\r\n\x1b[201~"),
            vec![
                Key::PasteStart,
                Key::Char('a'),
                Key::Char('b'),
                Key::Char('\t'),
                Key::Char('c'),
                Key::Newline,
                Key::PasteEnd,
            ],
            "裸 ESC 与 DEL 仍然丢弃，只有 Tab 按字面插入"
        );
        assert!(!decoder.pasting(), "第二段粘贴也正常结束");
    }

    #[test]
    fn paste_region_survives_split_feeds() {
        let mut decoder = Decoder::new();
        // 开始标记可以切在参数中间：`\x1b[200` + `~`。
        assert!(decoder.feed(b"\x1b[200").is_empty(), "参数只到一半，先留着");
        assert_eq!(
            decoder.feed(b"~x"),
            vec![Key::PasteStart, Key::Char('x')],
            "标记拼上后进入粘贴区间，后面的字节按字面插入"
        );
        assert!(decoder.pasting());

        // `\r` 落在读取边界上：下一批字节里的 `\n` 必须与它合成一个换行。
        assert!(
            decoder.feed(b"\r").is_empty(),
            "可能是 CRLF 的前半个，先留着"
        );
        assert_eq!(decoder.feed(b"\ny"), vec![Key::Newline, Key::Char('y')]);

        // 粘贴内容里的 `\x1b[2` 不是结束标记：`m` 一到就吐出三个可见字符。
        assert!(
            decoder.feed(b"\x1b[2").is_empty(),
            "可能是结束标记的前缀，先留着"
        );
        assert_eq!(
            decoder.feed(b"m"),
            vec![Key::Char('['), Key::Char('2'), Key::Char('m')],
            "裸 ESC 丢弃，其余按字面插入"
        );

        // 结束标记也可以只到一半。
        assert!(decoder.feed(b"\x1b[201").is_empty(), "结束标记只到一半");
        assert_eq!(decoder.feed(b"~"), vec![Key::PasteEnd]);
        assert!(!decoder.pasting());
    }

    #[test]
    fn invalid_utf8_becomes_replacement_and_flags_once() {
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.feed(b"a\xffb"),
            vec![Key::Char('a'), Key::Char('\u{fffd}'), Key::Char('b')],
            "非法字节用替换字符顶上，不吞掉后面的字符"
        );
        assert!(decoder.take_invalid(), "读到非法 UTF-8 必须置位");
        assert!(!decoder.take_invalid(), "take_invalid 取走即清零");
        assert_eq!(decoder.feed(b"c"), vec![Key::Char('c')]);
        assert!(!decoder.take_invalid(), "下一次合法读取不再告警");

        // 半个字符先留着（不急着报替换字符），补齐后发现是坏序列才置位。
        let mut decoder = Decoder::new();
        assert!(decoder.feed(&[0xe4, 0xbd]).is_empty(), "半个字符不算定论");
        assert!(!decoder.take_invalid(), "还没定论，不置位");
        assert_eq!(
            decoder.feed(b"x"),
            vec![Key::Char('\u{fffd}'), Key::Char('x')],
            "补上非续字节后确认是坏序列"
        );
        assert!(decoder.take_invalid(), "坏序列补齐后置位一次");
        assert!(!decoder.take_invalid(), "再取就没了");
    }

    #[test]
    fn pending_is_capped_and_reset() {
        let mut decoder = Decoder::new();
        // 一条永远等不到终结字节的 CSI（全是中间字节）：先攒着。
        assert!(decoder.feed(b"\x1b[!").is_empty(), "半条 CSI 先攒着");
        // 攒过上限就认定是垃圾串：整体丢弃并报一个 Other。
        assert_eq!(
            decoder.feed(b"!!!!!!!!!!!!!!!!!!!!"),
            vec![Key::Other],
            "超过上限的垃圾串按一个 Other 丢弃"
        );
        assert_eq!(
            decoder.feed(b"a"),
            vec![Key::Char('a')],
            "丢弃之后缓存干净，新字节照常解码"
        );

        // 上限的边界正好是 16：16 字节还留着，17 字节才丢弃。
        let mut decoder = Decoder::new();
        let sixteen = b"\x1b[!!!!!!!!!!!!!!";
        assert_eq!(sixteen.len(), 16, "夹具假设：`\\x1b[` + 14 个中间字节");
        assert!(
            decoder.feed(sixteen).is_empty(),
            "正好 16 字节还留着，不出按键"
        );
        assert_eq!(
            decoder.feed(b"~"),
            vec![Key::Other],
            "留下的 16 字节仍在：补上终结字节后整条按 Other 消耗（若已被丢弃，这里会出 `Char('~')`）"
        );

        let mut decoder = Decoder::new();
        let seventeen = b"\x1b[!!!!!!!!!!!!!!!";
        assert_eq!(seventeen.len(), 17, "夹具假设：越界 1 个字节");
        assert_eq!(
            decoder.feed(seventeen),
            vec![Key::Other],
            "17 字节越界：整体丢弃并报一个 Other"
        );
        assert_eq!(
            decoder.feed(b"a"),
            vec![Key::Char('a')],
            "越界丢弃之后缓存也干净"
        );
    }

    /// Esc 消歧：TUI 每 tick 调 [`Decoder::take_lone_escape`]，把用户按的 Esc 从「可能是半条转义
    /// 序列」里解救出来；经典编辑器不调它，既有解码行为零变化。
    #[test]
    fn take_lone_escape_only_eats_a_lone_escape() {
        // 孤零零的 ESC：吃掉并报 true，缓存随之清空。
        let mut decoder = Decoder::new();
        assert!(
            decoder.feed(b"\x1b").is_empty(),
            "裸 ESC 留在缓存里等后续字节"
        );
        assert!(decoder.take_lone_escape(), "孤零零的 ESC 被吃掉");
        assert!(!decoder.take_lone_escape(), "吃过了：第二次返回 false");
        assert_eq!(
            decoder.feed(b"[A"),
            vec![Key::Char('['), Key::Char('A')],
            "缓存已清空（R7 的已知边界：被 tick 吃掉的 ESC 带走了半条序列的引子）"
        );

        // 半条 CSI 不是孤 ESC：缓存原样不动，拼上终结字节仍解成 Up。
        let mut decoder = Decoder::new();
        assert!(decoder.feed(b"\x1b[").is_empty(), "只有引子的 CSI 先留着");
        assert!(
            !decoder.take_lone_escape(),
            "半条 CSI 不许被当成裸 ESC 吃掉"
        );
        assert_eq!(decoder.feed(b"A"), vec![Key::Up], "拼上终结字节仍是 Up");

        // 缓存为空 / 别的尾巴：一律不动缓存。
        let mut decoder = Decoder::new();
        assert!(!decoder.take_lone_escape(), "空缓存无事发生");
        assert!(decoder.feed(b"\x1b[2").is_empty(), "半条 CSI 参数先留着");
        assert!(!decoder.take_lone_escape(), "半条 CSI 参数不许吃");
        assert_eq!(
            decoder.feed(b"00~"),
            vec![Key::PasteStart],
            "缓存原样：拼上后仍是 PasteStart"
        );

        // 粘贴区间里的裸 ESC 可能是结束标记（`\x1b[201~`）的开头：不许吃。
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.feed(b"\x1b[200~x\x1b"),
            vec![Key::PasteStart, Key::Char('x')],
            "粘贴区间里的裸 ESC 是结束标记的前缀，留在缓存里"
        );
        assert!(!decoder.take_lone_escape(), "粘贴区间里不许吃 ESC");
        assert_eq!(decoder.feed(b"[201~"), vec![Key::PasteEnd]);

        // 完整序列与「半条序列」的既有口径不受影响。
        assert_eq!(decode(b"\x1b[A"), vec![Key::Up]);
        assert!(decode(b"\x1b").is_empty(), "裸 ESC 单独喂仍是等后续字节");
    }
}
