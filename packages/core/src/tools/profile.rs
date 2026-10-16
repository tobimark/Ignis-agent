//! 调度运行的执行边界：白名单 + 风险上限执行器 [`ProfileExecutor`]。
//!
//! 调度运行以 `Preauthorized` 策略发起，job 的 profile 是模型能碰到什么的硬边界：
//! 白名单外的工具既不出现在 [`ProfileExecutor::specs`]（模型看不见），直接调用也会在到达
//! [`ToolRuntime`] 之前被拒掉，并留一条 `deny` 审批记录（mode = `preauthorized`）——被拒的调用
//! 永远不执行，也不产生副作用。白名单内的调用还要过风险上限：`risk=safe` 的 job 只跑分类为
//! `Safe` 的调用，`shell_run` / `fs_write` 这类「会改动系统或外发数据」的调用在这里被拒（内层的
//! 预授权策略本来会放行它们，那是 `risk=elevated` 的语义）。两条边界都留同一形状的 `deny` 记录。
//! 过了边界的调用原样委托给内层执行器：它自己的策略、审批提示与留痕照常生效。

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use ignis_runtime::{RiskLevel, ToolCall, ToolExecutor, ToolResult, ToolSpec};
use ignis_storage::{Db, NewApproval, now_millis};
use tokio_util::sync::CancellationToken;

use super::ToolRuntime;
use super::runtime::risk_name;
use crate::scheduler::ProfileRisk;

/// 执行边界执行器：包住一个 [`ToolRuntime`]，把「job 允许干什么」钉在它前面。
pub struct ProfileExecutor {
    inner: Arc<ToolRuntime>,
    allowed: HashSet<String>,
    /// job profile 的风险上限：`Safe` 时白名单内的非 `Safe` 调用同样被拒。
    risk: ProfileRisk,
    db: Arc<Db>,
    task_id: String,
}

impl ProfileExecutor {
    /// 包装内层执行器：`allowed` / `risk` 是 job profile 的工具白名单与风险上限，`db` /
    /// `task_id` 供两条拒绝路径留痕（`approvals` 行的字段与执行器本体同源）。
    pub fn new(
        inner: Arc<ToolRuntime>,
        allowed: HashSet<String>,
        risk: ProfileRisk,
        db: Arc<Db>,
        task_id: String,
    ) -> Self {
        Self {
            inner,
            allowed,
            risk,
            db,
            task_id,
        }
    }

    /// 白名单外的调用。
    fn deny_outside_allowlist(&self, call: &ToolCall) -> ToolResult {
        self.deny(
            call,
            format!("profile 拒绝：{}", call.name),
            format!("该任务 profile 不允许工具 `{}`（已记录）", call.name),
        )
    }

    /// 白名单内、但超出 profile 风险上限（`risk=safe`）的调用。
    fn deny_over_risk(&self, call: &ToolCall) -> ToolResult {
        self.deny(
            call,
            format!("profile 风险上限拒绝：{}", call.name),
            format!(
                "该任务 profile 风险上限为 safe，拒绝 `{}`（已记录）",
                call.name
            ),
        )
    }

    /// 两条边界的统一拒绝路径：留一条 `deny` / `preauthorized` 记录（`risk` 记该调用的真实分类，
    /// 不是编出来的等级）并把人话拒绝理由交回模型。
    ///
    /// 记录写不进去时必须说清楚审计失败，不能回「已记录」的假话（拒绝本身不依赖写成功——本来就
    /// 执行不了）。
    fn deny(&self, call: &ToolCall, summary: String, message: String) -> ToolResult {
        let approval = NewApproval {
            task_id: self.task_id.clone(),
            tool: call.name.clone(),
            summary: summary.clone(),
            // 记该调用的真实分类（白名单外的只读工具记 `safe`），与执行器本体的口径一致
            risk: risk_name(self.inner.classify_call(call)).to_string(),
            decision: "deny".to_string(),
            mode: "preauthorized".to_string(),
            decided_at: now_millis(),
        };
        match self.db.insert_approval(&approval) {
            Ok(()) => ToolResult {
                ok: false,
                output: Some(message),
            },
            Err(error) => ToolResult {
                ok: false,
                output: Some(format!("审计写入失败，未执行：{summary}（{error}）")),
            },
        }
    }
}

