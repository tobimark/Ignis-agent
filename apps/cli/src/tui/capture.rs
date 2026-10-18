//! 捕获落点与净化：渲染器写进共享缓冲的字节 → 只能「显示、不能驱动终端」的文本。
//!
//! 反目标（设计 §2.2）：模型输出绝不能驱动终端控制。这条防线只有一个入口——渲染器把字节写进
//! [`CaptureSink`]，滚动区摄入时经 [`sanitize`] 净化，只放行渲染器自己的 SGR 白名单。

use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};

/// ESC（0x1B）：所有转义序列的引子。
const ESC: u8 = 0x1b;

/// BEL（0x07）：OSC 的一种收尾标记。
const BEL: u8 = 0x07;

/// 渲染器自己的 SGR 白名单：`render::palette` 的两档 dim（`2m`/`90m`）加上 bold/error/accent
/// 与 reset。**只有这六个**进滚动区：其余 ESC/CSI/OSC 一律是「模型想驱动终端」的信号。
const ALLOWED_SGR: [&[u8]; 6] = [
    b"\x1b[0m",
    b"\x1b[1m",
    b"\x1b[2m",
    b"\x1b[31m",
    b"\x1b[36m",
    b"\x1b[90m",
];

/// 渲染器的输出落点：把字节追加进共享缓冲；`flush` 是 no-op（渲染器每个事件自己 flush）。
pub struct CaptureSink(Arc<Mutex<Vec<u8>>>);

impl CaptureSink {
    /// 接一根共享缓冲（渲染器与滚动区各持一份 `Arc`）。
    pub fn new(buffer: Arc<Mutex<Vec<u8>>>) -> Self {
        Self(buffer)
    }

    /// 建一对：`(sink, buffer)`。
    pub fn pair() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        (Self::new(Arc::clone(&buffer)), buffer)
    }
}

impl Write for CaptureSink {
    /// 追加（永远吃下全部字节：渲染器只关心 `write_all` 的成败）。
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        lock(&self.0).extend_from_slice(buf);
        Ok(buf.len())
    }

    /// no-op：渲染器每个事件自己 flush，捕获侧没有需要排空的缓冲。
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 排空共享缓冲（`std::mem::take`）；锁中毒取回内部值，不 panic。
pub fn drain(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    std::mem::take(&mut *lock(buffer))
}

