# Ignis v0.1 阶段三（P3）：调度与守护 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**目标：** 让 Ignis 在用户退出 CLI 后继续按计划工作——jobs（interval/常用 cron）→ 守护进程按受限 profile 以非交互策略发起 Task → 执行记录与 job_runs 落库 → 失败必通知、模型可 `notify_user` 条件告警（去重）→ `ignis alerts` 可查；以 `examples/test-server` 夹具复现 Demo C。

**架构：** 迁移 003（`jobs`/`job_runs`/`alerts`）+ DAO（storage）；调度核心（解析、next_due、校验）在 `ignis-core::scheduler`；告警与 `Notifier` trait 在 `ignis-core::alerts`；守护进程逻辑（pidfile/flock/自守护/调度循环/通知投递）在 `ignis-gateway`；`schedule_create`/`notify_user` 为元工具（经现有 ToolRuntime 注册）；调度运行 = `Preauthorized` 策略 + **profile 白名单执行器**（`ProfileExecutor` 包装 `ToolRuntime`，白名单外的工具对模型不可见、调用被拒并记录）。

**技术栈：** 沿用 P1/P2（**零新依赖**：5 字段 cron 手写解析；自守护用「re-exec 自身 + setsid + 日志重定向」，不用 daemonize crate；单实例锁用 `libc::flock`；桌面通知直接 spawn `notify-send`）。

**设计文档（Spec）：** `docs/superpowers/specs/2026-10-15-ignis-v0.1-design.md` §6.7（Scheduler/Gateway/通知）、决策 #18（调度仅 native）、#19（告警与去重）、§7 Demo C、验收 #6/#8/#9。**前置：** P2 完成（HEAD 0f5ed81，288 测试全绿）。

## 全局约束

继承 P1/P2 全部约束（edition 2024；依赖方向 `core → {runtime, storage}`、新增 `gateway → core`；提交规范与尾注；收口三连）。P3 新增：

- **零新依赖**（离线）。`ignis-gateway` 为新 crate（workspace 成员 `packages/gateway`）。
- **间隔格式（锁死）**：`<正整数><s|m|h>`（`30s`、`10m`、`2h`；≥1s）。**Cron（锁死）**：5 字段 `min hour dom mon dow`；支持 `*`、数字、`a-b`、`a,b,c`、`*/N`；不支持名称/秒/年 → 中文错误。`cron` 下次触发 = 严格晚于现在的最近匹配时间。
- **任务模型（迁移 003 DDL）**：
  - `jobs(id TEXT PK, name TEXT NOT NULL, prompt TEXT NOT NULL, schedule_kind TEXT NOT NULL[interval|cron], schedule_expr TEXT NOT NULL, kernel TEXT NOT NULL, profile_json TEXT NOT NULL, limits_json TEXT NOT NULL, workspace TEXT NOT NULL, status TEXT NOT NULL[active|paused], created_at INTEGER NOT NULL, last_run_at INTEGER NULL)`
  - `job_runs(id TEXT PK, job_id TEXT NOT NULL, task_id TEXT NULL, started_at INTEGER NOT NULL, ended_at INTEGER NULL, status TEXT NOT NULL[running|success|failed|aborted|limit_reached], summary TEXT NULL, error TEXT NULL, input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0)` + 索引 `(job_id, started_at)`
  - `alerts(id INTEGER PK AUTOINCREMENT, severity TEXT NOT NULL[info|warning|critical], title TEXT NOT NULL, body TEXT NULL, source_job_id TEXT NULL, source_task_id TEXT NULL, dedup_key TEXT NOT NULL, created_at INTEGER NOT NULL, read_at INTEGER NULL)` + 索引 `(dedup_key, created_at)` 与 `(read_at)`
  - 任务 kind 新增取值 `schedule`（TEXT 列，无迁移）。
