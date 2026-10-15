//! 审批执行器：把注册表包上「分类 → 策略 → 提示 → 记录 → 执行」。
//!
//! 每次调用先按工具名与参数定风险等级（敏感路径用 [`is_sensitive`]，命令用
//! [`is_dangerous_command`]），再由 [`Policy`] 判定：`Allow` 直接执行、`Deny` 拒绝、
//! `Ask` 交给 [`ApprovalHandler`] 询问。需要判定的调用（非 `Safe`）无论放行还是拒绝都
//! 在 `approvals` 表留一条记录；真正执行前再检查取消信号；工具输出与错误文本统一过
//! [`Redactor`] 后才回填给模型。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ignis_runtime::{
    ApprovalDecision, ApprovalHandler, ApprovalRequest, Limits, RiskLevel, ToolCall, ToolExecutor,
    ToolResult, ToolSpec,
};
use ignis_storage::{Db, NewApproval};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{Registry, ToolCtx};
use crate::security::{
    ApprovalMode, Policy, PolicyDecision, Redactor, is_dangerous_command, is_sensitive,
};

/// 摘要里的命令最多保留的字符数。
const COMMAND_SUMMARY_CHARS: usize = 80;

/// 工具调用执行器：注册表 + 审批策略 + 审批方 + 审批记录 + 输出脱敏。
pub struct ToolRuntime {
    registry: Registry,
    policy: Arc<Policy>,
    approvals: Arc<dyn ApprovalHandler>,
    db: Arc<Db>,
    task_id: String,
    workspace: PathBuf,
    limits: Limits,
    redactor: Redactor,
}

impl ToolRuntime {
    /// 装配执行器。参数较多是计划钉死的构造签名（Task 12 的 `TaskRunner` 按此注入），
    /// 不做参数对象包装。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Registry,
        policy: Arc<Policy>,
        approvals: Arc<dyn ApprovalHandler>,
        db: Arc<Db>,
        task_id: String,
        workspace: PathBuf,
        limits: Limits,
        redactor: Redactor,
    ) -> Self {
        Self {
            registry,
            policy,
            approvals,
            db,
            task_id,
            workspace,
            limits,
            redactor,
        }
    }

    /// 审批闸门：`Some(结果)` 表示本次调用不得执行。非 `Safe` 调用无论如何都在这里留痕。
    async fn gate(&self, call: &ToolCall, summary: &str, risk: RiskLevel) -> Option<ToolResult> {
        match self.policy.decide(risk, &call.name) {
            PolicyDecision::Allow => {
                // Safe 不记录：审批表只留需要判定的调用（Task 16 的 demo_a 依赖这一点）。
                // 被策略直接放行的非 Safe 调用（预授权、会话规则）同样留一条 `allow`。
                if risk != RiskLevel::Safe {
                    self.record(call, summary, risk, "allow");
                }
                None
            }
            PolicyDecision::Deny(reason) => {
                self.record(call, summary, risk, "deny");
                Some(self.result(false, format!("denied by policy: {reason}")))
            }
            PolicyDecision::Ask => {
                let request = ApprovalRequest {
                    tool: call.name.clone(),
                    summary: summary.to_string(),
                    args: call.args.clone(),
                    risk,
                };
                let (decision, allowed) = match self.approvals.decide(request).await {
                    ApprovalDecision::AllowOnce => ("allow", true),
                    ApprovalDecision::AllowSession => {
                        // 会话级授权：同一工具的后续 Elevated 调用不再提示
                        self.policy.remember_session(&call.name);
                        ("allow_session", true)
                    }
                    ApprovalDecision::Reject => ("deny", false),
                };
                self.record(call, summary, risk, decision);
                if allowed {
                    None
                } else {
                    Some(self.result(false, format!("denied by policy: 未获批准：{summary}")))
                }
            }
        }
    }

    /// 追加一条审批记录（放行与拒绝都要留痕），`decision` / `mode` / `risk` 用锁定的字符串取值。
    fn record(&self, call: &ToolCall, summary: &str, risk: RiskLevel, decision: &str) {
        let approval = NewApproval {
            task_id: self.task_id.clone(),
            tool: call.name.clone(),
            summary: summary.to_string(),
            risk: risk_name(risk).to_string(),
            decision: decision.to_string(),
            mode: mode_name(self.policy.mode()).to_string(),
            decided_at: now_millis(),
        };
        // 留痕失败不改变已经做出的决定（本次调用按审批结果返回，DB 故障由上层命令报错暴露）。
        let _ = self.db.insert_approval(&approval);
    }

    /// 统一出口：所有返回给模型的结果都在这里过一遍脱敏。
    fn result(&self, ok: bool, text: impl Into<String>) -> ToolResult {
        ToolResult {
            ok,
            output: Some(self.redactor.redact(&text.into())),
        }
    }
}

