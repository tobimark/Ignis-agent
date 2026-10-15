//! SSE 分帧解码：把 TCP 字节流切成完整的 `data:` 负载。
//!
//! 只做分帧，不解释内容——JSON → `ChatChunk` 的映射在上级模块。
//! 缓冲按字节做、收全整行后才按 UTF-8 解码，因此多字节字符被分片切开也不会乱码。

use crate::openai::LlmError;

/// SSE 行解码器：喂进任意大小的字节片，吐出其中已经收全的 `data:` 负载。
///
/// 注释行（`:` 开头）与 `event:` / `id:` / `retry:` 等字段一律忽略；
/// 空行（事件分隔）不产出负载；`data:` 后无内容的空负载原样交出，由上层决定是否忽略。
#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一段字节，返回其中已经收全的 `data:` 负载；不完整的行留在缓冲里等下一片。
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, LlmError> {
        self.buffer.extend_from_slice(bytes);
        let mut payloads = Vec::new();
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop(); // 去掉 '\n'
            if line.last() == Some(&b'\r') {
                line.pop(); // 兼容 CRLF 行尾
            }
            let line = String::from_utf8(line)
                .map_err(|err| LlmError::Malformed(format!("SSE 行不是合法 UTF-8：{err}")))?;
            if let Some(payload) = data_payload(&line) {
                payloads.push(payload.to_string());
            }
        }
        Ok(payloads)
    }
}

/// `data:` 行 → 负载；注释行、其他字段行与空行都返回 `None`。
fn data_payload(line: &str) -> Option<&str> {
    let payload = line.strip_prefix("data:")?;
    // 规范允许字段值前有一个空格
    Some(payload.strip_prefix(' ').unwrap_or(payload))
}

#[cfg(test)]
mod tests {
    use super::SseDecoder;

    /// brief 钉死：行未收全不吐负载；心跳注释行（`:`）忽略。
    #[test]
    fn sse_decoder_handles_split_boundaries_and_comments() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: {\"a\":").unwrap().is_empty()); // 行未收全
        assert_eq!(
            d.push(b"1}\n\n: keep-alive\n\ndata: [DONE]\n\n").unwrap(),
            vec!["{\"a\":1}".to_string(), "[DONE]".to_string()] // 心跳行忽略
        );
    }

    /// 真实中转常发 CRLF 与其他字段行：只认 `data:`，行尾 `\r` 不能混进负载。
    #[test]
    fn sse_decoder_ignores_other_fields_and_crlf() {
        let mut d = SseDecoder::new();
        assert_eq!(
            d.push(b"event: message\r\nid: 7\r\ndata: {\"x\":1}\r\n\r\ndata:\r\n\r\n")
                .unwrap(),
            // 空负载原样交出，由上层决定忽略与否
            vec!["{\"x\":1}".to_string(), String::new()]
        );
    }

    /// 多字节字符被 TCP 分片切开：按字节缓冲整行后再解码，不能变成乱码。
    #[test]
    fn sse_decoder_keeps_multibyte_split_across_pushes() {
        let mut d = SseDecoder::new();
        let line = "data: {\"t\":\"你好\"}\n\n".as_bytes();
        let (head, tail) = line.split_at(14); // 切在「你」的三个字节中间
        assert!(d.push(head).unwrap().is_empty());
        assert_eq!(d.push(tail).unwrap(), vec!["{\"t\":\"你好\"}".to_string()]);
    }
}
