//! 审批执行器：把注册表包上「分类 → 策略 → 提示 → 记录 → 执行」。
//!
//! 每次调用先按工具名与参数定风险等级（敏感路径用 [`is_sensitive`]，命令用
//! [`is_dangerous_command`]；路径按工作区内的解析结果判定，`notes.txt -> .env` 这类别名
//! 同样算敏感），再由 [`Policy`] 判定：`Allow` 直接执行、`Deny` 拒绝、`Ask` 交给
//! [`ApprovalHandler`] 询问。需要判定的调用（非 `Safe`）无论放行还是拒绝都在 `approvals`
//! 表留一条记录——**记录写不进去就不执行**（fail-closed）。摘要进入审批提示与审批表之前先过
//! [`Redactor`]；真正执行前再检查取消信号；工具输出与错误文本统一过 [`Redactor`] 后才回填给模型。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ignis_runtime::{
    ApprovalDecision, ApprovalHandler, ApprovalRequest, Limits, RiskLevel, ToolCall, ToolExecutor,
    ToolResult, ToolSpec,
};
use ignis_storage::{Db, NewApproval, StorageError};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{Registry, ToolCtx};
use crate::security::{
    ApprovalMode, Policy, PolicyDecision, Redactor, is_dangerous_command, is_sensitive,
    resolve_in_workspace,
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
    /// 来源调度任务 id（`TaskRequest.job_id` 透传）；交互运行没有它。
    job_id: Option<String>,
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
        job_id: Option<String>,
    ) -> Self {
        Self {
            registry,
            policy,
            approvals,
            db,
            task_id,
            job_id,
            workspace,
            limits,
            redactor,
        }
    }

    /// 审批闸门：`Some(结果)` 表示本次调用不得执行。非 `Safe` 调用无论如何都在这里留痕；
    /// 留痕写不进去则一律不执行（记录是这次授权的唯一凭据，必须 fail-closed）。
    async fn gate(&self, call: &ToolCall, summary: &str, risk: RiskLevel) -> Option<ToolResult> {
        // 摘要会进审批提示与 `approvals` 表（`ignis task show` 会打印），先过脱敏
        let summary = self.redactor.redact(summary);
        match self.policy.decide(risk, &call.name) {
            PolicyDecision::Allow => {
                // Safe 不记录：审批表只留需要判定的调用（Task 16 的 demo_a 依赖这一点）。
                // 被策略直接放行的非 Safe 调用（预授权、会话规则）同样留一条 `allow`。
                if risk != RiskLevel::Safe
                    && let Err(error) = self.record(call, &summary, risk, "allow")
                {
                    return Some(self.audit_failed(&summary, &error));
                }
                None
            }
            PolicyDecision::Deny(reason) => {
                if let Err(error) = self.record(call, &summary, risk, "deny") {
                    return Some(self.audit_failed(&summary, &error));
                }
                Some(self.result(false, format!("denied by policy: {reason}")))
            }
            PolicyDecision::Ask => {
                let request = ApprovalRequest {
                    tool: call.name.clone(),
                    summary: summary.clone(),
                    args: call.args.clone(),
                    risk,
                };
                let (decision, allowed, remember_session) =
                    match self.approvals.decide(request).await {
                        ApprovalDecision::AllowOnce => ("allow", true, false),
                        ApprovalDecision::AllowSession => ("allow_session", true, true),
                        ApprovalDecision::Reject => ("deny", false, false),
                    };
                // 记录先于放行：写不进去就不执行，也不把会话规则记下来
                if let Err(error) = self.record(call, &summary, risk, decision) {
                    return Some(self.audit_failed(&summary, &error));
                }
                if remember_session {
                    // 会话级授权：同一工具的后续 Elevated 调用不再提示
                    self.policy.remember_session(&call.name);
                }
                if allowed {
                    None
                } else {
                    Some(self.result(false, format!("denied by policy: 未获批准：{summary}")))
                }
            }
        }
    }

    /// fail-closed：审批记录写不进去时拒绝执行，并给出存储错误便于排查。
    fn audit_failed(&self, summary: &str, error: &StorageError) -> ToolResult {
        self.result(false, format!("审计写入失败，未执行：{summary}（{error}）"))
    }

    /// 追加一条审批记录（放行与拒绝都要留痕），`decision` / `mode` / `risk` 用锁定的字符串取值。
    fn record(
        &self,
        call: &ToolCall,
        summary: &str,
        risk: RiskLevel,
        decision: &str,
    ) -> Result<(), StorageError> {
        let approval = NewApproval {
            task_id: self.task_id.clone(),
            tool: call.name.clone(),
            summary: summary.to_string(),
            risk: risk_name(risk).to_string(),
            decision: decision.to_string(),
            mode: mode_name(self.policy.mode()).to_string(),
            decided_at: now_millis(),
        };
        self.db.insert_approval(&approval)
    }

    /// 统一出口：所有返回给模型的结果都在这里过一遍脱敏。
    fn result(&self, ok: bool, text: impl Into<String>) -> ToolResult {
        ToolResult {
            ok,
            output: Some(self.redactor.redact(&text.into())),
        }
    }

    /// 本次调用的风险分类。profile 拒绝留痕（[`crate::tools::ProfileExecutor`]）要记真实等级，
    /// 而不是编一个；分类口径与闸门用的是同一份。
    pub(crate) fn classify_call(&self, call: &ToolCall) -> RiskLevel {
        classify(call, &self.workspace)
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

        let risk = classify(&call, &self.workspace);
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
            task_id: Some(self.task_id.clone()),
            job_id: self.job_id.clone(),
        };
        match tool.run(&call.args, &ctx).await {
            Ok(output) => self.result(true, output),
            Err(err) => self.result(false, err.to_string()),
        }
    }
}

