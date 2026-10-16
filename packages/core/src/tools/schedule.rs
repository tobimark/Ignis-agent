//! `schedule_create` 元工具：把「每隔 10 分钟检查一次」这类请求落成一条 `jobs` 行。
//!
//! 解析与校验全部委托给 [`crate::scheduler`]（与守护进程共用同一份 interval/cron 语义与 profile
//! 校验）：`schedule` 参数先按 interval 解析、失败再按 cron，两边都不认就报中文错误。工具本身只做
//! 三件事——组装 profile/limits、拒绝不可用的工具名、插行并回一段可读摘要（摘要既是审批提示的
//! 上下文，也是模型回读的凭据）。
//!
//! 分类为 `Elevated`（见 [`crate::tools::ToolRuntime`] 的 `classify`）：创建 job 决定的是「以后
//! 自动干什么」，交互运行弹一次确认、`--yes` 预授权、无人值守直接拒绝。真正限制 job 能干什么的
//! 是它自己的 profile 白名单（执行时由 [`crate::tools::ProfileExecutor`] 兜底）。

use std::sync::Arc;

use async_trait::async_trait;
use ignis_runtime::ToolSpec;
use ignis_storage::{Db, NewJob};
use serde_json::{Value, json};

use super::util::{optional_str, required_str};
use super::{Tool, ToolCtx, ToolError};
use crate::scheduler::{
    ProfileRisk, ScheduleKind, SchedulerError, parse_profile, parse_schedule, validate_job,
};

/// `schedule_create` 的注入依赖。
///
/// CLI 与守护进程装配 [`super::meta_tools`] 时构造它：`available_tools` 取
/// [`super::available_tool_names()`]（两侧因此共用同一份「可用工具」口径）。工具执行时的工作区以
/// [`ToolCtx::workspace`] 为准（不另存默认工作区：调度任务的工作区就是创建它的那次运行的工作区）。
pub struct SchedulerDeps {
    /// 落 `jobs` 行的库。
    pub db: Arc<Db>,
    /// 可用工具名清单：新 job 的 profile 白名单必须 ⊆ 它，否则当场拒绝。
    pub available_tools: Vec<String>,
}

/// 不传 `tools` 时的默认白名单：读两个文件工具 + 跑脚本 + 通知用户。
const DEFAULT_TOOLS: [&str; 4] = ["fs_read", "fs_list", "shell_run", "notify_user"];

/// 调度运行的内核：v0.1 只跑 native（决策 #18），没有第二个选项，也就没有对应参数。
const KERNEL: &str = "native";

/// 调度任务创建工具；依赖由 [`super::meta_tools`] 注入。
pub(super) struct ScheduleCreate {
    deps: SchedulerDeps,
}