- **profile/limits（锁死）**：`profile_json = {"tools": ["shell_run", …], "risk": "safe"|"elevated"}`（`risk='elevated'` 表示该 job 可在无审批下执行 Elevated 工具，Dangerous 一律拒绝）；`limits_json` = 完整 `runtime::Limits` serde。调度运行策略 = `Preauthorized{unsafe_ops:false}` + `ProfileExecutor` 白名单。
- **运行与通知语义（锁死）**：失败必通知（`warning`，标题 `任务失败：{job name}`）；重叠执行跳过（每 job 互斥，跳过只写 daemon 日志不建 run 行）；**停机错过的执行不补跑**（daemon 启动/重载时 next_due 从「现在」重算；仅对既有 job 生效，不改写库内时间）；告警去重 = `(job_id 或 task_id, title)` 在 15 分钟窗口内只落库一次（去重命中时工具返回「已去重」说明）；`notify_user` 分类 `Safe`；`schedule_create` 分类 `Elevated`（交互确认即审批提示；`--yes` 预授权；无人值守拒绝）。
- **daemon（锁死）**：`ignis daemon start` = 以 `setsid` re-exec 自身 `ignis daemon run --foreground`（stdout/stderr 重定向到 `~/.ignis/logs/daemon.log`），父进程等到 pidfile 出现后打印 `daemon started (pid N)`；子进程对 `~/.ignis/ignis.pid` 持 `libc::flock` 独占非阻塞（失败 → 退出并报「已在运行」）；`stop` = 读 pid + SIGTERM + 5s 内轮询 `kill(pid,0)`，超时 SIGKILL，成功后清 pidfile；`status` 依 flock 探测 + pid 存活。SIGTERM → 优雅退出（等当前运行收尾，≤10s）。
- 调度运行的 Task 仍走完整 `TaskRunner`（tasks 行 kind=schedule、JSONL 日志、审批记录、脱敏），workspace = job.workspace；daemon 启动时把遗留 `running` 的 job_runs 收成 `failed`（error=`interrupted`）——防重启重复执行与悬挂。
- 文案：CLI chrome 英文、工具结果/错误中文（沿用）。
- 触碰点最小化：P2 挂起 minor 维持挂起（本计划不夹带），除「若改动同文件则顺手」并在报告中说明。

## 评审重点（Review Focus）

1. **停机不补跑、重启不重复**：daemon 重启后不得立即执行错过的时点（next_due 从 now 重算）；遗留 `running` job_runs 被收成 failed；测试落在 T2（due 计算）与 T5/T7（重启场景）。
2. **profile 白名单是真边界**：白名单外工具既不出现在 `specs()`（模型看不见）也在直接调用时被拒 + 落 deny 记录；`risk='elevated'` 也不放行 Dangerous；测试落在 T4。
3. **重叠互斥**：同一 job 上一次运行未结束时到期 → 跳过（有日志、无新 run 行）；测试落在 T5（模拟长任务）与 T7。
4. **告警去重窗口**：同 `(job/task, title)` 15 分钟内第二次 `notify_user` 不落新行但返回可解释文本；窗口外落新行；测试落在 T3。
5. **守护进程健壮性**：flock 单实例（双 start 第二个报错退出）；`stop` 幂等（未运行 → 友好提示退出 1？定：未运行 → 提示 + 退出 0）；pidfile 残留（进程已死）不被误判为运行；测试落在 T5 与 T6 手动冒烟。

## 后续阶段

P4 兼容内核与交付（codex/claude、内核切换与能力门控、doctor 完整版、Docker/systemd、README、验收 harness、真实 relay spike 收口）。

---

### Task 1: storage：迁移 003（jobs/job_runs/alerts）+ DAO

**Files:** Create `packages/storage/src/{jobs.rs,alerts.rs}`；Modify `migrations.rs`、`lib.rs`、`db.rs`（如有）；Test: 内联 + `packages/storage/tests/schedule_tables.rs`