/// 风险分类：元工具里 `schedule_create` 恒定 `Elevated`、其余恒定 `Safe`，敏感路径优先于工具
/// 本身，命令再看危险命令库。
fn classify(call: &ToolCall, workspace: &Path) -> RiskLevel {
    // `schedule_create` 写的是「以后自动干什么」：创建时必须显式确认（交互 Ask / `--yes` 预授权 /
    // 无人值守拒绝），因此恒定 Elevated；判定放在敏感路径与回退臂之前——元工具不解析路径，参数里
    // 混进 `path` 之类也不该升级成 Dangerous。
    if call.name == "schedule_create" {
        return RiskLevel::Elevated;
    }
    // 其余元工具只写 agent 自有状态（记忆事实、技能目录、告警收件箱，用户可经 CLI 改删），也不
    // 解析任何路径：恒定 Safe，且判定放在最前面——参数里混进 `path` 之类也不该改分类。
    if matches!(
        call.name.as_str(),
        "remember" | "load_skill" | "notify_user"
    ) {
        return RiskLevel::Safe;
    }
    // 任意工具的路径参数指向敏感文件 → 危险：读 `.env`、写 `id_rsa` 都要显式授权
    let sensitive = call
        .args
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|path| resolves_to_sensitive(workspace, path));
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

