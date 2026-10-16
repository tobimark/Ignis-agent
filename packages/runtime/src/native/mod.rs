//! native 内核：从零实现的执行循环——多轮工具调用、用量记账、上限与中止。
//!
//! 事件序列契约（Task 12 的 TaskRunner 按之消费）：模型请求逐 `TextDelta` 下发；本轮有工具调用时
//! 按 index 顺序串行 `ToolExec` / `ToolResult`；随后进入下一轮；**恰好一条 `Done` 收束整条流**。
//! 所有异常路径都折成 `Done`：取消 → `Aborted`；步数 / 墙钟 / token → `LimitReached` + 对应标记；
//! 模型或协议错误 → `Failed` + 人话。上限标记是与 Task 12 / 验收脚本约定的字符串，勿改。

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{StreamExt as _, stream};
use tokio::sync::mpsc::{Sender, channel};
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;

use crate::openai::OpenAiClient;
use crate::openai::assembler::ToolCallAssembler;
use crate::{
    BoxStream, Capabilities, ChatMessage, Role, RunRequest, RunStatus, Runtime, RuntimeEvent,
    ToolCall, ToolSpec, Usage,
};

/// 事件通道容量（brief 钉死 64）：消费者慢只会让循环在发送处等待，不会无界缓冲。
const EVENT_CHANNEL: usize = 64;
/// 步数耗尽（`Limits::max_steps`）。
const ERR_MAX_STEPS: &str = "max_steps";
/// 任务墙钟到期（`Limits::task_timeout_secs`）。**跨内核的契约字符串**：codex 等外置内核适配器
/// 与验收脚本按逐字比较判定超时，改字即改协议（故与 `MAX_TASK_TIMEOUT` 一样提为 `pub(crate)`
/// 供各内核复用，不许各自复制字面量）。
pub(crate) const ERR_TASK_TIMEOUT: &str = "task_timeout";
/// token 预算耗尽（`Limits::token_budget`，按累计 in+out 计）。
const ERR_TOKEN_BUDGET: &str = "token_budget";
/// 墙钟上限的兜底：`Instant + Duration` 溢出会 panic，而 panic 会吞掉唯一的 `Done`；
/// 超过一年的超时按一年算——对一次任务而言与「不限时」等价。外置内核适配器复用同一兜底。
pub(crate) const MAX_TASK_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// native 内核：注入 OpenAI 兼容客户端，按统一 `Runtime` 协议跑执行循环。
///
/// 模型与端点随 [`NativeRuntime::new`] 的客户端固定；请求里的 `model` 字段由上层保证一致。
pub struct NativeRuntime {
    /// 循环跑在 `tokio::spawn` 出去的任务里（事件流是 `'static`），故用 `Arc` 持有。
    client: Arc<OpenAiClient>,
}

impl NativeRuntime {
    pub fn new(client: OpenAiClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }
}

#[async_trait::async_trait]
impl Runtime for NativeRuntime {
    fn id(&self) -> &'static str {
        "native"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            approvals: true,
            usage: true,
        }
    }

    async fn run(
        &self,
        req: RunRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, RuntimeEvent> {
        let (tx, rx) = channel(EVENT_CHANNEL);
        let client = Arc::clone(&self.client);
        tokio::spawn(async move {
            // 终局事件只发一次：`drive` 返回的 `Some` 一定是 `Done`，`None` 表示接收端已断开
            if let Some(event) = drive(&client, req, cancel, &tx).await {
                let _ = tx.send(event).await;
            }
        });
        Box::pin(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        }))
    }
}

