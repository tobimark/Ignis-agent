//! OpenAI 兼容模型客户端：流式请求、SSE 解析、工具调用分片与钉死的重试规则。
//!
//! 重试只发生在「还没拿到响应」时（连接失败 / 429 / 5xx）；一旦 2xx 到手，
//! 流中途的任何错误都作为流内 `Err` 抛给上层，不再重试——重试会把已经展示给用户的
//! 半截回答重放一遍。请求体与响应解析都走显式映射（不依赖契约类型的 serde 派生），
//! 因为线上形状（`tool_call_id`、`tool_calls`、`stream_options`）是跨模块契约。

use std::collections::VecDeque;
use std::time::Duration;

use futures::{StreamExt, stream};
use serde_json::{Value, json};

use crate::{BoxStream, ChatMessage, ModelConfig, Role, ToolSpec, Usage};

pub mod assembler;
pub mod sse;

use sse::SseDecoder;

/// SSE 结束标记。
const DONE: &str = "[DONE]";
/// 默认重试次数（brief 钉死）。
const DEFAULT_MAX_RETRIES: u32 = 3;
/// 默认退避基数（brief 钉死）：实际等待 = `base * 2^(n-1)` + 0–20% 抖动。
const DEFAULT_BACKOFF: Duration = Duration::from_millis(500);
/// 退避指数上限：避免 `2^n` 溢出，也避免理论上的超长睡眠。
const MAX_BACKOFF_SHIFT: u32 = 6;
/// 错误信息里保留的响应体字符数。
const ERROR_BODY_LIMIT: usize = 300;

/// 模型调用的失败；`Display` 面向终端用户，故一律说人话（brief 钉死文案）。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// 401：key 无效或过期。
    #[error("模型鉴权失败 (HTTP 401)：检查 apiKey/OPENAI_API_KEY")]
    Unauthorized,
    /// 429：限流或额度耗尽。
    #[error("模型速率限制 (HTTP 429)：稍后重试或检查额度")]
    RateLimited,
    /// 5xx：服务端故障。
    #[error("模型服务端错误 (HTTP {status})：{body}")]
    ServerError { status: u16, body: String },
    /// 其余非 2xx：请求本身有问题，重试没有意义。
    #[error("模型请求被拒绝 (HTTP {status})：{body}")]
    RequestRejected { status: u16, body: String },
    /// 响应不是约定的形状（SSE 负载不是 JSON、工具调用分片不完整等）。
    #[error("模型响应格式异常：{0}")]
    Malformed(String),
    /// 传输层失败：建连、TLS、以及 2xx 之后的流中断。
    #[error("模型请求失败：{0}")]
    Transport(String),
    /// 构造参数不合法（base_url 等）。
    #[error("模型配置无效：{0}")]
    Config(String),
}

impl LlmError {
    /// 是否值得重试：连接级失败、限流、5xx 是瞬时故障；其余重试没有意义（brief 钉死）。
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::RateLimited | Self::ServerError { .. }
        )
    }
}

/// 一个流式响应块：文本增量、工具调用增量、结束原因与（末块的）用量。
#[derive(Debug, Clone, PartialEq)]
pub struct ChatChunk {
    pub text: Option<String>,
    pub tool_call_deltas: Vec<ToolCallDelta>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

/// 工具调用增量：`id` / `name` 只在首个分片出现，`args_delta` 需要按 index 拼接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub args_delta: Option<String>,
}

/// OpenAI 兼容流式客户端。持有 api key，因此不派生 `Debug`（避免 key 进日志）。
pub struct OpenAiClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
    max_retries: u32,
    backoff: Duration,
}