#[async_trait]
impl ToolExecutor for ToolRuntime {
    fn specs(&self) -> Vec<ToolSpec> {
        self.registry.specs()
    }

    fn summarize(&self, call: &ToolCall) -> String {
        summarize(call)
    }

    async fn execute(&self, call: ToolCall, cancel: CancellationToken) -> ToolResult {
        let summary = summarize(&call);
        // 未注册的工具绝不执行：编造出来的调用要明确失败，而不是替它挑一个相近的工具
        let Some(tool) = self.registry.get(&call.name) else {
            return self.result(false, format!("未知工具 `{}`：未注册，未执行", call.name));
        };

        let risk = classify(&call);
        if let Some(denied) = self.gate(&call, &summary, risk).await {
            return denied;
        }
        // 已获批但任务已取消：不产生任何副作用
        if cancel.is_cancelled() {
            return self.result(false, format!("已取消（cancelled），未执行：{summary}"));
        }

        let ctx = ToolCtx {
            workspace: &self.workspace,
            cancel,
            limits: &self.limits,
        };
        match tool.run(&call.args, &ctx).await {
            Ok(output) => self.result(true, output),
            Err(err) => self.result(false, err.to_string()),
        }
    }
}

/// 风险分类：敏感路径优先于工具本身，命令再看危险命令库。
fn classify(call: &ToolCall) -> RiskLevel {
    // 任意工具的原始路径参数命中敏感文件清单 → 危险：读 `.env`、写 `id_rsa` 都要显式授权
    let sensitive = call
        .args
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|path| is_sensitive(Path::new(path)));
    if sensitive {
        return RiskLevel::Dangerous;
    }
    match call.name.as_str() {
        "fs_read" | "fs_list" => RiskLevel::Safe,
        "fs_write" | "fs_edit" => RiskLevel::Elevated,
        "shell_run" => match call.args.get("command").and_then(Value::as_str) {
            Some(command) if is_dangerous_command(command) => RiskLevel::Dangerous,
            _ => RiskLevel::Elevated,
        },
        "http_fetch" => RiskLevel::Elevated,
        // 未知工具不会走到这里（`execute` 先查注册表并直接返回错误结果），保守按 Elevated
        _ => RiskLevel::Elevated,
    }
}

/// 行动摘要（锁死格式）：审批提示、`ToolExec` 事件与任务日志共用同一条文案。
fn summarize(call: &ToolCall) -> String {
    let path = call.args.get("path").and_then(Value::as_str).unwrap_or(".");
    let text = |key: &str| call.args.get(key).and_then(Value::as_str).unwrap_or("");
    match call.name.as_str() {
        "fs_read" => format!("Reading {path}"),
        "fs_list" => format!("Listing {path}"),
        "fs_write" => format!("Writing {path}"),
        "fs_edit" => format!("Editing {path}"),
        "shell_run" => format!(
            "Running: {}",
            truncate_chars(text("command"), COMMAND_SUMMARY_CHARS)
        ),
        "http_fetch" => format!("Fetching {}", text("url")),
        other => format!("Running {other}"),
    }
}