**Interfaces:**
- Consumes: P1/P2 的 `Db`/迁移框架。
- Produces（签名逐字）：
  - `NewJob {id: String, name: String, prompt: String, schedule_kind: String, schedule_expr: String, kernel: String, profile_json: String, limits_json: String, workspace: String}`；`JobRecord {…同列 + status, created_at, last_run_at: Option<i64>}`
  - `Db::{insert_job, get_job, list_jobs(include_paused: bool), set_job_status(id, status), remove_job(id), touch_job_last_run(id, at)}`
  - `NewJobRun {id, job_id, task_id: Option<String>, started_at}`；`JobRunRecord {…, ended_at: Option<i64>, status, summary: Option<String>, error: Option<String>, input_tokens, output_tokens}`
  - `Db::{insert_job_run, finish_job_run(id, status, summary, error, input, output, ended_at), update_job_run_task_id(id, task_id), list_job_runs(job_id: Option<&str>, limit), sweep_stale_job_runs(now) -> Result<usize>}`（把 `status='running'` 全部收成 failed/`interrupted`，返回条数）
  - `NewAlert {severity, title, body: Option<String>, source_job_id: Option<String>, source_task_id: Option<String>, dedup_key}`；`AlertRecord {…, id, created_at, read_at: Option<i64>}`
  - `Db::{insert_alert, find_recent_alert(&self, dedup_key: &str, window_secs: i64, now: i64) -> Result<Option<AlertRecord>>, list_alerts(include_read: bool, limit), mark_alerts_read(now) -> Result<usize>, unread_alert_count}`
- [ ] **Step 1: 写失败测试**（迁移 user_version==3 幂等；jobs/job_runs/alerts 往返与排序；`sweep_stale_job_runs` 只收 running 且返回条数；`find_recent_alert` 窗口内/外；`mark_alerts_read` 只动未读）；**Step 2: 失败确认**；**Step 3: 实现（DDL 按全局约束原文）**；**Step 4: 收口并提交**（`feat(storage): 新增调度与告警表（迁移 003）`）

---

### Task 2: core::scheduler（interval/cron 解析、next_due、校验）

**Files:** Create `packages/core/src/scheduler/{mod.rs,schedule.rs,due.rs}`；Modify `lib.rs`；Test: 内联

**Interfaces:**
- Produces:
  - `enum ScheduleKind { Interval, Cron }`（serde lowercase）；`enum ScheduleSpec { Interval { every_secs: u64 }, Cron(CronExpr) }`
  - `fn parse_schedule(kind: &str, expr: &str) -> Result<ScheduleSpec, SchedulerError>`（`<n><s|m|h>`；cron 5 字段按全局约束；错误中文含原表达式）
  - `CronExpr::parse(&str) -> Result<CronExpr, SchedulerError>`；`CronExpr::next_after(&self, after: i64 /*unix secs*/) -> Option<i64>`（严格晚于；手写分钟级推进，bounds：搜索上限 4 年 → None 视为不可达并报错）
  - `fn next_due(spec: &ScheduleSpec, now_secs: i64, last_fired: Option<i64>) -> i64`：Interval → `last_fired.map(|t| t + every).filter(|t| t > now).unwrap_or(now + every)`（**无补跑**）；Cron → `next_after(now)`
  - `JobProfile { tools: Vec<String>, risk: ProfileRisk }`、`ProfileRisk::{Safe, Elevated}`；`JobLimits = runtime::Limits`；`parse_profile(json) -> Result<JobProfile>`（未知字段拒绝；空 tools → 错误）；`parse_limits(json) -> Result<Limits>`
  - `validate_job(profile: &JobProfile, available: &[String]) -> Result<(), SchedulerError>`（tools 必须 ⊆ 可用工具名）
- [ ] **Step 1: 写失败测试**（interval 解析/边界/错误；cron：`*/5 * * * *`、`0 9 * * 1-5`、`30 2 1,15 * *` 的 next_after 已知时点断言；`next_due` 的无补跑语义（last_fired 很久之前 → now+every）；profile 校验含未知工具与空列表）；**Step 2: 失败**；**Step 3: 实现**；**Step 4: 收口并提交**（`feat(core): 增加调度解析与到期计算`）

---

### Task 3: core::alerts + `notify_user` 元工具

**Files:** Create `packages/core/src/alerts/{mod.rs,notifier.rs}`、`packages/core/src/tools/notify.rs`；Modify `tools/mod.rs`、`tools/meta.rs`（meta 组合）、`tools/runtime.rs`（classify Safe 臂）；Test: 内联