impl OpenAiClient {
    /// 按配置构造；`api_key` 由上层从环境变量或配置读出后注入（runtime 不读环境）。
    pub fn new(cfg: &ModelConfig, api_key: String) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|err| LlmError::Config(format!("HTTP 客户端构造失败：{err}")))?;
        let base = cfg.base_url.trim_end_matches('/');
        let endpoint = format!("{base}/chat/completions");
        // 早失败：base_url 连 URL 都不是时，报配置错误而不是等第一次请求
        reqwest::Url::parse(&endpoint)
            .map_err(|err| LlmError::Config(format!("base_url 无效：{base}：{err}")))?;
        Ok(Self {
            http,
            endpoint,
            model: cfg.model.clone(),
            api_key,
            max_retries: DEFAULT_MAX_RETRIES,
            backoff: DEFAULT_BACKOFF,
        })
    }

    /// 覆盖重试次数（默认 3）；收到 2xx 之后永不重试，与此值无关。
    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    /// 覆盖退避基数（默认 500ms）；实际等待 = `base * 2^(n-1)` + 0–20% 抖动。
    pub fn with_backoff(mut self, base: Duration) -> Self {
        self.backoff = base;
        self
    }

    /// 发起一次流式对话：返回 chunk 流，流内 `Err` 表示 2xx 之后的失败（不再重试）。
    pub async fn stream_chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        system: &str,
    ) -> Result<BoxStream<'static, Result<ChatChunk, LlmError>>, LlmError> {
        let body = request_body(&self.model, messages, tools, system);
        let response = self.send_with_retries(&body).await?;
        Ok(Box::pin(chunk_stream(response)))
    }

    /// 发送请求：只在「没拿到响应」或「值得重试的非 2xx」时重试，次数用尽后返回最后一次错误。
    async fn send_with_retries(&self, body: &Value) -> Result<reqwest::Response, LlmError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let outcome = self
                .http
                .post(&self.endpoint)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", self.api_key),
                )
                .json(body)
                .send()
                .await
                .map_err(|err| LlmError::Transport(err.to_string()));
            let error = match outcome {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => status_error(response).await,
                Err(err) => err,
            };
            if attempt > self.max_retries || !error.is_retryable() {
                return Err(error);
            }
            tokio::time::sleep(backoff_delay(self.backoff, attempt)).await;
        }
    }
}

/// 请求体（brief 钉死形状）：显式映射，不依赖契约类型的 serde 派生。
///
/// `assistant` 的 `tool_calls` 以「arguments JSON 字符串」下发，`tool` 消息带 `tool_call_id`；
/// 没有工具时省略 `tools` 字段（部分兼容层会拒绝空数组）。
fn request_body(model: &str, messages: &[ChatMessage], tools: &[ToolSpec], system: &str) -> Value {
    let mut mapped = vec![json!({"role": "system", "content": system})];
    mapped.extend(messages.iter().map(message_body));
    let mut body = json!({
        "model": model,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": mapped,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        },
                    })
                })
                .collect(),
        );
    }
    body
}

/// 单条消息：role 用小写 OpenAI 命名；`tool_call_id` / `tool_calls` 由 core 按角色填好，这里只搬运。
fn message_body(message: &ChatMessage) -> Value {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut body = json!({"role": role, "content": message.content});
    if let Some(tool_call_id) = &message.tool_call_id {
        body["tool_call_id"] = json!(tool_call_id);
    }
    if !message.tool_calls.is_empty() {
        body["tool_calls"] = Value::Array(
            message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": call.args.to_string()},
                    })
                })
                .collect(),
        );
    }
    body
}

/// 第 `attempt` 次重试前的等待时长：`base * 2^(attempt-1)` + 0–20% 抖动（brief 钉死）。
fn backoff_delay(base: Duration, attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(MAX_BACKOFF_SHIFT);
    let scaled = base.saturating_mul(1 << shift);
    // 抖动取系统时钟纳秒低位（0–199‰）；这里不需要可复现的随机分布，只要别让请求撞在一起
    let permille = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos())
        % 200) as u64;
    scaled.saturating_add(scaled.mul_f64(permille as f64 / 1000.0))
}

/// 非 2xx 响应 → 人话错误；响应体只留开头一小段，避免错误信息被巨响应撑爆。
async fn status_error(response: reqwest::Response) -> LlmError {
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => truncate(&body, ERROR_BODY_LIMIT),
        Err(err) => format!("<读取响应体失败：{err}>"),
    };
    let code = status.as_u16();
    match code {
        401 => LlmError::Unauthorized,
        429 => LlmError::RateLimited,
        _ if status.is_server_error() => LlmError::ServerError { status: code, body },
        _ => LlmError::RequestRejected { status: code, body },
    }
}

