//! Ignis Runtime 契约：消息、工具、审批与执行内核共用的类型和 trait。
//!
//! [`RuntimeEvent`] 的 serde 形状即任务日志（JSONL）的格式，属于跨模块契约，不得随意改动。
//! [`openai`] 是 native 内核用的 OpenAI 兼容模型客户端，其形状同样属于跨模块契约。
//! [`native`] 是 native 内核的执行循环，其事件序列与上限语义同样是跨模块契约。
//! [`codex`] 是 codex CLI 兼容内核，其事件映射逐条对照探测记录
//! （`docs/spikes/2026-10-15-external-kernels-probe.md`）；[`claude`] 是 claude CLI 兼容内核，
//! 同一份探测记录里也有它的小节。[`cli_process`] 是两个外部 CLI 内核共用的内部进程/事件基础设施，
//! [`render`] 则是它们共用的 canonical 会话渲染（两者都不属于对外契约）。

use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub mod claude;
mod cli_process;
pub mod codex;
pub mod native;
pub mod openai;
pub mod render;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

/// 内核事件流：`Runtime::run` 的返回值。
pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

/// 对话消息的角色（OpenAI 兼容命名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 一条对话消息：`tool_call_id` 仅 `Tool` 角色使用，`tool_calls` 仅 `Assistant` 角色使用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

/// 暴露给模型的工具描述（对应 OpenAI `tools[].function`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// 模型请求的一次工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// 工具执行结果；`output` 为 `None` 表示无输出文本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub ok: bool,
    pub output: Option<String>,
}

/// 用量记账：`estimated` 标记外部内核的估算值。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated: bool,
}

/// 工具调用的风险等级（小写序列化：`safe` / `elevated` / `dangerous`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Safe,
    Elevated,
    Dangerous,
}

/// 发起审批时交给审批方的上下文。
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub tool: String,
    pub summary: String,
    pub args: serde_json::Value,
    pub risk: RiskLevel,
}

/// 审批决定（snake_case 序列化：`allow_once` / `allow_session` / `reject`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Reject,
}

/// 审批回调：交互式 CLI 与脚本化测试提供实现。
#[async_trait::async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn decide(&self, req: ApprovalRequest) -> ApprovalDecision;
}

/// 工具执行器：core 实现（含审批与记录），随 [`RunRequest`] 注入给内核循环。
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    fn summarize(&self, call: &ToolCall) -> String;
    async fn execute(&self, call: ToolCall, cancel: CancellationToken) -> ToolResult;
}

/// 运行结束时的事件状态；序列化字符串与数据库 `tasks.status` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Success,
    Failed,
    Aborted,
    LimitReached,
}

impl RunStatus {
    /// 跨模块契约字符串：`tasks.status`、`job_runs.status` 与 JSONL 的 `done.status` 共用同一份
    /// 取值（与 [`RunStatus`] 的 serde 小写名逐字一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Success => "success",
            RunStatus::Failed => "failed",
            RunStatus::Aborted => "aborted",
            RunStatus::LimitReached => "limit_reached",
        }
    }
}

/// 内核事件：JSONL 任务日志直接序列化本类型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    TextDelta {
        text: String,
    },
    ToolExec {
        name: String,
        summary: String,
        args: serde_json::Value,
    },
    ToolResult {
        name: String,
        ok: bool,
        output: Option<String>,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        estimated: bool,
    },
    Done {
        status: RunStatus,
        summary: Option<String>,
        error: Option<String>,
    },
}

/// 内核能力声明：外置内核可能缺工具注入或用量记账。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub tools: bool,
    pub approvals: bool,
    pub usage: bool,
}

/// 模型协议；v0.1 只有 OpenAI 兼容协议。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelProtocol {
    #[default]
    OpenAi,
}

/// 模型接入配置，字段可逐个缺省。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub protocol: ModelProtocol,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            protocol: ModelProtocol::OpenAi,
        }
    }
}

/// 运行上限，字段可逐个缺省。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub max_steps: u32,
    pub task_timeout_secs: u64,
    pub token_budget: u64,
    pub max_retries: u32,
    pub shell_timeout_secs: u64,
    pub tool_output_limit: usize,
    pub http_max_bytes: usize,
    pub context_char_budget: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 30,
            task_timeout_secs: 900,
            token_budget: 100_000,
            max_retries: 3,
            shell_timeout_secs: 120,
            tool_output_limit: 65_536,
            http_max_bytes: 1_048_576,
            context_char_budget: 200_000,
        }
    }
}

/// 一次运行的输入；持有 `Arc<dyn ToolExecutor>`，故不派生 `Clone`/`Debug`。
pub struct RunRequest {
    pub messages: Vec<ChatMessage>,
    pub system_prompt: String,
    pub model: ModelConfig,
    pub limits: Limits,
    pub executor: Arc<dyn ToolExecutor>,
}

