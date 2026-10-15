//! 测试替身：runtime 自身测试与下游 crate（feature `test-support`）共用。
//!
//! 构造器一律返回 `Arc<Self>`，便于直接强制转换为 `Arc<dyn ToolExecutor>` /
//! `Arc<dyn Runtime>` / `Arc<dyn ApprovalHandler>`。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{
    ApprovalDecision, ApprovalHandler, ApprovalRequest, BoxStream, Capabilities, ChatMessage,
    RunRequest, Runtime, RuntimeEvent, ToolCall, ToolExecutor, ToolResult, ToolSpec,
};

/// 脚本化工具执行器：按序弹出预置结果，并记录收到的每次调用。
pub struct MockExecutor {
    results: Mutex<VecDeque<ToolResult>>,
    calls: Mutex<Vec<ToolCall>>,
}

impl MockExecutor {
    pub fn new(results: Vec<ToolResult>) -> Arc<Self> {
        Arc::new(Self {
            results: Mutex::new(results.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// 到目前为止收到的全部工具调用。
    pub fn calls(&self) -> Vec<ToolCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ToolExecutor for MockExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "echo".into(),
            description: "Echo the given text back (test double)".into(),
            parameters: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            }),
        }]
    }

    /// 复刻 core::tools 的摘要文案（Task 9 锁定格式），便于断言 ToolExec 事件。
    fn summarize(&self, call: &ToolCall) -> String {
        let path = call
            .args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let arg = |key: &str| call.args.get(key).and_then(|v| v.as_str()).unwrap_or("");
        match call.name.as_str() {
            "fs_read" => format!("Reading {path}"),
            "fs_list" => format!("Listing {path}"),
            "fs_write" => format!("Writing {path}"),
            "fs_edit" => format!("Editing {path}"),
            "shell_run" => format!("Running: {}", arg("command")),
            "http_fetch" => format!("Fetching {}", arg("url")),
            other => format!("Running {other}"),
        }
    }

    async fn execute(&self, call: ToolCall, _cancel: CancellationToken) -> ToolResult {
        self.calls.lock().unwrap().push(call);
        // 结果队列耗尽时返回失败结果，避免测试悄悄拿到假成功
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ToolResult {
                ok: false,
                output: Some("mock executor: no scripted result left".into()),
            })
    }
}

/// 记录一次运行请求的关键字段，供断言内核看到的上下文。
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedRequest {
    pub system_prompt: String,
    pub messages: Vec<ChatMessage>,
}

/// 脚本化内核：按序回放事件，并记录每次 `run` 收到的请求。
pub struct MockRuntime {
    events: Vec<RuntimeEvent>,
    requests: Mutex<Vec<RecordedRequest>>,
}

impl MockRuntime {
    pub fn new(events: Vec<RuntimeEvent>) -> Arc<Self> {
        Arc::new(Self {
            events,
            requests: Mutex::new(Vec::new()),
        })
    }

    /// 到目前为止收到的全部运行请求。
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Runtime for MockRuntime {
    fn id(&self) -> &'static str {
        "mock"
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
        _cancel: CancellationToken,
    ) -> BoxStream<'static, RuntimeEvent> {
        let RunRequest {
            messages,
            system_prompt,
            ..
        } = req;
        self.requests.lock().unwrap().push(RecordedRequest {
            system_prompt,
            messages,
        });
        Box::pin(futures::stream::iter(self.events.clone()))
    }
}

/// 脚本化审批：按序弹出决定并记录收到的请求；决策队列耗尽时返回 `Reject`（安全默认）。
pub struct ScriptedApproval {
    decisions: Mutex<VecDeque<ApprovalDecision>>,
    seen: Mutex<Vec<ApprovalRequest>>,
}

impl ScriptedApproval {
    pub fn new(decisions: Vec<ApprovalDecision>) -> Arc<Self> {
        Arc::new(Self {
            decisions: Mutex::new(decisions.into()),
            seen: Mutex::new(Vec::new()),
        })
    }

    /// 到目前为止收到的全部审批请求。
    pub fn seen(&self) -> Vec<ApprovalRequest> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl ApprovalHandler for ScriptedApproval {
    async fn decide(&self, req: ApprovalRequest) -> ApprovalDecision {
        self.seen.lock().unwrap().push(req);
        self.decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ApprovalDecision::Reject)
    }
}