#[async_trait]
impl ToolExecutor for ProfileExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner
            .specs()
            .into_iter()
            .filter(|spec| self.allowed.contains(&spec.name))
            .collect()
    }

    fn summarize(&self, call: &ToolCall) -> String {
        self.inner.summarize(call)
    }

    async fn execute(&self, call: ToolCall, cancel: CancellationToken) -> ToolResult {
        // 白名单外的调用不落到内层：内层连「未知工具」这条错误路径都不会被走到
        if !self.allowed.contains(&call.name) {
            return self.deny_outside_allowlist(&call);
        }
        // 风险上限：分类口径与内层闸门同一份（`ToolRuntime::classify_call`），`risk=safe` 的 job
        // 里白名单放行的 shell_run / fs_write 在这里就被拒，不会因为预授权策略而悄悄跑起来
        if self.risk == ProfileRisk::Safe && self.inner.classify_call(&call) != RiskLevel::Safe {
            return self.deny_over_risk(&call);
        }
        self.inner.execute(call, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use ignis_runtime::test_support::ScriptedApproval;
    use ignis_runtime::{Limits, ToolCall, ToolExecutor, ToolResult, ToolSpec};
    use ignis_storage::{ApprovalRecord, Db};
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::ProfileExecutor;
    use crate::scheduler::ProfileRisk;
    use crate::security::{ApprovalMode, Policy, Redactor};
    use crate::tools::{Tool, ToolCtx, ToolError, ToolRuntime, default_registry};

    /// 本次任务的 id：审批记录按它归属。
    const TASK_ID: &str = "01J9Z6Q3T7R8S9V0W1X2Y3Z4AB";

    /// 单个测试独占的临时工作区，析构时清理（本 crate 不引入 tempfile 依赖）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-tools-profile-{}-{}",
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

    /// 记录型工具：每执行一次就 +1；白名单边界的两侧（没被调用 / 委托到了）都靠它证明。
    struct Recorder {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for Recorder {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "recorder".into(),
                description: "Test double that counts every execution.".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }

        async fn run(&self, _args: &Value, _ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok("recorder ran".into())
        }
    }

    /// 测试环境：内层是真实的 `ToolRuntime`（注册表 = 内置工具 + 记录型工具），外层按用例给的白
    /// 名单包 [`ProfileExecutor`]。
    struct Env {
        dir: TempDir,
        db: Arc<Db>,
        inner: Arc<ToolRuntime>,
        recorder_calls: Arc<AtomicUsize>,
    }

    impl Env {
        fn new() -> Env {
            let dir = TempDir::new();
            let db = Arc::new(Db::open_in_memory().unwrap());
            let recorder_calls = Arc::new(AtomicUsize::new(0));
            let mut registry = default_registry();
            registry.register(Box::new(Recorder {
                calls: Arc::clone(&recorder_calls),
            }));
            let inner = Arc::new(ToolRuntime::new(
                registry,
                // 调度运行的策略形状：预授权、危险操作不放行（内层自己会留痕）
                Arc::new(Policy::new(ApprovalMode::Preauthorized {
                    unsafe_ops: false,
                })),
                ScriptedApproval::new(Vec::new()),
                Arc::clone(&db),
                TASK_ID.to_string(),
                dir.path().to_path_buf(),
                Limits::default(),
                Redactor::new(Vec::new()),
                None,
            ));
            Env {
                dir,
                db,
                inner,
                recorder_calls,
            }
        }

        /// 按给定的工具白名单包一层；风险上限默认 `elevated`（`schedule_create` 的默认值）。
        fn profiled(&self, allowed: &[&str]) -> ProfileExecutor {
            self.profiled_as(allowed, ProfileRisk::Elevated)
        }

        /// 指定风险上限包一层（`risk=safe` 的执行边界用例）。
        fn profiled_as(&self, allowed: &[&str], risk: ProfileRisk) -> ProfileExecutor {
            ProfileExecutor::new(
                Arc::clone(&self.inner),
                allowed
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect::<HashSet<String>>(),
                risk,
                Arc::clone(&self.db),
                TASK_ID.to_string(),
            )
        }

        /// 审批记录（按决定先后）。
        fn approvals(&self) -> Vec<ApprovalRecord> {
            self.db.list_approvals(TASK_ID).unwrap()
        }

        /// 记录型工具被真正执行的次数。
        fn recorder_calls(&self) -> usize {
            self.recorder_calls.load(Ordering::Relaxed)
        }
    }

    /// 一次调用（`id` 由工具名派生，断言里用不到）。
    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            name: name.to_string(),
            args,
        }
    }

    async fn execute(executor: &ProfileExecutor, call: ToolCall) -> ToolResult {
        executor.execute(call, CancellationToken::new()).await
    }

    /// `specs()` 只含白名单里注册过的工具，顺序仍是内层注册顺序；白名单里没注册的名字不会凭空
    /// 出现。
    #[tokio::test]
    async fn specs_only_expose_the_allowlist_in_registry_order() {
        let env = Env::new();
        let executor = env.profiled(&["fs_read", "recorder", "ghost"]);

        let names: Vec<String> = executor.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(names, ["fs_read", "recorder"]);
        // 白名单外的内置工具一个都不可见
        assert!(!names.contains(&"fs_write".to_string()), "{names:?}");

        // 内核要求 ToolExecutor: Send + Sync
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ProfileExecutor>();
    }

    /// 白名单外的调用：拒绝 + 一条 `deny` / `preauthorized` 记录（risk 是该调用的真实分类），
    /// 且不碰内层。
    #[tokio::test]
    async fn a_tool_outside_the_allowlist_is_denied_recorded_and_never_reaches_inner() {
        let env = Env::new();
        let executor = env.profiled(&["fs_read"]);

        let denied = execute(&executor, call("recorder", json!({}))).await;
        assert!(!denied.ok);
        assert_eq!(
            denied.output.as_deref(),
            Some("该任务 profile 不允许工具 `recorder`（已记录）")
        );
        assert_eq!(env.recorder_calls(), 0, "白名单外的调用不得落到内层");

        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task_id, TASK_ID);
        assert_eq!(rows[0].tool, "recorder");
        assert_eq!(rows[0].summary, "profile 拒绝：recorder");
        assert_eq!(rows[0].decision, "deny");
        assert_eq!(rows[0].mode, "preauthorized");
        assert!(rows[0].decided_at > 0);
        // 记录里是调用的真实分类，不是编出来的等级
        assert_eq!(rows[0].risk, "elevated");

        // 只读工具被拒时同样留痕，但 risk 记 `safe`
        let read = execute(&executor, call("fs_list", json!({"path": "."}))).await;
        assert!(!read.ok);
        assert_eq!(
            read.output.as_deref(),
            Some("该任务 profile 不允许工具 `fs_list`（已记录）")
        );
        let rows = env.approvals();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].risk, "safe");
    }

    /// 被拒的写工具不得产生任何副作用（拒绝发生在内层之前）。
    #[tokio::test]
    async fn a_denied_write_tool_leaves_no_side_effects() {
        let env = Env::new();
        let executor = env.profiled(&["fs_read"]);

        let denied = execute(
            &executor,
            call("fs_write", json!({"path": "out.txt", "content": "hi"})),
        )
        .await;

        assert!(!denied.ok);
        assert!(
            denied
                .output
                .unwrap()
                .contains("profile 不允许工具 `fs_write`")
        );
        assert!(!env.dir.path().join("out.txt").exists(), "拒绝后不得落盘");
        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary, "profile 拒绝：fs_write");
        assert_eq!(rows[0].risk, "elevated");
    }

    /// 白名单内的调用委托给内层：工具真的执行，内层自己的审批/审计（预授权 → allow）照常生效；
    /// `summarize` 也原样走内层。
    #[tokio::test]
    async fn an_allowlisted_tool_is_delegated_to_inner() {
        let env = Env::new();
        let executor = env.profiled(&["recorder"]);

        let recorder = call("recorder", json!({}));
        assert_eq!(executor.summarize(&recorder), "Running recorder");

        let result = execute(&executor, recorder).await;

        assert!(result.ok, "{:?}", result.output);
        assert_eq!(result.output.as_deref(), Some("recorder ran"));
        assert_eq!(env.recorder_calls(), 1, "白名单内的调用必须委托到内层");
        // 记录行只有内层那一条（委托路径上不再额外写 profile 记录），文案也是内层的摘要
        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, "recorder");
        assert_eq!(rows[0].summary, "Running recorder");
        assert_eq!(
            (rows[0].decision.as_str(), rows[0].mode.as_str()),
            ("allow", "preauthorized")
        );
    }

    /// `risk=safe` 的 job：白名单里的非 Safe 调用（`shell_run`）也被拒——分类口径与内层闸门同一份，
    /// 拒绝留一条 `deny` / `preauthorized` 记录（risk 记真实等级 `elevated`），且不碰内层。
    #[tokio::test]
    async fn a_safe_profile_denies_an_allowlisted_non_safe_call() {
        let env = Env::new();
        let executor = env.profiled_as(&["shell_run"], ProfileRisk::Safe);

        let denied = execute(
            &executor,
            call("shell_run", json!({"command": "touch side-effect.txt"})),
        )
        .await;

        assert!(!denied.ok);
        assert_eq!(
            denied.output.as_deref(),
            Some("该任务 profile 风险上限为 safe，拒绝 `shell_run`（已记录）")
        );
        assert!(
            !env.dir.path().join("side-effect.txt").exists(),
            "被拒的调用不得落到内层（命令真的没跑）"
        );
        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, "shell_run");
        assert_eq!(rows[0].summary, "profile 风险上限拒绝：shell_run");
        assert_eq!(rows[0].decision, "deny");
        assert_eq!(rows[0].mode, "preauthorized");
        assert_eq!(rows[0].risk, "elevated");
        assert!(rows[0].decided_at > 0);
    }

    /// `risk=elevated`（默认）：白名单里的 `shell_run` 照常委托到内层，内层的预授权记录照旧。
    #[tokio::test]
    async fn an_elevated_profile_allows_an_allowlisted_non_safe_call() {
        let env = Env::new();
        let executor = env.profiled_as(&["shell_run"], ProfileRisk::Elevated);

        let ok = execute(&executor, call("shell_run", json!({"command": "echo hi"}))).await;

        assert!(ok.ok, "{:?}", ok.output);
        let output = ok.output.unwrap();
        assert!(output.contains("exit code: 0"), "{output}");
        assert!(output.contains("hi"), "{output}");
        // 放行路径不再多写 profile 记录：只有内层那条 allow/preauthorized
        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, "shell_run");
        assert_eq!(
            (rows[0].decision.as_str(), rows[0].mode.as_str()),
            ("allow", "preauthorized")
        );
    }

    /// `risk=safe` 的白名单内 Safe 调用（`fs_read`）不受风险上限影响：照常执行。
    #[tokio::test]
    async fn a_safe_profile_allows_a_safe_call() {
        let env = Env::new();
        std::fs::write(env.dir.path().join("a.txt"), "hello").unwrap();
        let executor = env.profiled_as(&["fs_read"], ProfileRisk::Safe);

        let ok = execute(&executor, call("fs_read", json!({"path": "a.txt"}))).await;

        assert!(ok.ok, "{:?}", ok.output);
        let output = ok.output.unwrap();
        assert!(output.contains("hello"), "{output}");
        // Safe 调用内层不记录（审批表只留需要判定的调用）
        assert!(env.approvals().is_empty());
    }

    /// 审计写不进去时必须说清楚（不能回「已记录」的假话），并且照样不执行。
    #[tokio::test]
    async fn a_failed_audit_write_is_reported_instead_of_claiming_a_record() {
        let env = Env::new();
        let executor = env.profiled(&["fs_read"]);
        // 共享连接上的写操作失败（`PRAGMA query_only` 后 INSERT 报只读库错误），读仍然可用
        env.db
            .with_conn(|conn| conn.execute_batch("PRAGMA query_only = ON"))
            .unwrap();

        let denied = execute(&executor, call("recorder", json!({}))).await;

        assert!(!denied.ok);
        let output = denied.output.unwrap();
        assert!(output.contains("审计写入失败"), "{output}");
        assert!(output.contains("未执行"), "{output}");
        assert!(output.contains("profile 拒绝：recorder"), "{output}");
        assert_eq!(env.recorder_calls(), 0);
        assert!(env.approvals().is_empty(), "写失败是真实发生的：表里仍是空");
    }
}