/// 执行循环本体：返回唯一的终局 `Done`；`None` 表示事件接收端已断开（连 `Done` 都不必发）。
///
/// 结构（brief 钉死）：`loop { 预检上限 → 组装请求 → 流式消费（竞速取消/墙钟）→ 有工具调用则
/// 串行执行并回填 → 否则收尾 }`。
async fn drive(
    client: &OpenAiClient,
    request: RunRequest,
    cancel: CancellationToken,
    tx: &Sender<RuntimeEvent>,
) -> Option<RuntimeEvent> {
    let RunRequest {
        mut messages,
        system_prompt,
        limits,
        executor,
        ..
    } = request;
    let specs = executor.specs();
    let deadline =
        Instant::now() + Duration::from_secs(limits.task_timeout_secs).min(MAX_TASK_TIMEOUT);
    let timer = sleep_until(deadline.into());
    tokio::pin!(timer);
    let mut steps: u32 = 0;
    let mut used = Usage::default();

    loop {
        // 每轮请求前的预检。取消优先：用户主动停止不该报成上限。token 按累计 in+out 计，超预算
        // 就到此为止（末轮跑完才发现超限的，由下面的轮末复核收口）。
        if cancel.is_cancelled() {
            return Some(done(RunStatus::Aborted, None, None));
        }
        steps += 1;
        if steps > limits.max_steps {
            return Some(done(
                RunStatus::LimitReached,
                None,
                Some(ERR_MAX_STEPS.into()),
            ));
        }
        if used.input_tokens + used.output_tokens > limits.token_budget {
            return Some(done(
                RunStatus::LimitReached,
                None,
                Some(ERR_TOKEN_BUDGET.into()),
            ));
        }
        if Instant::now() >= deadline {
            return Some(done(
                RunStatus::LimitReached,
                None,
                Some(ERR_TASK_TIMEOUT.into()),
            ));
        }

        let history = trim_history(&messages, limits.context_char_budget);
        let mut round = tokio::select! {
            _ = cancel.cancelled() => Round::Cancelled,
            () = &mut timer => Round::TimedOut,
            round = fetch_round(client, &history, &specs, &system_prompt, tx) => round,
        };
        // 轮末复核：本地模型可能在一轮内同步吐完，取消/超时信号会落在「本轮已跑完」之后，
        // 不补这一刀就会把已中止的轮次报成成功。
        if matches!(round, Round::Completed { .. }) {
            if cancel.is_cancelled() {
                round = Round::Cancelled;
            } else if Instant::now() >= deadline {
                round = Round::TimedOut;
            }
        }

        match round {
            Round::Cancelled => return Some(done(RunStatus::Aborted, None, None)),
            Round::TimedOut => {
                return Some(done(
                    RunStatus::LimitReached,
                    None,
                    Some(ERR_TASK_TIMEOUT.into()),
                ));
            }
            Round::Detached => return None,
            Round::Failed(error) => return Some(done(RunStatus::Failed, None, Some(error))),
            Round::Completed { text, usage, calls } => {
                if let Some(usage) = usage {
                    used.input_tokens += usage.input_tokens;
                    used.output_tokens += usage.output_tokens;
                    if !emit(
                        tx,
                        RuntimeEvent::Usage {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            estimated: usage.estimated,
                        },
                    )
                    .await
                    {
                        return None;
                    }
                }
                // 轮末复核预算：循环顶部的预检只在「还要发起下一轮」时才走到，末轮的超额用量
                // 必须在这里收口，否则这一轮的超限会被下面的 `Success` 吞掉（`Usage` 已先发）。
                if used.input_tokens + used.output_tokens > limits.token_budget {
                    return Some(done(
                        RunStatus::LimitReached,
                        None,
                        Some(ERR_TOKEN_BUDGET.into()),
                    ));
                }
                // 没有工具调用 = 模型给出最终答复，本轮即终局
                if calls.is_empty() {
                    return Some(done(RunStatus::Success, Some(text), None));
                }
                // 回填顺序（brief 钉死）：一条 assistant(tool_calls)，其后每个结果一条 tool 消息
                messages.push(ChatMessage {
                    role: Role::Assistant,
                    content: text,
                    tool_call_id: None,
                    tool_calls: calls.clone(),
                });
                for call in calls {
                    // 取消后不再发起新的工具调用；正在跑的工具由 executor 自己收尸（Task 7/8）
                    if cancel.is_cancelled() {
                        return Some(done(RunStatus::Aborted, None, None));
                    }
                    let summary = executor.summarize(&call);
                    if !emit(
                        tx,
                        RuntimeEvent::ToolExec {
                            name: call.name.clone(),
                            summary,
                            args: call.args.clone(),
                        },
                    )
                    .await
                    {
                        return None;
                    }
                    let result = executor.execute(call.clone(), cancel.clone()).await;
                    if !emit(
                        tx,
                        RuntimeEvent::ToolResult {
                            name: call.name.clone(),
                            ok: result.ok,
                            output: result.output.clone(),
                        },
                    )
                    .await
                    {
                        return None;
                    }
                    messages.push(ChatMessage {
                        role: Role::Tool,
                        content: result.output.unwrap_or_default(),
                        tool_call_id: Some(call.id),
                        tool_calls: vec![],
                    });
                }
            }
        }
    }
}

