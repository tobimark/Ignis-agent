//! 任务编排：上下文组装、JSONL 任务日志、记账与脱敏。
//!
//! [`TaskRunner::run`] 的一次运行：生成 task id → 写 `tasks` 行（`running`）与 JSONL 首行
//! `task_start` → 用 [`ToolRuntime`] 与 [`prompts::build_system_prompt`] 组装 [`RunRequest`]
//! → 消费内核事件（每行带 `ts` 且整行过 [`Redactor`] 后落盘，同时转发给 sink、累加用量）
//! → 终态时 `finish_task` 并补 JSONL `done` 行。

pub mod prompts;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use ignis_runtime::{
    ApprovalHandler, ChatMessage, Role, RunRequest, RunStatus, Runtime, RuntimeEvent, ToolExecutor,
    Usage,
};
use ignis_storage::{Db, NewTask, StorageError, TaskLog};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::security::{Policy, Redactor};
use crate::tools::{ToolRuntime, default_registry};

/// 任务来源：`ignis run` 的一次性运行，或 REPL 的一轮输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Run,
    Repl,
}

/// `tasks.kind` 与 JSONL `task_start.kind` 的取值。
fn kind_str(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Run => "run",
        TaskKind::Repl => "repl",
    }
}

/// `tasks.status` 与 JSONL `done.status` 的取值：跨模块契约字符串，逐个显式映射。
fn status_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Success => "success",
        RunStatus::Failed => "failed",
        RunStatus::Aborted => "aborted",
        RunStatus::LimitReached => "limit_reached",
    }
}

/// 一次任务的输入。
pub struct TaskRequest {
    /// 本轮用户输入。
    pub prompt: String,
    /// 已有对话上下文（REPL 的历史轮次）；一次性运行传空。
    pub history: Vec<ChatMessage>,
    pub kind: TaskKind,
    /// 工具操作的项目根目录。
    pub workspace: PathBuf,
}

/// 一次任务的结果；`messages` 即下一轮应当带上的上下文。
pub struct TaskOutcome {
    pub id: String,
    pub status: RunStatus,
    /// 仅成功时有值，取内核 `Done.summary`。
    pub final_text: Option<String>,
    pub usage: Usage,
    pub messages: Vec<ChatMessage>,
}

