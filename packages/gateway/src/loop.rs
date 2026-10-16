//! 调度循环：按 job 的 interval/cron 节奏发起完整 `TaskRunner` 运行。
//!
//! 四条不变量（P3 评审重点）：
//! 1. **无补跑（锚在创建/上次运行时刻）**：每轮都从「现在」与锚点现算到期时刻；interval 的锚点
//!    优先取 `last_run_at`，从未运行过时取 `created_at`——① 新 job 的首次到期 = `created_at + 间隔`，
//!    重扫（≤ [`RESCAN_SECS`] 一轮）不会把它向后推，间隔大于重扫周期也能准时触发；②
//!    `created_at + 间隔` 已经过去（停机期间错过的时点）不补跑，重启后的首次执行至少在一个间隔
//!    之后；③ 首次运行后由 `last_run_at` 接管节奏（`fired + every` 仍晚于现在就按它）。
//! 2. **不重叠**：每个 job 有一把运行互斥标志，上一次运行没结束时再到期只记一行
//!    `overlap skipped: {job}` 日志（不落运行行）。
//! 3. **失败必通知**：非 `success` 的终态（含 `TaskRunner` 的基础设施错误）都升一条
//!    `warning` 告警，标题 `任务失败：{job.name}`，来源填 job/task id。
//! 4. **pause/resume/新建在下一次醒来生效**：每轮都从库里重读 active job；就算没有任何 job 到期，
//!    也最多睡 [`RESCAN_SECS`] 就重新扫一遍。
//!
//! 每轮运行的执行边界 = job 自己的 profile 白名单（[`ignis_core::scheduler::JobProfile`]，经
//! `TaskRunner::new` 的 `profile` 参数包成 `ProfileExecutor`）+ 预授权策略
//!（`Preauthorized { unsafe_ops: false }`，危险操作一律拒绝）+ [`PolicyOnlyApproval`]（策略已
//! 判定，被问到就拒绝）；限额取 `job.limits_json`（经每轮配置传给 `TaskRunner`）。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use ignis_core::alerts::{AlertSink, Notifier};
use ignis_core::config::Config;
use ignis_core::memory::MemoryStore;
use ignis_core::scheduler::{
    JobProfile, NEVER_DUE, ScheduleSpec, next_due, parse_limits, parse_profile, parse_schedule,
};
use ignis_core::security::{ApprovalMode, Policy};
use ignis_core::skills::SkillCatalog;
use ignis_core::task::{TaskKind, TaskRequest, TaskRunner};
use ignis_runtime::{
    ApprovalDecision, ApprovalHandler, ApprovalRequest, Limits, RunStatus, Runtime,
};
use ignis_storage::{Db, JobRecord, NewJobRun, now_millis, now_secs};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::Result;

/// 没有任何 job 到期时也最多睡这么久就重新扫一遍：pause/resume/新建最迟在一个扫描周期内生效。
///
/// `pub` 是刻意的：端到端用例（Demo C）要按这个窗口对齐「暂停/恢复在哪一轮被观察到」的时刻，
/// 写死一个 30s 的副本会在窗口调整时静默过期。
pub const RESCAN_SECS: i64 = 30;
/// 收到取消后等在飞运行收尾的上限（brief 钉死 10s）；超时的直接 abort。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// 收尾轮询步长。
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

/// 调度循环：守护进程前台运行的主循环。
pub struct SchedulerLoop {
    db: Arc<Db>,
    /// 生效配置（模型、上限、密钥）；每个 job 的限额从 job 自己的 `limits_json` 派生。
    config: Arc<Config>,
    runtime: Arc<dyn Runtime>,
    /// 告警投递渠道：`notify_user` 与失败告警共用（渠道组合由调用方注入）。
    notifier: Arc<dyn Notifier>,
    memory: Arc<MemoryStore>,
    skills: Arc<SkillCatalog>,
    /// `TaskRunner` 的任务日志目录（`{home}/logs`）。
    log_dir: PathBuf,
    /// 没有 job 到期时的重扫周期（秒）。生产装配恒为 [`RESCAN_SECS`]；测试专用构造器
    /// `with_rescan_secs` 可把它调短，在秒级复现「间隔 > 重扫周期」的调度滑移。
    rescan_secs: i64,
}

impl SchedulerLoop {
    /// 装配循环：依赖全部由调用方注入（守护进程在 `daemon::run_foreground` 里组装生产实现，
    /// 单测注入 mock 内核与记录型渠道）。
    pub fn new(
        db: Arc<Db>,
        config: Arc<Config>,
        runtime: Arc<dyn Runtime>,
        notifier: Arc<dyn Notifier>,
        memory: Arc<MemoryStore>,
        skills: Arc<SkillCatalog>,
        log_dir: PathBuf,
    ) -> Self {
        Self {
            db,
            config,
            runtime,
            notifier,
            memory,
            skills,
            log_dir,
            rescan_secs: RESCAN_SECS,
        }
    }

    /// 测试专用：覆盖重扫周期。生产装配路径（`daemon::run_foreground` → [`SchedulerLoop::new`]）
    /// 不经过这里，公开行为恒为 [`RESCAN_SECS`]。
    #[cfg(test)]
    fn with_rescan_secs(mut self, secs: i64) -> Self {
        self.rescan_secs = secs;
        self
    }