/// 执行内核：native / codex / claude 实现同一事件协议。
#[async_trait::async_trait]
pub trait Runtime: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    async fn run(
        &self,
        req: RunRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, RuntimeEvent>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt;
    use serde_json::json;

    use super::*;
    use crate::test_support::{MockExecutor, MockRuntime, RecordedRequest, ScriptedApproval};

    #[test]
    fn runtime_event_json_shape_is_locked() {
        let ev = RuntimeEvent::TextDelta { text: "hi".into() };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({"type":"text_delta","text":"hi"})
        );
        let ev = RuntimeEvent::ToolExec {
            name: "fs_read".into(),
            summary: "Reading a.txt".into(),
            args: json!({"path":"a.txt"}),
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({"type":"tool_exec","name":"fs_read","summary":"Reading a.txt","args":{"path":"a.txt"}})
        );
    }

    /// 四个终态的契约字符串逐个钉住（`tasks.status` / `job_runs.status` / JSONL 共用）。
    #[test]
    fn run_status_as_str_pins_the_contract_strings() {
        assert_eq!(RunStatus::Success.as_str(), "success");
        assert_eq!(RunStatus::Failed.as_str(), "failed");
        assert_eq!(RunStatus::Aborted.as_str(), "aborted");
        assert_eq!(RunStatus::LimitReached.as_str(), "limit_reached");
    }

    #[test]
    fn limits_defaults_are_locked() {
        let l = Limits::default();
        assert_eq!(
            (
                l.max_steps,
                l.task_timeout_secs,
                l.token_budget,
                l.max_retries
            ),
            (30, 900, 100_000, 3)
        );
        assert_eq!(
            (
                l.shell_timeout_secs,
                l.tool_output_limit,
                l.http_max_bytes,
                l.context_char_budget
            ),
            (120, 65_536, 1_048_576, 200_000)
        );
    }

    #[test]
    fn model_config_roundtrips_with_default_api_key_env() {
        let cfg: ModelConfig =
            serde_json::from_value(json!({"base_url":"https://x/v1","model":"m"})).unwrap();
        assert_eq!(cfg.api_key_env, "OPENAI_API_KEY");
        assert_eq!(cfg.protocol, ModelProtocol::OpenAi);
    }

    #[tokio::test]
    async fn mocks_satisfy_object_safe_traits() {
        let exec: Arc<dyn ToolExecutor> = MockExecutor::new(vec![]);
        assert_eq!(exec.specs().len(), 1);
        let rt: Arc<dyn Runtime> = MockRuntime::new(vec![RuntimeEvent::Done {
            status: RunStatus::Success,
            summary: None,
            error: None,
        }]);
        assert_eq!(rt.id(), "mock");
    }

    #[tokio::test]
    async fn mock_executor_returns_scripted_results_and_records_calls() {
        let exec = MockExecutor::new(vec![ToolResult {
            ok: true,
            output: Some("a.txt".into()),
        }]);
        let call = ToolCall {
            id: "c1".into(),
            name: "fs_list".into(),
            args: json!({}),
        };
        let result = exec.execute(call.clone(), CancellationToken::new()).await;
        assert!(result.ok);
        assert_eq!(result.output.as_deref(), Some("a.txt"));
        assert_eq!(exec.calls(), vec![call]);
        // 队列耗尽后的兜底结果应为失败，避免测试悄悄拿到假成功
        let exhausted = exec
            .execute(
                ToolCall {
                    id: "c2".into(),
                    name: "fs_list".into(),
                    args: json!({}),
                },
                CancellationToken::new(),
            )
            .await;
        assert!(!exhausted.ok);
    }

    #[tokio::test]
    async fn mock_runtime_replays_events_and_records_request() {
        let events = vec![
            RuntimeEvent::TextDelta { text: "hi".into() },
            RuntimeEvent::Done {
                status: RunStatus::Success,
                summary: Some("hi".into()),
                error: None,
            },
        ];
        let rt = MockRuntime::new(events.clone());
        let req = RunRequest {
            messages: vec![ChatMessage {
                role: Role::User,
                content: "ping".into(),
                tool_call_id: None,
                tool_calls: vec![],
            }],
            system_prompt: "sys".into(),
            model: ModelConfig::default(),
            limits: Limits::default(),
            executor: MockExecutor::new(vec![]),
        };
        let replayed: Vec<RuntimeEvent> =
            rt.run(req, CancellationToken::new()).await.collect().await;
        assert_eq!(replayed, events);
        assert_eq!(
            rt.requests(),
            vec![RecordedRequest {
                system_prompt: "sys".into(),
                messages: vec![ChatMessage {
                    role: Role::User,
                    content: "ping".into(),
                    tool_call_id: None,
                    tool_calls: vec![],
                }],
            }]
        );
    }

    #[tokio::test]
    async fn scripted_approval_pops_decisions_and_rejects_when_exhausted() {
        let approval = ScriptedApproval::new(vec![ApprovalDecision::AllowOnce]);
        let request = |tool: &str| ApprovalRequest {
            tool: tool.into(),
            summary: format!("Running {tool}"),
            args: json!({"path": "a.txt"}),
            risk: RiskLevel::Elevated,
        };
        assert_eq!(
            approval.decide(request("fs_write")).await,
            ApprovalDecision::AllowOnce
        );
        // 决策队列耗尽 → 安全默认拒绝
        assert_eq!(
            approval.decide(request("fs_write")).await,
            ApprovalDecision::Reject
        );
        assert_eq!(approval.seen().len(), 2);
    }
}