/// 任务编排的失败原因；内核自身的失败是 `Done{Failed}` 事件，不走这里。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// 任务行或 JSONL 日志的读写失败。
    #[error("任务存储失败：{0}")]
    Storage(#[from] StorageError),
    /// 事件与日志行的 JSON 互转失败。
    #[error("任务日志序列化失败：{0}")]
    Json(#[from] serde_json::Error),
}

/// 任务编排器：把「请求 + 内核 + 策略」跑成一条可查询、可回放的任务记录。
pub struct TaskRunner {
    db: Arc<Db>,
    /// 生效配置：模型、上限与（环境变量覆盖后的）密钥。
    config: Arc<Config>,
    runtime: Arc<dyn Runtime>,
    policy: Arc<Policy>,
    approvals: Arc<dyn ApprovalHandler>,
    log_dir: PathBuf,
    /// 任务日志与服务层的脱敏器；工具执行器另持一份（[`Redactor`] 不派生 `Clone`）。
    redactor: Redactor,
}

impl TaskRunner {
    /// 装配运行器：依赖全部由调用方注入，便于替换内核与审批方。
    pub fn new(
        db: Arc<Db>,
        config: Arc<Config>,
        runtime: Arc<dyn Runtime>,
        policy: Arc<Policy>,
        approvals: Arc<dyn ApprovalHandler>,
        log_dir: PathBuf,
    ) -> Self {
        Self {
            db,
            redactor: Redactor::new(config.api_key.iter().cloned().collect()),
            config,
            runtime,
            policy,
            approvals,
            log_dir,
        }
    }

    /// 跑一次任务。
    ///
    /// 返回 `Err` 只表示记账/落盘这类基础设施失败；内核自身的失败（含取消、触限）都是
    /// `Done` 事件，照常收尾并返回 [`TaskOutcome`]。
    pub async fn run(
        &self,
        req: TaskRequest,
        sink: UnboundedSender<RuntimeEvent>,
        cancel: CancellationToken,
    ) -> Result<TaskOutcome, CoreError> {
        let task_id = uuid::Uuid::now_v7().to_string();
        let kernel = self.runtime.id();
        let workspace = req.workspace.clone();
        // 日志目录先就位：建不出来就不留下一条永远 running 的任务行
        let log = TaskLog::new(&self.log_dir)?;

        self.db.insert_task(&NewTask {
            id: task_id.clone(),
            kind: kind_str(req.kind).to_string(),
            prompt: req.prompt.clone(),
            kernel: kernel.to_string(),
            workspace: workspace.display().to_string(),
            started_at: now_millis(),
        })?;
        self.append_jsonl(
            &log,
            &task_id,
            json!({
                "type": "task_start",
                "task_id": task_id,
                "kind": kind_str(req.kind),
                "kernel": kernel,
                "workspace": workspace.display().to_string(),
            }),
        )?;

        // 上下文：历史轮次 + 本轮输入；工具执行器按本次任务（task_id/工作区/上限）装配
        let mut messages = req.history;
        messages.push(chat_message(Role::User, req.prompt.clone()));

        let executor = Arc::new(ToolRuntime::new(
            default_registry(),
            Arc::clone(&self.policy),
            Arc::clone(&self.approvals),
            Arc::clone(&self.db),
            task_id.clone(),
            workspace.clone(),
            self.config.limits,
            Redactor::new(self.config.api_key.iter().cloned().collect()),
        ));
        let request = RunRequest {
            messages: messages.clone(),
            system_prompt: prompts::build_system_prompt(&workspace, &executor.specs()),
            model: self.config.model.clone(),
            limits: self.config.limits,
            executor,
        };

        // 事件循环：落盘 → 转发给调用方 → 记账，全程保持内核事件顺序
        let mut usage = Usage::default();
        let mut done = None;
        let mut stream = self.runtime.run(request, cancel).await;
        while let Some(event) = stream.next().await {
            self.append_jsonl(&log, &task_id, serde_json::to_value(&event)?)?;
            let _ = sink.send(event.clone());

            match &event {
                RuntimeEvent::Usage {
                    input_tokens,
                    output_tokens,
                    estimated,
                } => {
                    // 饱和累加：异常内核的上报不该把进程打崩
                    usage.input_tokens = usage.input_tokens.saturating_add(*input_tokens);
                    usage.output_tokens = usage.output_tokens.saturating_add(*output_tokens);
                    usage.estimated |= *estimated;
                }
                RuntimeEvent::Done {
                    status,
                    summary,
                    error,
                } => done = Some((*status, summary.clone(), error.clone())),
                _ => {}
            }
        }

        // 收尾：内核没给 `Done`（实现缺陷）也按失败记，任务不得停在 running
        let (status, summary, error) = done.unwrap_or((
            RunStatus::Failed,
            None,
            Some("内核事件流未以 done 收尾，按失败处理".to_string()),
        ));
        // 只有成功才把 `Done.summary` 当最终答复交回调用方
        let final_text = match status {
            RunStatus::Success => summary.clone(),
            _ => None,
        };
        self.db.finish_task(
            &task_id,
            status_str(status),
            summary.as_deref(),
            error.as_deref(),
            usage.input_tokens,
            usage.output_tokens,
            usage.estimated,
            now_millis(),
        )?;
        self.append_jsonl(
            &log,
            &task_id,
            json!({
                "type": "done",
                "task_id": task_id,
                "status": status_str(status),
                "summary": summary,
                "error": error,
            }),
        )?;

        // P1 约定：跨轮上下文只带最终答复，不带内核内部的工具调用细节
        if let Some(text) = &final_text {
            messages.push(chat_message(Role::Assistant, text.clone()));
        }
        Ok(TaskOutcome {
            id: task_id,
            status,
            final_text,
            usage,
            messages,
        })
    }

    /// 落一行 JSONL。
    ///
    /// 先把整个对象（含 `ts`）序列化、再过一遍 [`Redactor`]，最后解析回 `Value` 交给
    /// [`TaskLog`]（写出的字节与脱敏后的整行一致）。必须是「整行」：`ToolExecutor::summarize`
    /// 不做脱敏，它的文案会原样出现在 `ToolExec.summary` 里，整行脱敏是密钥不落盘的唯一拦截点。
    fn append_jsonl(
        &self,
        log: &TaskLog,
        task_id: &str,
        mut record: Value,
    ) -> Result<(), CoreError> {
        record["ts"] = json!(now_rfc3339());
        let line = self.redactor.redact(&serde_json::to_string(&record)?);
        log.append(task_id, &serde_json::from_str(&line)?)?;
        Ok(())
    }
}

/// 组装一条纯文本消息（工具调用字段留空）。
fn chat_message(role: Role, content: impl Into<String>) -> ChatMessage {
    ChatMessage {
        role,
        content: content.into(),
        tool_call_id: None,
        tool_calls: Vec::new(),
    }
}

/// JSONL 行的 `ts`：RFC 3339（`ignis task show` 靠它渲染时间）。
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// epoch 毫秒；时钟早于 epoch（人工设置过系统时间）时退化成 0，保证任务行仍能写入。
fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_runtime::test_support::{MockRuntime, ScriptedApproval};
    use ignis_runtime::{ChatMessage, Role, RunStatus, RuntimeEvent, Usage};
    use ignis_storage::Db;
    use serde_json::{Value, json};
    use tokio::sync::mpsc::{self, UnboundedReceiver};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::security::ApprovalMode;

    /// 单个测试独占的临时目录：工作区、日志与真实 DB 文件都在里面，析构时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-core-task-{}-{label}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 测试环境：临时工作区 + 真实 DB 文件 + 脚本化内核 + 组装好的运行器。
    struct Env {
        dir: TempDir,
        db: Arc<Db>,
        runtime: Arc<MockRuntime>,
        runner: TaskRunner,
        log_dir: PathBuf,
    }

    impl Env {
        fn new(label: &str, events: Vec<RuntimeEvent>, api_key: Option<&str>) -> Self {
            let dir = TempDir::new(label);
            let log_dir = dir.path().join("logs");
            let db = Arc::new(Db::open(&dir.path().join("ignis.db")).unwrap());
            let runtime = MockRuntime::new(events);
            let config = Arc::new(Config {
                workspace_root: dir.path().to_path_buf(),
                api_key: api_key.map(str::to_string),
                ..Config::default()
            });
            let runner = TaskRunner::new(
                Arc::clone(&db),
                config,
                Arc::clone(&runtime) as Arc<dyn Runtime>,
                // 这些用例的内核都不调用工具执行器：策略与审批方只要求「可实现」
                Arc::new(Policy::new(ApprovalMode::Preauthorized {
                    unsafe_ops: false,
                })),
                ScriptedApproval::new(Vec::new()),
                log_dir.clone(),
            );
            Self {
                dir,
                db,
                runtime,
                runner,
                log_dir,
            }
        }

        /// 一条一次性运行请求：工作区就是临时目录，无历史轮次。
        fn request(&self, prompt: &str) -> TaskRequest {
            TaskRequest {
                prompt: prompt.into(),
                history: Vec::new(),
                kind: TaskKind::Run,
                workspace: self.dir.path().to_path_buf(),
            }
        }

        /// 跑一次任务，返回结果、sink 转发的事件与落盘行。
        async fn run(&self, req: TaskRequest) -> Report {
            let (sink, received) = mpsc::unbounded_channel();
            let outcome = self
                .runner
                .run(req, sink, CancellationToken::new())
                .await
                .expect("基础设施正常时 run 不应返回 Err");
            Report {
                outcome,
                forwarded: drain(received),
                lines: self.log_lines(),
            }
        }

        /// 本次运行的日志文件：目录里恰好一个 `.jsonl`。
        fn log_path(&self) -> PathBuf {
            let mut files: Vec<PathBuf> = fs::read_dir(&self.log_dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .collect();
            assert_eq!(files.len(), 1, "每次运行应恰好写一个日志文件：{files:?}");
            files.pop().unwrap()
        }

        /// 日志原文（用于「密钥是否落盘」这类逐字断言）。
        fn log_raw(&self) -> String {
            fs::read_to_string(self.log_path()).unwrap()
        }

        /// 日志行：每行一个 JSON 对象。
        fn log_lines(&self) -> Vec<Value> {
            self.log_raw()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    /// 一次运行的观察结果。
    struct Report {
        outcome: TaskOutcome,
        /// sink 转发给调用方的事件。
        forwarded: Vec<RuntimeEvent>,
        /// 落盘的 JSONL 行。
        lines: Vec<Value>,
    }

    impl Report {
        /// 日志行的 `type` 序列。
        fn types(&self) -> Vec<&str> {
            self.lines
                .iter()
                .map(|line| line["type"].as_str().unwrap())
                .collect()
        }

        /// 末行：运行器补的收尾 `done` 行（比内核 `Done` 事件多一个 `task_id`）。
        fn done_line(&self) -> &Value {
            self.lines.last().unwrap()
        }

        /// 倒数第二行：内核 `Done` 事件原样落盘的那一行。
        fn kernel_done_line(&self) -> &Value {
            &self.lines[self.lines.len() - 2]
        }
    }

    /// `run` 返回后发送端已析构，队列里的事件可一次读空。
    fn drain(mut received: UnboundedReceiver<RuntimeEvent>) -> Vec<RuntimeEvent> {
        let mut events = Vec::new();
        while let Ok(event) = received.try_recv() {
            events.push(event);
        }
        events
    }

    fn message(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    fn user(content: &str) -> ChatMessage {
        message(Role::User, content)
    }

    fn assistant(content: &str) -> ChatMessage {
        message(Role::Assistant, content)
    }

    fn success(summary: Option<&str>) -> RuntimeEvent {
        RuntimeEvent::Done {
            status: RunStatus::Success,
            summary: summary.map(str::to_string),
            error: None,
        }
    }

    #[tokio::test]
    async fn persists_task_row_and_events_in_order() {
        let events = vec![
            RuntimeEvent::TextDelta { text: "Hel".into() },
            RuntimeEvent::TextDelta { text: "lo".into() },
            RuntimeEvent::ToolExec {
                name: "fs_read".into(),
                summary: "Reading a.txt".into(),
                args: json!({"path": "a.txt"}),
            },
            RuntimeEvent::ToolResult {
                name: "fs_read".into(),
                ok: true,
                output: Some("contents".into()),
            },
            RuntimeEvent::Usage {
                input_tokens: 10,
                output_tokens: 5,
                estimated: false,
            },
            success(Some("done")),
        ];
        let env = Env::new("success", events.clone(), None);

        // 历史轮次应该原样带进上下文
        let mut req = env.request("do it");
        req.history = vec![user("first"), assistant("first answer")];
        let report = env.run(req).await;

        // TaskOutcome：id 即任务行主键与日志文件名；成功时 final_text 是 Done.summary
        assert_eq!(report.outcome.status, RunStatus::Success);
        assert_eq!(report.outcome.final_text.as_deref(), Some("done"));
        assert_eq!(
            report.outcome.messages,
            vec![
                user("first"),
                assistant("first answer"),
                user("do it"),
                assistant("done"),
            ]
        );
        assert_eq!(
            report.outcome.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                estimated: false,
            }
        );
        assert_eq!(
            env.log_path().file_name().unwrap().to_str().unwrap(),
            format!("{}.jsonl", report.outcome.id)
        );

        // tasks 行：内核名、工作区、状态、摘要与用量都落到库里
        let row = env
            .db
            .get_task(&report.outcome.id)
            .unwrap()
            .expect("任务行");
        assert_eq!(row.status, "success");
        assert_eq!(row.kind, "run");
        assert_eq!(row.kernel, "mock");
        assert_eq!(row.prompt, "do it");
        assert_eq!(row.workspace, env.dir.path().display().to_string());
        assert_eq!(row.summary.as_deref(), Some("done"));
        assert_eq!(row.error, None);
        assert_eq!(
            (row.input_tokens, row.output_tokens, row.usage_estimated),
            (10, 5, false)
        );
        assert!(row.started_at > 0);
        assert!(row.ended_at.unwrap() >= row.started_at);

        // JSONL：task_start 打头；中间与内核事件一一对应（含内核的 `Done` 事件）；
        // 末行是运行器补的收尾 `done`
        assert_eq!(
            report.types(),
            [
                "task_start",
                "text_delta",
                "text_delta",
                "tool_exec",
                "tool_result",
                "usage",
                "done",
                "done",
            ]
        );
        assert_eq!(report.lines[0]["task_id"], json!(report.outcome.id));
        assert_eq!(report.lines[0]["kind"], json!("run"));
        assert_eq!(report.lines[0]["kernel"], json!("mock"));
        assert_eq!(
            report.lines[0]["workspace"],
            json!(env.dir.path().display().to_string())
        );
        assert_eq!(report.lines[1]["text"], json!("Hel"));
        assert_eq!(report.kernel_done_line()["summary"], json!("done"));
        assert!(report.kernel_done_line().get("task_id").is_none());
        assert_eq!(report.done_line()["task_id"], json!(report.outcome.id));
        assert_eq!(report.done_line()["status"], json!("success"));
        assert_eq!(report.done_line()["summary"], json!("done"));
        // 每行都带可解析的 RFC 3339 时间戳（`ignis task show` 靠它渲染 HH:MM:SS）
        for line in &report.lines {
            let ts = line["ts"].as_str().unwrap_or_default();
            assert!(chrono::DateTime::parse_from_rfc3339(ts).is_ok(), "{line}");
        }

        // sink 收到的事件与内核事件完全一致（含顺序）
        assert_eq!(report.forwarded, events);

        // 内核看到的上下文：历史 + 本轮输入；system prompt 点名工作区与工具
        let seen = env.runtime.requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].messages,
            vec![user("first"), assistant("first answer"), user("do it")]
        );
        assert!(
            seen[0]
                .system_prompt
                .contains(&env.dir.path().display().to_string()),
            "{}",
            seen[0].system_prompt
        );
        assert!(seen[0].system_prompt.contains("fs_read"));
    }

    #[tokio::test]
    async fn failed_run_records_error() {
        let env = Env::new(
            "failed",
            vec![
                RuntimeEvent::TextDelta {
                    text: "half".into(),
                },
                RuntimeEvent::Done {
                    status: RunStatus::Failed,
                    summary: Some("half".into()),
                    error: Some("boom".into()),
                },
            ],
            None,
        );

        let report = env.run(env.request("break it")).await;

        // 内核失败是 Done{Failed} 事件，不是 Err
        assert_eq!(report.outcome.status, RunStatus::Failed);
        // 失败不把半截答复当上下文
        assert_eq!(report.outcome.final_text, None);
        assert_eq!(report.outcome.messages, vec![user("break it")]);

        let row = env.db.get_task(&report.outcome.id).unwrap().unwrap();
        assert_eq!(row.status, "failed");
        assert!(
            row.error.as_deref().unwrap().contains("boom"),
            "{:?}",
            row.error
        );
        assert_eq!(row.summary.as_deref(), Some("half"));
        assert!(row.ended_at.is_some());

        assert_eq!(report.types(), ["task_start", "text_delta", "done", "done"]);
        assert_eq!(report.done_line()["task_id"], json!(report.outcome.id));
        assert_eq!(report.done_line()["status"], json!("failed"));
        assert!(
            report.done_line()["error"]
                .as_str()
                .unwrap()
                .contains("boom")
        );
    }

    #[tokio::test]
    async fn usage_sums_across_events() {
        let env = Env::new(
            "usage",
            vec![
                RuntimeEvent::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    estimated: false,
                },
                RuntimeEvent::Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    estimated: true,
                },
                success(Some("ok")),
            ],
            None,
        );

        let report = env.run(env.request("sum")).await;

        assert_eq!(
            report.outcome.usage,
            Usage {
                input_tokens: 11,
                output_tokens: 7,
                estimated: true,
            }
        );
        let row = env.db.get_task(&report.outcome.id).unwrap().unwrap();
        assert_eq!(
            (row.input_tokens, row.output_tokens, row.usage_estimated),
            (11, 7, true)
        );
    }

    #[tokio::test]
    async fn jsonl_is_redacted() {
        // `ToolExecutor::summarize()` 不做脱敏，摘要会原样进 `ToolExec.summary`：
        // 落盘前整行过 Redactor 是唯一的拦截点。
        let env = Env::new(
            "redacted",
            vec![
                RuntimeEvent::ToolExec {
                    name: "shell_run".into(),
                    summary: "Running: echo sk-test-secret".into(),
                    args: json!({"command": "echo sk-test-secret"}),
                },
                RuntimeEvent::ToolResult {
                    name: "shell_run".into(),
                    ok: true,
                    output: Some("sk-test-secret\n".into()),
                },
                success(Some("ok")),
            ],
            Some("sk-test-secret"),
        );

        let report = env.run(env.request("say the secret")).await;

        let raw = env.log_raw();
        assert!(!raw.contains("sk-test-secret"), "{raw}");
        assert!(raw.contains("***"), "{raw}");
        // 脱敏只替换密钥，行数与事件序列不受影响
        assert_eq!(
            report.types(),
            ["task_start", "tool_exec", "tool_result", "done", "done"]
        );
        assert_eq!(report.lines[1]["summary"], json!("Running: echo ***"));
        assert_eq!(report.lines[1]["args"]["command"], json!("echo ***"));
        assert_eq!(report.lines[2]["output"], json!("***\n"));
        assert_eq!(report.outcome.status, RunStatus::Success);
    }

    #[tokio::test]
    async fn terminal_statuses_are_written_verbatim() {
        // `tasks.status` 与 JSONL `done.status` 的字符串是跨模块契约，四个终态逐个钉住
        for (status, name) in [
            (RunStatus::Success, "success"),
            (RunStatus::Failed, "failed"),
            (RunStatus::Aborted, "aborted"),
            (RunStatus::LimitReached, "limit_reached"),
        ] {
            let env = Env::new(
                name,
                vec![RuntimeEvent::Done {
                    status,
                    summary: None,
                    error: None,
                }],
                None,
            );

            let report = env.run(env.request("run")).await;

            assert_eq!(report.outcome.status, status);
            let row = env.db.get_task(&report.outcome.id).unwrap().unwrap();
            assert_eq!(row.status, name);
            assert_eq!(report.done_line()["status"], json!(name));
        }
    }

    #[tokio::test]
    async fn stream_without_done_still_leaves_a_terminal_row() {
        // 内核没给 Done（实现缺陷）也要有终态，任务不能停在 running
        let env = Env::new(
            "no-done",
            vec![RuntimeEvent::TextDelta { text: "hi".into() }],
            None,
        );

        let report = env.run(env.request("hi")).await;

        assert_eq!(report.outcome.status, RunStatus::Failed);
        assert_eq!(report.types(), ["task_start", "text_delta", "done"]);
        assert_eq!(report.done_line()["status"], json!("failed"));
        let row = env.db.get_task(&report.outcome.id).unwrap().unwrap();
        assert_eq!(row.status, "failed");
        let error = row.error.as_deref().unwrap();
        assert!(error.contains("done"), "{error}");
        assert!(row.ended_at.is_some());
    }
}