    /// 跑调度循环直到 `cancel` 触发。
    ///
    /// 启动先清场（遗留的 `running` 运行行收成 `failed`/`interrupted`，防重启重复执行与悬挂），
    /// 随后每轮：从库里重读 active job → 按 `next_due` 算出各自到期时刻与下一次唤醒时刻
    /// （`min(最近到期, now + `[`RESCAN_SECS`]`)`）→ 睡到那时 → 执行到期的 job（该 job 已有运行
    /// 在飞时只记一行 `overlap skipped`）。取消后不再接受新运行，等在飞运行收尾（≤10s）再返回。
    pub async fn run(&self, cancel: CancellationToken) -> Result<()> {
        // 启动清场：上一个守护进程被 SIGKILL / 崩溃时留下的行收成终态（`failed` / `interrupted`），
        // 免得重启后 `job_runs` 与 `tasks` 里挂着永远 `running` 的行、或把错过的时点当成待跑。
        // pidfile 上的独占锁保证同一 home 只有一个守护进程，这里收掉的每一行都属于自己。
        let now = now_millis();
        let swept_runs = self.db.sweep_stale_job_runs(now)?;
        println!("scheduler: startup sweep collected {swept_runs} stale run(s)");
        let swept_tasks = self.db.sweep_stale_scheduled_tasks(now)?;
        println!("scheduler: startup sweep collected {swept_tasks} stale task(s)");

        let running = RunningJobs::default();
        // 在飞的调度运行：收尾时按这个清单等（句柄随结束自然收走）
        let mut tasks: Vec<JoinHandle<()>> = Vec::new();
        loop {
            tasks.retain(|task| !task.is_finished());
            let plan = match self.plan(now_secs()) {
                Ok(plan) => plan,
                Err(error) => {
                    eprintln!(
                        "warning: 调度扫描失败，{}s 后重试：{error}",
                        self.rescan_secs
                    );
                    if !sleep_secs(self.rescan_secs, &cancel).await {
                        break;
                    }
                    continue;
                }
            };
            if !sleep_until(plan.wake, &cancel).await {
                break;
            }

            let now = now_secs();
            for (job, due_at) in plan.due {
                if due_at > now {
                    // 被更早的 job 提前叫醒：还没轮到它
                    continue;
                }
                let Some(guard) = running.try_acquire(&job.id) else {
                    println!("overlap skipped: {} ({})", job.name, job.id);
                    continue;
                };
                let run_id = format!("{}-{}", now_millis(), next_run_seq());
                if let Err(error) = self.db.insert_job_run(&NewJobRun {
                    id: run_id.clone(),
                    job_id: job.id.clone(),
                    task_id: None,
                    started_at: now_millis(),
                }) {
                    eprintln!(
                        "warning: 运行行写入失败，跳过本次 {}（{}）：{error}",
                        job.name, job.id
                    );
                    // guard 在这里析构：运行位立刻释放，下一个到期时刻可以再试
                    continue;
                }
                println!("run started: {} ({}) run={run_id}", job.name, job.id);
                let deps = self.run_deps();
                // 优雅退出的口径是「等当前运行收尾」（上限 10s），不打断在飞运行：这里给它一个新
                // 令牌，取消只停在循环这一层；超时没收尾的任务由 `shutdown` abort。
                let run_cancel = CancellationToken::new();
                tasks.push(tokio::spawn(async move {
                    execute_run(deps, job, run_id, guard, run_cancel).await;
                }));
            }
        }

        shutdown(tasks).await;
        println!("scheduler: stopped");
        Ok(())
    }

    /// 一次扫描：每个 active 且会到期的 job 与它的到期时刻，以及下一次该醒来的时刻。
    fn plan(&self, now: i64) -> Result<Plan> {
        let jobs = self.db.list_jobs(false)?;
        let mut wake = now.saturating_add(self.rescan_secs);
        let mut due = Vec::new();
        for job in jobs {
            let spec = match parse_schedule(&job.schedule_kind, &job.schedule_expr) {
                Ok(spec) => spec,
                Err(error) => {
                    // 手工改库才会出现的坏表达式：跳过它，别拖垮整个循环
                    eprintln!(
                        "warning: 调度表达式无法解析，跳过 {}（{}）：{error}",
                        job.name, job.id
                    );
                    continue;
                }
            };
            let due_at = due_at(&spec, &job, now);
            if due_at == NEVER_DUE {
                // 永不到期：既不触发，也不参与「下一次醒来」的计算
                continue;
            }
            wake = wake.min(due_at);
            due.push((job, due_at));
        }
        Ok(Plan { due, wake })
    }

    /// 搬进 spawned 任务的共享依赖。
    fn run_deps(&self) -> RunDeps {
        RunDeps {
            db: Arc::clone(&self.db),
            config: Arc::clone(&self.config),
            runtime: Arc::clone(&self.runtime),
            notifier: Arc::clone(&self.notifier),
            memory: Arc::clone(&self.memory),
            skills: Arc::clone(&self.skills),
            log_dir: self.log_dir.clone(),
        }
    }
}

/// 一个 job 的下一次到期时刻（epoch 秒）。
///
/// interval 的锚点优先取 `last_run_at`（跑过），从未运行过时取 `created_at`（创建时刻）——
/// 把「从未运行」的 job 锚在创建时刻，重扫才不会把到期时刻一次次推后：间隔大于重扫周期
/// （[`RESCAN_SECS`]）也能在 `created_at + 间隔` 准时触发。锚点已过期时由 `next_due` 的
/// 「无补跑」口径兜住（给 `now + every`）。cron 不看锚点，只看表达式。库里存毫秒，
/// `next_due` 用秒。
fn due_at(spec: &ScheduleSpec, job: &JobRecord, now_secs: i64) -> i64 {
    // 从没跑过（last_run_at 为 NULL）→ 锚在创建时刻；跑过 → 从上次运行起算
    let last_fired = job
        .last_run_at
        .or(Some(job.created_at))
        .map(|millis| millis.div_euclid(1_000));
    next_due(spec, now_secs, last_fired)
}

