//! 任务编排：上下文组装、JSONL 任务日志、记账与脱敏。
//!
//! [`TaskRunner::run`] 的一次运行：生成 task id → 写 `tasks` 行（`running`）与 JSONL 首行
//! `task_start` → 向记忆层取已知事实（失败只警告，见 [`TaskRunner::run`] 的说明）、把内置工具与
//! 元工具组装成 [`ToolRuntime`]，连事实与技能目录一起交给 [`prompts::build_system_prompt`]
//! → 消费内核事件（每行带 `ts` 且整行过 [`Redactor`] 后落盘；转发给 sink 的是同一道脱敏后的
//! 副本，而用量累加、技能去重与工具计数读原始事件）→ 终态时 `finish_task` 并补 JSONL 收尾行
//! `task_end`（与首行 `task_start` 对称；内核自己的 `Done` 事件已落成一行 `done`，收尾行不能与
//! 它同名）。

pub mod prompts;

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use ignis_runtime::{
    ApprovalHandler, ChatMessage, Role, RunRequest, RunStatus, Runtime, RuntimeEvent, ToolExecutor,
    Usage,
};
use ignis_storage::{Db, NewTask, StorageError, TaskLog, now_millis};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::alerts::{AlertSink, Notifier};
use crate::config::Config;
use crate::memory::MemoryStore;
use crate::scheduler::JobProfile;
use crate::security::{Policy, Redactor};
use crate::skills::SkillCatalog;
use crate::tools::{
    ProfileExecutor, SchedulerDeps, ToolRuntime, available_tool_names, default_registry, meta_tools,
};

/// 任务来源：`ignis run` 的一次性运行、REPL 的一轮输入，或守护进程的调度运行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Run,
    Repl,
    /// 调度任务到期后由 `ignis-gateway` 的守护进程发起（T5）。
    Schedule,
}

/// `tasks.kind` 与 JSONL `task_start.kind` 的取值。
fn kind_str(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Run => "run",
        TaskKind::Repl => "repl",
        TaskKind::Schedule => "schedule",
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
    /// 来源调度任务 id：守护进程的调度运行填 `job.id`（透传给本轮工具的
    /// [`crate::tools::ToolCtx::job_id`]，`notify_user` 的 provenance 靠它）；交互运行为 `None`。
    pub job_id: Option<String>,
}

/// 一次任务的结果；`messages` 即下一轮应当带上的上下文。
pub struct TaskOutcome {
    pub id: String,
    pub status: RunStatus,
    /// 仅成功时有值，取内核 `Done.summary`。
    pub final_text: Option<String>,
    /// 内核给出的失败原因（`Done.error`，已过 [`Redactor`]）：命令层把它打到终端，
    /// `ignis task show` 打印的 `tasks.error` 是同一份文本；成功终态下内核不给原因，
    /// 值为 `None`。
    pub error: Option<String>,
    pub usage: Usage,
    pub messages: Vec<ChatMessage>,
    /// 本次任务真正加载过的技能：事件流里 `load_skill` 调用按首次出现顺序去重（重复加载只记一次）；
    /// 事件流没以 `Done` 收尾时只记已经看到的那些。命令层的结束行据此标注 `skills: …`。
    pub skills_used: Vec<String>,
    /// 本轮工具调用次数：事件流里 `RuntimeEvent::ToolExec` 的条数（同名工具的重复调用各算一次，
    /// `load_skill` 同样计入，`ToolResult` 不算）。事件流没以 `Done` 收尾时统计的也是已经看到的
    /// 那些；与 [`TaskOutcome::skills_used`] 一样是本轮统计，不落库，供命令层结束行标注 `N tools`。
    pub tool_calls: usize,
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
    /// 长期记忆：组装期注入已知事实，元工具 `remember` 往这里写。
    memory: Arc<MemoryStore>,
    /// 技能目录：组装期注入目录（渐进披露第一步），元工具 `load_skill` 读正文。
    skills: Arc<SkillCatalog>,
    /// 告警投递渠道：每轮任务的 `AlertSink` 用它投递，元工具 `notify_user` 的告警因此真的
    /// 送得出去（真实渠道由 T5 的 gateway 注入；CLI 侧暂时是空组合）。
    notifier: Arc<dyn Notifier>,
    /// job profile：`Some` 表示这是调度运行（P3 Task 5），本轮执行器会包一层
    /// [`ProfileExecutor`]，白名单外的工具对模型不可见、调用被拒；`None`（`ignis run` / REPL）
    /// 时行为与扩展前完全一致。
    profile: Option<JobProfile>,
}