**Interfaces:**
- Produces:
  - `pub struct AlertRecord`（复用 storage 类型 re-export）；`#[async_trait] pub trait Notifier: Send + Sync { async fn notify(&self, alert: &AlertRecord) -> Result<(), NotifyError>; }`
  - `MultiNotifier::new(Vec<Arc<dyn Notifier>>)`；`pub struct AlertSink { db: Arc<Db>, notifier: Arc<dyn Notifier>, window_secs: i64 /*=900*/ }`
  - `AlertSink::raise(&self, severity, title, body, job: Option<&str>, task: Option<&str>) -> Result<RaiseOutcome, AlertError>`，`RaiseOutcome::{Sent(AlertRecord), Deduped(AlertRecord /*既是命中的旧行*/)}`（去重键 = `(job 或 task 或 "cli", title)`）；命中窗口内不落新行、不投递；窗口外落行并 `notifier.notify`（投递失败只警告不 fail）
  - `notify_user` 工具：args `{severity: "info"|"warning"|"critical", title, body?}`；结果 `已发送告警（{severity}）：{title}` / `告警已去重（{title}，冷却中）`；分类 `Safe`；**provenance（Ruling）**：扩展 `ToolCtx` 增 `task_id: Option<String>` 与 `job_id: Option<String>`（`ToolRuntime::execute` 填自身 task_id；`job_id` 由 TaskRunner 透传，见 T5），notify_user 以之填 `source_task_id`/`source_job_id`；该扩展为机械性改动，需同步各工具单测的 `ToolCtx` 构造
- [ ] **Step 1: 写失败测试**（窗口内第二次 Deduped 且行数不变；窗口外新行；notifier 被调用一次/零次（mock Notifier）；severity 校验；缺 title → 中文错误；`notify_user` 经 ToolRuntime::execute 为 Safe 且无审批行）；**Step 2: 失败**；**Step 3: 实现**；**Step 4: 收口并提交**（`feat(core): 增加告警与 notify_user（去重与通知投递）`）

---

### Task 4: `schedule_create` 元工具 + ProfileExecutor

**Files:** Create `packages/core/src/tools/schedule.rs`、`packages/core/src/tools/profile.rs`；Modify `tools/{mod.rs,meta.rs}`、`tools/runtime.rs`（classify Elevated 臂；`meta_tools` 增加 schedule_create 注入所需依赖——见 Produces）；Test: 内联

**Interfaces:**
- Consumes: T1 DAO、T2 解析/校验、T3 notify_user 名、P2 `meta_tools(memory, skills)`。
- Produces:
  - `meta_tools(memory, skills, scheduler: SchedulerDeps)` 扩展：`SchedulerDeps { db: Arc<Db>, available_tools: Vec<String>, default_workspace: PathBuf }`；`schedule_create` args `{name: string, prompt: string, schedule: string /*如 "10m" 或 "0 9 * * 1-5"*/, tools?: string[] /*默认 [fs_read, fs_list, shell_run, notify_user]*/, risk?: "safe"|"elevated" /*默认 elevated*/}`；行为：解析 schedule（自动判 interval/cron）→ 组装 profile/limits（limits 取 `ctx.limits` 全量）→ `validate_job` → 插 jobs 行（每 job 唯一 id）→ 结果文本摘要：`已创建调度任务 {name}（{expr}，工具：{…}，risk={…}）—— 若守护进程未运行请执行 ignis daemon start`（文本用于审批提示与模型回读）
  - `ProfileExecutor { inner: Arc<ToolRuntime>, allowed: HashSet<String> }`：`specs()` 过滤白名单；`execute`：白名单外 → `ToolResult{ok:false, "该任务 profile 不允许工具 …（已记录）"}` + 落 `deny` 审批行（mode=preauthorized）；白名单内委托 inner。