/// 一次扫描的结果。
struct Plan {
    /// 会到期的 active job 与到期时刻（epoch 秒），按 `list_jobs` 的加载顺序。
    due: Vec<(JobRecord, i64)>,
    /// 下一次该醒来的时刻（epoch 秒）：`min(最近到期时刻, now + RESCAN_SECS)`。
    wake: i64,
}

/// 一次调度运行的共享依赖（`Arc` 克隆进 spawned 任务）。
#[derive(Clone)]
struct RunDeps {
    db: Arc<Db>,
    config: Arc<Config>,
    runtime: Arc<dyn Runtime>,
    notifier: Arc<dyn Notifier>,
    memory: Arc<MemoryStore>,
    skills: Arc<SkillCatalog>,
    log_dir: PathBuf,
}

/// 一次调度运行：解析边界 → 完整 `TaskRunner`（kind=schedule）→ 运行行收尾 → 失败告警。
async fn execute_run(
    deps: RunDeps,
    job: JobRecord,
    run_id: String,
    guard: RunningGuard,
    cancel: CancellationToken,
) {
    // 运行结束（含 panic 展开）时释放这个 job 的运行位
    let _guard = guard;

    let (profile, limits) = match parse_run_config(&job) {
        Ok(boundary) => boundary,
        Err(reason) => {
            eprintln!(
                "warning: 调度运行配置不合法（{} / {}）：{reason}",
                job.name, job.id
            );
            fail_before_start(&deps, &job, &run_id, &reason).await;
            return;
        }
    };

    // 每轮任务的配置：限额取 `job.limits_json`，其余照抄守护进程的生效配置（模型、密钥）
    let config = Arc::new(Config {
        limits,
        ..(*deps.config).clone()
    });
    // 调度运行的策略：预授权、危险操作不放行；策略已给出结论，被问到即拒绝
    let policy = Arc::new(Policy::new(ApprovalMode::Preauthorized {
        unsafe_ops: false,
    }));
    let approvals: Arc<dyn ApprovalHandler> = Arc::new(PolicyOnlyApproval);
    let runner = TaskRunner::new(
        Arc::clone(&deps.db),
        config,
        Arc::clone(&deps.runtime),
        policy,
        approvals,
        deps.log_dir.clone(),
        Arc::clone(&deps.memory),
        Arc::clone(&deps.skills),
        Arc::clone(&deps.notifier),
        Some(profile),
    );
    let request = TaskRequest {
        prompt: job.prompt.clone(),
        history: Vec::new(),
        kind: TaskKind::Schedule,
        workspace: PathBuf::from(&job.workspace),
        job_id: Some(job.id.clone()),
    };
    // 事件流不渲染（JSONL 已落盘），sink 只是 `TaskRunner::run` 的必需通道
    let (sink, _events) = mpsc::unbounded_channel();
    let outcome = runner.run(request, sink, cancel).await;
    let finished = now_millis();

    match outcome {
        Ok(outcome) => {
            if let Err(error) = deps.db.update_job_run_task_id(&run_id, &outcome.id) {
                eprintln!("warning: 回填运行行的 task_id 失败（{run_id}）：{error}");
            }
            // `job_runs.status` 与 `tasks.status` 是同一张表：契约字符串只在 `RunStatus` 一处映射
            let status = outcome.status.as_str();
            if let Err(error) = deps.db.finish_job_run(
                &run_id,
                status,
                outcome.final_text.as_deref(),
                outcome.error.as_deref(),
                outcome.usage.input_tokens,
                outcome.usage.output_tokens,
                finished,
            ) {
                eprintln!("warning: 收尾运行行失败（{run_id}）：{error}");
            }
            println!("run finished: {} ({status}) run={run_id}", job.name);
            if outcome.status != RunStatus::Success {
                raise_failure(&deps, &job, outcome.error.as_deref(), Some(&outcome.id)).await;
            }
        }
        Err(error) => {
            let reason = error.to_string();
            eprintln!("warning: 调度运行基础设施失败（{}）：{reason}", job.id);
            if let Err(error) =
                deps.db
                    .finish_job_run(&run_id, "failed", None, Some(&reason), 0, 0, finished)
            {
                eprintln!("warning: 收尾运行行失败（{run_id}）：{error}");
            }
            println!("run failed: {} run={run_id}", job.name);
            raise_failure(&deps, &job, Some(&reason), None).await;
        }
    }
    // 不管成没成，这次运行都算「跑过了」：`next_due` 从它起算
    if let Err(error) = deps.db.touch_job_last_run(&job.id, finished) {
        eprintln!("warning: 更新 last_run_at 失败（{}）：{error}", job.id);
    }
}

/// 解析 job 的运行边界（profile 白名单 + 限额）；任一处坏掉都不跑——两者是硬边界，不能因为
/// 库里的 JSON 坏了就悄悄退化成「无白名单 + 默认限额」。
fn parse_run_config(job: &JobRecord) -> std::result::Result<(JobProfile, Limits), String> {
    let profile = parse_profile(&job.profile_json).map_err(|error| error.to_string())?;
    let limits = parse_limits(&job.limits_json).map_err(|error| error.to_string())?;
    Ok((profile, limits))
}

/// 没进到 `TaskRunner` 就失败的运行（profile/limits 不合法）：同样落终态、推进 `last_run_at` 并告警。
async fn fail_before_start(deps: &RunDeps, job: &JobRecord, run_id: &str, reason: &str) {
    let finished = now_millis();
    if let Err(error) = deps
        .db
        .finish_job_run(run_id, "failed", None, Some(reason), 0, 0, finished)
    {
        eprintln!("warning: 收尾运行行失败（{run_id}）：{error}");
    }
    println!("run failed: {} run={run_id}", job.name);
    if let Err(error) = deps.db.touch_job_last_run(&job.id, finished) {
        eprintln!("warning: 更新 last_run_at 失败（{}）：{error}", job.id);
    }
    raise_failure(deps, job, Some(reason), None).await;
}

