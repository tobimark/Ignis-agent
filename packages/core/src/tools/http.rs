//! HTTP 工具：`http_fetch`，v0.1 唯一主动出网的入口。
//!
//! 出网请求必须可预期：只允许 `http`/`https`，重定向至多 3 跳，单次请求（建连 + 读完响应体）
//! 有总时限（默认 30s，测试用 `HttpFetch::with_timeout` 收窄）。响应体流式读取：读满
//! `limits.http_max_bytes` 就停下，余量既不进内存也不落盘，输出里以截断标记标明；字节一律
//! 按 lossy UTF-8 解码，二进制响应不会 panic。

use std::time::Duration;

use async_trait::async_trait;
use ignis_runtime::ToolSpec;
use reqwest::header::CONTENT_TYPE;
use reqwest::redirect::Policy;
use reqwest::{Client, Method, Request, Response, StatusCode, Url};
use serde_json::{Value, json};

use super::util::{optional_str, required_str};
use super::{Tool, ToolCtx, ToolError};

/// 建连超时（brief 钉死 10s）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 单次请求总时限（brief 钉死 30s）：建连 + 发送 + 读完（或读满上限）响应体。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 最多跟随的重定向跳数（brief 钉死 3）。
const MAX_REDIRECTS: usize = 3;
/// 出网请求的 User-Agent（brief 钉死）。
const USER_AGENT: &str = "ignis/0.1";

/// 抓取一个 http(s) URL。
pub(super) struct HttpFetch {
    client: Client,
    /// 单次请求总时限：`tokio::time::timeout` 包住「发送 + 读体」。
    timeout: Duration,
}

impl HttpFetch {
    /// 生产构造：单次请求总时限 30s。
    pub(super) fn new() -> Self {
        Self::with_timeout(REQUEST_TIMEOUT)
    }

    /// 指定单次请求总时限；测试用它把 30s 收窄到毫秒级。
    pub(super) fn with_timeout(timeout: Duration) -> Self {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(Policy::limited(MAX_REDIRECTS))
            .user_agent(USER_AGENT)
            .build()
            .expect("HTTP 客户端必须可构造");
        Self { client, timeout }
    }
}

/// 读到的响应：状态、content-type 与（可能被截断的）响应体。
struct Fetched {
    status: StatusCode,
    content_type: Option<String>,
    body: Vec<u8>,
    truncated: bool,
}

#[async_trait]
impl Tool for HttpFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "http_fetch".into(),
            description: "Fetch an http:// or https:// URL and return the status code, content type and response body. Redirects are followed up to 3 times; the body is decoded as lossy UTF-8 and capped at the configured byte limit.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute http:// or https:// URL to fetch"
                    },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST"],
                        "description": "HTTP method (default \"GET\")"
                    },
                    "body": {
                        "type": "string",
                        "description": "Request body sent as-is (typically with method POST)"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
        let url = required_str(args, "url")?;
        let method = parse_method(optional_str(args, "method")?)?;
        let body = optional_str(args, "body")?;
        let request = build_request(&self.client, url, method, body)?;
        let max_bytes = ctx.limits.http_max_bytes;

        let outcome =
            tokio::time::timeout(self.timeout, fetch(&self.client, request, max_bytes)).await;
        match outcome {
            Err(_elapsed) => Err(ToolError::message(format!(
                "请求超时（timeout {:?}）：{url}",
                self.timeout
            ))),
            Ok(Err(err)) => Err(ToolError::message(format!("请求失败：{url}：{err}"))),
            Ok(Ok(fetched)) => Ok(render_output(&fetched, max_bytes)),
        }
    }
}

/// `method` 参数：缺省 GET，只认 GET/POST（大小写不敏感）。
fn parse_method(method: Option<&str>) -> Result<Method, ToolError> {
    let method = method.unwrap_or("GET");
    match method.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::GET),
        "POST" => Ok(Method::POST),
        _ => Err(ToolError::message(format!(
            "不支持的 method：{method}（只支持 GET/POST）"
        ))),
    }
}

/// 构造请求：URL 必须能解析且 scheme 是 http/https，body 原样带上（不自动补 content-type）。
fn build_request(
    client: &Client,
    url: &str,
    method: Method,
    body: Option<&str>,
) -> Result<Request, ToolError> {
    let parsed =
        Url::parse(url).map_err(|err| ToolError::message(format!("URL 无效：{url}：{err}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ToolError::message(format!(
            "只允许 http/https URL（scheme = {}）：{url}",
            parsed.scheme()
        )));
    }
    let mut builder = client.request(method, parsed);
    if let Some(body) = body {
        builder = builder.body(body.to_string());
    }
    builder
        .build()
        .map_err(|err| ToolError::message(format!("构造请求失败：{url}：{err}")))
}