impl ScheduleCreate {
    pub(super) fn new(deps: SchedulerDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Tool for ScheduleCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "schedule_create".into(),
            description: "Create a recurring job for this workspace: the Ignis daemon runs the \
                          prompt on the given schedule under a restricted profile, so only the \
                          listed tools are available to it. Give an interval like `10m`/`2h` or \
                          a five-field cron expression (`min hour dom mon dow`) like \
                          `0 9 * * 1-5`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Short job name shown by `ignis schedule list`, e.g. `disk-watch`"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Instruction the job runs on every fire; it should be self-contained"
                    },
                    "schedule": {
                        "type": "string",
                        "description": "Interval like `10m`/`2h`, or a five-field cron expression like `0 9 * * 1-5`"
                    },
                    "tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tools the job may use; defaults to fs_read, fs_list, shell_run and notify_user"
                    },
                    "risk": {
                        "type": "string",
                        "enum": ["safe", "elevated"],
                        "description": "Risk level of the job profile; `elevated` lets the job run tools that would otherwise ask for approval. Defaults to elevated"
                    }
                },
                "required": ["name", "prompt", "schedule"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
        let name = required_str(args, "name")?;
        if name.trim().is_empty() {
            return Err(ToolError::message(
                "参数 `name` 不能是空白：给这个调度任务一个能认出它的短名字（如 disk-watch）",
            ));
        }
        let prompt = required_str(args, "prompt")?;
        if prompt.trim().is_empty() {
            return Err(ToolError::message(
                "参数 `prompt` 不能是空白：调度任务每次到期都要执行它，必须是一条自包含的指令",
            ));
        }
        let schedule = required_str(args, "schedule")?.trim();
        let tools = tools_arg(args)?;
        let risk = risk_arg(args)?;

        // 表达式先按 interval、失败再按 cron：两种写法与守护进程共用 `crate::scheduler` 的同一份
        // 解析，这里只判定类型（落库的 `schedule_kind`）
        let kind = resolve_kind(schedule)?;
        // profile 走 T2 的 `parse_profile`：空列表、空白项、重名与字段形状由同一份校验兜底
        let raw_profile = serde_json::to_string(&json!({"tools": tools, "risk": risk}))
            .map_err(|error| ToolError::message(format!("任务 profile 组装失败：{error}")))?;
        let profile = parse_profile(&raw_profile).map_err(scheduler_failure)?;
        validate_job(&profile, &self.deps.available_tools).map_err(scheduler_failure)?;

        let job = NewJob {
            // 每建一个 job 都是一条新行：10 位小写十六进制短 id（见 [`new_job_id`]）
            id: new_job_id(),
            name: name.to_string(),
            prompt: prompt.to_string(),
            schedule_kind: kind.as_str().to_string(),
            schedule_expr: schedule.to_string(),
            kernel: KERNEL.to_string(),
            // 落库的是校验通过的规范形状（工具名已裁空白、顺序不变）
            profile_json: serde_json::to_string(&profile)
                .map_err(|error| ToolError::message(format!("任务 profile 序列化失败：{error}")))?,
            // limits 取 `ctx.limits` 的全量 serde 形状：字段名与运行时逐个一致，守护进程建任务时
            // 原样反序列化回 `Limits`
            limits_json: serde_json::to_string(ctx.limits)
                .map_err(|error| ToolError::message(format!("任务限额序列化失败：{error}")))?,
            // job 在哪个工作区跑：就是本次任务的工作区（`--workspace` / 配置里的根）
            workspace: ctx.workspace.display().to_string(),
        };
        self.deps
            .db
            .insert_job(&job)
            .map_err(|error| ToolError::message(format!("调度任务入库失败：{error}")))?;

        // 摘要既是审批提示的上下文，也是模型回读的凭据：名字、节奏、白名单与风险等级一次讲清
        Ok(format!(
            "已创建调度任务 {name}（{schedule}，工具：{}，risk={}）—— 若守护进程未运行请执行 \
             ignis daemon start",
            profile.tools.join(","),
            risk_name(risk),
        ))
    }
}

/// 新 job 的 id：`Uuid::new_v4()` 的 simple（32 位小写十六进制）串取前 [`JOB_ID_CHARS`] 位。
///
/// 短 id 是给人用的：`schedule list` / `history` / `alerts` 直接展示整串，用户能把它原样粘回
/// `pause` / `resume` / `remove` / `history`。不用 uuid v7 的前缀——它的前 8 个十六进制字符是
/// 毫秒时间戳的高位（约每 65 秒才进位），相近时刻建的 job 会撞成同一个前缀。
fn new_job_id() -> String {
    // simple 形式固定 32 个字符，取前 10 位不会越界
    uuid::Uuid::new_v4().simple().to_string()[..JOB_ID_CHARS].to_string()
}

/// job id 的字符数：16^10 ≈ 1.1e12 种取值（uuid v4 随机源），本地调度器足够用。
const JOB_ID_CHARS: usize = 10;

/// 自动判定调度类型：先按 interval 解析（`10m`、`2h`），失败再按 5 字段 cron（`0 9 * * 1-5`）；
/// 两边都不认时报中文错误，点名表达式与两种可接受写法。
fn resolve_kind(expr: &str) -> Result<ScheduleKind, ToolError> {
    if parse_schedule("interval", expr).is_ok() {
        return Ok(ScheduleKind::Interval);
    }
    if parse_schedule("cron", expr).is_ok() {
        return Ok(ScheduleKind::Cron);
    }
    Err(ToolError::message(format!(
        "无法解析调度表达式「{expr}」：请用间隔形式（如 10m、2h）或 5 字段 cron 表达式（如 \
         0 9 * * 1-5）"
    )))
}

/// `tools` 参数：缺省（或显式 `null`）取 [`DEFAULT_TOOLS`]；类型不对、项不是字符串立刻报中文
/// 错误，空列表 / 空白项 / 重名留给 [`parse_profile`]。
fn tools_arg(args: &Value) -> Result<Vec<String>, ToolError> {
    match args.get("tools") {
        None | Some(Value::Null) => Ok(DEFAULT_TOOLS
            .iter()
            .map(|name| (*name).to_owned())
            .collect()),
        Some(value) => value
            .as_array()
            .ok_or_else(|| ToolError::message("参数 `tools` 必须是字符串数组"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| ToolError::message("参数 `tools` 的每一项都必须是字符串"))
            })
            .collect(),
    }
}