/// 失败告警（best-effort）：`warning` 级、标题 `任务失败：{name}`、来源填 job 与（有则）task。
///
/// 告警行与渠道投递都走 [`AlertSink`]（与 `notify_user` 同一把去重键：同一次运行的失败告警与
/// 模型自己的 `notify_user` 在窗口内只打扰用户一次）。
async fn raise_failure(deps: &RunDeps, job: &JobRecord, body: Option<&str>, task_id: Option<&str>) {
    let alerts = AlertSink::new(Arc::clone(&deps.db), Arc::clone(&deps.notifier));
    let title = format!("任务失败：{}", job.name);
    if let Err(error) = alerts
        .raise("warning", &title, body, Some(&job.id), task_id)
        .await
    {
        eprintln!("warning: 失败告警未落库（{}）：{error}", job.id);
    }
}

/// 运行行 id 的进程内序号：id 取 `{epoch_millis}-{序号}`（同进程内必不重复；跨进程由各自的
/// 库/home 分开，守护进程还有 flock 保证同一 home 只有一个）。
fn next_run_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 每个 job 的「有运行在飞」互斥标志：到期时先抢占，运行结束（含 panic 展开）时释放。
#[derive(Clone, Default)]
struct RunningJobs {
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl RunningJobs {
    /// 抢占某个 job 的运行位；该 job 已有运行在飞时返回 `None`（重叠 → 跳过）。
    fn try_acquire(&self, job_id: &str) -> Option<RunningGuard> {
        let mut in_flight = self.lock();
        if !in_flight.insert(job_id.to_string()) {
            return None;
        }
        drop(in_flight);
        Some(RunningGuard {
            jobs: self.clone(),
            job_id: job_id.to_string(),
        })
    }

    /// 标志表；锁中毒只可能来自其它线程 panic，取回继续。
    fn lock(&self) -> MutexGuard<'_, HashSet<String>> {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// 占用标志的句柄：析构即释放。
struct RunningGuard {
    jobs: RunningJobs,
    job_id: String,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.jobs.lock().remove(&self.job_id);
    }
}

/// 优雅退出：等在飞运行收尾，最多 [`SHUTDOWN_GRACE`]；超时的直接 abort（那几行的终态留给下次
/// 启动的 `sweep_stale_job_runs`）。
async fn shutdown(tasks: Vec<JoinHandle<()>>) {
    let in_flight = tasks.iter().filter(|task| !task.is_finished()).count();
    if in_flight == 0 {
        return;
    }
    println!(
        "scheduler: waiting up to {}s for {in_flight} in-flight run(s)",
        SHUTDOWN_GRACE.as_secs()
    );
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    while tasks.iter().any(|task| !task.is_finished()) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(SHUTDOWN_POLL).await;
    }
    for task in tasks {
        if !task.is_finished() {
            println!("scheduler: in-flight run(s) timed out, aborting");
            task.abort();
        }
    }
}

/// 睡到 `target`（epoch 秒）；`cancel` 触发时立刻返回 `false`（调用方据此退出循环）。
async fn sleep_until(target: i64, cancel: &CancellationToken) -> bool {
    let wait = (target - now_secs()).max(0) as u64;
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(Duration::from_secs(wait)) => true,
    }
}

/// 睡 `secs` 秒；`cancel` 触发时立刻返回 `false`。
async fn sleep_secs(secs: i64, cancel: &CancellationToken) -> bool {
    let wait = Duration::from_secs(secs.max(0) as u64);
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(wait) => true,
    }
}

/// 仅按策略执行的审批方：调度运行的策略是预授权，策略已经给出结论、不会被问到；一旦被问到
/// （策略漏判）一律拒绝，与 CLI 的实现同一口径。
struct PolicyOnlyApproval;