/// 捕获字节 → 可进滚动区的文本。
///
/// 只放行渲染器自己的 SGR 白名单、`\n` 与 `\t`；其余 ESC（连同它后面的 CSI/OSC 序列）与 C0/DEL
/// 控制字节一律丢掉、不留残渣。非法 UTF-8 按替换字符顶上（防御性，渲染器本体只写合法 UTF-8）。
pub fn sanitize(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == ESC {
            // 白名单命中的整段原样拷贝；其余整条丢掉（连同它的参数，不留 `[2J` 这类残渣）。
            match ALLOWED_SGR
                .iter()
                .find(|sequence| bytes[index..].starts_with(sequence))
            {
                Some(sequence) => {
                    out.extend_from_slice(sequence);
                    index += sequence.len();
                }
                None => index = skip_escape(bytes, index),
            }
            continue;
        }
        match byte {
            // 仅有的两个例外：换行与制表符是滚动区的排版字符。
            b'\n' | b'\t' => out.push(byte),
            // 其余 C0 与 DEL：丢掉。
            byte if is_control(byte) => {}
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 丢掉 `bytes[start..]` 开头的一条转义序列（`start` 是 ESC 的下标），返回它之后的字节下标。
///
/// - CSI（`\x1b[`）：吃到终结字节（0x40..=0x7E）为止；破序列只吃掉引子；
/// - OSC（`\x1b]`）：吃到 BEL 或 ST（`\x1b\`）为止；没有收尾就吃到末尾（宁可丢干净）；
/// - 其余 ESE（`\x1b` + 中间字节 + 终结字节）：连同终结字节一起丢；
/// - 裸 `\x1b`（后面没有字节或跟着控制字节）：只丢 ESC 本身。
fn skip_escape(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    match bytes.get(index) {
        Some(b'[') => {
            index += 1;
            while let Some(&byte) = bytes.get(index) {
                if (0x40..=0x7e).contains(&byte) {
                    return index + 1; // 终结字节：整条 CSI 到此为止
                }
                if !(0x20..=0x3f).contains(&byte) {
                    return index; // 破序列：只吃掉 `\x1b[`
                }
                index += 1;
            }
            index // 没有终结字节：吃到末尾
        }
        Some(b']') => {
            index += 1;
            while let Some(&byte) = bytes.get(index) {
                if byte == BEL {
                    return index + 1;
                }
                if byte == ESC && bytes.get(index + 1) == Some(&b'\\') {
                    return index + 2; // ST：`\x1b\`
                }
                index += 1;
            }
            index
        }
        Some(&byte) if (0x20..=0x7e).contains(&byte) => {
            index += 1;
            while bytes
                .get(index)
                .is_some_and(|&b| (0x20..=0x2f).contains(&b))
            {
                index += 1; // 中间字节可以有好几个（`\x1b(0` 之类）
            }
            if bytes
                .get(index)
                .is_some_and(|&b| (0x30..=0x7e).contains(&b))
            {
                index + 1
            } else {
                index
            }
        }
        _ => index, // 裸 ESC：只丢 ESC 本身
    }
}

/// 控制字节：C0（0x00..=0x1F）与 DEL（0x7F）。0x80 以上是 UTF-8 序列的一部分，不算控制字节。
fn is_control(byte: u8) -> bool {
    byte < 0x20 || byte == 0x7f
}

/// 取共享缓冲的锁；锁中毒（持锁线程 panic 过）取回内部值，不 panic。
fn lock(buffer: &Arc<Mutex<Vec<u8>>>) -> MutexGuard<'_, Vec<u8>> {
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::{CaptureSink, drain, sanitize};

    /// 净化只放行渲染器自己的六个 SGR 串与 `\n`/`\t`，其余 ESC/CSI/OSC 与控制字节一律丢掉。
    #[test]
    fn sanitize_keeps_the_renderer_palette_and_drops_every_other_escape() {
        // 六个白名单 SGR：原样保留（`render::palette` 的四档 + reset/加粗/暗色）。
        for sgr in [
            "\u{1b}[0m",
            "\u{1b}[1m",
            "\u{1b}[2m",
            "\u{1b}[31m",
            "\u{1b}[36m",
            "\u{1b}[90m",
        ] {
            assert_eq!(sanitize(sgr.as_bytes()), sgr, "{sgr:?} 必须原样保留");
        }
        assert_eq!(
            sanitize("\u{1b}[2m粗\u{1b}[0m".as_bytes()),
            "\u{1b}[2m粗\u{1b}[0m",
            "该保留的一整段逐字保留（UTF-8 内容也不动）"
        );

        // 其余 ESC：连同它后面的 CSI/OSC 序列整条丢掉，不留 `[2J` 这类残渣。
        for dropped in [
            "\u{1b}[2J",                  // 清屏
            "\u{1b}[?1049l",              // 退出备用屏
            "\u{1b}[1;5A",                // 光标定位/未绑定 CSI
            "\u{1b}]0;title\u{7}",        // OSC 改标题（BEL 收尾）
            "\u{1b}]8;;http://x\u{1b}\\", // OSC 8 超链接（ST 收尾）
            "\u{1b}",                     // 裸 ESC
            "\u{1b}[",                    // 半条 CSI
            "\u{1b}(B",                   // 其余 ESE（字符集切换）
        ] {
            assert_eq!(
                sanitize(dropped.as_bytes()),
                "",
                "{dropped:?} 必须整条丢掉、不留残渣"
            );
        }

        // 混在文本里的序列：只丢序列本身，文本一字不动。
        assert_eq!(sanitize(b"a\x1b[2Jb"), "ab");
        assert_eq!(
            sanitize("\u{1b}[2mbe\u{1b}[2Jta\u{1b}[0m".as_bytes()),
            "\u{1b}[2mbeta\u{1b}[0m"
        );

        // C0 与 DEL 丢弃；`\n` 与 `\t` 是仅有的两个例外。
        assert_eq!(sanitize(b"a\x08b\x00c\x7fd"), "abcd");
        assert_eq!(sanitize(b"line1\nline2\ttab"), "line1\nline2\ttab");
        assert_eq!(
            sanitize(b"a\rb"),
            "ab",
            "回车不在白名单里（渲染器只写 \\n）"
        );

        // 普通 UTF-8 文本逐字保留；非法 UTF-8 用替换字符顶上（防御性）。
        assert_eq!(sanitize("你好，world".as_bytes()), "你好，world");
        assert_eq!(sanitize(b"a\xffb"), "a\u{fffd}b");
        assert_eq!(sanitize(b""), "", "空输入给空串");
    }

    /// 不变式：净化之后的文本里除 `\n`/`\t` 外没有控制字节，且每个 `ESC` 都恰好开启一条白名单
    /// SGR——「模型输出不能驱动终端」这条防线最终就落在它上面。
    #[test]
    fn sanitize_never_lets_a_control_byte_through() {
        let nasty = [
            "\u{1b}[2J\u{1b}[?1049h\u{1b}]0;x\u{7}ok\n\t\u{1b}[2m粗\u{1b}[0m",
            "a\u{1b}\u{8}b\u{7f}c\u{0}d",
            "\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\",
            "\u{1b}[38;5;9mx\u{1b}[?25l\u{1b}[1;1H",
        ];
        for input in nasty {
            let clean = sanitize(input.as_bytes());
            for byte in clean.bytes() {
                // `\n`/`\t` 是排版字符；`ESC` 只准出现在下一条断言认得的白名单 SGR 里。
                assert!(
                    byte >= 0x20 || byte == b'\n' || byte == b'\t' || byte == 0x1b,
                    "{input:?} → {clean:?} 里漏出了控制字节 {byte:#04x}"
                );
            }
            // 每个 ESC 起头都必须是白名单之一（净化后的 ESC 只能来自白名单拷贝）。
            let mut rest = clean.as_str();
            while let Some(pos) = rest.find('\u{1b}') {
                assert!(
                    super::ALLOWED_SGR
                        .iter()
                        .any(|sequence| rest.as_bytes()[pos..].starts_with(sequence)),
                    "{input:?} → {clean:?} 里漏出了非白名单 ESC"
                );
                rest = &rest[pos + 1..];
            }
        }
    }

    /// 捕获落点是「追加到共享缓冲」；`drain` 取回并清空（每 tick 排空一次）。
    #[test]
    fn capture_sink_appends_and_drain_empties() {
        let (mut sink, buffer) = CaptureSink::pair();
        sink.write_all(b"first").expect("写共享缓冲不会失败");
        sink.write_all(b"second").expect("写共享缓冲不会失败");
        sink.flush().expect("flush 是 no-op，不失败");

        assert_eq!(drain(&buffer), b"firstsecond".to_vec(), "两段都取回");
        assert!(drain(&buffer).is_empty(), "排空之后再排空得空");

        // 排空之后还能继续写：下一 tick 的内容照常进缓冲。
        sink.write_all(b"third").expect("排空不影响后续写入");
        assert_eq!(drain(&buffer), b"third".to_vec());

        // `new` 显式接一根共享缓冲（渲染器与滚动区各持一份 `Arc`）。
        let shared = Arc::new(Mutex::new(Vec::new()));
        let mut sink = CaptureSink::new(Arc::clone(&shared));
        sink.write_all(b"x").expect("写共享缓冲不会失败");
        assert_eq!(drain(&shared), b"x".to_vec());
    }

    /// 锁中毒（持锁的线程 panic 过）也取回内部值：生产路径只许做减法，不许 panic。
    #[test]
    fn drain_survives_a_poisoned_lock() {
        let (mut sink, buffer) = CaptureSink::pair();
        sink.write_all(b"kept").expect("写共享缓冲不会失败");

        // 另起一线程持锁 panic：这把锁从此中毒。
        let poisoning = Arc::clone(&buffer);
        let joined = std::thread::spawn(move || {
            let _guard = poisoning.lock().expect("第一次加锁不该失败");
            panic!("故意把锁弄中毒（用例预期，不是失败）");
        })
        .join();
        assert!(joined.is_err(), "线程按计划 panic 了");
        assert!(buffer.is_poisoned(), "锁确实中毒");

        assert_eq!(drain(&buffer), b"kept".to_vec(), "中毒也要取回内部值");
        sink.write_all(b"more")
            .expect("中毒的锁照样能追加（取回内部值）");
        assert_eq!(drain(&buffer), b"more".to_vec());
    }
}
