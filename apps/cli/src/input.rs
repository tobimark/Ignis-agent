//! 终端输入的宽松解码。
//!
//! 终端、输入法与剪贴板都可能送进非法 UTF-8 字节，而 std 的 `read_line` 遇到这种输入会硬
//! 失败（`stream did not contain valid UTF-8`）：在 REPL 里它会直接结束整个会话（历史与
//! 上下文一起丢），在审批里它会静默掐断后续问答。系统边界送来的字节按替换字符处理才是安全
//! 默认——与 `fs_read`、`doctor` 对文件与命令输出的口径一致（`from_utf8_lossy`）。

use std::io::{self, BufRead};

/// 读一行（含行尾 `\n`，行内容与 `BufRead::read_line` 口径一致）：返回
/// `(文本, 是否全为合法 UTF-8)`。
///
/// EOF 返回 `Ok(None)`；真正的 I/O 错误照旧上抛，由调用方决定是否致命。
pub fn read_line_lossy<R: BufRead>(reader: &mut R) -> io::Result<Option<(String, bool)>> {
    let mut bytes = Vec::new();
    if reader.read_until(b'\n', &mut bytes)? == 0 {
        return Ok(None);
    }
    let valid = std::str::from_utf8(&bytes).is_ok();
    Ok(Some((String::from_utf8_lossy(&bytes).into_owned(), valid)))
}

/// 行迭代器：非法 UTF-8 行按替换字符交出；EOF 与读取错误都结束迭代（与原先
/// `lines().map_while(Result::ok)` 的结束语义一致，但坏字节不再吞掉后续行）。
pub fn lossy_lines<R: BufRead>(reader: R) -> impl Iterator<Item = String> {
    let mut reader = reader;
    std::iter::from_fn(move || match read_line_lossy(&mut reader) {
        Ok(Some((line, _))) => Some(line),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{lossy_lines, read_line_lossy};

    /// 核心回归：非法字节不能再变成致命错误——按替换字符读出并如实标记。
    /// （修复前 REPL 走 std 的 `read_line`，一个 `\x80` 就以
    /// `stream did not contain valid UTF-8` 结束整个会话。）
    #[test]
    fn invalid_bytes_decode_lossily_and_are_flagged() {
        let mut cursor = Cursor::new(b"\x80\xe4\xbd\xa0\xe5\xa5\xbd\n".to_vec());
        let (line, valid) = read_line_lossy(&mut cursor).unwrap().unwrap();
        assert!(!valid, "含非法字节必须被标记");
        assert_eq!(line, "\u{FFFD}你好\n");
    }

    /// 合法输入逐字节不变（含行尾换行），与原 `read_line` 口径一致。
    #[test]
    fn valid_input_round_trips_byte_identically() {
        let mut cursor = Cursor::new("你好吗\n".as_bytes().to_vec());
        let (line, valid) = read_line_lossy(&mut cursor).unwrap().unwrap();
        assert!(valid);
        assert_eq!(line, "你好吗\n");
    }

    /// EOF 与空行是两个不同的结果：前者 `None`，后者是带换行的空文本。
    #[test]
    fn eof_is_none_and_blank_line_is_empty_text() {
        let mut empty = Cursor::new(Vec::new());
        assert!(read_line_lossy(&mut empty).unwrap().is_none());

        let mut blank = Cursor::new(b"\n".to_vec());
        let (line, valid) = read_line_lossy(&mut blank).unwrap().unwrap();
        assert_eq!((line.as_str(), valid), ("\n", true));
    }

    /// 迭代器：坏行不再吞掉后续行，直到 EOF 才结束。
    #[test]
    fn lossy_lines_keeps_going_past_an_invalid_line() {
        let input = b"ok\n\xff\xfe bad\ny\n".to_vec();
        assert_eq!(
            lossy_lines(Cursor::new(input)).collect::<Vec<_>>(),
            vec!["ok\n", "\u{FFFD}\u{FFFD} bad\n", "y\n"]
        );
    }

    /// 「可继续的坏字节」与「真正的读取错误」是两回事：后者仍要上抛（标志位只承担前者）。
    #[test]
    fn io_errors_still_propagate() {
        struct Failing;
        impl std::io::Read for Failing {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("boom"))
            }
        }
        let mut reader = std::io::BufReader::new(Failing);
        assert!(read_line_lossy(&mut reader).is_err());
    }

    /// EOF 前的最后一行没有换行符也必须作为一行交出（如 `printf '\200'` 的形态）。
    #[test]
    fn final_line_without_newline_counts_as_a_line() {
        let mut valid_tail = Cursor::new(b"tail".to_vec());
        assert_eq!(
            read_line_lossy(&mut valid_tail).unwrap().unwrap(),
            ("tail".to_string(), true)
        );

        let mut invalid_tail = Cursor::new(b"\x80".to_vec());
        assert_eq!(
            read_line_lossy(&mut invalid_tail).unwrap().unwrap(),
            ("\u{FFFD}".to_string(), false)
        );
    }
}