/// 取前 `max` 个字符：摘要按字符（不是字节）截断，多字节命令不会被切坏。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// `RiskLevel` 的 serde 小写名，与 `approvals.risk` 的取值契约一致。
fn risk_name(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Safe => "safe",
        RiskLevel::Elevated => "elevated",
        RiskLevel::Dangerous => "dangerous",
    }
}

/// `approvals.mode` 的取值契约：`interactive` / `preauthorized` / `unattended`。
fn mode_name(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Interactive => "interactive",
        ApprovalMode::Preauthorized { .. } => "preauthorized",
        ApprovalMode::Unattended => "unattended",
    }
}

/// epoch 毫秒；时钟早于 epoch（人工设置过系统时间）时退化成 0，保证记录仍能写入。
fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_runtime::test_support::ScriptedApproval;
    use ignis_runtime::{
        ApprovalDecision, ApprovalHandler, Limits, RiskLevel, ToolCall, ToolExecutor, ToolResult,
    };
    use ignis_storage::{ApprovalRecord, Db};
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::tools::default_registry;

    /// 本次任务的 id：审批记录按它归属。
    const TASK_ID: &str = "01J9Z6Q3T7R8S9V0W1X2Y3Z4AB";

    /// 单个测试独占的临时工作区，析构时清理（本 crate 不引入 tempfile 依赖）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-core-runtime-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 测试环境：临时工作区 + 内存库 + 脚本化审批，按给定模式与脱敏器装配 [`ToolRuntime`]。
    struct Env {
        ws: TempDir,
        db: Arc<Db>,
        approval: Arc<ScriptedApproval>,
        runtime: ToolRuntime,
    }

    impl Env {
        fn new(mode: ApprovalMode, decisions: Vec<ApprovalDecision>) -> Self {
            Self::with_redactor(mode, decisions, Redactor::new(Vec::new()))
        }

        /// 需要观察脱敏的用例自带密钥集合。
        fn with_redactor(
            mode: ApprovalMode,
            decisions: Vec<ApprovalDecision>,
            redactor: Redactor,
        ) -> Self {
            let ws = TempDir::new();
            let db = Arc::new(Db::open_in_memory().unwrap());
            let policy = Arc::new(Policy::new(mode));
            let approval = ScriptedApproval::new(decisions);
            let runtime = ToolRuntime::new(
                default_registry(),
                policy,
                Arc::clone(&approval) as Arc<dyn ApprovalHandler>,
                Arc::clone(&db),
                TASK_ID.to_string(),
                ws.path().to_path_buf(),
                Limits::default(),
                redactor,
            );
            Self {
                ws,
                db,
                approval,
                runtime,
            }
        }

        fn path_of(&self, relative: &str) -> PathBuf {
            self.ws.path().join(relative)
        }

        /// 审批记录（按决定先后）。
        fn approvals(&self) -> Vec<ApprovalRecord> {
            self.db.list_approvals(TASK_ID).unwrap()
        }

        async fn call(&self, name: &str, args: Value) -> ToolResult {
            self.execute(name, args, CancellationToken::new()).await
        }

        async fn execute(&self, name: &str, args: Value, cancel: CancellationToken) -> ToolResult {
            let call = ToolCall {
                id: format!("call-{name}"),
                name: name.to_string(),
                args,
            };
            self.runtime.execute(call, cancel).await
        }
    }

    #[tokio::test]
    async fn safe_read_does_not_ask() {
        let env = Env::new(ApprovalMode::Interactive, Vec::new());
        std::fs::write(env.path_of("a.txt"), "hello\n").unwrap();

        let result = env.call("fs_read", json!({"path": "a.txt"})).await;

        assert!(result.ok, "{:?}", result.output);
        assert_eq!(result.output.as_deref(), Some("hello\n"));
        // Safe 工具既不提示，也不留审批记录
        assert!(env.approval.seen().is_empty());
        assert!(env.approvals().is_empty());
    }

    #[tokio::test]
    async fn elevated_write_records_approval() {
        let env = Env::new(ApprovalMode::Interactive, vec![ApprovalDecision::AllowOnce]);

        let result = env
            .call("fs_write", json!({"path": "out.txt", "content": "hi"}))
            .await;

        assert!(result.ok, "{:?}", result.output);
        assert_eq!(
            std::fs::read_to_string(env.path_of("out.txt")).unwrap(),
            "hi"
        );
        // 审批请求带摘要、原始参数与风险等级
        let seen = env.approval.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].tool, "fs_write");
        assert_eq!(seen[0].summary, "Writing out.txt");
        assert_eq!(seen[0].args, json!({"path": "out.txt", "content": "hi"}));
        assert_eq!(seen[0].risk, RiskLevel::Elevated);

        // 记录行：decision / mode / risk 都是锁定的字符串取值
        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task_id, TASK_ID);
        assert_eq!(rows[0].tool, "fs_write");
        assert_eq!(rows[0].summary, "Writing out.txt");
        assert_eq!(rows[0].risk, "elevated");
        assert_eq!(rows[0].decision, "allow");
        assert_eq!(rows[0].mode, "interactive");
        assert!(rows[0].decided_at > 0);
    }

    #[tokio::test]
    async fn reject_prevents_side_effects() {
        let env = Env::new(ApprovalMode::Interactive, vec![ApprovalDecision::Reject]);

        let result = env
            .call("fs_write", json!({"path": "out.txt", "content": "hi"}))
            .await;

        assert!(!result.ok);
        let output = result.output.unwrap();
        assert!(output.starts_with("denied by policy:"), "{output}");
        assert!(output.contains("out.txt"), "{output}");
        // 拒绝即无副作用
        assert!(!env.path_of("out.txt").exists());

        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].decision, "deny");
        assert_eq!(rows[0].mode, "interactive");
    }

    #[tokio::test]
    async fn session_allow_skips_second_prompt() {
        let env = Env::new(
            ApprovalMode::Interactive,
            vec![ApprovalDecision::AllowSession],
        );

        let first = env
            .call("fs_write", json!({"path": "a.txt", "content": "a"}))
            .await;
        assert!(first.ok, "{:?}", first.output);
        let second = env
            .call("fs_write", json!({"path": "b.txt", "content": "b"}))
            .await;
        assert!(second.ok, "{:?}", second.output);

        // 第二次同工具走会话规则：不再调用审批方，但文件照常写入
        assert_eq!(env.approval.seen().len(), 1);
        assert_eq!(std::fs::read_to_string(env.path_of("b.txt")).unwrap(), "b");
        let decisions: Vec<String> = env
            .approvals()
            .into_iter()
            .map(|row| row.decision)
            .collect();
        assert_eq!(decisions, ["allow_session", "allow"]);

        // 会话规则按工具粒度（kind_key = 工具名）：另一个 Elevated 工具仍要提示
        let edited = env
            .call(
                "fs_edit",
                json!({"path": "a.txt", "old_string": "a", "new_string": "z"}),
            )
            .await;
        // 决策队列已空 → 脚本化审批的安全默认是 Reject
        assert!(!edited.ok);
        assert_eq!(env.approval.seen().len(), 2);
        assert_eq!(env.approval.seen()[1].tool, "fs_edit");
        assert_eq!(std::fs::read_to_string(env.path_of("a.txt")).unwrap(), "a");
    }

    #[tokio::test]
    async fn unattended_denies_and_records() {
        let env = Env::new(ApprovalMode::Unattended, Vec::new());

        let result = env
            .call("fs_write", json!({"path": "out.txt", "content": "hi"}))
            .await;

        assert!(!result.ok);
        let output = result.output.unwrap();
        assert!(output.starts_with("denied by policy:"), "{output}");
        assert!(!env.path_of("out.txt").exists());
        // 非交互模式不提示，但仍然留痕
        assert!(env.approval.seen().is_empty());
        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].decision, "deny");
        assert_eq!(rows[0].mode, "unattended");
        assert_eq!(rows[0].risk, "elevated");
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_result() {
        let env = Env::new(ApprovalMode::Interactive, Vec::new());

        let result = env.call("fs_missing", json!({"path": "a.txt"})).await;

        assert!(!result.ok);
        let output = result.output.unwrap();
        assert!(output.contains("未知工具"), "{output}");
        assert!(output.contains("fs_missing"), "{output}");
        // 未注册的工具既不提示，也不留痕
        assert!(env.approval.seen().is_empty());
        assert!(env.approvals().is_empty());
    }

    #[tokio::test]
    async fn secret_in_output_is_redacted() {
        let env = Env::with_redactor(
            ApprovalMode::Interactive,
            Vec::new(),
            Redactor::new(vec!["sk-x".into()]),
        );
        std::fs::write(env.path_of("a.txt"), "token=sk-x\n").unwrap();

        let result = env.call("fs_read", json!({"path": "a.txt"})).await;

        assert!(result.ok);
        let output = result.output.unwrap();
        assert!(!output.contains("sk-x"), "{output}");
        assert_eq!(output, "token=***\n");

        // 错误信息同样脱敏（路径里带密钥）
        let result = env
            .call("fs_read", json!({"path": "sk-x-missing.txt"}))
            .await;
        assert!(!result.ok);
        let output = result.output.unwrap();
        assert!(!output.contains("sk-x"), "{output}");
    }

    #[tokio::test]
    async fn classification_follows_the_pinned_table() {
        // Interactive + 全部拒绝：只观察「哪些调用触发了提示」与「提示里的风险等级」
        let env = Env::new(ApprovalMode::Interactive, vec![ApprovalDecision::Reject; 6]);
        std::fs::write(env.path_of("a.txt"), "hi\n").unwrap();
        std::fs::write(env.path_of(".env"), "K=v\n").unwrap();

        for (name, args) in [
            // Safe：不提示
            ("fs_read", json!({"path": "a.txt"})),
            ("fs_list", json!({"path": "."})),
            // 敏感路径优先于工具本身
            ("fs_read", json!({"path": ".env"})),
            ("fs_write", json!({"path": "b.txt", "content": "x"})),
            (
                "fs_edit",
                json!({"path": "a.txt", "old_string": "hi", "new_string": "yo"}),
            ),
            ("shell_run", json!({"command": "ls -la"})),
            ("shell_run", json!({"command": "sudo rm -rf /"})),
            ("http_fetch", json!({"url": "https://example.com"})),
        ] {
            let _ = env.call(name, args).await;
        }

        let prompts: Vec<(String, RiskLevel)> = env
            .approval
            .seen()
            .into_iter()
            .map(|request| (request.tool, request.risk))
            .collect();
        assert_eq!(
            prompts,
            [
                ("fs_read".to_string(), RiskLevel::Dangerous),
                ("fs_write".to_string(), RiskLevel::Elevated),
                ("fs_edit".to_string(), RiskLevel::Elevated),
                ("shell_run".to_string(), RiskLevel::Elevated),
                ("shell_run".to_string(), RiskLevel::Dangerous),
                ("http_fetch".to_string(), RiskLevel::Elevated),
            ]
        );
        // 被拒的调用不产生副作用；Safe 的调用照常执行（只是不留记录）
        assert_eq!(
            std::fs::read_to_string(env.path_of("a.txt")).unwrap(),
            "hi\n"
        );
        assert!(!env.path_of("b.txt").exists());
        assert_eq!(env.approvals().len(), 6);
    }

    #[tokio::test]
    async fn preauthorized_allow_records_each_non_safe_call() {
        // Task 16 的 demo_a 断言：预授权（--yes）下每个非 Safe 调用各留一条 allow/preauthorized
        let env = Env::new(
            ApprovalMode::Preauthorized { unsafe_ops: false },
            Vec::new(),
        );

        let written = env
            .call("fs_write", json!({"path": "out.txt", "content": "hi"}))
            .await;
        assert!(written.ok, "{:?}", written.output);
        assert!(env.approval.seen().is_empty());

        // Safe 调用不留记录，所以表里仍是写操作那一条
        let read = env.call("fs_read", json!({"path": "out.txt"})).await;
        assert!(read.ok, "{:?}", read.output);

        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, "fs_write");
        assert_eq!(rows[0].decision, "allow");
        assert_eq!(rows[0].mode, "preauthorized");

        // --unsafe：危险命令放行并真的执行，同样留一条 allow 记录
        let unsafe_env = Env::new(ApprovalMode::Preauthorized { unsafe_ops: true }, Vec::new());
        let removed = unsafe_env
            .call("shell_run", json!({"command": "rm -rf gone"}))
            .await;
        assert!(removed.ok, "{:?}", removed.output);
        assert!(
            removed
                .output
                .as_deref()
                .unwrap()
                .starts_with("exit code: 0"),
            "{:?}",
            removed.output
        );
        let rows = unsafe_env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, "shell_run");
        assert_eq!(rows[0].risk, "dangerous");
        assert_eq!(rows[0].decision, "allow");
        assert_eq!(rows[0].mode, "preauthorized");
    }

    #[tokio::test]
    async fn cancel_before_execution_returns_cancelled() {
        let env = Env::new(ApprovalMode::Interactive, vec![ApprovalDecision::AllowOnce]);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = env
            .execute(
                "fs_write",
                json!({"path": "out.txt", "content": "hi"}),
                cancel,
            )
            .await;

        assert!(!result.ok);
        let output = result.output.unwrap();
        assert!(output.contains("已取消"), "{output}");
        assert!(output.contains("cancelled"), "{output}");
        assert!(!env.path_of("out.txt").exists(), "取消后不得产生副作用");
        // 审批已经发生：留痕保留
        assert_eq!(env.approvals().len(), 1);
    }

    #[test]
    fn summarize_follows_the_locked_formats() {
        let call = |name: &str, args: Value| ToolCall {
            id: "c1".into(),
            name: name.into(),
            args,
        };

        assert_eq!(
            summarize(&call("fs_read", json!({"path": "a.txt"}))),
            "Reading a.txt"
        );
        assert_eq!(summarize(&call("fs_list", json!({}))), "Listing .");
        assert_eq!(
            summarize(&call("fs_write", json!({"path": "a/b.txt"}))),
            "Writing a/b.txt"
        );
        assert_eq!(
            summarize(&call("fs_edit", json!({"path": "a.txt"}))),
            "Editing a.txt"
        );
        assert_eq!(
            summarize(&call("shell_run", json!({"command": "ls -la"}))),
            "Running: ls -la"
        );
        assert_eq!(
            summarize(&call("http_fetch", json!({"url": "https://x/y"}))),
            "Fetching https://x/y"
        );
        assert_eq!(
            summarize(&call("fs_missing", json!({}))),
            "Running fs_missing"
        );

        // 命令截到 80 字符，按字符计数：多字节命令不得被切坏
        assert_eq!(
            summarize(&call("shell_run", json!({"command": "命".repeat(200)}))),
            format!("Running: {}", "命".repeat(80))
        );
    }

    #[test]
    fn specs_delegate_to_the_registry() {
        let env = Env::new(ApprovalMode::Interactive, Vec::new());
        assert_eq!(env.runtime.specs(), default_registry().specs());
        // 内核要求 ToolExecutor: Send + Sync
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ToolRuntime>();
    }
}