- [ ] **Step 1: 写失败测试**（schedule_create：interval/cron 自动判定、未知工具报错、默认 tools/risk、落库字段断言；经 ToolRuntime 为 Elevated（交互 Ask/--yes Allow/无人值守 Deny）；ProfileExecutor：specs 过滤、白名单外拒绝且落 deny 行、白名单内委托）；**Step 2: 失败**；**Step 3: 实现**；**Step 4: 收口并提交**（`feat(core): 增加 schedule_create 与 profile 白名单执行器`）

---

### Task 5: gateway：守护进程与调度循环

**Files:** Create `packages/gateway/{Cargo.toml,src/lib.rs,src/daemon.rs,src/loop.rs,src/notify.rs}`；Modify 根 `Cargo.toml`（成员+路径依赖）、`apps/cli`（daemon 模式入口，最小接线）；Test: 内联 + `packages/gateway/tests/daemon_lifecycle.rs`

**Interfaces:**
- Produces:
  - `daemon::{start(home) -> Result<StartOutcome>, run_foreground(home) -> Result<()>, stop(home) -> Result<StopOutcome>, status(home) -> DaemonStatus}`（start：探测 flock → re-exec `{current_exe} daemon run --foreground`（`setsid` + stdout/stderr 重定向 `logs/daemon.log` + 同环境）→ 等 pidfile（≤2s）；run_foreground：flock + 写 pid + 装 SIGTERM/SIGINT → 调 loop；stop 幂等）
  - `loop::SchedulerLoop { db, config, runtime, policy_base, approvals, memory, skills, log_dir, notifier }`：`run(cancel)` —— 启动：`sweep_stale_job_runs` + 加载 active jobs（next_due = now 起算，**无补跑**）→ 循环：取最小 due，`sleep_until`；到期：若该 job 是否有运行中（互斥标志）→ 跳过（日志）；否则 spawn：`insert_job_run(running)` → 构造 profile 策略（`Preauthorized{unsafe_ops:false}` + `ProfileExecutor`）→ `TaskRunner`（kind=schedule，workspace=job.workspace，limits=job.limits）→ 结束：`finish_job_run` + `touch_job_last_run`；status 非 success → 失败告警（`任务失败：{name}`）；重算 next_due
  - `notify::{LogNotifier, DesktopNotifier}`（DesktopNotifier：非 macOS/Linux 或 `notify-send` 不存在 → 降级为日志一行；spawn `notify-send -u <urgency> <title> <body>` 不阻塞）
  - 透传：`TaskRequest` 增 `job_id: Option<String>`（TaskRunner → `ToolCtx.job_id`，供 notify_user provenance）；`job_runs.task_id` 在 run 结束后用 `update_job_run_task_id`（T1 DAO）回填；失败告警的 `source_task_id`/`source_job_id` 由 loop 直接填
- [ ] **Step 1: 写失败测试**（loop 单测用 mock runtime + 短 interval=1s：到期产生 run 行、`job_runs` 收尾、失败告警、互斥跳过（首个运行 sleep 长于 interval→只有 1 行 run）、重启后不补跑（新 loop 的首次执行 ≥ interval 之后）；daemon lifecycle（真实进程）：start→status(running)→stop→status(stopped)，双 start 第二个报错，stop 幂等）；**Step 2: 失败**；**Step 3: 实现**；**Step 4: 收口并提交**（`feat(gateway): 增加守护进程与调度循环`）

---

### Task 6: CLI：daemon/schedule/alerts 子命令 + 创建后自拉起提示

**Files:** Create `apps/cli/src/commands/{daemon.rs,schedule.rs,alerts.rs}`；Modify `cli.rs`、`commands/mod.rs`（renderer 记录 `saw_schedule_create` 标志）、`main.rs`（`daemon run` 隐藏入口）；Test: 内联 + 手动冒烟