/// 发送请求并读回响应：状态、content-type 与最多 `max_bytes` 的响应体。
async fn fetch(
    client: &Client,
    request: Request,
    max_bytes: usize,
) -> Result<Fetched, reqwest::Error> {
    let mut response = client.execute(request).await?;
    let status = response.status();
    // content-type 常带 charset 参数，原样回显；缺失时由输出层补 unknown
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (body, truncated) = read_body(&mut response, max_bytes).await?;
    Ok(Fetched {
        status,
        content_type,
        body,
        truncated,
    })
}

/// 流式读响应体：最多 `max_bytes` 字节，超出即停止读取（余量直接丢弃）。
///
/// 返回还带一个标记：只有「确认读取时还有余量」才为真——恰好读满上限且响应正好结束时
/// 不算截断。
async fn read_body(
    response: &mut Response,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await? {
        let remaining = max_bytes.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, truncated))
}

/// 组装输出：`status: {n}\ncontent-type: {t}\n{body 截断至 http_max_bytes}`。
///
/// lossy 解码可能让文本长于原始字节（非法字节 → 3 字节的 U+FFFD），所以这里也按上限裁到
/// 字符边界；读取时被截断、或解码后仍超出上限，都在尾部留截断标记。
fn render_output(fetched: &Fetched, max_bytes: usize) -> String {
    let content_type = fetched.content_type.as_deref().unwrap_or("unknown");
    let mut output = format!(
        "status: {}\ncontent-type: {content_type}\n",
        fetched.status.as_u16()
    );
    let text = String::from_utf8_lossy(&fetched.body);
    let mut end = text.len().min(max_bytes);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&text[..end]);
    if fetched.truncated || end < text.len() {
        output.push_str(&format!("\n… [truncated: body exceeded {max_bytes} bytes]"));
    }
    output
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use ignis_runtime::Limits;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::tools::{Tool, ToolCtx};

    /// 假服务器的 handler：完整请求文本 → 要写回的原始响应字节。
    type Handler = dyn Fn(&str) -> Vec<u8> + Send + Sync;

    /// 手写假 HTTP 服务器（brief：只用 std，不引入测试框架依赖）：监听 127.0.0.1 的随机
    /// 端口，每条连接交给 handler；Drop 时唤醒并回收 accept 线程，不留监听端口。
    struct FakeServer {
        base: String,
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeServer {
        /// `handler` 收到完整请求文本（请求行 + 头 + 按 Content-Length 读满的 body），
        /// 返回要写回的原始响应字节。
        fn start<F>(handler: F) -> Self
        where
            F: Fn(&str) -> Vec<u8> + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").expect("假服务器应能绑定回环端口");
            let addr = listener.local_addr().unwrap();
            let handler: Arc<Handler> = Arc::new(handler);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_flag = Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    if stop_flag.load(Ordering::SeqCst) {
                        break; // Drop 的自连接唤醒：拿到连接直接退出，listener 随线程结束关闭
                    }
                    let handler = Arc::clone(&handler);
                    std::thread::spawn(move || serve(stream, handler.as_ref()));
                }
            });
            Self {
                base: format!("http://{addr}"),
                addr,
                stop,
                thread: Some(thread),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // 自连接唤醒阻塞在 accept 的线程；connect 失败说明它已经退出
            let _ = TcpStream::connect(self.addr);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// 处理一条连接：读完整请求，写回 handler 的响应，再关掉写端。
    fn serve(mut stream: TcpStream, handler: &Handler) {
        let request = read_request(&mut stream);
        let _ = stream.write_all(&handler(&request));
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Write);
    }

    /// 读一条完整请求：先读到 header 结束，再按 Content-Length 读满 body。
    fn read_request(stream: &mut TcpStream) -> String {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = loop {
            if let Some(end) = header_end(&buf) {
                break Some(end);
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let Some(end) = end else {
            return String::from_utf8_lossy(&buf).into_owned();
        };
        let wanted = end + content_length(&buf[..end]);
        while buf.len() < wanted {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
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

    /// 一条带 `Content-Length` 与 `Connection: close` 的响应。
    fn raw_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// 302 到 `location` 的空响应。
    fn redirect_response(location: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    /// 在 `limits` 下执行一次工具调用：`extra` 是 `url` 之外的参数。
    async fn run(
        tool: &HttpFetch,
        url: &str,
        extra: Value,
        limits: &Limits,
    ) -> Result<String, ToolError> {
        let mut args = extra;
        args["url"] = json!(url);
        let ctx = ToolCtx {
            workspace: Path::new("."),
            cancel: CancellationToken::new(),
            limits,
        };
        tool.run(&args, &ctx).await
    }

    /// 断言失败信息里只带输出开头/结尾，别把 1MB 响应糊到终端；都按字符边界取。
    fn head(output: &str) -> String {
        output.chars().take(80).collect()
    }

    fn tail(output: &str) -> String {
        let mut start = output.len().saturating_sub(80);
        while start > 0 && !output.is_char_boundary(start) {
            start -= 1;
        }
        output[start..].to_string()
    }

    /// 5MB 响应：只读到上限就停，输出带上截断标记，绝不把余量拉进内存。
    #[tokio::test]
    async fn fetches_and_caps_body() {
        let server = FakeServer::start(|_| {
            raw_response("200 OK", "text/plain", &vec![b'x'; 5 * 1024 * 1024])
        });
        let limits = Limits::default();

        let output = run(&HttpFetch::new(), &server.url("/big"), json!({}), &limits)
            .await
            .unwrap();

        let header = "status: 200\ncontent-type: text/plain\n";
        assert!(output.starts_with(header), "输出头不对：{}", head(&output));
        assert!(output.contains("[truncated"), "输出应带截断标记");
        let body = &output[header.len()..];
        let cap = limits.http_max_bytes;
        let marker = format!("\n… [truncated: body exceeded {cap} bytes]");
        assert!(
            body.ends_with(&marker),
            "截断标记缺失，尾部：{}",
            tail(body)
        );
        // 上限 + 标记，多一个字节都说明把 5MB 全读了
        assert_eq!(body.len(), cap + marker.len());
    }

    /// 非 UTF-8 响应体：lossy 解码，不 panic。
    #[tokio::test]
    async fn non_utf8_body_is_lossy() {
        let server = FakeServer::start(|_| {
            raw_response("200 OK", "application/octet-stream", &[0xff, 0xfe, b'a'])
        });
        let limits = Limits::default();

        let output = run(&HttpFetch::new(), &server.url("/bin"), json!({}), &limits)
            .await
            .unwrap();

        assert!(output.starts_with("status: 200\n"), "{output}");
        assert!(output.contains('\u{fffd}'), "非法字节应被替换：{output}");
        assert!(output.ends_with('a'), "{output}");
    }

    /// 服务端 40 秒不响应：短时限的工具必须带 timeout 错误及时返回。
    #[tokio::test]
    async fn request_timeout_is_reported() {
        // 收尾时置位让假服务器提前退出，不在测试进程里留一个真睡 40 秒的线程
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let server = FakeServer::start(move |_| {
            for _ in 0..400 {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            raw_response("200 OK", "text/plain", b"late")
        });
        let limits = Limits::default();

        let tool = HttpFetch::with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let err = run(&tool, &server.url("/slow"), json!({}), &limits)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "超时应很快返回，实际 {:?}",
            started.elapsed()
        );
        done.store(true, Ordering::SeqCst);
    }

    /// 302 跳到目标；超过 `limited(3)` 的重定向环必须失败而不是无限跟随。
    #[tokio::test]
    async fn follows_redirects_up_to_limit() {
        let server = FakeServer::start(|request| {
            if request.starts_with("GET /b ") {
                raw_response("200 OK", "text/plain", b"redirected")
            } else {
                // /a → /b 一跳；/loop 永远指向自己
                redirect_response(if request.starts_with("GET /a ") {
                    "/b"
                } else {
                    "/loop"
                })
            }
        });
        let limits = Limits::default();

        let output = run(&HttpFetch::new(), &server.url("/a"), json!({}), &limits)
            .await
            .unwrap();
        assert!(output.starts_with("status: 200\n"), "{output}");
        assert!(output.ends_with("redirected"), "{output}");

        let err = run(&HttpFetch::new(), &server.url("/loop"), json!({}), &limits)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("redirect"), "{err}");
    }

    /// POST 的请求行与 body 必须真的送到服务端（服务端把收到的原样回显）。
    #[tokio::test]
    async fn posts_the_request_body() {
        let server = FakeServer::start(|request| {
            let request_line = request.lines().next().unwrap_or_default();
            let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
            raw_response(
                "200 OK",
                "text/plain",
                format!("{request_line}\n{body}").as_bytes(),
            )
        });
        let limits = Limits::default();

        let output = run(
            &HttpFetch::new(),
            &server.url("/echo"),
            json!({"method": "POST", "body": "hello=ignis"}),
            &limits,
        )
        .await
        .unwrap();

        assert!(output.contains("POST /echo HTTP/1.1"), "{output}");
        assert!(output.ends_with("hello=ignis"), "{output}");
    }

    /// 只有 http/https 能出网；其余 scheme 与非 URL 输入都给出可读的 Err。
    #[tokio::test]
    async fn rejects_non_http_schemes() {
        let limits = Limits::default();

        for url in ["file:///etc/passwd", "ftp://example.com/x"] {
            let err = run(&HttpFetch::new(), url, json!({}), &limits)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("http"), "url = {url}：{err}");
        }

        let err = run(&HttpFetch::new(), "not a url", json!({}), &limits)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a url"), "{err}");

        let err = run(
            &HttpFetch::new(),
            "https://example.com",
            json!({"method": "DELETE"}),
            &limits,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("DELETE"), "{err}");
    }
}