/// 发起一轮请求并消费整条流：文本即时出事件，工具调用分片交给 assembler 收口。
async fn fetch_round(
    client: &OpenAiClient,
    history: &[ChatMessage],
    specs: &[ToolSpec],
    system_prompt: &str,
    tx: &Sender<RuntimeEvent>,
) -> Round {
    let mut stream = match client.stream_chat(history, specs, system_prompt).await {
        Ok(stream) => stream,
        Err(err) => return Round::Failed(err.to_string()),
    };
    let mut text = String::new();
    let mut usage = None;
    let mut assembler = ToolCallAssembler::new();
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(err) => return Round::Failed(err.to_string()),
        };
        if let Some(delta) = chunk.text {
            text.push_str(&delta);
            if !emit(tx, RuntimeEvent::TextDelta { text: delta }).await {
                return Round::Detached;
            }
        }
        for delta in &chunk.tool_call_deltas {
            assembler.push_delta(
                delta.index,
                delta.id.as_deref(),
                delta.name.as_deref(),
                delta.args_delta.as_deref(),
            );
        }
        if chunk.usage.is_some() {
            usage = chunk.usage;
        }
    }
    match assembler.finish() {
        Ok(calls) => Round::Completed { text, usage, calls },
        Err(err) => Round::Failed(err.to_string()),
    }
}

/// 一轮请求的结果。
enum Round {
    /// 本轮正常结束：累计文本、本轮用量、组装好的工具调用（已按 index 升序）。
    Completed {
        text: String,
        usage: Option<Usage>,
        calls: Vec<ToolCall>,
    },
    /// 模型 / 传输 / 协议错误（`LlmError` 的 `Display` 已是人话）。
    Failed(String),
    /// 用户取消。
    Cancelled,
    /// 墙钟到点。
    TimedOut,
    /// 事件接收端已断开（流被丢弃）：循环收摊，不留后台任务。
    Detached,
}

/// 唯一的终局事件：所有退出路径都经这里取 `Done`，由 [`Runtime::run`] 的任务发一次。
fn done(status: RunStatus, summary: Option<String>, error: Option<String>) -> RuntimeEvent {
    RuntimeEvent::Done {
        status,
        summary,
        error,
    }
}

/// 发一个事件；`false` 表示接收端已断开（循环随即收摊）。
///
/// 发送后让出一次执行权，事件消费者（以及按下取消的人）才有机会插进来：本地模型可能在一轮内
/// 同步吐完所有增量，没有让出点的话，「收到首个 `TextDelta` 后取消」只能等整轮结束才生效。
async fn emit(tx: &Sender<RuntimeEvent>, event: RuntimeEvent) -> bool {
    if tx.send(event).await.is_err() {
        return false;
    }
    tokio::task::yield_now().await;
    true
}

/// 按字符预算裁剪历史：超预算时从最旧整段丢弃「turn」（`user` 起到下一个 `user` 前），
/// assistant 的 `tool_calls` 与对应 `tool` 结果因此同进同出。system 提示不在这里（走
/// [`RunRequest::system_prompt`]），消息开头若有非 `user` 消息，跟着第一段走。
///
/// 预算只统计消息正文（`content`）字符数。最新一轮永不丢弃：单轮就超预算时宁可超出，
/// 也不把用户当次提问挡在模型之外。
pub fn trim_history(messages: &[ChatMessage], char_budget: usize) -> Vec<ChatMessage> {
    let mut starts = vec![0];
    starts.extend(
        messages
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, message)| message.role == Role::User)
            .map(|(index, _)| index),
    );
    let mut keep_from = 0;
    while keep_from + 1 < starts.len() && char_count(&messages[starts[keep_from]..]) > char_budget {
        keep_from += 1;
    }
    messages[starts[keep_from]..].to_vec()
}