**Interfaces:**
- Produces:
  - `ignis daemon start|stop|status`（隐藏 `daemon run --foreground`；文案按 T5 结果）；`ignis schedule list|pause <id>|resume <id>|remove <id>|history [<id>] [--limit 20]`（list 显示 id 前 8、name、status、kind/expr、last_run、next due（按 T2 规则现算））；`ignis alerts [--all] [--read]`（默认未读；`--read` 展示并把未读标记已读；显示 severity/title/时间/来源）
  - 自拉起：任务结束后若本轮事件中出现 `schedule_create` 且 `daemon::status != Running`：交互模式 → 单次提示 `Daemon is not running; start it now? [y/N]`（y → `daemon start`）；非交互 → 打印提示行。每会话最多提示一次。
- [ ] **Step 1: 写失败测试**（schedule 行/alerts 行格式化；`--read` 语义（先读后标）；next due 展示函数）；**Step 2: 失败**；**Step 3: 实现**；**Step 4: 手动冒烟**（daemon start/status/stop、schedule 全命令、alerts 两态，贴输出）；**Step 5: 收口并提交**（`feat(cli): 增加 daemon/schedule/alerts 子命令与自拉起提示`）

---

### Task 7: 夹具与 Demo C 端到端（含暂停/恢复与重启不重复）

**Files:** Create `examples/test-server/check-health.sh`（+x；`--simulate <pct>` 覆盖磁盘百分比，否则 `df` 解析；输出 JSON `{disk_pct, mem_pct, docker}`）；Create `apps/cli/tests/demo_c.rs`；Modify `apps/cli/tests/common/mod.rs`（`read_alerts`/`read_job_runs` helper）

**Interfaces:**
- 流程（用一个 fake model + one `IGNIS_HOME`）：
  1. `ignis run "每隔 2 秒检查测试服务器，磁盘超过 85% 就通知我" --yes`；脚本 `schedule_create{name:"disk-watch", prompt:"运行 examples/test-server/check-health.sh，磁盘超过 85% 就用 notify_user 告警", schedule:"2s"}` → `Text`；断言 jobs 行 active 且 profile 含 `notify_user`、approvals 有一条 `allow/preauthorized`（schedule_create 的审计）。
  2. 以 `IGNIS_*` 环境启动 `ignis daemon start --foreground` 子进程；等 ≥1 次运行（轮询 `job_runs` ≤10s）；断言：无 CLI 进程时 run 行出现、status 收尾、tasks 行 kind=schedule。
  3. 在夹具目录写入模拟值（或任务 prompt 里用 `--simulate 92`）：让下一次运行触发 `notify_user`；断言 alerts 行（severity=warning）、`ignis alerts` 输出含标题、daemon 日志出现 `notify-send` 或降级行。
  4. 去重：连续两次告警（两轮运行同标题）只落一行（窗口内）。
  5. `ignis schedule pause <id>` → 2 个间隔内无新 run；`resume` → 恢复。
  6. 重启：SIGTERM daemon → 立即再 start → 首轮新运行不早于一个间隔（无补跑）；`job_runs` 总数只 +（自然轮次），遗留 `running`（人为在 stop 前杀掉）被收成 failed/`interrupted`。
- 测试预算：总时长 ≤ ~40s（轮询步进 200ms）。
- [ ] **Step 1: 写夹具并手动确认输出**；**Step 2: 写 `demo_c.rs`**；**Step 3: 运行**（Run: `cargo test -p ignis-cli --test demo_c`，Expected: PASS）；**Step 4: 收口并提交**（`test(e2e): 增加 test-server 夹具与 Demo C 调度守护测试`）

---

## 覆盖与自评

- 映射：§6.7（daemon/任务模型/失败策略/通知通道/告警来源与去重/创建流程）→ T1/T2/T3/T4/T5/T6/T7；决策 #18 → T4/T5（调度仅 native：T4 校验 kernel 仅 native）；#19 → T3/T5；§7 Demo C → T7；验收 #6/#8/#9 → T5/T6/T7。
- 排序约束（预检）：T3 先行于 T4（profile 默认含 `notify_user` 名与可用工具校验）；T5 依赖 T1-T4 全部；T6 依赖 T5 的进程管理接口。
- 计划内**不做**：Telegram（Notifier 预留）、跨机调度、错过补跑、重试策略（默认零重试已是规格）、`ignis doctor` 的 daemon 检查（P4 完整版）。