/// 路径是否指向敏感文件：先看原始拼写，再看它在工作区内的解析结果——
/// `notes.txt -> .env` 这类别名读的仍是 `.env`，不能因为参数看着无害就放行。
/// 解析失败（越界、悬空链接等）时退回原始拼写的判定；执行阶段本来就会拒绝该路径。
fn resolves_to_sensitive(workspace: &Path, raw_path: &str) -> bool {
    if is_sensitive(Path::new(raw_path)) {
        return true;
    }
    resolve_in_workspace(workspace, raw_path).is_ok_and(|resolved| is_sensitive(&resolved))
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

/// `RiskLevel` 的 serde 小写名，与 `approvals.risk` 的取值契约一致；profile 拒绝留痕
/// （[`crate::tools::ProfileExecutor`]）共用同一份映射。
pub(crate) fn risk_name(risk: RiskLevel) -> &'static str {
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

/// epoch 毫秒；时钟早于 epoch（人工设置过系统时间）时退化成 0，保证记录仍能写入。profile 拒绝
/// 留痕（[`crate::tools::ProfileExecutor`]）共用同一份时间口径。
pub(crate) fn now_millis() -> i64 {
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
                // 交互运行没有来源 job；job_id 的透传链路由 `tools::meta` 的用例覆盖
                None,
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

        /// 让共享连接上的写操作失败（`PRAGMA query_only` 后 INSERT 报只读库错误），
        /// 用于验证「审批记录写不进去就不执行」；读（`list_approvals`）仍然可用。
        fn make_writes_fail(&self) {
            self.db
                .with_conn(|conn| conn.execute_batch("PRAGMA query_only = ON"))
                .unwrap();
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
    async fn audit_write_failure_fails_closed() {
        // 放行路径：审批已获批准，但记录写不进去 → 不执行，副作用不发生
        let allowed = Env::new(ApprovalMode::Interactive, vec![ApprovalDecision::AllowOnce]);
        allowed.make_writes_fail();
        let result = allowed
            .call("fs_write", json!({"path": "out.txt", "content": "hi"}))
            .await;
        assert!(!result.ok);
        let output = result.output.unwrap();
        assert!(output.contains("审计写入失败"), "{output}");
        assert!(output.contains("未执行"), "{output}");
        assert!(output.contains("Writing out.txt"), "{output}");
        assert!(output.contains("sqlite error"), "{output}");
        assert_eq!(allowed.approval.seen().len(), 1, "批准后才轮到记录失败");
        assert!(!allowed.path_of("out.txt").exists(), "审计失败后不得执行");

        // 拒绝路径：策略拒绝同样要留痕，留不下也不执行
        let denied = Env::new(ApprovalMode::Unattended, Vec::new());
        denied.make_writes_fail();
        let result = denied
            .call("fs_write", json!({"path": "out.txt", "content": "hi"}))
            .await;
        assert!(!result.ok);
        let output = result.output.unwrap();
        assert!(output.contains("审计写入失败"), "{output}");
        assert!(output.contains("未执行"), "{output}");
        assert!(!denied.path_of("out.txt").exists());
        // 写失败是真实发生的：读回来仍是空表
        assert!(denied.approvals().is_empty());
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
    async fn summary_is_redacted_in_prompt_and_row() {
        let env = Env::with_redactor(
            ApprovalMode::Interactive,
            vec![ApprovalDecision::AllowOnce],
            Redactor::new(vec!["sk-x".into()]),
        );

        let result = env.call("shell_run", json!({"command": "echo sk-x"})).await;

        assert!(result.ok, "{:?}", result.output);
        // 审批方看到的摘要已脱敏（提示与 `approvals.summary` 同一份文案）
        let seen = env.approval.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].summary, "Running: echo ***");
        assert!(!seen[0].summary.contains("sk-x"), "{}", seen[0].summary);
        // 落库的摘要同样脱敏：`ignis task show` 会打印这一列
        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary, "Running: echo ***");
        assert!(!rows[0].summary.contains("sk-x"), "{}", rows[0].summary);
        // 工具输出也不带明文
        let output = result.output.unwrap();
        assert!(!output.contains("sk-x"), "{output}");
    }

    #[tokio::test]
    async fn symlink_to_sensitive_file_is_dangerous() {
        // `notes.txt -> .env`：参数看着无害，读的却是敏感文件
        let interactive = Env::new(ApprovalMode::Interactive, vec![ApprovalDecision::Reject]);
        std::fs::write(interactive.path_of(".env"), "K=v\n").unwrap();
        std::fs::write(interactive.path_of("a.txt"), "hi\n").unwrap();
        std::os::unix::fs::symlink(".env", interactive.path_of("notes.txt")).unwrap();

        let read = |path: &str| ToolCall {
            id: "c1".into(),
            name: "fs_read".into(),
            args: json!({"path": path}),
        };
        assert_eq!(
            classify(&read("notes.txt"), interactive.ws.path()),
            RiskLevel::Dangerous
        );
        // 普通文件仍是 Safe（不因解析而误报）
        assert_eq!(
            classify(&read("a.txt"), interactive.ws.path()),
            RiskLevel::Safe
        );

        // Interactive：软链接读要提示，且按 Dangerous 提示
        let result = interactive
            .call("fs_read", json!({"path": "notes.txt"}))
            .await;
        assert!(!result.ok);
        let seen = interactive.approval.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].summary, "Reading notes.txt");
        assert_eq!(seen[0].risk, RiskLevel::Dangerous);
        let rows = interactive.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].risk, "dangerous");
        assert_eq!(rows[0].decision, "deny");

        // Unattended：Dangerous 直接拒绝并留痕（若是 Safe 就既不管也不记）
        let unattended = Env::new(ApprovalMode::Unattended, Vec::new());
        std::fs::write(unattended.path_of(".env"), "K=v\n").unwrap();
        std::os::unix::fs::symlink(".env", unattended.path_of("notes.txt")).unwrap();
        let result = unattended
            .call("fs_read", json!({"path": "notes.txt"}))
            .await;
        assert!(!result.ok);
        assert!(
            result.output.unwrap().starts_with("denied by policy:"),
            "unattended 下应被拒绝"
        );
        assert!(unattended.approval.seen().is_empty());
        let rows = unattended.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, "fs_read");
        assert_eq!(rows[0].risk, "dangerous");
        assert_eq!(rows[0].decision, "deny");
        assert_eq!(rows[0].mode, "unattended");

        // 放行（--unsafe）时读到的确实是 `.env` 的内容：别名不是只看参数就能放过的
        let unsafe_env = Env::new(ApprovalMode::Preauthorized { unsafe_ops: true }, Vec::new());
        std::fs::write(unsafe_env.path_of(".env"), "K=v\n").unwrap();
        std::os::unix::fs::symlink(".env", unsafe_env.path_of("notes.txt")).unwrap();
        let result = unsafe_env
            .call("fs_read", json!({"path": "notes.txt"}))
            .await;
        assert!(result.ok, "{:?}", result.output);
        assert_eq!(result.output.as_deref(), Some("K=v\n"));
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

    #[test]
    fn meta_tools_classify_as_safe() {
        let call = |name: &str, args: Value| ToolCall {
            id: "c1".into(),
            name: name.into(),
            args,
        };
        let workspace = Path::new("/home/dev/app");

        // remember/load_skill/notify_user 只写 agent 自有状态（记忆事实、技能目录与告警收件箱，
        // CLI 可改可删）→ 恒 Safe；判定放在敏感路径与回退臂之前：参数里混进 `path` 也不改分类
        //（元工具不解析路径）
        for (name, args) in [
            (
                "remember",
                json!({"scope": "user", "key": "k", "text": "值"}),
            ),
            (
                "remember",
                json!({"scope": "project", "key": "k", "text": "值", "path": ".env"}),
            ),
            ("load_skill", json!({"name": "server-health"})),
            (
                "load_skill",
                json!({"name": "server-health", "path": "id_rsa"}),
            ),
            (
                "notify_user",
                json!({"severity": "warning", "title": "磁盘 92%"}),
            ),
            (
                "notify_user",
                json!({"severity": "critical", "title": "磁盘满了", "path": ".env"}),
            ),
        ] {
            assert_eq!(
                classify(&call(name, args), workspace),
                RiskLevel::Safe,
                "{name}"
            );
        }
    }

    /// `schedule_create` 恒定 `Elevated`：判定放在敏感路径检查与回退臂之前——元工具不解析路径，
    /// 参数里混进 `path` 也不会升级成 Dangerous（回退臂同样兜不住它，只是恰好同值）。
    #[test]
    fn schedule_create_classifies_as_elevated() {
        let call = |args: Value| ToolCall {
            id: "c1".into(),
            name: "schedule_create".into(),
            args,
        };
        let workspace = Path::new("/home/dev/app");

        for args in [
            json!({"name": "disk-watch", "prompt": "检查磁盘", "schedule": "10m"}),
            json!({"name": "standup", "prompt": "总结提交", "schedule": "0 9 * * 1-5"}),
            json!({
                "name": "reader",
                "prompt": "只读地看一眼",
                "schedule": "10m",
                "tools": ["fs_read"],
                "path": ".env",
            }),
        ] {
            assert_eq!(
                classify(&call(args.clone()), workspace),
                RiskLevel::Elevated,
                "{args}"
            );
        }
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