#[async_trait]
impl ApprovalHandler for PolicyOnlyApproval {
    async fn decide(&self, _req: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Reject
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use async_trait::async_trait;
    use ignis_core::alerts::NotifyError;
    use ignis_core::tools::available_tool_names;
    use ignis_runtime::test_support::MockRuntime;
    use ignis_runtime::{BoxStream, Capabilities, RunRequest, RuntimeEvent};
    use ignis_storage::{AlertRecord, JobRunRecord, NewJob, NewJobRun, NewTask};

    use super::*;

    /// 单个测试独占的临时 home：真实 DB 文件、日志目录与工作区都在里面，析构时清理。
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-gateway-loop-{}-{label}-{}",
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

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 记录型渠道：把投递过的告警按顺序留下来（真实实现以外唯一需要观察的部分）。
    struct RecordingNotifier {
        alerts: Mutex<Vec<AlertRecord>>,
    }

    impl RecordingNotifier {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                alerts: Mutex::new(Vec::new()),
            })
        }

        fn alerts(&self) -> Vec<AlertRecord> {
            self.alerts.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Notifier for RecordingNotifier {
        async fn notify(&self, alert: &AlertRecord) -> Result<(), NotifyError> {
            self.alerts.lock().unwrap().push(alert.clone());
            Ok(())
        }
    }

    /// 一次运行被内核看到的内容（漂移与 limits 断言用）。
    #[derive(Clone, Debug)]
    struct SeenRun {
        limits: Limits,
        tools: Vec<String>,
    }

    /// 测试内核：可选延迟之后回放事件（复用 [`MockRuntime`]），并记下每次看到的限额与工具表；
    /// 延迟用来观察「上一次运行还没结束又再一次到期」。
    struct TestRuntime {
        replay: Arc<MockRuntime>,
        delay: Duration,
        runs: Mutex<Vec<SeenRun>>,
    }

    impl TestRuntime {
        fn new(events: Vec<RuntimeEvent>) -> Arc<Self> {
            Self::with_delay(events, Duration::ZERO)
        }

        fn with_delay(events: Vec<RuntimeEvent>, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                replay: MockRuntime::new(events),
                delay,
                runs: Mutex::new(Vec::new()),
            })
        }

        fn runs(&self) -> Vec<SeenRun> {
            self.runs.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Runtime for TestRuntime {
        fn id(&self) -> &'static str {
            self.replay.id()
        }

        fn capabilities(&self) -> Capabilities {
            self.replay.capabilities()
        }

        async fn run(
            &self,
            req: RunRequest,
            cancel: CancellationToken,
        ) -> BoxStream<'static, RuntimeEvent> {
            self.runs.lock().unwrap().push(SeenRun {
                limits: req.limits,
                tools: req
                    .executor
                    .specs()
                    .into_iter()
                    .map(|spec| spec.name)
                    .collect(),
            });
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.replay.run(req, cancel).await
        }
    }

    /// 测试环境：真实 home/DB/日志目录 + 测试内核 + 记录型渠道。
    struct Env {
        home: TempHome,
        db: Arc<Db>,
        runtime: Arc<TestRuntime>,
        notifier: Arc<RecordingNotifier>,
        config: Arc<Config>,
        log_dir: PathBuf,
    }

    impl Env {
        fn new(label: &str, events: Vec<RuntimeEvent>) -> Self {
            Self::build(label, TestRuntime::new(events))
        }

        fn with_delay(label: &str, events: Vec<RuntimeEvent>, delay: Duration) -> Self {
            Self::build(label, TestRuntime::with_delay(events, delay))
        }

        fn build(label: &str, runtime: Arc<TestRuntime>) -> Self {
            let home = TempHome::new(label);
            let log_dir = home.path().join("logs");
            let db = Arc::new(Db::open(&home.path().join("ignis.db")).unwrap());
            let config = Arc::new(Config {
                workspace_root: home.path().to_path_buf(),
                api_key: Some("sk-test".into()),
                ..Config::default()
            });
            Self {
                home,
                db,
                runtime,
                notifier: RecordingNotifier::new(),
                config,
                log_dir,
            }
        }

        /// 组装一个循环；重启场景每次调用都拿到一个新的循环（同一份库/渠道）。
        fn scheduler(&self) -> SchedulerLoop {
            SchedulerLoop::new(
                Arc::clone(&self.db),
                Arc::clone(&self.config),
                Arc::clone(&self.runtime) as Arc<dyn Runtime>,
                Arc::clone(&self.notifier) as Arc<dyn Notifier>,
                MemoryStore::new(Arc::clone(&self.db), self.home.path().join("memory")),
                SkillCatalog::load(&self.home.path().join("skills")),
                self.log_dir.clone(),
            )
        }

        /// 插一条 active job；返回它的 id 与（用同一份 JSON 组装的）profile。
        fn add_job(&self, name: &str, schedule_kind: &str, schedule_expr: &str) -> String {
            self.add_job_with_profile(
                name,
                schedule_kind,
                schedule_expr,
                r#"{"tools":["fs_read"],"risk":"elevated"}"#,
            )
        }

        /// 插一条 active job，profile 由用例给定。
        fn add_job_with_profile(
            &self,
            name: &str,
            schedule_kind: &str,
            schedule_expr: &str,
            profile_json: &str,
        ) -> String {
            let id = format!("job-{name}");
            self.db
                .insert_job(&NewJob {
                    id: id.clone(),
                    name: name.into(),
                    prompt: format!("检查 {name} 的状态"),
                    schedule_kind: schedule_kind.into(),
                    schedule_expr: schedule_expr.into(),
                    kernel: "native".into(),
                    profile_json: profile_json.into(),
                    limits_json: r#"{"max_steps":5}"#.into(),
                    workspace: self.home.path().display().to_string(),
                })
                .unwrap();
            id
        }

        /// 某个 job 的运行行，按 `started_at` 升序（第 0 条就是第一次运行）。
        fn runs(&self, job_id: &str) -> Vec<JobRunRecord> {
            let mut runs = self.db.list_job_runs(Some(job_id), 20).unwrap();
            runs.reverse();
            runs
        }

        /// 全部告警行（含已读）。
        fn alerts(&self) -> Vec<AlertRecord> {
            self.db.list_alerts(true, 20).unwrap()
        }
    }

    /// 一次内核上报的用量事件。
    fn usage_event() -> RuntimeEvent {
        RuntimeEvent::Usage {
            input_tokens: 7,
            output_tokens: 3,
            estimated: false,
        }
    }

    /// 成功终态事件。
    fn success_event(summary: &str) -> RuntimeEvent {
        RuntimeEvent::Done {
            status: RunStatus::Success,
            summary: Some(summary.into()),
            error: None,
        }
    }

    /// 失败终态事件。
    fn failed_event(error: &str) -> RuntimeEvent {
        RuntimeEvent::Done {
            status: RunStatus::Failed,
            summary: Some("半截答复".into()),
            error: Some(error.into()),
        }
    }

    /// 起一个循环；测试结束前必须 [`stop_loop`]（不留后台任务）。
    fn spawn_loop(scheduler: SchedulerLoop, cancel: CancellationToken) -> JoinHandle<Result<()>> {
        tokio::spawn(async move { scheduler.run(cancel).await })
    }

    /// 取消并等循环收尾（在飞运行由循环自己等）。
    async fn stop_loop(cancel: CancellationToken, handle: JoinHandle<Result<()>>) {
        cancel.cancel();
        let finished = tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .expect("循环没有在取消后收尾")
            .expect("循环任务不该 panic");
        finished.expect("取消退出不是错误");
    }

    /// 轮询等条件成立（每 50ms 一次），超时 panic 并说明在等什么。
    async fn wait_for(timeout: Duration, what: &str, mut check: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if check() {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "等待超时：{what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 到期 → 一次完整调度运行：`job_runs` 落行并收尾、`tasks.kind=schedule`、JSONL 落盘、
    /// `last_run_at` 推进、成功不告警。
    #[tokio::test]
    async fn a_due_job_runs_and_records_a_finished_run_row() {
        let env = Env::new("due", vec![usage_event(), success_event("检查完成")]);
        let job_id = env.add_job("disk-watch", "interval", "1s");

        let cancel = CancellationToken::new();
        let running = spawn_loop(env.scheduler(), cancel.clone());

        wait_for(Duration::from_secs(5), "第一条运行行", || {
            !env.runs(&job_id).is_empty()
        })
        .await;
        wait_for(Duration::from_secs(5), "运行行收尾", || {
            env.runs(&job_id)[0].ended_at.is_some()
        })
        .await;

        let run = env.runs(&job_id)[0].clone();
        assert_eq!(run.status, "success");
        assert_eq!(run.summary.as_deref(), Some("检查完成"));
        assert_eq!(run.error, None);
        assert_eq!((run.input_tokens, run.output_tokens), (7, 3));
        let task_id = run.task_id.clone().expect("task_id 应回填");

        // 任务行：kind=schedule、工作区 = job.workspace、状态与用量都落到库里
        let task = env.db.get_task(&task_id).unwrap().expect("任务行");
        assert_eq!(task.kind, "schedule");
        assert_eq!(task.workspace, env.home.path().display().to_string());
        assert_eq!(task.status, "success");
        assert_eq!(task.summary.as_deref(), Some("检查完成"));
        // TaskRunner 的 JSONL 链路照常落盘
        assert!(env.log_dir.join(format!("{task_id}.jsonl")).is_file());

        // last_run_at 推进；成功不告警
        let job = env.db.get_job(&job_id).unwrap().unwrap();
        assert!(job.last_run_at.is_some());
        assert!(env.alerts().is_empty());
        assert!(env.notifier.alerts().is_empty());
        assert!(!env.runtime.runs().is_empty());

        stop_loop(cancel, running).await;
    }

    /// 非 success：`job_runs` 记 error 与用量，并升一条 `warning` 失败告警
    /// （标题 `任务失败：{name}`，来源 job/task），渠道真的收到投递。
    #[tokio::test]
    async fn a_failing_run_records_the_error_and_raises_a_failure_alert() {
        let env = Env::new("failed", vec![usage_event(), failed_event("boom")]);
        let job_id = env.add_job("disk-watch", "interval", "1s");

        let cancel = CancellationToken::new();
        let running = spawn_loop(env.scheduler(), cancel.clone());

        wait_for(Duration::from_secs(5), "失败告警落库", || {
            !env.alerts().is_empty()
        })
        .await;

        let run = env.runs(&job_id)[0].clone();
        assert_eq!(run.status, "failed");
        assert_eq!(run.error.as_deref(), Some("boom"));
        // 失败不把半截答复当摘要（与 `TaskOutcome::final_text` 的口径一致）
        assert_eq!(run.summary, None);
        assert_eq!((run.input_tokens, run.output_tokens), (7, 3));
        let task_id = run.task_id.clone().expect("task_id 应回填");

        let alerts = env.alerts();
        assert_eq!(alerts.len(), 1, "同键在去重窗口内只落一行：{alerts:?}");
        assert_eq!(alerts[0].severity, "warning");
        assert_eq!(alerts[0].title, "任务失败：disk-watch");
        assert_eq!(alerts[0].body.as_deref(), Some("boom"));
        assert_eq!(alerts[0].source_job_id.as_deref(), Some(job_id.as_str()));
        assert_eq!(alerts[0].source_task_id.as_deref(), Some(task_id.as_str()));
        // 渠道收到的就是落库的那一行
        assert_eq!(env.notifier.alerts(), alerts);

        stop_loop(cancel, running).await;
    }

    /// 上一次运行还没结束就再一次到期：只记 `overlap skipped`（不落运行行），等它结束后才轮到下一轮。
    #[tokio::test]
    async fn an_overlapping_run_is_skipped_without_a_new_row() {
        let env = Env::with_delay(
            "overlap",
            vec![success_event("慢活干完了")],
            Duration::from_secs(3),
        );
        let job_id = env.add_job("slow", "interval", "1s");

        let cancel = CancellationToken::new();
        let running = spawn_loop(env.scheduler(), cancel.clone());

        wait_for(Duration::from_secs(4), "第一次运行开始", || {
            !env.runs(&job_id).is_empty()
        })
        .await;
        // 第一次运行还要 ~3s 才结束，期间每 1s 都会再到期：全部跳过
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(env.runs(&job_id).len(), 1, "重叠期间不得落第二行运行");
        assert_eq!(env.runs(&job_id)[0].ended_at, None, "第一行仍在跑");

        // 它一结束，下一轮到点就会起第二次
        wait_for(Duration::from_secs(6), "第二次运行", || {
            env.runs(&job_id).len() >= 2
        })
        .await;
        stop_loop(cancel, running).await;
    }

    /// pause/resume 在「下一次醒来」生效：暂停期间一行不落，恢复后在下一个到点时刻继续。
    #[tokio::test]
    async fn pause_and_resume_take_effect_at_the_next_wake() {
        let env = Env::new("pause", vec![success_event("ok")]);
        // pacer 让循环每秒醒一次：pause/resume 不必等满 30s 的重扫周期
        let pacer = env.add_job("pacer", "interval", "1s");
        let subject = env.add_job("subject", "interval", "1s");
        env.db.set_job_status(&subject, "paused").unwrap();

        let cancel = CancellationToken::new();
        let running = spawn_loop(env.scheduler(), cancel.clone());

        // 循环确实在扫：pacer 在跑，被暂停的 subject 一行都没有
        wait_for(Duration::from_secs(5), "pacer 第一次运行", || {
            !env.runs(&pacer).is_empty()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(env.runs(&subject).is_empty(), "暂停的 job 不该运行");

        // 恢复：下一个到点时刻（≤ 1s）就继续
        env.db.set_job_status(&subject, "active").unwrap();
        wait_for(
            Duration::from_secs(5),
            "恢复后的第一次运行",
            || !env.runs(&subject).is_empty(),
        )
        .await;

        // 再暂停：先等一拍让「暂停前已经排好的那次到期」落地，之后计数不得再涨
        env.db.set_job_status(&subject, "paused").unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let settled = env.runs(&subject).len();
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(env.runs(&subject).len(), settled, "暂停后不得再运行");
        assert!(!env.runs(&pacer).is_empty(), "pacer 仍在跑");

        stop_loop(cancel, running).await;
    }

    /// 重启不补跑：停机错过的时间点不补，新循环的首次执行至少在一个间隔之后。
    #[tokio::test]
    async fn a_restart_does_not_run_the_missed_ticks() {
        let env = Env::new("restart", vec![success_event("ok")]);
        let job_id = env.add_job("watch", "interval", "1s");

        // 第一段：跑一次，然后停掉
        let cancel = CancellationToken::new();
        let running = spawn_loop(env.scheduler(), cancel.clone());
        wait_for(
            Duration::from_secs(5),
            "第一段的第一次运行",
            || !env.runs(&job_id).is_empty(),
        )
        .await;
        stop_loop(cancel, running).await;

        // 停机超过一个间隔：错过的时点不该被补上
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let before = env.runs(&job_id).len();

        // 第二段：新循环的首次执行不早于一个间隔之后
        let started = tokio::time::Instant::now();
        let cancel = CancellationToken::new();
        let running = spawn_loop(env.scheduler(), cancel.clone());
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            env.runs(&job_id).len(),
            before,
            "重启后不得立刻补跑错过的时点"
        );
        wait_for(
            Duration::from_secs(4),
            "重启后的第一次运行",
            || env.runs(&job_id).len() > before,
        )
        .await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "首次执行必须在一个间隔之后（实际 {elapsed:?}）"
        );
        stop_loop(cancel, running).await;
    }

    /// 启动清场：遗留的 `running` 运行行被收成 `failed` / `error=interrupted`。
    #[tokio::test]
    async fn startup_sweeps_stale_running_rows() {
        let env = Env::new("sweep", vec![success_event("ok")]);
        // 上一个守护进程留下的「正在跑」：进程没了，行还挂着
        env.db
            .insert_job_run(&NewJobRun {
                id: "stale".into(),
                job_id: "job-gone".into(),
                task_id: Some("task-gone".into()),
                started_at: 1,
            })
            .unwrap();

        let cancel = CancellationToken::new();
        let running = spawn_loop(env.scheduler(), cancel.clone());

        let stale = |env: &Env| {
            env.db
                .list_job_runs(None, 10)
                .unwrap()
                .into_iter()
                .find(|run| run.id == "stale")
                .unwrap()
        };
        wait_for(Duration::from_secs(3), "启动清场", || {
            stale(&env).status == "failed"
        })
        .await;
        let row = stale(&env);
        assert_eq!(row.error.as_deref(), Some("interrupted"));
        assert!(row.ended_at.is_some());

        stop_loop(cancel, running).await;
    }

    /// 启动清场也要收 `tasks`：被 SIGKILL 打断的调度运行只有 `job_runs` 被收的话，任务行会永远
    /// 挂在 `running`（`task list` 里一条永远跑不完的调度任务）。`run` / `repl` 的活行不归它管。
    #[tokio::test]
    async fn startup_sweeps_stale_scheduled_task_rows() {
        let env = Env::new("sweep-tasks", vec![success_event("ok")]);
        // 遗留在飞调度任务（kind=schedule、status=running）：上一个守护进程被打断时的现场
        env.db
            .insert_task(&NewTask {
                id: "stale-schedule".into(),
                kind: "schedule".into(),
                prompt: "检查磁盘".into(),
                kernel: "native".into(),
                workspace: env.home.path().display().to_string(),
                started_at: 1,
            })
            .unwrap();
        // 交互运行的活行（kind=run）：清场不该碰它
        env.db
            .insert_task(&NewTask {
                id: "interactive".into(),
                kind: "run".into(),
                prompt: "读一下配置".into(),
                kernel: "native".into(),
                workspace: env.home.path().display().to_string(),
                started_at: 2,
            })
            .unwrap();

        let cancel = CancellationToken::new();
        let running = spawn_loop(env.scheduler(), cancel.clone());

        wait_for(Duration::from_secs(3), "调度任务行被清场", || {
            env.db
                .get_task("stale-schedule")
                .unwrap()
                .is_some_and(|task| task.status == "failed")
        })
        .await;
        let swept = env.db.get_task("stale-schedule").unwrap().unwrap();
        assert_eq!(swept.error.as_deref(), Some("interrupted"));
        assert!(swept.ended_at.is_some());
        let untouched = env.db.get_task("interactive").unwrap().unwrap();
        assert_eq!(untouched.status, "running");
        assert_eq!(untouched.ended_at, None);

        stop_loop(cancel, running).await;
    }

    /// `NEVER_DUE` 的 job 既不触发也不抢唤醒：别的 job 照跑，它一行都没有。
    #[tokio::test]
    async fn a_never_due_job_does_not_block_the_others() {
        let env = Env::new("never-due", vec![success_event("ok")]);
        let never = env.add_job("feb-30", "cron", "0 0 30 2 *");
        let watch = env.add_job("watch", "interval", "1s");

        let cancel = CancellationToken::new();
        let running = spawn_loop(env.scheduler(), cancel.clone());

        wait_for(Duration::from_secs(5), "watch 运行", || {
            !env.runs(&watch).is_empty()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(env.runs(&never).is_empty(), "永不到期的 job 不该运行");

        stop_loop(cancel, running).await;
    }

    /// 调度运行看到的执行边界：profile 白名单过滤后的工具表 == `available_tool_names()`
    /// （防 `meta_tools` 装配与清单漂移），限额取 `job.limits_json`。
    #[tokio::test]
    async fn the_scheduled_run_sees_the_job_profile_and_limits() {
        let env = Env::new("boundary", vec![success_event("ok")]);
        let names = available_tool_names();
        let profile_json = format!(
            "{{\"tools\":[{}],\"risk\":\"elevated\"}}",
            names
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        env.add_job_with_profile("boundary", "interval", "1s", &profile_json);

        let cancel = CancellationToken::new();
        let running = spawn_loop(env.scheduler(), cancel.clone());

        wait_for(Duration::from_secs(5), "第一次运行", || {
            !env.runtime.runs().is_empty()
        })
        .await;
        let seen = env.runtime.runs()[0].clone();
        // 白名单 ⊆ 真实注册表、且不多不少：两边漂移（清单多一个、注册少一个）都会失败
        assert_eq!(seen.tools, names);
        // limits = job.limits_json（这里 `{"max_steps":5}`），不是守护进程配置里的默认值
        assert_eq!(
            seen.limits,
            Limits {
                max_steps: 5,
                ..Limits::default()
            }
        );

        stop_loop(cancel, running).await;
    }

    /// 一个只带时间字段的 interval job 行：到期锚点计算专用（`created_at` 与 `last_run_at`
    /// 决定首次与后续的到期时刻）。
    fn job_at(created_ms: i64, last_run_at_ms: Option<i64>) -> JobRecord {
        JobRecord {
            id: "job-due".into(),
            name: "due".into(),
            prompt: "检查".into(),
            schedule_kind: "interval".into(),
            schedule_expr: "60s".into(),
            kernel: "native".into(),
            profile_json: r#"{"tools":["fs_read"],"risk":"elevated"}"#.into(),
            limits_json: r#"{"max_steps":5}"#.into(),
            workspace: "/tmp/ignis".into(),
            status: "active".into(),
            created_at: created_ms,
            last_run_at: last_run_at_ms,
        }
    }

    /// P4-T6b：到期时刻的锚点。从未运行过的 interval job 锚在 `created_at`——连续重扫必须得到
    /// 同一个时刻（否则间隔大于重扫周期的 job 永远等不到到期）；跑过之后 `last_run_at` 保节奏。
    #[test]
    fn a_never_run_interval_is_anchored_to_its_creation_time() {
        let created_secs: i64 = 1_700_000_000;
        let created_ms = created_secs * 1_000;
        let spec = parse_schedule("interval", "60s").unwrap();

        // 重扫发生在 created+1 / +31 / +59（30s 重扫窗口内）：三次都报同一个 created+60
        assert_eq!(
            due_at(&spec, &job_at(created_ms, None), created_secs + 1),
            created_secs + 60
        );
        assert_eq!(
            due_at(&spec, &job_at(created_ms, None), created_secs + 31),
            created_secs + 60
        );
        assert_eq!(
            due_at(&spec, &job_at(created_ms, None), created_secs + 59),
            created_secs + 60
        );

        // 创建时刻已经过去一整格（守护进程停机跨度）：不补跑，从「现在」重新起算
        assert_eq!(
            due_at(&spec, &job_at(created_ms, None), created_secs + 61),
            created_secs + 121
        );

        // 跑过一次之后的拍子由 last_run_at 接管：fired + 60 仍晚于现在 → 保持原节奏
        let ran_at = (created_secs + 60) * 1_000;
        assert_eq!(
            due_at(&spec, &job_at(created_ms, Some(ran_at)), created_secs + 90),
            created_secs + 120
        );
    }

    /// P4-T6b 判别用例：重扫周期 1s、间隔 3s、从未运行——修复前每轮 `now + every` 被重扫推后，
    /// 8s 内一次都不触发；修复后锚在创建时刻，~3s 触发。
    #[tokio::test]
    async fn an_interval_longer_than_the_rescan_fires_a_never_run_job() {
        let env = Env::new("rescan-anchor", vec![success_event("ok")]);
        let job_id = env.add_job("anchored", "interval", "3s");

        let cancel = CancellationToken::new();
        let started = tokio::time::Instant::now();
        let running = spawn_loop(env.scheduler().with_rescan_secs(1), cancel.clone());

        wait_for(
            Duration::from_secs(8),
            "首次运行（间隔 3s > 重扫 1s）",
            || !env.runs(&job_id).is_empty(),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(2),
            "首次执行必须在一个间隔边界之后（实际 {elapsed:?}）"
        );

        stop_loop(cancel, running).await;
    }
}