impl TaskRunner {
    /// 装配运行器：依赖全部由调用方注入，便于替换内核与审批方。
    ///
    /// 参数较多是计划钉死的构造签名（P2 Task 5 在末尾追加了记忆与技能目录，P3 Task 3 追加了
    /// 告警渠道，P3 Task 5 追加了 job profile），不做参数对象包装。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Arc<Db>,
        config: Arc<Config>,
        runtime: Arc<dyn Runtime>,
        policy: Arc<Policy>,
        approvals: Arc<dyn ApprovalHandler>,
        log_dir: PathBuf,
        memory: Arc<MemoryStore>,
        skills: Arc<SkillCatalog>,
        notifier: Arc<dyn Notifier>,
        profile: Option<JobProfile>,
    ) -> Self {
        Self {
            db,
            redactor: Redactor::new(config.api_key.iter().cloned().collect()),
            config,
            runtime,
            policy,
            approvals,
            log_dir,
            memory,
            skills,
            notifier,
            profile,
        }
    }

    /// 跑一次任务。
    ///
    /// 返回 `Err` 只表示记账/落盘这类基础设施失败；内核自身的失败（含取消、触限）都是
    /// `Done` 事件，照常收尾并返回 [`TaskOutcome`]。
    ///
    /// 基础设施失败（含事件循环里的落盘失败）也会先把任务行收成 `failed` 再返回 `Err`
    /// （见 [`TaskRunner::fail_infrastructure`]）：任务行不允许停在 `running`。
    ///
    /// 组装期向记忆层取事实是 fail-soft 的：读不出来（表缺失、库损坏……）只往 stderr 打一行警告，
    /// 本轮不注入事实，任务照跑——记忆不是任务的前提，技能段与工具装配都不受它影响。
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
            // `tasks.prompt` 是 `ignis task show/list` 直接打印的一列：与 JSONL 同一道脱敏，
            // 否则用户把密钥粘进提示词就等于把它明文写进了库
            prompt: self.redactor.redact(&req.prompt),
            kernel: kernel.to_string(),
            workspace: workspace.display().to_string(),
            started_at: now_millis(),
        })?;

        // 任务行已落库：此后任何基础设施失败（含事件循环里的落盘失败）都要先把这一行收成
        // 终态，否则 `task list` 会一直显示 running。
        let mut usage = Usage::default();
        match self
            .execute(&log, &task_id, req, sink, cancel, &mut usage)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                self.fail_infrastructure(&log, &task_id, &error, usage);
                Err(error)
            }
        }
    }

    /// 任务行写入之后的一次运行：`task_start` → 消费内核事件（落盘 → 转发 → 记账）→
    /// `finish_task` + 收尾行 `task_end`。
    ///
    /// 与 [`TaskRunner::run`] 分开，是为了让「任务行写入之后的任何 `Err`」都被统一的兜底收尾
    /// （[`TaskRunner::fail_infrastructure`]）接住：这里每个 `?` 都不会漏掉那一行。
    async fn execute(
        &self,
        log: &TaskLog,
        task_id: &str,
        req: TaskRequest,
        sink: UnboundedSender<RuntimeEvent>,
        cancel: CancellationToken,
        usage: &mut Usage,
    ) -> Result<TaskOutcome, CoreError> {
        self.append_jsonl(
            log,
            task_id,
            json!({
                "type": "task_start",
                "task_id": task_id,
                "kind": kind_str(req.kind),
                "kernel": self.runtime.id(),
                "workspace": req.workspace.display().to_string(),
            }),
        )?;

        // 上下文：历史轮次 + 本轮输入；工具执行器按本次任务（task_id/工作区/上限）装配
        let workspace = req.workspace.clone();
        let mut messages = req.history;
        messages.push(chat_message(Role::User, req.prompt.clone()));

        // 组装期向记忆层要「这次任务该知道的事实」（本工作区的 project 事实 + 按 prompt 检索到的
        // user/global 事实）。任何失败都只是这一轮没有事实可注入：一行警告、空事实集，任务照跑。
        let facts = match self.memory.prompt_facts(&workspace, &req.prompt) {
            Ok(facts) => facts,
            Err(error) => {
                eprintln!("warning: 读取记忆失败，本次任务不注入已知事实：{error}");
                Vec::new()
            }
        };

        // 注册表 = 内置工具 + 元工具（remember 写记忆、load_skill 取技能正文、notify_user 升告警、
        // schedule_create 建调度任务）；顺序即模型看到的工具表顺序，元工具排在末尾
        let mut registry = default_registry();
        // 告警出口：告警行落库（CLI 未读收件箱即渠道①），投递走构造期注入的渠道组合——
        // 真实渠道由 T5 的 gateway 注入，这一层只管把渠道接上
        let alerts = Arc::new(AlertSink::new(
            Arc::clone(&self.db),
            Arc::clone(&self.notifier),
        ));
        // 调度任务创建：可用工具名清单与守护进程共用一份口径（工作区取本次任务的工作区）
        let scheduler = SchedulerDeps {
            db: Arc::clone(&self.db),
            available_tools: available_tool_names(),
        };
        for tool in meta_tools(
            Arc::clone(&self.memory),
            Arc::clone(&self.skills),
            alerts,
            scheduler,
        ) {
            registry.register(tool);
        }
        let tool_runtime = Arc::new(ToolRuntime::new(
            registry,
            Arc::clone(&self.policy),
            Arc::clone(&self.approvals),
            Arc::clone(&self.db),
            task_id.to_string(),
            workspace.clone(),
            self.config.limits,
            Redactor::new(self.config.api_key.iter().cloned().collect()),
            // 来源 job id 由守护进程的调度循环经 `TaskRequest` 透传；交互运行没有 job
            req.job_id.clone(),
        ));
        // 调度运行（`Some(profile)`）：job 的 profile 白名单与风险上限是执行边界，包在真实执行器
        // 外面——`executor.specs()`（工具表与 system prompt）已经过滤，白名单外的调用、以及
        // `risk=safe` 时超出上限的调用，都在本轮到达内层之前就被拒并留痕；`None`（`ignis run` /
        // REPL）原样交给内核，行为与扩展前一致。
        let executor: Arc<dyn ToolExecutor> = match &self.profile {
            Some(profile) => Arc::new(ProfileExecutor::new(
                tool_runtime,
                profile.tools.iter().cloned().collect(),
                profile.risk,
                Arc::clone(&self.db),
                task_id.to_string(),
            )),
            None => tool_runtime,
        };
        let request = RunRequest {
            messages: messages.clone(),
            system_prompt: prompts::build_system_prompt(
                &workspace,
                &executor.specs(),
                // 能力门控：工具段依内核能力二选一（外部内核的工具对 Ignis 不可用）
                !self.runtime.capabilities().tools,
                &facts,
                self.skills.metas(),
            ),
            model: self.config.model.clone(),
            limits: self.config.limits,
            executor,
        };

        // 事件循环：落盘 → 转发给调用方 → 记账，全程保持内核事件顺序
        let mut done = None;
        // 用过的技能：只认 `load_skill` 的调用，按首次出现顺序去重；无终态事件时也带着已看到的那些
        let mut skills_used: Vec<String> = Vec::new();
        // 工具调用计数：每个 `ToolExec` 事件算一次，无终态事件时也带着已看到的那些
        let mut tool_calls: usize = 0;
        let mut stream = self.runtime.run(request, cancel).await;
        while let Some(event) = stream.next().await {
            self.append_jsonl(log, task_id, serde_json::to_value(&event)?)?;
            // 转发的副本先过脱敏器（渲染面：CLI 终端 / 将来的 TUI）；下面的记账继续读原始事件
            let _ = sink.send(self.redact_for_sink(&event));

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
                // 每次 `ToolExec` 都计一次数：不看工具名、不看参数是否合法，也不去重
                RuntimeEvent::ToolExec { name, args, .. } => {
                    tool_calls += 1;
                    // 参数缺 `name` 或类型不是字符串的调用不算数：没读到任何一个技能
                    if name == "load_skill"
                        && let Some(skill) = args.get("name").and_then(Value::as_str)
                        && !skills_used.iter().any(|seen| seen == skill)
                    {
                        skills_used.push(skill.to_owned());
                    }
                }
                RuntimeEvent::Done {
                    status,
                    summary,
                    error,
                } => done = Some((*status, summary.clone(), error.clone())),
                _ => {}
            }
        }

        // 收尾：内核没给 `Done`（实现缺陷，例如内核任务 panic 掉了）也按失败记，任务不得停在 running
        let (status, summary, error) = done.unwrap_or((
            RunStatus::Failed,
            None,
            Some("内核事件流未以 done 收尾（内核任务可能 panic），按失败处理".to_string()),
        ));
        // 只有成功才把 `Done.summary` 当最终答复交回调用方
        let final_text = match status {
            RunStatus::Success => summary.clone(),
            _ => None,
        };
        // 落库与交回调用方的文本先过脱敏：`tasks.summary` / `tasks.error` 会被
        // `ignis task show` 原样打印，`TaskOutcome::error` 会被命令层直接打到终端。
        // JSONL 收尾行的整体脱敏不依赖这两份值，但同一份文本两次脱敏是幂等的。
        let summary = summary.map(|text| self.redactor.redact(&text));
        let error = error.map(|text| self.redactor.redact(&text));
        self.db.finish_task(
            task_id,
            status.as_str(),
            summary.as_deref(),
            error.as_deref(),
            usage.input_tokens,
            usage.output_tokens,
            usage.estimated,
            now_millis(),
        )?;
        // 收尾行与首行 `task_start` 对称。类型名不能取 `done`：那会与内核 `Done` 事件的落盘行
        // 同名，回放时被解析成第二个 `RuntimeEvent::Done`，终态与事件无从分辨。
        self.append_jsonl(
            log,
            task_id,
            json!({
                "type": "task_end",
                "task_id": task_id,
                "status": status.as_str(),
                "summary": summary,
                "error": error,
            }),
        )?;

        // P1 约定：跨轮上下文只带最终答复，不带内核内部的工具调用细节
        if let Some(text) = &final_text {
            messages.push(chat_message(Role::Assistant, text.clone()));
        }
        Ok(TaskOutcome {
            id: task_id.to_string(),
            status,
            final_text,
            error,
            usage: *usage,
            messages,
            skills_used,
            tool_calls,
        })
    }

    /// 基础设施中途失败时的兜底收尾：把已写入的任务行收成 `failed`，再补一条 `task_end`。
    ///
    /// 两步都忽略各自的失败——DB 或磁盘本身就是可能的故障源，调用方要的是原始 `Err`；唯一
    /// 不能接受的结果是任务行停在 `running`。
    fn fail_infrastructure(&self, log: &TaskLog, task_id: &str, error: &CoreError, usage: Usage) {
        // 与正常收尾同一道脱敏：这条消息会进 `tasks.error`，`ignis task show` 会原样打印
        let message = self.redactor.redact(&error.to_string());
        let _ = self.db.finish_task(
            task_id,
            RunStatus::Failed.as_str(),
            None,
            Some(&message),
            usage.input_tokens,
            usage.output_tokens,
            usage.estimated,
            now_millis(),
        );
        let _ = self.append_jsonl(
            log,
            task_id,
            json!({
                "type": "task_end",
                "task_id": task_id,
                "status": RunStatus::Failed.as_str(),
                "summary": Value::Null,
                "error": message,
            }),
        );
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

    /// 转发给 sink 的事件副本：整条事件过 [`Redactor`]（与 JSONL 落盘同一手法——
    /// `serde_json` 往返，递归替换每个字符串值里的密钥），CLI 渲染器等 sink 消费者不再可能
    /// 打印已知密钥（spec §2.3 的反目标）。
    ///
    /// 只改这一份副本：用量累加、技能去重与工具计数继续读原始事件，记账不受替换影响。
    fn redact_for_sink(&self, event: &RuntimeEvent) -> RuntimeEvent {
        let Ok(mut value) = serde_json::to_value(event) else {
            // 同一枚事件此前已在 `append_jsonl` 成功序列化过一次，这里理论上不可达；万一发生
            // 也转发原事件：宁可少一道替换，不能 panic，也不能把事件从 UI 流里吞掉。
            return event.clone();
        };
        redact_strings(&self.redactor, &mut value);
        match serde_json::from_value(value) {
            Ok(redacted) => redacted,
            Err(_) => {
                // 脱敏只替换字符串内容、不动事件形状，解析回来同样理论上不可达；回退与上
                // 一段相同：转发原事件、不 panic（落盘面另有 JSONL 整行脱敏兜底）。
                event.clone()
            }
        }
    }
}

