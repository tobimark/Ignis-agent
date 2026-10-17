//! 脚本化假模型服务：手写 HTTP/1.1 + SSE，只用 std（离线缓存里没有 axum）。
//!
//! 内核测试（native 循环、端到端场景）用它驱动流式对话：每个请求消耗一步脚本，
//! 服务端按脚本回一段 SSE、一个错误响应，或干脆断流；收到的请求体会被记录，
//! 供断言「回填给模型的消息长什么样」。绑定 `127.0.0.1:0` 随机端口，句柄 `Drop`
//! 时回收线程与端口，测试之间互不干扰。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

/// 读请求的超时：够本地回环用，又不会在测试收尾时把线程挂太久。
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 假模型上报的固定用量（测试据此断言记账）。
const PROMPT_TOKENS: u64 = 12;
const COMPLETION_TOKENS: u64 = 3;
/// 客户端必须 POST 到这里；路径不对就回 404，让 URL 拼错立刻暴露。
const CHAT_PATH: &str = "/v1/chat/completions";

/// 假模型的一步脚本；每个请求消耗一步，脚本用尽后回 500（不 panic）。
#[derive(Debug, Clone)]
pub enum ScriptStep {
    /// 纯文本回答：文本拆成 3 个 delta，末块带 `finish_reason: "stop"` 与 usage。
    Text(String),
    /// 推理 + 正文：先对 `reasoning` 走 `delta.reasoning_content`（拆 3 片），
    /// 再对 `text` 走 `delta.content`（拆 3 片），末块带 `finish_reason: "stop"` 与 usage。
    ReasoningText { reasoning: String, text: String },
    /// 文本 + 工具调用：`calls` 为 `(name, args)`，每个调用的 arguments 拆成 3 个分片。
    ToolCalls {
        text: String,
        calls: Vec<(String, Value)>,
    },
    /// 非 2xx 响应：驱动错误映射与重试测试。
    Error { status: u16, body: String },
    /// 先发一段 200 + SSE 字节，随即断流：驱动「流已开始不重试」测试。
    AbruptClose(String),
    /// 收下请求但不做任何响应直接断开：驱动连接级重试测试。
    DropConnection,
}