/// 消息正文的字符数（历史预算的计量单位）。
fn char_count(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|message| message.content.chars().count())
        .sum()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures::StreamExt;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use crate::openai::OpenAiClient;
    use crate::test_support::{MockExecutor, ScriptStep, ScriptedModel, ScriptedModelHandle};
    use crate::{
        Capabilities, ChatMessage, Limits, ModelConfig, Role, RunRequest, RunStatus, Runtime,
        RuntimeEvent, ToolCall, ToolExecutor, ToolResult,
    };

    use super::{NativeRuntime, trim_history};

    /// 指向脚本化假模型的客户端（退避压到 1ms，测试不睡真时间）。
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

    /// 运行请求：默认上限 + 一条用户消息；用例按需改字段。
    fn request(model: &ScriptedModelHandle, executor: Arc<dyn ToolExecutor>) -> RunRequest {
        RunRequest {
            messages: vec![message(Role::User, "hi")],
            system_prompt: "you are ignis".into(),
            model: ModelConfig {
                base_url: model.base_url(),
                model: "scripted-model".into(),
                ..ModelConfig::default()
            },
            limits: Limits::default(),
            executor,
        }
    }

    /// 跑一次到底，收齐事件。
    async fn collect_events(
        runtime: &NativeRuntime,
        req: RunRequest,
        cancel: CancellationToken,
    ) -> Vec<RuntimeEvent> {
        runtime.run(req, cancel).await.collect().await
    }

    /// 保留集合里每条 `tool` 结果是否都有配对的 assistant `tool_calls`（「不孤立」的判据）。
    fn tool_results_are_paired(messages: &[ChatMessage]) -> bool {
        messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .all(|tool| {
                messages.iter().any(|holder| {
                    holder.role == Role::Assistant
                        && holder
                            .tool_calls
                            .iter()
                            .any(|call| Some(&call.id) == tool.tool_call_id.as_ref())
                })
            })
    }

    /// 内核标识与能力声明（brief 钉死）。
    #[test]
    fn native_runtime_declares_identity_and_capabilities() {
        let model = ScriptedModel::start(vec![]);
        let runtime = NativeRuntime::new(client(&model));
        assert_eq!(runtime.id(), "native");
        assert_eq!(
            runtime.capabilities(),
            Capabilities {
                tools: true,
                approvals: true,
                usage: true
            }
        );
    }

    /// brief 钉死：纯文本一轮跑完 → 逐 `TextDelta` + 恰好一条 `Done{Success, summary=="hello"}`。
    #[tokio::test]
    async fn text_only_run_finishes_with_summary() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("hello".into())]);
        let executor: Arc<dyn ToolExecutor> = MockExecutor::new(vec![]);
        let runtime = NativeRuntime::new(client(&model));

        let events = collect_events(
            &runtime,
            request(&model, executor),
            CancellationToken::new(),
        )
        .await;

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::Done { .. }))
                .count(),
            1,
            "{events:?}"
        );
        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Done {
                status: RunStatus::Success,
                summary: Some("hello".into()),
                error: None,
            })
        );
        // 假模型固定上报 12/3，用量事件紧挨着 Done 之前
        assert_eq!(
            events[events.len() - 2],
            RuntimeEvent::Usage {
                input_tokens: 12,
                output_tokens: 3,
                estimated: false
            }
        );
        assert_eq!(model.requests().len(), 1);
    }

    /// brief 钉死：工具调用真被执行、结果回填给模型；事件按 index 顺序串行出 `ToolExec`/`ToolResult`。
    #[tokio::test]
    async fn tool_call_is_executed_and_result_fed_back() {
        let model = ScriptedModel::start(vec![
            ScriptStep::ToolCalls {
                text: "看一下".into(),
                calls: vec![("fs_list".into(), json!({}))],
            },
            ScriptStep::Text("done".into()),
        ]);
        let mock = MockExecutor::new(vec![ToolResult {
            ok: true,
            output: Some("a.txt".into()),
        }]);
        let executor: Arc<dyn ToolExecutor> = mock.clone();

        let events = collect_events(
            &NativeRuntime::new(client(&model)),
            request(&model, executor),
            CancellationToken::new(),
        )
        .await;

        // 事件：工具调用前是文本增量，ToolExec 带 executor 的摘要，ToolResult 带 ok 与输出
        let exec_index = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::ToolExec { .. }))
            .expect("应有 ToolExec 事件");
        assert_eq!(
            events[exec_index],
            RuntimeEvent::ToolExec {
                name: "fs_list".into(),
                summary: "Listing .".into(),
                args: json!({}),
            }
        );
        assert_eq!(
            events[exec_index + 1],
            RuntimeEvent::ToolResult {
                name: "fs_list".into(),
                ok: true,
                output: Some("a.txt".into()),
            }
        );
        assert_eq!(
            mock.calls(),
            vec![ToolCall {
                id: "call_1".into(),
                name: "fs_list".into(),
                args: json!({}),
            }]
        );

        // 回填：第 2 个请求里 assistant(tool_calls) 紧跟一条 tool 消息
        let requests = model.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        let messages = requests[1]["messages"]
            .as_array()
            .expect("messages 应为数组");
        let assistant = messages
            .iter()
            .position(|message| message["role"] == "assistant")
            .expect("应有 assistant 消息");
        assert_eq!(messages[assistant]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            messages[assistant]["tool_calls"][0]["function"]["name"],
            "fs_list"
        );
        assert_eq!(messages[assistant + 1]["role"], "tool");
        assert_eq!(messages[assistant + 1]["content"], "a.txt");
        assert_eq!(messages[assistant + 1]["tool_call_id"], "call_1");

        // 末轮文本收尾 → 恰好一条 Done{Success}
        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Done {
                status: RunStatus::Success,
                summary: Some("done".into()),
                error: None,
            })
        );
    }

    /// 一轮里两个工具调用：事件按 index 顺序串行（`ToolExec(a)`→`ToolResult(a)`→`ToolExec(b)`→
    /// `ToolResult(b)`），下一轮请求带回一条挂两个 id 的 assistant 与两条同序的 `tool` 消息。
    #[tokio::test]
    async fn one_round_with_two_tool_calls_executes_serially_and_feeds_back_both() {
        let model = ScriptedModel::start(vec![
            ScriptStep::ToolCalls {
                text: "查两个".into(),
                calls: vec![
                    ("fs_list".into(), json!({})),
                    ("fs_read".into(), json!({"path": "notes.md"})),
                ],
            },
            ScriptStep::Text("done".into()),
        ]);
        let mock = MockExecutor::new(vec![
            ToolResult {
                ok: true,
                output: Some("a.txt".into()),
            },
            ToolResult {
                ok: true,
                output: Some("note".into()),
            },
        ]);
        let executor: Arc<dyn ToolExecutor> = mock.clone();

        let events = collect_events(
            &NativeRuntime::new(client(&model)),
            request(&model, executor),
            CancellationToken::new(),
        )
        .await;

        // 事件：两条调用各出一对 ToolExec / ToolResult，严格交替 = 串行（a 收工后 b 才发起）
        let tools: Vec<&RuntimeEvent> = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::ToolExec { .. } | RuntimeEvent::ToolResult { .. }
                )
            })
            .collect();
        assert_eq!(tools.len(), 4, "{events:?}");
        assert_eq!(
            tools[0],
            &RuntimeEvent::ToolExec {
                name: "fs_list".into(),
                summary: "Listing .".into(),
                args: json!({}),
            }
        );
        assert_eq!(
            tools[1],
            &RuntimeEvent::ToolResult {
                name: "fs_list".into(),
                ok: true,
                output: Some("a.txt".into()),
            }
        );
        assert_eq!(
            tools[2],
            &RuntimeEvent::ToolExec {
                name: "fs_read".into(),
                summary: "Reading notes.md".into(),
                args: json!({"path": "notes.md"}),
            }
        );
        assert_eq!(
            tools[3],
            &RuntimeEvent::ToolResult {
                name: "fs_read".into(),
                ok: true,
                output: Some("note".into()),
            }
        );
        // executor 按 index 顺序收到两个调用（id 由假模型按 index 生成）
        assert_eq!(
            mock.calls(),
            vec![
                ToolCall {
                    id: "call_1".into(),
                    name: "fs_list".into(),
                    args: json!({}),
                },
                ToolCall {
                    id: "call_2".into(),
                    name: "fs_read".into(),
                    args: json!({"path": "notes.md"}),
                },
            ]
        );

        // 回填：第 2 个请求里 assistant 消息带 BOTH id，其后紧跟两条 `tool` 消息，顺序一致
        let requests = model.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        let messages = requests[1]["messages"]
            .as_array()
            .expect("messages 应为数组");
        let assistant = messages
            .iter()
            .position(|message| message["role"] == "assistant")
            .expect("应有 assistant 消息");
        let tool_calls = messages[assistant]["tool_calls"]
            .as_array()
            .expect("assistant 应带 tool_calls");
        let ids: Vec<&str> = tool_calls
            .iter()
            .map(|call| call["id"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["call_1", "call_2"], "{tool_calls:?}");
        assert_eq!(messages[assistant + 1]["role"], "tool");
        assert_eq!(messages[assistant + 1]["tool_call_id"], "call_1");
        assert_eq!(messages[assistant + 1]["content"], "a.txt");
        assert_eq!(messages[assistant + 2]["role"], "tool");
        assert_eq!(messages[assistant + 2]["tool_call_id"], "call_2");
        assert_eq!(messages[assistant + 2]["content"], "note");

        // 末轮文本收尾 → 恰好一条 Done{Success}
        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Done {
                status: RunStatus::Success,
                summary: Some("done".into()),
                error: None,
            })
        );
    }

    /// brief 钉死：步数用尽 → `LimitReached("max_steps")`，不再执行第 2 轮的工具。
    #[tokio::test]
    async fn step_limit_stops_loop() {
        let tool_calls = || ScriptStep::ToolCalls {
            text: String::new(),
            calls: vec![("fs_list".into(), json!({}))],
        };
        let model = ScriptedModel::start(vec![tool_calls(), tool_calls()]);
        let mock = MockExecutor::new(vec![ToolResult {
            ok: true,
            output: Some("a.txt".into()),
        }]);
        let executor: Arc<dyn ToolExecutor> = mock.clone();
        let mut req = request(&model, executor);
        req.limits.max_steps = 1;

        let events = collect_events(
            &NativeRuntime::new(client(&model)),
            req,
            CancellationToken::new(),
        )
        .await;

        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Done {
                status: RunStatus::LimitReached,
                summary: None,
                error: Some("max_steps".into()),
            })
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::Done { .. }))
                .count(),
            1
        );
        assert_eq!(mock.calls().len(), 1, "第 2 轮不得再发起工具调用");
        assert_eq!(model.requests().len(), 1, "步数用尽后不得再请求模型");
    }

    /// brief 钉死：收到首个 `TextDelta` 后取消 → `Done{Aborted}`。
    #[tokio::test]
    async fn cancel_token_aborts_mid_stream() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("hello".into())]);
        let executor: Arc<dyn ToolExecutor> = MockExecutor::new(vec![]);
        let cancel = CancellationToken::new();
        let mut stream = NativeRuntime::new(client(&model))
            .run(request(&model, executor), cancel.clone())
            .await;

        let first = stream.next().await.expect("应至少有一个事件");
        assert!(matches!(first, RuntimeEvent::TextDelta { .. }), "{first:?}");
        cancel.cancel();
        let rest: Vec<RuntimeEvent> = stream.collect().await;

        assert!(
            rest.iter().any(|event| matches!(
                event,
                RuntimeEvent::Done {
                    status: RunStatus::Aborted,
                    ..
                }
            )),
            "{rest:?}"
        );
        assert!(
            !rest.iter().any(|event| matches!(
                event,
                RuntimeEvent::Done {
                    status: RunStatus::Success,
                    ..
                }
            )),
            "取消后不得报成功：{rest:?}"
        );
        assert_eq!(
            rest.iter()
                .filter(|event| matches!(event, RuntimeEvent::Done { .. }))
                .count(),
            1,
            "{rest:?}"
        );
    }

    /// 取消发生在首轮请求之前：直接 `Done{Aborted}`，不碰模型。
    #[tokio::test]
    async fn cancel_before_first_request_aborts() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("hello".into())]);
        let executor: Arc<dyn ToolExecutor> = MockExecutor::new(vec![]);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let events = collect_events(
            &NativeRuntime::new(client(&model)),
            request(&model, executor),
            cancel,
        )
        .await;

        assert_eq!(
            events,
            vec![RuntimeEvent::Done {
                status: RunStatus::Aborted,
                summary: None,
                error: None,
            }]
        );
        assert!(model.requests().is_empty(), "取消后不得发起请求");
    }

    /// 墙钟上限：到点即停（`task_timeout_secs=0` 时预检就挡下请求），标记为 `task_timeout`。
    #[tokio::test]
    async fn task_timeout_stops_before_request() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("hello".into())]);
        let executor: Arc<dyn ToolExecutor> = MockExecutor::new(vec![]);
        let mut req = request(&model, executor);
        req.limits.task_timeout_secs = 0;

        let events = collect_events(
            &NativeRuntime::new(client(&model)),
            req,
            CancellationToken::new(),
        )
        .await;

        assert_eq!(
            events,
            vec![RuntimeEvent::Done {
                status: RunStatus::LimitReached,
                summary: None,
                error: Some("task_timeout".into()),
            }]
        );
        assert!(model.requests().is_empty(), "超时预检应挡在请求之前");
    }

    /// 墙钟在轮中途到点（brief 钉死）：整轮与 `sleep_until` 竞速，收成 `task_timeout`。
    ///
    /// 用「收下请求就断连」的假模型把首轮拖住：客户端按 500ms 起退避重试，重试次数用尽要 3.5s，
    /// 而墙钟 1s 就到——到点的一定是墙钟（真坏了也只会是「失败」而不是挂死）。
    #[tokio::test]
    async fn task_timeout_aborts_mid_round() {
        let model = ScriptedModel::start(vec![
            ScriptStep::DropConnection,
            ScriptStep::DropConnection,
            ScriptStep::DropConnection,
            ScriptStep::DropConnection,
        ]);
        let executor: Arc<dyn ToolExecutor> = MockExecutor::new(vec![]);
        // 故意用默认（500ms 起）退避与默认重试次数，让首轮一定跑不过墙钟
        let cfg = ModelConfig {
            base_url: model.base_url(),
            model: "scripted-model".into(),
            ..ModelConfig::default()
        };
        let client = OpenAiClient::new(&cfg, "test-key".into()).unwrap();
        let mut req = request(&model, executor);
        req.limits.task_timeout_secs = 1;

        let events =
            collect_events(&NativeRuntime::new(client), req, CancellationToken::new()).await;

        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Done {
                status: RunStatus::LimitReached,
                summary: None,
                error: Some("task_timeout".into()),
            })
        );
        assert!(!model.requests().is_empty(), "首轮请求应已发出");
    }

    /// token 预算在**末轮**被突破（单轮即超）：`Usage` 先到，随后是 `LimitReached("token_budget")`，
    /// 绝不把超限的一轮报成 `Success`。
    ///
    /// 这是「预算只在请求前预检」时漏掉的那条路径：单轮跑完就收工，循环顶部再也走不到。
    #[tokio::test]
    async fn token_budget_exceeded_in_final_round_is_limit_reached() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("hello".into())]);
        let executor: Arc<dyn ToolExecutor> = MockExecutor::new(vec![]);
        let mut req = request(&model, executor);
        req.limits.token_budget = 10; // 假模型每轮上报 12 + 3 = 15

        let events = collect_events(
            &NativeRuntime::new(client(&model)),
            req,
            CancellationToken::new(),
        )
        .await;

        let usage_index = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::Usage { .. }))
            .expect("应有 Usage 事件");
        let Some(RuntimeEvent::Done {
            status,
            summary,
            error,
        }) = events.last()
        else {
            panic!("末事件应为 Done：{events:?}");
        };
        assert_eq!(*status, RunStatus::LimitReached, "{events:?}");
        assert!(summary.is_none(), "{summary:?}");
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("token_budget")),
            "{error:?}"
        );
        assert!(
            usage_index < events.len() - 1,
            "Usage 事件应先于超限 Done：{events:?}"
        );
        assert_eq!(model.requests().len(), 1);
    }

    /// 模型错误（401）：`Done{Failed}` 带人话错误，不静默、不重试。
    #[tokio::test]
    async fn model_error_finishes_as_failed_with_human_message() {
        let model = ScriptedModel::start(vec![ScriptStep::Error {
            status: 401,
            body: "{\"error\":\"bad key\"}".into(),
        }]);
        let executor: Arc<dyn ToolExecutor> = MockExecutor::new(vec![]);

        let events = collect_events(
            &NativeRuntime::new(client(&model)),
            request(&model, executor),
            CancellationToken::new(),
        )
        .await;

        let Some(RuntimeEvent::Done {
            status,
            summary,
            error,
        }) = events.last()
        else {
            panic!("末事件应为 Done：{events:?}");
        };
        assert_eq!(*status, RunStatus::Failed);
        assert!(summary.is_none());
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("模型鉴权失败")),
            "{error:?}"
        );
    }

    /// brief 钉死：超预算时从最旧整段丢弃 turn（user 起到下一个 user 前），不拆散工具结果。
    #[test]
    fn trim_history_drops_oldest_turns_intact() {
        let call = ToolCall {
            id: "call_1".into(),
            name: "fs_list".into(),
            args: json!({}),
        };
        let messages = vec![
            // 第 1 轮：最旧，应被整段丢弃
            message(Role::User, "u1"),      // 2 字符
            message(Role::Assistant, "a1"), // 2
            // 第 2 轮：含工具调用与结果，必须同生共死
            message(Role::User, "u2"), // 2
            ChatMessage {
                role: Role::Assistant,
                content: "a2".into(),
                tool_call_id: None,
                tool_calls: vec![call.clone()],
            }, // 2
            ChatMessage {
                role: Role::Tool,
                content: "r2".into(),
                tool_call_id: Some(call.id.clone()),
                tool_calls: vec![],
            }, // 2
            // 第 3 轮：最新
            message(Role::User, "u3"),      // 2
            message(Role::Assistant, "a3"), // 2
        ];

        // 共 14 字符：预算 10 时正好丢掉最旧一轮，留下最新两轮
        let kept = trim_history(&messages, 10);
        assert_eq!(kept, messages[2..].to_vec());
        assert_eq!(kept.len(), 5, "{kept:?}");

        // 预算够大：原样返回
        assert_eq!(trim_history(&messages, 14), messages);

        // 预算再小也不丢最新一轮（否则模型连当次提问都看不到）
        assert_eq!(trim_history(&messages, 1), messages[5..].to_vec());

        // 工具结果不孤立：保留集合里每条 tool 消息都有配对的 assistant(tool_calls)
        assert!(tool_results_are_paired(&kept));
        assert!(tool_results_are_paired(&trim_history(&messages, 6)));
        // 判据本身有判别力：只剩 tool 结果、配对消息被拆掉的切片会被判为孤立
        assert!(!tool_results_are_paired(&messages[4..5]));
        assert_eq!(messages[4].role, Role::Tool);
    }
}