/// `risk` 参数：缺省 `elevated`（job 可执行会改动系统或外发数据的工具）；只认 `safe` / `elevated`。
fn risk_arg(args: &Value) -> Result<ProfileRisk, ToolError> {
    match optional_str(args, "risk")? {
        None => Ok(ProfileRisk::Elevated),
        Some("safe") => Ok(ProfileRisk::Safe),
        Some("elevated") => Ok(ProfileRisk::Elevated),
        Some(other) => Err(ToolError::message(format!(
            "参数 `risk` 只能是 safe 或 elevated，收到「{other}」"
        ))),
    }
}

/// `profile_json` / 摘要里的风险等级字面量（与 [`ProfileRisk`] 的 serde 小写名一致）。
fn risk_name(risk: ProfileRisk) -> &'static str {
    match risk {
        ProfileRisk::Safe => "safe",
        ProfileRisk::Elevated => "elevated",
    }
}

/// 调度层错误转工具错误：解析与校验的消息本身就是中文人话（并带上出错的表达式或工具名），
/// 原样回填给模型。
fn scheduler_failure(error: SchedulerError) -> ToolError {
    ToolError::message(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use ignis_runtime::test_support::ScriptedApproval;
    use ignis_runtime::{
        ApprovalDecision, ApprovalHandler, Limits, RiskLevel, ToolCall, ToolExecutor, ToolResult,
    };
    use ignis_storage::{ApprovalRecord, Db, JobRecord};
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::{ScheduleCreate, SchedulerDeps};
    use crate::scheduler::{ProfileRisk, parse_limits, parse_profile};
    use crate::security::{ApprovalMode, Policy, Redactor};
    use crate::tools::{ToolRuntime, available_tool_names, default_registry};

    /// 本次任务的 id：审批记录按它归属。
    const TASK_ID: &str = "01J9Z6Q3T7R8S9V0W1X2Y3Z4AB";

    /// 单个测试独占的临时工作区，析构时清理（本 crate 不引入 tempfile 依赖）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-tools-schedule-{}-{}",
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

    /// `schedule_create` 的测试环境：真实内存库 + 临时工作区 + 脚本化审批。
    ///
    /// 注册表 = 内置工具 + `ScheduleCreate`（生产装配在 `meta_tools`，这里直接挂上，只观察这一个
    /// 工具的行为）。白名单校验用的可用工具名取生产同一份 [`available_tool_names`]。
    struct Env {
        dir: TempDir,
        db: Arc<Db>,
        approval: Arc<ScriptedApproval>,
        runtime: ToolRuntime,
    }

    impl Env {
        /// 交互运行：按序弹出排好的决定，队列耗尽即拒绝。
        fn interactive(decisions: Vec<ApprovalDecision>) -> Env {
            Env::build(ApprovalMode::Interactive, decisions, Limits::default())
        }

        /// `--yes`：预授权策略直接放行，留 `allow` / `preauthorized` 行。
        fn preauthorized() -> Env {
            Env::build(
                ApprovalMode::Preauthorized { unsafe_ops: false },
                Vec::new(),
                Limits::default(),
            )
        }

        /// 无人值守：非 `Safe` 调用一律拒绝并留 `deny` / `unattended` 行。
        fn unattended() -> Env {
            Env::build(ApprovalMode::Unattended, Vec::new(), Limits::default())
        }

        /// 预授权 + 自定义上限：验证 `limits_json` 就是 `ctx.limits` 的全量 serde 形状。
        fn preauthorized_with_limits(limits: Limits) -> Env {
            Env::build(
                ApprovalMode::Preauthorized { unsafe_ops: false },
                Vec::new(),
                limits,
            )
        }

        fn build(mode: ApprovalMode, decisions: Vec<ApprovalDecision>, limits: Limits) -> Env {
            let dir = TempDir::new();
            let db = Arc::new(Db::open_in_memory().unwrap());
            let mut registry = default_registry();
            // 工具正常路径的 job 工作区取 `ctx.workspace`（就是这里的临时目录）
            registry.register(Box::new(ScheduleCreate::new(SchedulerDeps {
                db: Arc::clone(&db),
                available_tools: available_tool_names(),
            })));
            let approval = ScriptedApproval::new(decisions);
            let runtime = ToolRuntime::new(
                registry,
                Arc::new(Policy::new(mode)),
                Arc::clone(&approval) as Arc<dyn ApprovalHandler>,
                Arc::clone(&db),
                TASK_ID.to_string(),
                dir.path().to_path_buf(),
                limits,
                Redactor::new(Vec::new()),
                // 交互运行没有来源 job
                None,
            );
            Env {
                dir,
                db,
                approval,
                runtime,
            }
        }

        async fn call(&self, args: Value) -> ToolResult {
            let call = ToolCall {
                id: "call-schedule_create".into(),
                name: "schedule_create".into(),
                args,
            };
            self.runtime.execute(call, CancellationToken::new()).await
        }

        /// 审批记录（按决定先后）。
        fn approvals(&self) -> Vec<ApprovalRecord> {
            self.db.list_approvals(TASK_ID).unwrap()
        }

        /// `jobs` 表里的行（含暂停的）。
        fn jobs(&self) -> Vec<JobRecord> {
            self.db.list_jobs(true).unwrap()
        }
    }

    /// 一份合法的默认参数（各用例只改自己关心的字段）。
    fn sample_args() -> Value {
        json!({"name": "disk-watch", "prompt": "检查磁盘，超过 85% 就告警", "schedule": "10m"})
    }

    /// epoch 毫秒；用来把 `created_at` 夹在调用前后之间。
    fn now_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }

    /// 全流程：interval 表达式 → Elevated 审批 → jobs 行（唯一 id、活跃、native、默认 profile 与
    /// limits）→ 摘要文案逐字。
    #[tokio::test]
    async fn interval_expression_creates_an_active_native_job_with_the_defaults() {
        let env = Env::interactive(vec![ApprovalDecision::AllowOnce]);
        let before = now_millis();

        let result = env.call(sample_args()).await;
        let after = now_millis();

        assert!(result.ok, "{:?}", result.output);
        assert_eq!(
            result.output.as_deref(),
            Some(
                "已创建调度任务 disk-watch（10m，工具：fs_read,fs_list,shell_run,notify_user，\
                 risk=elevated）—— 若守护进程未运行请执行 ignis daemon start"
            )
        );

        // Elevated：交互模式问一次，提示里带完整参数与风险等级
        let seen = env.approval.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].tool, "schedule_create");
        assert_eq!(seen[0].summary, "Running schedule_create");
        assert_eq!(seen[0].risk, RiskLevel::Elevated);
        assert_eq!(seen[0].args, sample_args());
        let rows = env.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].decision.as_str(), rows[0].mode.as_str()),
            ("allow", "interactive")
        );
        assert_eq!(rows[0].risk, "elevated");

        // jobs 行：每 job 一个 10 位短 id、活跃、native、工作区取 ctx.workspace
        let jobs = env.jobs();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.name, "disk-watch");
        assert_eq!(job.prompt, "检查磁盘，超过 85% 就告警");
        assert_eq!(job.schedule_kind, "interval");
        assert_eq!(job.schedule_expr, "10m");
        assert_eq!(job.kernel, "native");
        assert_eq!(job.workspace, env.dir.path().display().to_string());
        assert_eq!(job.status, "active");
        assert_eq!(job.last_run_at, None);
        assert!(
            job.created_at >= before && job.created_at <= after,
            "created_at = {} 不在 [{before}, {after}] 内",
            job.created_at
        );
        // 不是完整 uuid、也不带 version 位：10 位小写十六进制（uuid v4 随机串的前缀）
        assert_eq!(job.id.len(), 10, "job id = {}", job.id);
        assert!(
            job.id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "job id 应是 10 位小写十六进制：{}",
            job.id
        );
        assert_eq!(
            job.profile_json,
            r#"{"tools":["fs_read","fs_list","shell_run","notify_user"],"risk":"elevated"}"#
        );

        // limits 是 `ctx.limits` 的全量 serde 形状：只能是真实字段名
        assert_eq!(parse_limits(&job.limits_json).unwrap(), Limits::default());
        assert!(
            job.limits_json.contains("\"max_steps\""),
            "{}",
            job.limits_json
        );
        assert!(
            !job.limits_json.contains("max_turns"),
            "{}",
            job.limits_json
        );
    }

    /// interval 解析失败后按 cron 再试一次：`0 9 * * 1-5` 落成 cron 行，摘要回原表达式。
    #[tokio::test]
    async fn cron_expression_is_detected_when_interval_parsing_fails() {
        let env = Env::preauthorized();

        let result = env
            .call(json!({
                "name": "standup",
                "prompt": "总结昨天的提交",
                "schedule": "0 9 * * 1-5",
            }))
            .await;

        assert!(result.ok, "{:?}", result.output);
        assert_eq!(
            result.output.as_deref(),
            Some(
                "已创建调度任务 standup（0 9 * * 1-5，工具：fs_read,fs_list,shell_run,notify_user，\
                 risk=elevated）—— 若守护进程未运行请执行 ignis daemon start"
            )
        );
        let jobs = env.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].schedule_kind, "cron");
        assert_eq!(jobs[0].schedule_expr, "0 9 * * 1-5");
    }

    /// 每建一个 job 就是一条新行、一个新 id。
    #[tokio::test]
    async fn every_created_job_gets_a_fresh_id() {
        let env = Env::interactive(vec![
            ApprovalDecision::AllowOnce,
            ApprovalDecision::AllowOnce,
        ]);

        for name in ["first", "second"] {
            let result = env
                .call(json!({"name": name, "prompt": "干活", "schedule": "30m"}))
                .await;
            assert!(result.ok, "{name}: {:?}", result.output);
        }

        let jobs = env.jobs();
        assert_eq!(jobs.len(), 2);
        assert_ne!(jobs[0].id, jobs[1].id);
    }

    /// 白名单里有不存在的工具 → 中文错误一次列出全部，且不落行。
    #[tokio::test]
    async fn unknown_tool_names_are_rejected_in_one_chinese_error() {
        let env = Env::preauthorized();

        let result = env
            .call(json!({
                "name": "bad",
                "prompt": "干活",
                "schedule": "10m",
                "tools": ["fs_read", "nope", "also_nope"],
            }))
            .await;

        assert!(!result.ok);
        let output = result.output.unwrap();
        assert!(output.contains("不可用的工具"), "{output}");
        assert!(output.contains("nope、also_nope"), "{output}");
        assert!(env.jobs().is_empty(), "白名单校验失败不得落行");
    }

    /// 默认值：`tools` 缺省是四件套、`risk` 缺省 `elevated`；limits 取 `ctx.limits` 全量。
    #[tokio::test]
    async fn defaults_come_from_the_ctx_and_the_locked_default_table() {
        let limits = Limits {
            max_steps: 7,
            tool_output_limit: 1234,
            ..Limits::default()
        };
        let env = Env::preauthorized_with_limits(limits);

        let result = env
            .call(json!({"name": "custom", "prompt": "干活", "schedule": "2h"}))
            .await;

        assert!(result.ok, "{:?}", result.output);
        assert_eq!(
            result.output.as_deref(),
            Some(
                "已创建调度任务 custom（2h，工具：fs_read,fs_list,shell_run,notify_user，\
                 risk=elevated）—— 若守护进程未运行请执行 ignis daemon start"
            )
        );
        let job = &env.jobs()[0];
        assert_eq!(parse_limits(&job.limits_json).unwrap(), limits);
        assert!(
            job.limits_json.contains("\"tool_output_limit\":1234"),
            "{}",
            job.limits_json
        );
    }

    /// 显式 `tools` / `risk` 原样落库并在摘要里回显。
    #[tokio::test]
    async fn explicit_tools_and_risk_are_kept() {
        let env = Env::preauthorized();

        let result = env
            .call(json!({
                "name": "reader",
                "prompt": "只看一眼",
                "schedule": "15m",
                "tools": ["fs_read"],
                "risk": "safe",
            }))
            .await;

        assert!(result.ok, "{:?}", result.output);
        assert_eq!(
            result.output.as_deref(),
            Some(
                "已创建调度任务 reader（15m，工具：fs_read，risk=safe）—— \
                 若守护进程未运行请执行 ignis daemon start"
            )
        );
        let job = &env.jobs()[0];
        assert_eq!(job.profile_json, r#"{"tools":["fs_read"],"risk":"safe"}"#);
        let profile = parse_profile(&job.profile_json).unwrap();
        assert_eq!(profile.tools, ["fs_read"]);
        assert_eq!(profile.risk, ProfileRisk::Safe);
    }

    /// 两种形式都解析不了 → 中文错误，点名表达式与两种可接受写法，且不落行。
    #[tokio::test]
    async fn an_unparsable_schedule_names_the_expression_and_both_forms() {
        let env = Env::preauthorized();

        let result = env
            .call(json!({"name": "bad", "prompt": "干活", "schedule": "every 10 minutes"}))
            .await;

        assert!(!result.ok);
        let output = result.output.unwrap();
        assert!(output.contains("「every 10 minutes」"), "{output}");
        assert!(output.contains("10m"), "{output}");
        assert!(output.contains("cron"), "{output}");
        assert!(output.contains("0 9 * * 1-5"), "{output}");
        assert!(env.jobs().is_empty());
    }

    /// 空白 `name` / 空白 `prompt`：拒掉（中文错误），且不落 job 行——空白名字的任务在
    /// `schedule list` 里认不出来，空白 prompt 的 job 到点只会跑一条空指令。
    #[tokio::test]
    async fn blank_name_and_prompt_are_rejected() {
        let env = Env::preauthorized();

        for (args, needle) in [
            (
                json!({"name": "   ", "prompt": "干活", "schedule": "10m"}),
                "参数 `name` 不能是空白",
            ),
            (
                json!({"name": "x", "prompt": " \n\t ", "schedule": "10m"}),
                "参数 `prompt` 不能是空白",
            ),
        ] {
            let result = env.call(args.clone()).await;
            assert!(!result.ok, "{args}");
            let output = result.output.unwrap();
            assert!(output.contains(needle), "{args}: {output}");
        }

        assert!(env.jobs().is_empty(), "空白参数不落行");
    }

    /// 参数形态错误一律中文，且不落 job 行。
    #[tokio::test]
    async fn arguments_are_validated_in_chinese() {
        let env = Env::preauthorized();

        for (args, needle) in [
            (
                json!({"prompt": "干活", "schedule": "10m"}),
                "缺少必需的参数 `name`",
            ),
            (
                json!({"name": 3, "prompt": "干活", "schedule": "10m"}),
                "必须是字符串",
            ),
            (
                json!({"name": "x", "schedule": "10m"}),
                "缺少必需的参数 `prompt`",
            ),
            (
                json!({"name": "x", "prompt": "干活"}),
                "缺少必需的参数 `schedule`",
            ),
            (
                json!({"name": "x", "prompt": "干活", "schedule": 3}),
                "必须是字符串",
            ),
            (
                json!({"name": "x", "prompt": "干活", "schedule": "10m", "tools": "fs_read"}),
                "必须是字符串数组",
            ),
            (
                json!({"name": "x", "prompt": "干活", "schedule": "10m", "tools": ["fs_read", 3]}),
                "每一项都必须是字符串",
            ),
            (
                json!({"name": "x", "prompt": "干活", "schedule": "10m", "tools": []}),
                "tools 不能为空",
            ),
            (
                json!({"name": "x", "prompt": "干活", "schedule": "10m", "risk": "dangerous"}),
                "只能是 safe 或 elevated",
            ),
        ] {
            let result = env.call(args.clone()).await;
            assert!(!result.ok, "{args}");
            let output = result.output.unwrap();
            assert!(output.contains(needle), "{args}: {output}");
        }

        assert!(env.jobs().is_empty(), "参数不对就不落行");
    }

    /// 分类矩阵（锁死）：交互 Ask、`--yes` Allow + 留痕、无人值守 Deny；三种模式里只有真的放行
    /// 才落 job 行。
    #[tokio::test]
    async fn classification_matrix_is_ask_allow_deny() {
        // 交互：问一次，批准后执行
        let interactive = Env::interactive(vec![ApprovalDecision::AllowOnce]);
        assert!(interactive.call(sample_args()).await.ok);
        assert_eq!(interactive.approval.seen().len(), 1);
        assert_eq!(interactive.approvals().len(), 1);
        assert_eq!(interactive.jobs().len(), 1);

        // --yes：策略直接放行（不问），留一条 allow/preauthorized
        let yes = Env::preauthorized();
        assert!(yes.call(sample_args()).await.ok);
        assert!(yes.approval.seen().is_empty());
        let rows = yes.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].decision.as_str(), rows[0].mode.as_str()),
            ("allow", "preauthorized")
        );
        assert_eq!(yes.jobs().len(), 1);

        // 无人值守：拒绝、留 deny/unattended，不落 job
        let unattended = Env::unattended();
        let denied = unattended.call(sample_args()).await;
        assert!(!denied.ok);
        let output = denied.output.unwrap();
        assert!(output.starts_with("denied by policy:"), "{output}");
        assert!(unattended.approval.seen().is_empty());
        let rows = unattended.approvals();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].decision.as_str(), rows[0].mode.as_str()),
            ("deny", "unattended")
        );
        assert_eq!(rows[0].risk, "elevated");
        assert!(unattended.jobs().is_empty());
    }
}