/// 递归脱敏一个 JSON 值：只替换字符串叶子（对象键是事件字段名，不可能携带密钥），
/// 数组与对象的形状原样保留。
fn redact_strings(redactor: &Redactor, value: &mut Value) {
    match value {
        Value::String(text) => *text = redactor.redact(text),
        Value::Array(items) => {
            for item in items {
                redact_strings(redactor, item);
            }
        }
        Value::Object(fields) => {
            for field in fields.values_mut() {
                redact_strings(redactor, field);
            }
        }
        // 数字 / 布尔 / Null 不带可替换的文本
        Value::Number(_) | Value::Bool(_) | Value::Null => {}
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_runtime::native::NativeRuntime;
    use ignis_runtime::openai::OpenAiClient;
    use ignis_runtime::test_support::{MockRuntime, ScriptStep, ScriptedApproval, ScriptedModel};
    use ignis_runtime::{
        Capabilities, ChatMessage, ModelConfig, Role, RunStatus, RuntimeEvent, Usage,
    };
    use ignis_storage::Db;
    use serde_json::{Value, json};
    use tokio::sync::mpsc::{self, UnboundedReceiver};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::alerts::test_support::RecordingNotifier;
    use crate::memory::FactScope;
    use crate::scheduler::parse_profile;
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

    /// 脚本化内核的默认能力声明（native 形状：工具、审批、用量俱全）。
    fn default_capabilities() -> Capabilities {
        Capabilities {
            tools: true,
            approvals: true,
            usage: true,
        }
    }

    /// 测试环境：临时工作区 + 真实 DB 文件 + 脚本化内核 + 组装好的运行器。
    struct Env {
        dir: TempDir,
        db: Arc<Db>,
        runtime: Arc<MockRuntime>,
        /// 记忆层：事实写真实库，镜像写临时目录。
        memory: Arc<MemoryStore>,
        /// 技能目录：内置三技能 + 临时目录里的 `my-skill`。
        skills: Arc<SkillCatalog>,
        runner: TaskRunner,
        log_dir: PathBuf,
    }

    impl Env {
        fn new(label: &str, events: Vec<RuntimeEvent>, api_key: Option<&str>) -> Self {
            Self::with_profile(label, events, api_key, None)
        }

        /// 带 job profile 的环境：调度运行的 profile 白名单执行器靠它。
        fn with_profile(
            label: &str,
            events: Vec<RuntimeEvent>,
            api_key: Option<&str>,
            profile: Option<JobProfile>,
        ) -> Self {
            Self::build(label, events, api_key, profile, default_capabilities())
        }

        /// 带能力声明的环境：能力门控用例用它模拟 `tools: false` 的外部内核。
        fn with_capabilities(
            label: &str,
            events: Vec<RuntimeEvent>,
            capabilities: Capabilities,
        ) -> Self {
            Self::build(label, events, None, None, capabilities)
        }

        /// 三种构造共用的装配体。
        fn build(
            label: &str,
            events: Vec<RuntimeEvent>,
            api_key: Option<&str>,
            profile: Option<JobProfile>,
            capabilities: Capabilities,
        ) -> Self {
            let dir = TempDir::new(label);
            let log_dir = dir.path().join("logs");
            let db = Arc::new(Db::open(&dir.path().join("ignis.db")).unwrap());
            let runtime = MockRuntime::with_capabilities(events, capabilities);
            let config = Arc::new(Config {
                workspace_root: dir.path().to_path_buf(),
                api_key: api_key.map(str::to_string),
                ..Config::default()
            });
            // 一个用户技能：技能段里的 `(user)` 标注与描述都靠它断言（内置三技能随目录自带）
            let skills_dir = dir.path().join("skills");
            fs::create_dir_all(skills_dir.join("my-skill")).unwrap();
            fs::write(
                skills_dir.join("my-skill").join("SKILL.md"),
                "---\nname: my-skill\ndescription: Temporary skill for the task runner tests.\n\
                 ---\n# my-skill\n正文\n",
            )
            .unwrap();
            let memory = MemoryStore::new(Arc::clone(&db), dir.path().join("memory"));
            let skills = SkillCatalog::load(&skills_dir);
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
                Arc::clone(&memory),
                Arc::clone(&skills),
                // `MockRuntime` 只回放事件、不执行工具调用，这些用例不会真的投递告警；
                // 投递链路本身由 `notify_user_delivers_through_the_runner_injected_channel`
                // 用真内核 + 假模型端到端钉住
                Arc::new(RecordingNotifier::new()),
                profile,
            );
            Self {
                dir,
                db,
                runtime,
                memory,
                skills,
                runner,
                log_dir,
            }
        }

        /// 一条一次性运行请求：工作区就是临时目录，无历史轮次。
        fn request(&self, prompt: &str) -> TaskRequest {
            self.request_in(self.dir.path(), prompt)
        }

        /// 指定工作区的一条一次性运行请求。
        fn request_in(&self, workspace: &Path, prompt: &str) -> TaskRequest {
            TaskRequest {
                prompt: prompt.into(),
                history: Vec::new(),
                kind: TaskKind::Run,
                workspace: workspace.to_path_buf(),
                job_id: None,
            }
        }

        /// 记一条绑定给定工作区的 `project` 事实。
        fn remember_project(&self, workspace: &Path, key: &str, text: &str) {
            self.memory
                .remember(FactScope::Project, Some(workspace), key, text, None)
                .unwrap();
        }

        /// 内核每次请求看到的 system prompt（一次 `run` 对应一份）。
        fn system_prompts(&self) -> Vec<String> {
            self.runtime
                .requests()
                .into_iter()
                .map(|request| request.system_prompt)
                .collect()
        }

        /// 跑一次任务，返回结果、sink 转发的事件与落盘行。
        async fn run(&self, req: TaskRequest) -> Report {
            let (outcome, forwarded) = self
                .run_raw(req)
                .await
                .expect("基础设施正常时 run 不应返回 Err");
            Report {
                outcome,
                forwarded,
                lines: self.log_lines(),
            }
        }

        /// 跑一次任务，原样透传结果（用于断言基础设施失败路径）。
        async fn run_raw(
            &self,
            req: TaskRequest,
        ) -> Result<(TaskOutcome, Vec<RuntimeEvent>), CoreError> {
            let (sink, received) = mpsc::unbounded_channel();
            self.runner
                .run(req, sink, CancellationToken::new())
                .await
                .map(|outcome| (outcome, drain(received)))
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

        /// 末行：运行器补的收尾 `task_end` 行（比内核 `Done` 事件多一个 `task_id`）。
        fn task_end_line(&self) -> &Value {
            self.lines.last().unwrap()
        }

        /// 倒数第二行：内核 `Done` 事件原样落盘的那一行（`type:"done"`）。
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
                call_id: "call_1".into(),
            },
            RuntimeEvent::ToolResult {
                name: "fs_read".into(),
                ok: true,
                output: Some("contents".into()),
                call_id: "call_1".into(),
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
        // 成功没有失败原因可打印（命令层只对非成功状态输出 `error: …`）
        assert_eq!(report.outcome.error, None);
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
        // 末行是运行器补的收尾 `task_end`——不能也叫 `done`，否则会被回放成第二个终态事件
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
                "task_end",
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
        // 恰好一行 `done`：内核事件原样落盘，不带 `task_id`
        assert_eq!(report.kernel_done_line()["type"], json!("done"));
        assert_eq!(report.kernel_done_line()["summary"], json!("done"));
        assert!(report.kernel_done_line().get("task_id").is_none());
        assert_eq!(report.task_end_line()["type"], json!("task_end"));
        assert_eq!(report.task_end_line()["task_id"], json!(report.outcome.id));
        assert_eq!(report.task_end_line()["status"], json!("success"));
        assert_eq!(report.task_end_line()["summary"], json!("done"));
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
        // 失败原因随结果交回调用方（命令层据此打印 `error: …`），与落库的文本同源
        assert_eq!(report.outcome.error.as_deref(), Some("boom"));

        let row = env.db.get_task(&report.outcome.id).unwrap().unwrap();
        assert_eq!(row.status, "failed");
        assert!(
            row.error.as_deref().unwrap().contains("boom"),
            "{:?}",
            row.error
        );
        assert_eq!(row.summary.as_deref(), Some("half"));
        assert!(row.ended_at.is_some());

        assert_eq!(
            report.types(),
            ["task_start", "text_delta", "done", "task_end"]
        );
        assert_eq!(report.task_end_line()["task_id"], json!(report.outcome.id));
        assert_eq!(report.task_end_line()["status"], json!("failed"));
        assert!(
            report.task_end_line()["error"]
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

    /// `TaskOutcome::tool_calls`：本轮 `ToolExec` 事件数——同名工具的重复调用各算一次，
    /// 同一次调用的 `ToolResult` 不算（命令层的结束行据此标注 `2 tools`）。
    #[tokio::test]
    async fn tool_calls_counts_tool_exec_events() {
        let env = Env::new(
            "tool-calls",
            vec![
                tool_exec("fs_read", json!({ "path": "a.txt" })),
                RuntimeEvent::ToolResult {
                    name: "fs_read".into(),
                    ok: true,
                    output: Some("contents".into()),
                    call_id: "call_1".into(),
                },
                tool_exec("fs_read", json!({ "path": "b.txt" })),
                success(Some("done")),
            ],
            None,
        );

        let report = env.run(env.request("读两个文件")).await;

        assert_eq!(report.outcome.tool_calls, 2);
    }

    /// 纯文本一轮没有工具调用：计数为 0。
    #[tokio::test]
    async fn tool_calls_is_zero_for_a_text_only_run() {
        let env = Env::new(
            "tool-calls-zero",
            vec![
                RuntimeEvent::TextDelta {
                    text: "纯文本一轮".into(),
                },
                success(Some("done")),
            ],
            None,
        );

        let report = env.run(env.request("你好")).await;

        assert_eq!(report.outcome.tool_calls, 0);
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
                    call_id: "call_1".into(),
                },
                RuntimeEvent::ToolResult {
                    name: "shell_run".into(),
                    ok: true,
                    output: Some("sk-test-secret\n".into()),
                    call_id: "call_1".into(),
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
            ["task_start", "tool_exec", "tool_result", "done", "task_end"]
        );
        assert_eq!(report.lines[1]["summary"], json!("Running: echo ***"));
        assert_eq!(report.lines[1]["args"]["command"], json!("echo ***"));
        assert_eq!(report.lines[2]["output"], json!("***\n"));
        assert_eq!(report.outcome.status, RunStatus::Success);
    }

    /// F1：转发给 sink 的事件（渲染面：CLI 终端 / 将来的 TUI）先过脱敏器——已知密钥不许
    /// 到达任何 sink 消费者。事件里每一种携带文本的字段都应被替换成 `***`。
    #[tokio::test]
    async fn forwarded_events_are_redacted_for_sink_consumers() {
        let env = Env::new(
            "sink-redacted",
            vec![
                RuntimeEvent::TextDelta {
                    text: "答复里带 sk-test-secret".into(),
                },
                RuntimeEvent::ReasoningDelta {
                    text: "推理里带 sk-test-secret".into(),
                },
                RuntimeEvent::ToolExec {
                    name: "shell_run".into(),
                    summary: "Running: echo sk-test-secret".into(),
                    args: json!({"command": "echo sk-test-secret"}),
                    call_id: "call_1".into(),
                },
                RuntimeEvent::ToolResult {
                    name: "shell_run".into(),
                    ok: false,
                    output: Some("sk-test-secret\n第二行".into()),
                    call_id: "call_1".into(),
                },
                success(Some("ok")),
            ],
            Some("sk-test-secret"),
        );

        let report = env.run(env.request("run")).await;

        assert_eq!(report.forwarded.len(), 5);
        match &report.forwarded[0] {
            RuntimeEvent::TextDelta { text } => assert_eq!(text, "答复里带 ***"),
            other => panic!("第 0 条应为 TextDelta：{other:?}"),
        }
        match &report.forwarded[1] {
            RuntimeEvent::ReasoningDelta { text } => assert_eq!(text, "推理里带 ***"),
            other => panic!("第 1 条应为 ReasoningDelta：{other:?}"),
        }
        match &report.forwarded[2] {
            RuntimeEvent::ToolExec { summary, args, .. } => {
                assert_eq!(summary, "Running: echo ***");
                assert_eq!(args["command"], json!("echo ***"));
            }
            other => panic!("第 2 条应为 ToolExec：{other:?}"),
        }
        match &report.forwarded[3] {
            RuntimeEvent::ToolResult { output, .. } => {
                assert_eq!(output.as_deref(), Some("***\n第二行"));
            }
            other => panic!("第 3 条应为 ToolResult：{other:?}"),
        }
        // 整体兜底：转发出去的每一条序列化后都不含密钥
        for event in &report.forwarded {
            let raw = serde_json::to_string(event).unwrap();
            assert!(!raw.contains("sk-test-secret"), "{raw}");
        }
    }

    /// F1：只替换转发的副本——用量累加、技能去重与工具计数继续读原始事件，记账不因脱敏变形。
    #[tokio::test]
    async fn sink_copy_is_redacted_while_bookkeeping_uses_the_original_events() {
        let env = Env::new(
            "sink-only",
            vec![
                tool_exec("load_skill", json!({ "name": "my-skill" })),
                tool_exec("shell_run", json!({ "command": "echo sk-test-secret" })),
                RuntimeEvent::Usage {
                    input_tokens: 7,
                    output_tokens: 5,
                    estimated: false,
                },
                success(Some("done")),
            ],
            Some("sk-test-secret"),
        );

        let report = env.run(env.request("run")).await;

        // 记账读原始事件：工具计数仍是 2、`load_skill` 照常记进技能、用量原样累加
        assert_eq!(report.outcome.tool_calls, 2);
        assert_eq!(report.outcome.skills_used, ["my-skill"]);
        assert_eq!(
            report.outcome.usage,
            Usage {
                input_tokens: 7,
                output_tokens: 5,
                estimated: false,
            }
        );
        // 同一个环境的 sink 副本里，携带密钥的那条已被替换
        match &report.forwarded[1] {
            RuntimeEvent::ToolExec { args, .. } => assert_eq!(args["command"], json!("echo ***")),
            other => panic!("第 1 条应为 ToolExec：{other:?}"),
        }
    }

    /// M1：`tasks` 列与交给命令层的错误同样要脱敏——`ignis task show/list` 直接打印它们，
    /// 命令层也会把 `TaskOutcome::error` 打到终端，JSONL 脱敏不能替它们兜底。
    #[tokio::test]
    async fn task_row_and_outcome_error_are_redacted() {
        let env = Env::new(
            "row-redacted",
            vec![RuntimeEvent::Done {
                status: RunStatus::Failed,
                summary: Some("读到 sk-test-secret 后失败".into()),
                error: Some("鉴权失败：拒绝 sk-test-secret".into()),
            }],
            Some("sk-test-secret"),
        );

        let report = env.run(env.request("把 sk-test-secret 读出来")).await;

        let row = env.db.get_task(&report.outcome.id).unwrap().unwrap();
        // 提示词列：用户粘进提示词的密钥不落库
        assert_eq!(row.prompt, "把 *** 读出来");
        assert_eq!(row.summary.as_deref(), Some("读到 *** 后失败"));
        assert_eq!(row.error.as_deref(), Some("鉴权失败：拒绝 ***"));

        // 交回调用方的失败原因与落库的文本同源：命令层打印前不必再脱敏一次
        let error = report.outcome.error.as_deref().unwrap();
        assert_eq!(error, "鉴权失败：拒绝 ***");
        // JSONL 收尾行与任务行一致
        assert_eq!(report.task_end_line()["error"], json!("鉴权失败：拒绝 ***"));
        assert!(
            !env.log_raw().contains("sk-test-secret"),
            "{}",
            env.log_raw()
        );
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
            assert_eq!(report.task_end_line()["status"], json!(name));
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
        assert_eq!(report.types(), ["task_start", "text_delta", "task_end"]);
        assert_eq!(report.task_end_line()["status"], json!("failed"));
        let row = env.db.get_task(&report.outcome.id).unwrap().unwrap();
        assert_eq!(row.status, "failed");
        let error = row.error.as_deref().unwrap();
        assert!(error.contains("done"), "{error}");
        // 文本要能区分「内核实现缺陷」与「任务真的失败」：无终态事件往往就是内核任务 panic
        assert!(error.contains("panic"), "{error}");
        assert!(row.ended_at.is_some());
    }

    /// 任务行写入之后的基础设施失败（这里：日志目录不可写）也必须把行收成终态。
    ///
    /// 首行 `task_start` 就写不进去，正是「`insert_task` 成功、随后 `append_jsonl` 失败」的
    /// 最小复现：旧实现从这里 `?` 返回，任务行会永远停在 `running`。
    #[cfg(unix)]
    #[tokio::test]
    async fn unwritable_log_dir_still_fails_the_task_row() {
        use std::os::unix::fs::PermissionsExt;

        // root 无视权限位（CAP_DAC_OVERRIDE），0500 的目录照写：本用例的前提不成立，跳过
        // SAFETY: `geteuid` 只读进程的有效 uid，不触碰内存、无副作用。
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("跳过 unwritable_log_dir_still_fails_the_task_row：root 不受目录权限位约束");
            return;
        }

        let env = Env::new("readonly-log", vec![success(Some("unreachable"))], None);
        // 目录先存在再收紧到 0500：`TaskLog::new` 仍能打开（create_dir_all 对已存在目录是
        // no-op），写第一行时才被拒。
        fs::create_dir_all(&env.log_dir).unwrap();
        fs::set_permissions(&env.log_dir, fs::Permissions::from_mode(0o500)).unwrap();

        let result = env.run_raw(env.request("write into a read-only dir")).await;
        // 先恢复权限位，TempDir 析构才能清掉目录（断言失败时也一样）
        fs::set_permissions(&env.log_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let error = match result {
            Ok((outcome, _)) => panic!(
                "日志目录不可写时 run 必须返回 Err，却成功了：{}",
                outcome.id
            ),
            Err(error) => error,
        };
        assert!(matches!(error, CoreError::Storage(_)), "{error}");
        // 失败点在首行：内核根本没被调用
        assert!(env.runtime.requests().is_empty());
        assert_eq!(
            fs::read_dir(&env.log_dir).unwrap().count(),
            0,
            "只读目录里不该落任何日志行"
        );

        // 任务行绝不能停在 running：状态、错误与结束时间都要落库
        let rows = env.db.list_tasks(10).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].status, "failed");
        assert!(rows[0].ended_at.is_some());
        let message = rows[0].error.as_deref().unwrap();
        assert!(message.contains("Permission denied"), "{message}");
    }

    /// 组装期把记忆与技能目录注入 system prompt：段头逐字、事实逐条一行、project 在检索之前。
    #[tokio::test]
    async fn system_prompt_carries_known_facts_and_available_skills() {
        let env = Env::new("injected", vec![success(Some("done"))], None);
        let workspace = env.dir.path().to_path_buf();
        env.remember_project(&workspace, "pkg_manager", "这个项目用 uv 管理依赖");
        env.memory
            .remember(FactScope::User, None, "editor", "偏好 Neovim 编辑", None)
            .unwrap();

        let report = env.run(env.request("依赖帮我用 Neovim 改一下")).await;

        assert_eq!(report.outcome.status, RunStatus::Success);
        let prompts = env.system_prompts();
        assert_eq!(prompts.len(), 1);
        let prompt = &prompts[0];
        // 两段都追加在英文骨架之后，段头逐字（用户技能与内置技能同在一张目录里）
        assert!(prompt.contains("\nKnown facts:\n"), "{prompt}");
        assert!(
            prompt.contains("\nAvailable skills (call load_skill to read one):\n"),
            "{prompt}"
        );
        // 事实逐条一行：project 事实无条件注入，user 事实靠 prompt 里的检索项命中
        let project = prompt
            .find("- [project/pkg_manager] 这个项目用 uv 管理依赖")
            .unwrap_or_else(|| panic!("project 事实段缺失：{prompt}"));
        let retrieved = prompt
            .find("- [user/editor] 偏好 Neovim 编辑")
            .unwrap_or_else(|| panic!("检索事实段缺失：{prompt}"));
        assert!(
            project < retrieved,
            "project 事实必须排在检索事实之前：{prompt}"
        );
        assert!(
            prompt.contains("\n- my-skill: Temporary skill for the task runner tests. (user)\n"),
            "{prompt}"
        );
        assert!(prompt.contains("\n- server-health: "), "{prompt}");
        // 目录一行一条，与技能目录的元信息一一对应（不多不少）
        let skills_section = prompt
            .split_once("\nAvailable skills (call load_skill to read one):\n")
            .unwrap_or_else(|| panic!("技能段缺失：{prompt}"))
            .1;
        assert_eq!(
            skills_section.lines().count(),
            env.skills.metas().len(),
            "{skills_section}"
        );
        // 原有骨架没被替换
        assert!(prompt.contains("You are Ignis"), "{prompt}");
        assert!(
            prompt.contains("Answer in the user's language."),
            "{prompt}"
        );
    }

    /// 调度运行（`TaskRunner::new` 收 `Some(profile)`）：job 的 profile 白名单是执行边界——
    /// 白名单外的工具不该出现在内核看到的工具表里（system prompt 的工具清单由
    /// `executor.specs()` 组装），`ignis run` / REPL（`None`）则保持原样。
    #[tokio::test]
    async fn a_job_profile_limits_the_tools_the_kernel_sees() {
        let profile = parse_profile(r#"{"tools":["fs_read"],"risk":"elevated"}"#).unwrap();
        let env = Env::with_profile("profile", vec![success(Some("done"))], None, Some(profile));

        let report = env.run(env.request("读一下配置")).await;

        assert_eq!(report.outcome.status, RunStatus::Success);
        let prompts = env.system_prompts();
        assert_eq!(prompts.len(), 1);
        let prompt = &prompts[0];
        // 工具表由 `executor.specs()` 组装：白名单外的一个都不出现
        let tools_line = prompt
            .lines()
            .find(|line| line.starts_with("Available tools: "))
            .unwrap_or_else(|| panic!("工具表缺失：{prompt}"));
        assert_eq!(tools_line, "Available tools: fs_read");
        for hidden in [
            "fs_list",
            "fs_write",
            "fs_edit",
            "shell_run",
            "http_fetch",
            "notify_user",
            "schedule_create",
        ] {
            assert!(
                !prompt.contains(hidden),
                "白名单外的 {hidden} 不该可见：{prompt}"
            );
        }

        // 无 profile 的交互运行仍是全部工具：扩展不改旧行为
        let plain = Env::new("no-profile", vec![success(Some("done"))], None);
        let report = plain.run(plain.request("读一下配置")).await;
        assert_eq!(report.outcome.status, RunStatus::Success);
        let prompt = &plain.system_prompts()[0];
        assert!(prompt.contains("fs_write"), "{prompt}");
        assert!(prompt.contains("shell_run"), "{prompt}");
        assert!(prompt.contains("notify_user"), "{prompt}");
    }

    /// 能力门控经完整装配链再验一遍：`capabilities().tools == false` 的内核拿到的 system prompt
    /// 只有一行说明，工具清单整体不出现；事实段与技能段照旧注入（决策 #4）。
    #[tokio::test]
    async fn external_kernel_capabilities_hide_the_tool_list_from_the_system_prompt() {
        let env = Env::with_capabilities(
            "gated",
            vec![success(Some("done"))],
            Capabilities {
                tools: false,
                approvals: false,
                usage: false,
            },
        );
        env.remember_project(env.dir.path(), "pkg_manager", "这个项目用 uv 管理依赖");

        let report = env.run(env.request("看看项目怎么装依赖")).await;

        assert_eq!(report.outcome.status, RunStatus::Success);
        let prompts = env.system_prompts();
        assert_eq!(prompts.len(), 1);
        let prompt = &prompts[0];
        assert!(
            prompt.contains("Tools: unavailable in this kernel (Ignis tools do not run here)."),
            "{prompt}"
        );
        assert!(!prompt.contains("Available tools:"), "{prompt}");
        assert!(!prompt.contains("fs_read"), "{prompt}");
        // 记忆注入与技能目录不受门控影响：两段照旧
        assert!(
            prompt.contains("\nKnown facts:\n- [project/pkg_manager] 这个项目用 uv 管理依赖"),
            "{prompt}"
        );
        assert!(
            prompt.contains("\nAvailable skills (call load_skill to read one):\n"),
            "{prompt}"
        );
    }

    /// A/B 工作区隔离经完整注入链再验一遍：B 的 project 事实不会出现在 A 的 system prompt 里。
    #[tokio::test]
    async fn workspace_isolation_holds_through_the_runner() {
        let env = Env::new("isolation", vec![success(Some("done"))], None);
        let other = env.dir.path().join("other");
        fs::create_dir_all(&other).unwrap();
        env.remember_project(env.dir.path(), "pkg_manager", "A 用 uv 管理依赖");
        env.remember_project(&other, "pkg_manager", "B 用 poetry 管理依赖");

        // 两个工作区各跑一次，prompt 用同一句话（`run_raw`：同一环境跑两次不留两套日志断言）
        let a = env
            .run_raw(env.request("环境和依赖都怎么装"))
            .await
            .unwrap()
            .0;
        let b = env
            .run_raw(env.request_in(&other, "环境和依赖都怎么装"))
            .await
            .unwrap()
            .0;
        assert_eq!(
            (a.status, b.status),
            (RunStatus::Success, RunStatus::Success)
        );

        let prompts = env.system_prompts();
        assert_eq!(prompts.len(), 2);
        assert!(
            prompts[0].contains("- [project/pkg_manager] A 用 uv 管理依赖"),
            "{}",
            prompts[0]
        );
        assert!(
            !prompts[0].contains("B 用 poetry 管理依赖"),
            "{}",
            prompts[0]
        );
        assert!(
            prompts[1].contains("- [project/pkg_manager] B 用 poetry 管理依赖"),
            "{}",
            prompts[1]
        );
        assert!(!prompts[1].contains("A 用 uv 管理依赖"), "{}", prompts[1]);
    }

    /// `load_skill` 的调用按序去重记进 [`TaskOutcome::skills_used`]；别的工具与坏参数不算数。
    #[tokio::test]
    async fn skills_used_collects_load_skill_calls_in_order_without_duplicates() {
        let env = Env::new(
            "skills-used",
            vec![
                tool_exec("load_skill", json!({ "name": "my-skill" })),
                tool_exec("load_skill", json!({ "name": "server-health" })),
                tool_exec("load_skill", json!({ "name": "my-skill" })),
                tool_exec("fs_read", json!({ "path": "a.txt" })),
                // 参数不合法（缺 name）：系统提示里没有这个技能可加载，不算用过
                tool_exec("load_skill", json!({})),
                success(Some("done")),
            ],
            None,
        );

        let report = env.run(env.request("用技能干活")).await;

        assert_eq!(report.outcome.skills_used, ["my-skill", "server-health"]);
        // 计数不去重、也不看参数合法性：五条 `ToolExec` 全算（`load_skill` 同样计入）
        assert_eq!(report.outcome.tool_calls, 5);
    }

    /// 事件流没以 `Done` 收尾（内核实现缺陷）时，已看到的技能照样交回调用方。
    #[tokio::test]
    async fn skills_used_is_collected_without_a_done_event() {
        let env = Env::new(
            "skills-used-no-done",
            vec![
                tool_exec("load_skill", json!({ "name": "server-health" })),
                RuntimeEvent::TextDelta {
                    text: "半截".into(),
                },
            ],
            None,
        );

        let report = env.run(env.request("检查服务器")).await;

        assert_eq!(report.outcome.status, RunStatus::Failed);
        assert_eq!(report.outcome.skills_used, ["server-health"]);
    }

    /// 端到端：`TaskRunner` 构造期注入的记录型渠道必须真的收到 `notify_user` 的投递；
    /// 调度运行（`TaskRequest.job_id`）的来源 job id 一路透传到 `ToolCtx` 与告警来源。
    ///
    /// 只把模型换成脚本化假模型（[`ScriptedModel`]）：任务编排、native 内核、工具执行器、告警
    /// 落库与投递全是生产代码。内核必须是真内核——[`MockRuntime`] 只回放事件、不会真的执行
    /// 工具调用，投递链路在那里无从观察。
    #[tokio::test]
    async fn notify_user_delivers_through_the_runner_injected_channel() {
        let dir = TempDir::new("notify-e2e");
        let db = Arc::new(Db::open(&dir.path().join("ignis.db")).unwrap());
        let model = ScriptedModel::start(vec![
            ScriptStep::ToolCalls {
                text: String::new(),
                calls: vec![(
                    "notify_user".into(),
                    json!({"severity": "warning", "title": "磁盘 92%", "body": "根分区 92%（阈值 85%）"}),
                )],
            },
            ScriptStep::Text("已通知用户。".into()),
        ]);
        let config = Arc::new(Config {
            workspace_root: dir.path().to_path_buf(),
            model: ModelConfig {
                base_url: model.base_url(),
                model: "test-model".into(),
                ..ModelConfig::default()
            },
            ..Config::default()
        });
        let runtime: Arc<dyn Runtime> = Arc::new(NativeRuntime::new(
            OpenAiClient::new(&config.model, "test-key".into()).unwrap(),
        ));
        // 记录型渠道经 TaskRunner::new 注入：投递真的流经运行器
        let notifier = Arc::new(RecordingNotifier::new());
        let channel: Arc<dyn Notifier> = notifier.clone();
        let runner = TaskRunner::new(
            Arc::clone(&db),
            Arc::clone(&config),
            runtime,
            Arc::new(Policy::new(ApprovalMode::Preauthorized {
                unsafe_ops: false,
            })),
            ScriptedApproval::new(Vec::new()),
            dir.path().join("logs"),
            MemoryStore::new(Arc::clone(&db), dir.path().join("memory")),
            SkillCatalog::load(&dir.path().join("skills")),
            channel,
            // 本次按调度运行的形状发起：job id 由守护进程透传
            None,
        );

        let (sink, _received) = mpsc::unbounded_channel();
        let outcome = runner
            .run(
                TaskRequest {
                    prompt: "检查磁盘".into(),
                    history: Vec::new(),
                    kind: TaskKind::Schedule,
                    workspace: dir.path().to_path_buf(),
                    job_id: Some("job-7".into()),
                },
                sink,
                CancellationToken::new(),
            )
            .await
            .expect("端到端运行不应返回 Err");
        assert_eq!(outcome.status, RunStatus::Success);
        // 调度运行的 tasks.kind 是 `schedule`
        let task_row = db.get_task(&outcome.id).unwrap().expect("任务行");
        assert_eq!(task_row.kind, "schedule");

        // 渠道收到的就是落库的那一行，来源是本次运行的任务 + 透传进来的 job
        let delivered = notifier.alerts();
        assert_eq!(delivered.len(), 1, "注入的渠道应恰好收到一次投递");
        let rows = db.list_alerts(true, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(delivered[0], rows[0]);
        assert_eq!(
            (
                delivered[0].severity.as_str(),
                delivered[0].title.as_str(),
                delivered[0].body.as_deref(),
                delivered[0].source_job_id.as_deref(),
            ),
            (
                "warning",
                "磁盘 92%",
                Some("根分区 92%（阈值 85%）"),
                Some("job-7")
            )
        );
        assert_eq!(
            delivered[0].source_task_id.as_deref(),
            Some(outcome.id.as_str())
        );
        // 去重键的来源以 job 为准：同一次调度运行的 notify_user 与失败告警共用一把键
        assert_eq!(delivered[0].dedup_key, "job-7:磁盘 92%");

        // 工具结果也回到了模型手里：native 内核第二次请求带 `tool` 消息，回执逐字
        let requests = model.requests();
        assert_eq!(requests.len(), 2, "{requests:#?}");
        let messages = requests[1]["messages"].as_array().unwrap();
        assert!(
            messages.iter().any(|message| {
                message["role"] == "tool"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content == "已发送告警（warning）：磁盘 92%")
            }),
            "{messages:#?}"
        );
    }

    /// 子进程专用：真正跑 `facts` 表不可用时的 fail-soft 场景，让警告落到 stderr。
    ///
    /// 「警告确实写到了 stderr」只能在进程边界上观察：libtest 的输出捕获会把测试线程
    /// （以及它派生出的线程）里的打印宏收走，同进程里读不到 fd 2。父用例
    /// [`missing_facts_table_warns_and_the_task_still_succeeds`] 用
    /// `--include-ignored --nocapture` 在子进程里拉起本用例，再读它的 stderr。
    #[ignore = "由 missing_facts_table_warns_and_the_task_still_succeeds 在子进程里拉起"]
    #[tokio::test]
    async fn fail_soft_child_reports_a_missing_facts_table_on_stderr() {
        let env = Env::new("fail-soft-child", vec![success(Some("done"))], None);
        // 事实表没了（库被换过、迁移中途失败）：组装期只能退化成空事实集
        env.db
            .with_conn(|conn| conn.execute_batch("DROP TABLE facts"))
            .unwrap();

        let report = env.run(env.request("读一下项目配置")).await;

        assert_eq!(report.outcome.status, RunStatus::Success);
        let prompts = env.system_prompts();
        assert_eq!(prompts.len(), 1);
        assert!(!prompts[0].contains("Known facts:"), "{}", prompts[0]);
        // 技能段不归记忆管：两条注入链互不牵连
        assert!(prompts[0].contains("Available skills"), "{}", prompts[0]);
    }

    /// 记忆不可用时任务照跑（不因记忆失败而失败），只在 stderr 留一行警告。
    #[test]
    fn missing_facts_table_warns_and_the_task_still_succeeds() {
        let exe = std::env::current_exe().unwrap();
        let output = Command::new(&exe)
            .args(["--include-ignored", "--nocapture", "fail_soft_child"])
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "子进程应全绿（场景断言都在里面）：{:?}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout)
        );
        // 一行警告，任务不失败：fail-soft 的全部含义
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        assert!(stderr.contains("warning: 读取记忆失败"), "{stderr}");
        // 底层原因也带上，便于排查是哪儿断了
        assert!(stderr.contains("no such table: facts"), "{stderr}");
    }

    /// 一条工具执行事件（`ToolExec`）；`summary` 不在本模块的断言范围里。
    fn tool_exec(name: &str, args: Value) -> RuntimeEvent {
        RuntimeEvent::ToolExec {
            name: name.to_string(),
            summary: format!("Running {name}"),
            args,
            call_id: "call_1".into(),
        }
    }
}