/// 截到 `limit` 个字符，尾部标注省略（按字符裁，不破坏 UTF-8 边界）。
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit).collect();
    cut.push_str(" …[截断]");
    cut
}

/// 把 2xx 的响应体字节流解码成 chunk 流；流内错误直接上抛，不重试（brief 钉死）。
fn chunk_stream(
    response: reqwest::Response,
) -> impl stream::Stream<Item = Result<ChatChunk, LlmError>> + Send {
    stream::unfold(ChatStream::new(response), |mut state| async move {
        let chunk = state.next_chunk().await?;
        Some((chunk, state))
    })
}

/// 流式解析状态：字节流、SSE 解码缓冲与待消化的负载队列。
struct ChatStream {
    bytes: BoxStream<'static, Result<Vec<u8>, LlmError>>,
    decoder: SseDecoder,
    pending: VecDeque<String>,
    finished: bool,
}

impl ChatStream {
    fn new(response: reqwest::Response) -> Self {
        // 所有字节流错误都发生在 2xx 之后（首块之前），因此统一算「流中断」
        let bytes = response.bytes_stream().map(|item| {
            item.map(|chunk| chunk.to_vec())
                .map_err(|err| LlmError::Transport(format!("流中断：{err}")))
        });
        Self {
            bytes: Box::pin(bytes),
            decoder: SseDecoder::new(),
            pending: VecDeque::new(),
            finished: false,
        }
    }

    /// 下一个 chunk；返回 `None` 表示流结束。出错后立即置位 `finished`，只报一次。
    async fn next_chunk(&mut self) -> Option<Result<ChatChunk, LlmError>> {
        loop {
            if let Some(payload) = self.pending.pop_front() {
                if payload.trim() == DONE {
                    self.finished = true;
                    continue;
                }
                match chunk_from_payload(&payload) {
                    Ok(Some(chunk)) => return Some(Ok(chunk)),
                    Ok(None) => continue,
                    Err(err) => {
                        self.finished = true;
                        return Some(Err(err));
                    }
                }
            }
            if self.finished {
                return None;
            }
            match self.bytes.next().await {
                None => self.finished = true, // 服务端正常关流
                Some(Err(err)) => {
                    self.finished = true;
                    return Some(Err(err));
                }
                Some(Ok(bytes)) => match self.decoder.push(&bytes) {
                    Ok(payloads) => self.pending.extend(payloads),
                    Err(err) => {
                        self.finished = true;
                        return Some(Err(err));
                    }
                },
            }
        }
    }
}

/// 一行 SSE 负载 → `ChatChunk`；没有新信息的行（空行、只带 role 的 delta）返回 `None`。
fn chunk_from_payload(payload: &str) -> Result<Option<ChatChunk>, LlmError> {
    if payload.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(payload)
        .map_err(|err| LlmError::Malformed(format!("SSE 数据不是合法 JSON：{err}")))?;
    // 上游在流中报错时会把错误对象塞进 data 行：必须报出来，不能当空块吞掉
    if let Some(error) = value.get("error") {
        return Err(LlmError::Malformed(format!(
            "流含错误对象：{}",
            truncate(&error.to_string(), ERROR_BODY_LIMIT)
        )));
    }
    let choice = value.get("choices").and_then(|choices| choices.get(0));
    let delta = choice.and_then(|choice| choice.get("delta"));
    let text = delta
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
        // 有些兼容层会在首个块发空 content：空串不算文本增量
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let tool_call_deltas = match delta {
        Some(delta) => tool_call_deltas(delta)?,
        None => Vec::new(),
    };
    let finish_reason = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let usage = value.get("usage").and_then(parse_usage);
    if text.is_none() && tool_call_deltas.is_empty() && finish_reason.is_none() && usage.is_none() {
        return Ok(None);
    }
    Ok(Some(ChatChunk {
        text,
        tool_call_deltas,
        finish_reason,
        usage,
    }))
}

/// `delta.tool_calls[]` → 分片；缺 index 属响应格式异常（宁可报错也不静默丢调用）。
fn tool_call_deltas(delta: &Value) -> Result<Vec<ToolCallDelta>, LlmError> {
    let Some(calls) = delta.get("tool_calls") else {
        return Ok(Vec::new());
    };
    let calls = calls
        .as_array()
        .ok_or_else(|| LlmError::Malformed("delta.tool_calls 不是数组".into()))?;
    calls.iter().map(tool_call_delta).collect()
}