/// 假模型句柄：`base_url()` / `requests()` 供断言，`Drop` 时停掉服务线程。
pub struct ScriptedModelHandle {
    base_url: String,
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ScriptedModelHandle {
    /// 传给客户端配置的 base_url：`http://127.0.0.1:{port}/v1`。
    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// 到目前为止收到的请求体（按 JSON 解析；解析不了的原样记录成字符串）。
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for ScriptedModelHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // 自连接唤醒阻塞在 accept 的线程；connect 失败说明它已经退出
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 假模型服务入口。
pub struct ScriptedModel;

impl ScriptedModel {
    /// 绑定 127.0.0.1 的随机端口并启动服务线程；返回时端口已可用（同步 API）。
    pub fn start(script: Vec<ScriptStep>) -> ScriptedModelHandle {
        let listener = TcpListener::bind("127.0.0.1:0").expect("假模型服务应能绑定回环端口");
        let addr = listener.local_addr().expect("假模型服务应能读到本地地址");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || serve(listener, script, requests, stop))
        };
        ScriptedModelHandle {
            base_url: format!("http://{addr}/v1"),
            addr,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}

/// 顺序服务请求：读一份请求体、记录、按下一步脚本响应。任何读不动的情况都只记录后跳过。
fn serve(
    listener: TcpListener,
    script: Vec<ScriptStep>,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
) {
    let mut steps = script.into_iter();
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { break };
        if stop.load(Ordering::SeqCst) {
            break; // Drop 的唤醒连接：直接退出，listener 随线程结束关闭
        }
        let Some(raw) = read_request(&mut stream) else {
            // 连请求都读不全：回个 500 保证客户端不会干等，然后继续
            let _ = stream.write_all(&http_response(
                500,
                "application/json",
                br#"{"error":"scripted model: unreadable request"}"#,
            ));
            continue;
        };
        if request_path(&raw) != CHAT_PATH {
            let _ = stream.write_all(&http_response(
                404,
                "application/json",
                br#"{"error":"scripted model: unexpected path"}"#,
            ));
            continue; // 路径错不消耗脚本步
        }
        requests.lock().unwrap().push(request_body(&raw));
        match steps.next() {
            Some(step) => respond(&mut stream, &step),
            None => {
                let _ = stream.write_all(&http_response(
                    500,
                    "application/json",
                    br#"{"error":"scripted model: script exhausted"}"#,
                ));
            }
        }
    }
}

/// 按一步脚本响应；`DropConnection` 什么都不写，靠 `serve` 结束时关闭连接。
fn respond(stream: &mut TcpStream, step: &ScriptStep) {
    match step {
        ScriptStep::Text(text) => write_sse(stream, &text_chunks(text)),
        ScriptStep::ReasoningText { reasoning, text } => {
            write_sse(stream, &reasoning_text_chunks(reasoning, text))
        }
        ScriptStep::ToolCalls { text, calls } => write_sse(stream, &tool_call_chunks(text, calls)),
        ScriptStep::Error { status, body } => {
            let _ = stream.write_all(&http_response(*status, "application/json", body.as_bytes()));
        }
        ScriptStep::AbruptClose(text) => {
            // 声明比实际发送更长的 Content-Length 后关连接：客户端必须看到「流中断」错误
            let body = sse_lines(&[text_chunk(text)]);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len() + 512
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
        }
        ScriptStep::DropConnection => {}
    }
}

/// 纯文本回答的 chunk 序列（brief：文本拆 3 个 delta，usage 在末块）。
fn text_chunks(text: &str) -> Vec<Value> {
    let mut chunks: Vec<Value> = fragments(text)
        .into_iter()
        .map(|part| chunk(json!({"content": part}), None))
        .collect();
    chunks.push(chunk(json!({}), Some("stop")));
    chunks
}

/// 推理 + 正文回答的 chunk 序列：推理走 `delta.reasoning_content`、正文走 `delta.content`，
/// 各自按 [`fragments`] 拆片；末块带 `finish_reason: "stop"` 与 usage（与纯文本回答同形）。
fn reasoning_text_chunks(reasoning: &str, text: &str) -> Vec<Value> {
    let mut chunks: Vec<Value> = fragments(reasoning)
        .into_iter()
        .map(|part| chunk(json!({"reasoning_content": part}), None))
        .collect();
    chunks.extend(
        fragments(text)
            .into_iter()
            .map(|part| chunk(json!({"content": part}), None)),
    );
    chunks.push(chunk(json!({}), Some("stop")));
    chunks
}

/// 文本 + 工具调用的 chunk 序列：每个调用的 `arguments` 拆 3 个分片，
/// `id` / `name` 只在首个分片出现，`index` 每个分片都带。
fn tool_call_chunks(text: &str, calls: &[(String, Value)]) -> Vec<Value> {
    let mut chunks: Vec<Value> = fragments(text)
        .into_iter()
        .map(|part| chunk(json!({"content": part}), None))
        .collect();
    for (index, (name, args)) in calls.iter().enumerate() {
        let mut parts = fragments(&args.to_string()).into_iter();
        let first = parts.next().unwrap_or_default();
        chunks.push(chunk(
            json!({"tool_calls": [{
                "index": index,
                "id": format!("call_{}", index + 1),
                "type": "function",
                "function": {"name": name, "arguments": first},
            }]}),
            None,
        ));
        chunks.extend(parts.map(|part| {
            chunk(
                json!({"tool_calls": [{"index": index, "function": {"arguments": part}}]}),
                None,
            )
        }));
    }
    chunks.push(chunk(json!({}), Some("tool_calls")));
    chunks
}

/// 一个 OpenAI 兼容的流式 chunk：末块的 `usage` 由 [`chunk`] 一并带上。
fn chunk(delta: Value, finish_reason: Option<&str>) -> Value {
    let mut chunk = json!({
        "id": "chatcmpl-scripted",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "scripted-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    });
    if finish_reason.is_some() {
        chunk["usage"] = json!({
            "prompt_tokens": PROMPT_TOKENS,
            "completion_tokens": COMPLETION_TOKENS,
            "total_tokens": PROMPT_TOKENS + COMPLETION_TOKENS,
        });
    }
    chunk
}

/// 一段文本的流式 chunk（只带 content，不带 usage）。
fn text_chunk(text: &str) -> Value {
    chunk(json!({"content": text}), None)
}

/// 把文本按字符切成 3 片（不足 3 个字符就有几片），模拟真实的 arguments 分片下发。
fn fragments(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let width = chars.len().div_ceil(3);
    chars
        .chunks(width)
        .map(|part| part.iter().collect())
        .collect()
}

/// 一段 SSE 文本（不含 `data: [DONE]`）。
fn sse_lines(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    }
    body
}

/// 完整 SSE 响应体：chunks + `data: [DONE]`。
fn sse_body(chunks: &[Value]) -> String {
    let mut body = sse_lines(chunks);
    body.push_str("data: [DONE]\n\n");
    body
}

/// 写一个 `200` + SSE 响应，随后由 `serve` 关闭连接（`Connection: close`）。
fn write_sse(stream: &mut TcpStream, chunks: &[Value]) {
    let body = sse_body(chunks);
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// 一条带 `Content-Length` 与 `Connection: close` 的响应。
fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reason(status),
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// 常见状态码的原因短语（HTTP/1.1 响应行要求有这一段）。
fn reason(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// 读一条完整请求（请求行 + 头 + 按 Content-Length 读满的 body）；读不动返回 `None`。
fn read_request(stream: &mut TcpStream) -> Option<String> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let body_start = loop {
        if let Some(end) = header_end(&buf) {
            break end;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let wanted = body_start + content_length(&buf[..body_start]);
    while buf.len() < wanted {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// header 区结束的位置（`\r\n\r\n` 之后一个字节）。
fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

/// 解析大小写不敏感的 `Content-Length`；缺失或不可解析都按 0 处理。
fn content_length(header: &[u8]) -> usize {
    for line in String::from_utf8_lossy(header).split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// 请求行里的路径（`POST /v1/chat/completions HTTP/1.1` → 第二段）。
fn request_path(raw: &str) -> &str {
    raw.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
}

/// 请求体 → JSON；解析失败时原样记录成字符串（不 panic）。
fn request_body(raw: &str) -> Value {
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()))
}