/// 单个 `delta.tool_calls[]` 元素 → `ToolCallDelta`。
fn tool_call_delta(call: &Value) -> Result<ToolCallDelta, LlmError> {
    let index = call
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| LlmError::Malformed("工具调用分片缺少 index".into()))?;
    let function = call.get("function");
    Ok(ToolCallDelta {
        index: index as usize,
        id: call.get("id").and_then(Value::as_str).map(str::to_owned),
        name: function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        args_delta: function
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// 顶层 `usage` → 契约 `Usage`；字段缺失按 0 计（部分中转只回其一）。
fn parse_usage(usage: &Value) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    Some(Usage {
        input_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        estimated: false,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use serde_json::json;

    use crate::openai::assembler::ToolCallAssembler;
    use crate::test_support::{ScriptStep, ScriptedModel, ScriptedModelHandle};
    use crate::{ChatMessage, ModelConfig, Role, ToolCall, ToolSpec, Usage};

    use super::{ChatChunk, LlmError, OpenAiClient, request_body};

    /// 指向脚本化假模型的客户端：退避压到 1ms，测试不睡真时间。
    fn client(model: &ScriptedModelHandle) -> OpenAiClient {
        let cfg = ModelConfig {
            base_url: model.base_url(),
            model: "scripted-model".into(),
            ..ModelConfig::default()
        };
        OpenAiClient::new(&cfg, "test-key".into())
            .unwrap()
            .with_backoff(Duration::from_millis(1))
    }

    fn message(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: vec![],
        }
    }

    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs_read".into(),
            description: "Read a file".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        }]
    }

    /// brief 钉死：文本按 3 个 delta 流式到达，末块带 usage；请求体形状一并锁住。
    #[tokio::test]
    async fn streams_text_and_usage_from_scripted_model() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("你好，世界".into())]);
        let chunks: Vec<Result<ChatChunk, LlmError>> = client(&model)
            .stream_chat(&[message(Role::User, "hi")], &tools(), "you are ignis")
            .await
            .unwrap()
            .collect()
            .await;
        let chunks: Vec<ChatChunk> = chunks.into_iter().collect::<Result<_, _>>().unwrap();

        let texts: Vec<String> = chunks
            .iter()
            .filter_map(|chunk| chunk.text.clone())
            .collect();
        assert_eq!(texts.concat(), "你好，世界");
        assert_eq!(texts.len(), 3, "文本应分 3 个 delta 下发：{texts:?}");
        assert!(texts.iter().all(|text| !text.is_empty()));

        let last = chunks.last().unwrap();
        assert_eq!(last.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            last.usage,
            Some(Usage {
                input_tokens: 12,
                output_tokens: 3,
                estimated: false
            })
        );

        let requests = model.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["model"], "scripted-model");
        assert_eq!(requests[0]["stream"], json!(true));
        assert_eq!(
            requests[0]["stream_options"],
            json!({"include_usage": true})
        );
        assert_eq!(
            requests[0]["messages"][0],
            json!({"role":"system","content":"you are ignis"})
        );
        assert_eq!(
            requests[0]["messages"][1],
            json!({"role":"user","content":"hi"})
        );
        assert_eq!(requests[0]["tools"][0]["type"], "function");
        assert_eq!(requests[0]["tools"][0]["function"]["name"], "fs_read");
        assert_eq!(
            requests[0]["tools"][0]["function"]["description"],
            "Read a file"
        );
        assert_eq!(
            requests[0]["tools"][0]["function"]["parameters"]["required"],
            json!(["path"])
        );
    }

    /// 工具调用分片走完整链路：客户端取 `delta.tool_calls` → assembler 拼回调用。
    #[tokio::test]
    async fn tool_call_fragments_assemble_into_calls() {
        let model = ScriptedModel::start(vec![ScriptStep::ToolCalls {
            text: "先看一下".into(),
            calls: vec![
                ("fs_read".into(), json!({"path": "a.txt"})),
                ("fs_list".into(), json!({})),
            ],
        }]);
        let chunks: Vec<Result<ChatChunk, LlmError>> = client(&model)
            .stream_chat(&[message(Role::User, "读 a.txt")], &tools(), "sys")
            .await
            .unwrap()
            .collect()
            .await;
        let chunks: Vec<ChatChunk> = chunks.into_iter().collect::<Result<_, _>>().unwrap();

        let mut assembler = ToolCallAssembler::new();
        for chunk in &chunks {
            for delta in &chunk.tool_call_deltas {
                assembler.push_delta(
                    delta.index,
                    delta.id.as_deref(),
                    delta.name.as_deref(),
                    delta.args_delta.as_deref(),
                );
            }
        }
        assert_eq!(
            assembler.finish().unwrap(),
            vec![
                ToolCall {
                    id: "call_1".into(),
                    name: "fs_read".into(),
                    args: json!({"path":"a.txt"})
                },
                ToolCall {
                    id: "call_2".into(),
                    name: "fs_list".into(),
                    args: json!({})
                },
            ]
        );
        let text: String = chunks
            .iter()
            .filter_map(|chunk| chunk.text.clone())
            .collect();
        assert_eq!(text, "先看一下");
        assert_eq!(
            chunks.last().unwrap().finish_reason.as_deref(),
            Some("tool_calls")
        );
    }

    /// brief 钉死：连接未响应要重试；第二次是 401，按人话映射且不再重试。
    #[tokio::test]
    async fn connection_drop_retries_then_reports_readably() {
        let model = ScriptedModel::start(vec![
            ScriptStep::DropConnection,
            ScriptStep::Error {
                status: 401,
                body: "{\"error\":\"bad key\"}".into(),
            },
        ]);
        let err = client(&model)
            .with_max_retries(1)
            .stream_chat(&[message(Role::User, "hi")], &tools(), "sys")
            .await
            .err()
            .expect("401 应返回构造错误");

        assert!(err.to_string().contains("401"), "{err}");
        assert!(err.to_string().contains("模型鉴权失败"), "{err}");
        assert_eq!(model.requests().len(), 2);
    }

    /// brief 钉死：2xx 之后流中断只报一次错，绝不重试（重试会把半截回答重放给用户）。
    #[tokio::test]
    async fn no_retry_after_stream_started() {
        let model = ScriptedModel::start(vec![ScriptStep::AbruptClose("partial".into())]);
        let items: Vec<Result<ChatChunk, LlmError>> = client(&model)
            .with_max_retries(3)
            .stream_chat(&[message(Role::User, "hi")], &tools(), "sys")
            .await
            .unwrap()
            .collect()
            .await;

        assert_eq!(items.iter().filter(|item| item.is_err()).count(), 1);
        let text: String = items
            .iter()
            .filter_map(|item| item.as_ref().ok())
            .filter_map(|chunk| chunk.text.clone())
            .collect();
        assert_eq!(text, "partial", "断流前收到的文本应先到");
        let err = items
            .iter()
            .find_map(|item| item.as_ref().err())
            .expect("流里应有一个错误");
        assert!(err.to_string().contains("流中断"), "{err}");
        assert_eq!(model.requests().len(), 1);
    }

    /// 请求映射（brief 钉死形状）：assistant 带 `tool_calls`、tool 带 `tool_call_id`，空字段不出现。
    #[test]
    fn request_body_maps_tool_calls_and_tool_results() {
        let messages = vec![
            message(Role::User, "读 a.txt"),
            ChatMessage {
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "fs_read".into(),
                    args: json!({"path":"a.txt"}),
                }],
            },
            ChatMessage {
                role: Role::Tool,
                content: "a.txt".into(),
                tool_call_id: Some("call_1".into()),
                tool_calls: vec![],
            },
        ];
        let body = request_body("m", &messages, &tools(), "sys");

        assert_eq!(
            body["messages"][0],
            json!({"role":"system","content":"sys"})
        );
        assert_eq!(
            body["messages"][2],
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "fs_read", "arguments": "{\"path\":\"a.txt\"}"},
                }],
            })
        );
        assert_eq!(
            body["messages"][3],
            json!({"role":"tool","content":"a.txt","tool_call_id":"call_1"})
        );
        // 没有工具时省略 tools：部分兼容层拒绝空数组
        assert!(request_body("m", &[], &[], "sys").get("tools").is_none());
    }
}
