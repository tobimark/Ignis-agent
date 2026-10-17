//! 命令层：`run` 与 REPL 共用的会话装配（配置/存储/策略/内核/审批）与单次任务执行，
//! 以及 `task`/`config`/`memory`/`skills`/`doctor`/`daemon`/`schedule`/`alerts` 八组辅助命令。

pub mod alerts;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod kernel;
pub mod memory;
pub mod run;
pub mod schedule;
pub mod skills;
pub mod slash;
pub mod task;

use std::ffi::{OsStr, OsString};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Result, bail};
use ignis_core::alerts::{MultiNotifier, Notifier};
use ignis_core::config::{Config, OutputMode, ensure_home, ignis_home, load_config};
use ignis_core::memory::MemoryStore;
use ignis_core::security::{ApprovalMode, Policy};
use ignis_core::skills::SkillCatalog;
use ignis_core::task::{TaskKind, TaskOutcome, TaskRequest, TaskRunner};
use ignis_runtime::{ApprovalHandler, ChatMessage, RunStatus, Usage};
use ignis_storage::Db;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approval::{InteractiveApproval, PolicyOnlyApproval, resolve_mode};
use crate::cli::Cli;
use crate::render::{Renderer, RendererOptions, format_task_end, resolve_color, width_for_stdout};

use self::kernel::{
    NATIVE_KERNEL, known_kernel, missing_api_key, mk_runtime, mk_runtime_with_path, resolve_kernel,
};
// 能力提示行由 `run` 与 REPL 共用：路径保持在 `crate::commands::kernel_note`（内部实现搬进
// `kernel` 模块，调用方不用跟着改）
pub use self::kernel::kernel_note;

/// 一次 CLI 会话的共享依赖：`run` 与 REPL 都从这里取当前内核与工作区。
pub struct Session {
    /// 数据目录（`~/.ignis` 或 `IGNIS_HOME`）：自拉起要拿它探守护进程状态、拉起守护进程。
    home: PathBuf,
    /// 本次任务的工作区（`--workspace`，否则配置里的 `workspace_root`）。
    workspace: PathBuf,
    /// 与记忆层同一份库：banner 的未读告警计数从这里取，不重新开库。
    db: Arc<Db>,
    /// 生效配置（密钥已回填）：每轮任务的内核实现与运行器都由它装配。
    config: Arc<Config>,
    policy: Arc<Policy>,
    approvals: Arc<dyn ApprovalHandler>,
    /// 任务日志目录。
    log_dir: PathBuf,
    /// 与运行器同一份记忆存储与技能目录（`Arc` 克隆）：banner 计数从这里取，
    /// 不重新开库、也不重新扫描技能目录。
    memory: Arc<MemoryStore>,
    skills: Arc<SkillCatalog>,
    /// 告警投递渠道：每轮任务的运行器由它投递（CLI 侧暂时是空组合）。
    notifier: Arc<dyn Notifier>,
    /// 当前内核名：`/kernel` 只改它，切换只影响后续任务。
    kernel: String,
    /// 本会话的输出模式：初值取自 `config.ui.output_mode`，`/details` 只改这一份（不写回配置文件）。
    output_mode: OutputMode,
    /// 会话级模型覆盖（`/model <name>`）：只在 native 内核上生效，也不写回配置文件；
    /// `None` 表示照配置里的 `model.model` 走。
    model_override: Option<String>,
    /// 最近一轮任务的用量（`/status` 的 `last turn usage` 行）：`None` = 本会话还没跑过任务。
    last_usage: Option<Usage>,
    /// 本会话是否已经做过自拉起提示（每会话至多一次，REPL 跨轮次共享）。
    already_prompted: AtomicBool,
}

/// 工作区解析与校验：`--workspace` 优先，否则配置里的 `workspace_root`；解析出的路径必须是已存在
/// 的目录（`Session::new` 与 `memory add --scope project` 共用同一套口径与守卫）。
///
/// 守卫只在这一处：拼错的 `--workspace /nope` 在碰记忆库之前就报错，不会落一条指不到目录的
/// project 事实。
pub fn resolve_workspace(cli: &Cli, config: &Config) -> Result<PathBuf> {
    let workspace = cli
        .workspace
        .clone()
        .unwrap_or_else(|| config.workspace_root.clone());
    if !workspace.is_dir() {
        bail!("工作区不存在或不是目录：{}", workspace.display());
    }
    Ok(workspace)
}

/// 生效密钥的来源：配置文件里的 `api_key`（含 `IGNIS_API_KEY` 覆盖）优先，其次配置指定的
/// 环境变量（`model.api_key_env`）；两处都没有——或只有空串/纯空白——给 `None`。
///
/// 与内核无关：native 缺它不能跑（[`missing_api_key`] 给人话错误），两个外部内核不需要它
/// （用各自 CLI 的凭据、适配器不收 Ignis 的 key）——但只要有就照旧回填进 `config.api_key`，
/// 脱敏器因此对三个内核一视同仁。
fn resolve_api_key(config: &Config) -> Option<String> {
    resolve_api_key_with(config, |key| std::env::var(key).ok())
}

/// 带环境变量注入的变体（与 core 的 `load_config_with_env` 同一手法）：用例因此不必改进程环境
/// （测试可并行）。
///
/// 空白字符串一律视为未设置（与 core 的 `apply_env_overrides` 同一口径）：否则 doctor 报「未配置
/// API key」而 native 的 `run` 拿一把空 key 上路，直到第一次请求才 401。
fn resolve_api_key_with(config: &Config, get: impl Fn(&str) -> Option<String>) -> Option<String> {
    config
        .api_key
        .clone()
        .filter(|key| !key.trim().is_empty())
        .or_else(|| get(&config.model.api_key_env).filter(|key| !key.trim().is_empty()))
}

impl Session {
    /// 装配会话：配置 → 工作区 → 选定内核 → 存储 → 策略 → 凭据 → 审批。
    ///
    /// 缺 API key（只有 native 需要）、配置解析失败、工作区不存在、建库失败、选定内核不可用都是
    /// 预期内的失败，返回人话 `Err` 即可，调用方不会因此 panic 或打印堆栈。
    pub fn new(cli: &Cli) -> Result<Self> {
        // 配置里的 api_key 已被 IGNIS_API_KEY 覆盖过，这里只是读出生效值
        let home = ignis_home();
        // 数据目录先落地（新建时 0700）：密钥、任务库与日志都在它下面
        ensure_home(&home)?;
        let mut config: Config = load_config(&home)?;

        let workspace = resolve_workspace(cli, &config)?;

        // 生效内核先定下来（`--kernel` > 配置 `default_kernel` > `native`）：它决定下面要不要
        // Ignis 的模型凭据，也决定构造哪个内核实现
        let kernel = resolve_kernel(cli.kernel.as_deref(), &config.default_kernel)
            .map_err(anyhow::Error::msg)?;

        // stdin 是否为 tty 只有命令层知道，策略规则本身在 `resolve_mode` 里
        let mode = resolve_mode(cli.yes, cli.unsafe_ops, std::io::stdin().is_terminal());
        let db = Arc::new(Db::open(&home.join("ignis.db"))?);
        let policy = Arc::new(Policy::new(mode));

        // Ignis 自己的密钥只有 native 必须：两个外部内核用各自 CLI 的凭据（适配器不收 key），
        // 只配了 codex/claude 的用户不该被这一条挡在门外。拿到的密钥一律回填进 config：
        // `TaskRunner` 与每轮任务的 `ToolRuntime` 都从 `config.api_key` 取脱敏密钥，只把它交给
        // HTTP 客户端会让环境变量这条路完全绕过 `Redactor`——工具输出与任务提示词里的密钥会
        // 明文进 JSONL 与 `tasks` 表。
        let api_key = resolve_api_key(&config);
        if kernel == NATIVE_KERNEL && api_key.is_none() {
            bail!("{}", missing_api_key(&config));
        }
        config.api_key = api_key;
        // 会话级状态的初值：输出模式跟随配置（往后 `/details` 只改会话这一份），模型无覆盖
        let output_mode = config.ui.output_mode;
        let config = Arc::new(config);

        // 只有交互模式才该问到人：其余模式策略已经给出结论，被问到说明策略漏判 → 一律拒绝
        let approvals: Arc<dyn ApprovalHandler> = match mode {
            ApprovalMode::Interactive => Arc::new(InteractiveApproval),
            ApprovalMode::Preauthorized { .. } | ApprovalMode::Unattended => {
                Arc::new(PolicyOnlyApproval)
            }
        };

        // 选定内核先构造一次：缺二进制的错误在碰记忆库与第一轮任务之前就变成人话（构造开销小）
        mk_runtime(kernel, &config, &workspace).map_err(anyhow::Error::msg)?;

        // 记忆与技能目录都落在 `~/.ignis` 下：每轮任务的运行器在组装期把已知事实与技能目录注入
        // 系统提示，也把 `remember`/`load_skill`/`notify_user`/`schedule_create` 四个元工具挂进注册表
        let memory = MemoryStore::new(Arc::clone(&db), home.join("memory"));
        let skills = SkillCatalog::load(&home.join("skills"));

        Ok(Self {
            log_dir: home.join("logs"),
            home,
            workspace,
            db,
            config,
            policy,
            approvals,
            memory,
            skills,
            // 告警投递渠道：CLI 侧先留空组合——告警照常落库与去重（`ignis alerts` 的未读收件箱
            // 就是渠道①），真实渠道（notify-send / 日志 / IM）由 T5 的 gateway 在这里注入
            notifier: Arc::new(MultiNotifier::new(Vec::new())),
            kernel: kernel.to_string(),
            output_mode,
            model_override: None,
            last_usage: None,
            already_prompted: AtomicBool::new(false),
        })
    }

    /// 本次会话的工作区（REPL banner 用）。
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// 当前内核名（REPL 的 `/kernel` 无参显示与切换提示用它）。
    pub fn kernel(&self) -> &str {
        &self.kernel
    }

    /// 切换内核：校验名字与可用性（缺二进制的错误原样交回），只影响后续任务。
    pub fn set_kernel(&mut self, kernel: &str) -> Result<&'static str, String> {
        self.set_kernel_with_path(kernel, std::env::var_os("PATH").as_deref())
    }

    /// 带 PATH 注入的切换变体：可用性判定只看传进来的这份 PATH（与 [`mk_runtime_with_path`]
    /// 同一接缝），用例因此不必改进程环境（测试可并行）。
    fn set_kernel_with_path(
        &mut self,
        kernel: &str,
        path: Option<&OsStr>,
    ) -> Result<&'static str, String> {
        let kernel = known_kernel(kernel)?;
        // native 的可用性还包括「有凭据」：`Session::new` 已经回填过能拿到的密钥，这里还是空的
        // 就说明这份安装根本没有 key——切过去只会拿一把空 key 在第一次请求时 401，提前报同一句
        // 人话；外部内核没有 Ignis 凭据也能跑，不走这条
        if kernel == NATIVE_KERNEL && self.config.api_key.is_none() {
            return Err(missing_api_key(&self.config));
        }
        // 先构造一次：不可用的内核不切换
        mk_runtime_with_path(kernel, &self.config, &self.workspace, path)?;
        self.kernel = kernel.to_string();
        Ok(kernel)
    }

    /// 本会话的模型名：有会话级覆盖时是覆盖值，否则是配置里的 `model.model`（只有 native 用得上）。
    pub fn model(&self) -> &str {
        self.model_override
            .as_deref()
            .unwrap_or(&self.config.model.model)
    }

    /// 设置会话级模型覆盖（`/model <name>`）：只影响本会话的后续任务，不写回配置文件。
    pub fn set_model(&mut self, name: &str) {
        self.model_override = Some(name.to_string());
    }

    /// 本会话的输出模式（初值来自 `config.ui.output_mode`，`/details` 可切）。
    pub fn output_mode(&self) -> OutputMode {
        self.output_mode
    }

    /// 切换本会话的输出模式：只影响后续任务的渲染，不写回配置文件。
    pub fn set_output_mode(&mut self, mode: OutputMode) {
        self.output_mode = mode;
    }

    /// 最近一轮任务的用量（`/status` 的 `last turn usage` 行）：本会话还没跑过任务时是 `None`。
    pub fn last_usage(&self) -> Option<&Usage> {
        self.last_usage.as_ref()
    }

    /// 当前内核是不是主路径的 native（`/model` 的能力分档与 `/status` 的模型行看它）。
    pub fn is_native(&self) -> bool {
        self.kernel == NATIVE_KERNEL
    }

    /// 数据目录（`~/.ignis` 或 `IGNIS_HOME`）：历史文件等会话级产物落在它下面。
    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    /// 任务库：REPL 的 `/tasks`（与 banner 的未读告警计数）共用会话这一份连接，不重新开库。
    pub(crate) fn db(&self) -> &Db {
        &self.db
    }

    /// 技能目录：REPL 的 `/skills` 复用会话构造时的那一份（不重新扫描；reload 属 P4）。
    pub(crate) fn skills(&self) -> &SkillCatalog {
        &self.skills
    }

    /// 本轮任务的生效配置：native 且有会话级模型覆盖时克隆一份并换掉 `model.model`（覆盖不写回
    /// 配置文件，也不影响别家内核）；其余情形与 `self.config` 是同一个 `Arc`，不为每轮白克隆。
    fn effective_config(&self) -> Arc<Config> {
        let Some(name) = self.model_override.as_deref() else {
            return Arc::clone(&self.config);
        };
        if !self.is_native() {
            return Arc::clone(&self.config);
        }
        let mut config = (*self.config).clone();
        config.model.model = name.to_string();
        Arc::new(config)
    }

    /// banner 第 4 行的计数：`(技能数, active 事实数)`。
    ///
    /// 事实数读不出来时只打一行警告并退回 0（banner 显示 `Memory: 0 facts`）：状态行不该因为
    /// 记忆库读失败而拦住整个 REPL。
    pub fn banner_counts(&self) -> (usize, usize) {
        let skills = self.skills.metas().len();
        let memory = match self.memory.list(false) {
            Ok(facts) => facts.len(),
            Err(error) => {
                eprintln!("warning: 读取记忆计数失败：{error}");
                0
            }
        };
        (skills, memory)
    }

    /// banner 的未读告警计数（spec §6.1：启动时若有未读告警，顶部横幅提示）。
    ///
    /// 读不出来时打一行警告并回 `None`（banner 跳过那一行）：告警收件箱读不动不该拦住 REPL，
    /// 与记忆计数同一套 fail-soft 口径。
    pub fn unread_alerts(&self) -> Option<usize> {
        match self.db.unread_alert_count() {
            Ok(count) => Some(count),
            Err(error) => {
                eprintln!("warning: 读取未读告警计数失败：{error}");
                None
            }
        }
    }

    /// 跑一次任务：渲染与 `runner.run` 在**同一任务内交错消费**事件，Ctrl-C 监听任务负责中止本次运行。
    ///
    /// 每轮按当前内核装配运行器（native 建 HTTP 客户端，外部内核只做一次 PATH 检查），
    /// 因此 `/kernel` 的切换只影响后续任务。本轮配置由 [`Session::effective_config`] 给出
    /// （native 的会话级模型覆盖在这里生效）。渲染选项每轮重算：模式来自会话状态（`/details`
    /// 切过就按新的来），颜色/主题/宽度来自 `config.ui` + stdout 的 tty 状态（宽度每轮探测一次
    /// 终端）。结束后先 `finish()` 冲刷挂起内容再打结束行，随后把本轮用量记进会话
    /// （`/status` 的 `last turn usage` 行读它），再按本轮事件决定要不要提示自拉起守护进程。
    /// 监听任务必须随本次运行结束，否则 REPL 会攒下一串永远不会触发的监听器。
    ///
    /// 审批提示是审批层对 stdout 的直接写入（不经渲染器），异步管线下它与工具行的硬写序做不到
    /// ——记为已知限制，P2/P5 单一写者时收口。
    pub async fn run_task(
        &mut self,
        prompt: String,
        history: Vec<ChatMessage>,
        kind: TaskKind,
    ) -> Result<TaskOutcome> {
        // 本轮配置：native 的会话级模型覆盖在这里生效（外部内核用各自的默认模型）
        let config = self.effective_config();
        let runtime =
            mk_runtime(&self.kernel, &config, &self.workspace).map_err(anyhow::Error::msg)?;
        let runner = TaskRunner::new(
            Arc::clone(&self.db),
            Arc::clone(&config),
            runtime,
            Arc::clone(&self.policy),
            Arc::clone(&self.approvals),
            self.log_dir.clone(),
            Arc::clone(&self.memory),
            Arc::clone(&self.skills),
            Arc::clone(&self.notifier),
            // CLI 交互运行没有 job profile：执行器不包白名单（调度运行由守护进程注入）
            None,
        );

        let (sink, mut events) = mpsc::unbounded_channel();
        // 自拉起的判定看「本轮事件里出现过 schedule_create」：由渲染器在事件流里顺手记下
        let saw_schedule_create = Arc::new(AtomicBool::new(false));
        let options = renderer_options(
            &config,
            self.output_mode,
            std::io::stdout().is_terminal(),
            // 非 UTF-8 的取值照样算「已设置」：转换在 `no_color_value` 里做有损解码
            no_color_value(std::env::var_os("NO_COLOR")).as_deref(),
        );
        let mut renderer = Renderer::new(
            Box::new(std::io::stdout()),
            options,
            Arc::clone(&saw_schedule_create),
        );

        let cancel = CancellationToken::new();
        let watcher = spawn_ctrl_c_watcher(cancel.clone());

        let started = Instant::now();
        // 渲染在同一任务内与运行器交错消费（不另起渲染任务）：写序竞态压到最低，事件不再经第二个
        // 任务中转。`run` 返回后再把已经入队的剩余事件排干（通道随 sink 析构关闭）。
        let run = runner.run(
            TaskRequest {
                prompt,
                history,
                kind,
                workspace: self.workspace.clone(),
                // 交互运行没有来源 job（调度运行由守护进程填 job.id）
                job_id: None,
            },
            sink,
            cancel,
        );
        tokio::pin!(run);
        let mut events_open = true;
        let result = loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                event = events.recv(), if events_open => match event {
                    Some(event) => renderer.on_event(&event),
                    // sink 已经析构但 `run` 还没返回：通道不会再给事件，继续等它
                    None => events_open = false,
                },
            }
        };
        watcher.abort();
        // 排干剩余事件（`run` 返回时可能还有已经入队的）：结束行必须排在它们后面
        while let Ok(event) = events.try_recv() {
            renderer.on_event(&event);
        }
        renderer.finish();

        let outcome = result?;
        // 记本轮用量：`/status` 的 `last turn usage` 行读它（失败/触限的运行也有用量）
        self.last_usage = Some(outcome.usage);
        println!(
            "{}",
            format_task_end(
                &outcome.id,
                outcome.status,
                started.elapsed(),
                &outcome.usage,
                &outcome.skills_used,
                outcome.tool_calls,
                options.mode,
            )
        );
        // 非成功终态不能只留一个状态词：原因（内核给出、已过 Redactor）打到 stderr，与
        // `main` 的错误行同形。任务 id 已在结束行里，随后可直接 `ignis task show {id}`。
        if outcome.status != RunStatus::Success
            && let Some(error) = &outcome.error
        {
            eprintln!("error: {error}");
        }
        // 本轮建过调度任务、守护进程又没在跑时提示一次自拉起（每会话至多一次）
        daemon::autostart(
            self.home(),
            saw_schedule_create.load(Ordering::Relaxed),
            &self.already_prompted,
        )
        .await;
        Ok(outcome)
    }
}

/// 渲染选项：模式来自**会话状态**（`/details` 只改会话，不写回配置），主题来自 `config.ui`，
/// `ColorChoice` + stdout 的 tty 状态 + `NO_COLOR` 定颜色，宽度按 stdout 类型给
/// （tty → 探测终端列数，非 tty → 不折行）。
fn renderer_options(
    config: &Config,
    mode: OutputMode,
    is_tty: bool,
    no_color: Option<&str>,
) -> RendererOptions {
    RendererOptions {
        mode,
        color: resolve_color(config.ui.color, is_tty, no_color),
        theme: config.ui.theme,
        width: width_for_stdout(is_tty),
    }
}

/// `NO_COLOR` 环境变量的取值转换：存在即算「已设置」——值本身不解码，非 UTF-8 按
/// `to_string_lossy` 有损处理（no-color.org 的约定只看「存在且非空」，不看内容；空串由
/// [`resolve_color`] 按未设置处理）。
fn no_color_value(raw: Option<OsString>) -> Option<String> {
    raw.map(|value| value.to_string_lossy().into_owned())
}

/// 起一个 Ctrl-C 监听任务：收到信号就取消本次运行，返回的句柄由调用方在运行结束后 abort。
///
/// `tokio::signal::ctrl_c()` 装的是进程级处理；装不上（不支持的平台）时监听任务直接结束，
/// 不会 panic，也不会把用户卡在无法中止的运行里。
pub fn spawn_ctrl_c_watcher(cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    })
}

/// 会话夹具：本模块与 `slash` 的内联用例共用（临时 home/工作区 + 空记忆与技能目录，
/// 绝不碰真实 `~/.ignis`）。
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::commands::kernel::fixtures::TempDir;

    /// 最小会话：只装配场景要用的字段（临时 home/工作区 + 空记忆与技能目录）。
    pub(crate) fn session_fixture(label: &str, kernel: &str) -> (TempDir, Session) {
        session_fixture_with_config(
            label,
            kernel,
            Config {
                api_key: Some("test-key".into()),
                ..Config::default()
            },
        )
    }

    /// 一份 Ignis 凭据都没有的会话（外部内核起步的形状）：验 native 的凭据门槛。
    ///
    /// 环境变量名取一个不存在的（不走默认的 `OPENAI_API_KEY`），断言因此与运行者环境无关。
    pub(crate) fn keyless_session_fixture(label: &str, kernel: &str) -> (TempDir, Session) {
        session_fixture_with_config(
            label,
            kernel,
            Config {
                model: ignis_runtime::ModelConfig {
                    api_key_env: "IGNIS_T3_ABSENT_KEY".into(),
                    ..ignis_runtime::ModelConfig::default()
                },
                ..Config::default()
            },
        )
    }

    /// 两个会话夹具共用的装配体：配置由调用方给（其余字段只求「可实现」）。
    /// 会话级状态的初值与 [`Session::new`] 一致：输出模式取自配置、模型无覆盖、没有用量。
    pub(crate) fn session_fixture_with_config(
        label: &str,
        kernel: &str,
        config: Config,
    ) -> (TempDir, Session) {
        let dir = TempDir::new(label);
        let home = dir.path().to_path_buf();
        let db = Arc::new(Db::open(&home.join("ignis.db")).unwrap());
        let output_mode = config.ui.output_mode;
        let session = Session {
            home: home.clone(),
            workspace: home.clone(),
            db: Arc::clone(&db),
            config: Arc::new(config),
            policy: Arc::new(Policy::new(ApprovalMode::Unattended)),
            approvals: Arc::new(PolicyOnlyApproval),
            log_dir: home.join("logs"),
            memory: MemoryStore::new(db, home.join("memory")),
            skills: SkillCatalog::load(&home.join("skills")),
            notifier: Arc::new(MultiNotifier::new(Vec::new())),
            kernel: kernel.to_string(),
            already_prompted: AtomicBool::new(false),
            output_mode,
            model_override: None,
            last_usage: None,
        };
        (dir, session)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ignis_core::config::{ColorChoice, OutputMode, Theme, UiConfig};

    use super::fixtures::{keyless_session_fixture, session_fixture};
    use super::*;
    use crate::commands::kernel::fixtures::{TempDir, set_executable};

    /// 凭据来源与门槛：有 key（配置文件优先）就回填交回，没有时 native 报人话错误、外部内核照跑。
    #[test]
    fn api_key_resolution_feeds_native_and_gives_external_kernels_a_pass() {
        // 配置里的 key（含 `IGNIS_API_KEY` 覆盖）优先，原样交回
        let with_key = Config {
            api_key: Some("test-key".into()),
            ..Config::default()
        };
        assert_eq!(resolve_api_key(&with_key).as_deref(), Some("test-key"));

        // 两处都没有：来源是 `None`；错误文案点名该设哪个环境变量
        //（用不存在的变量名，断言不受运行者环境影响）
        let missing = Config {
            model: ignis_runtime::ModelConfig {
                api_key_env: "IGNIS_T3_ABSENT_KEY".into(),
                ..ignis_runtime::ModelConfig::default()
            },
            ..Config::default()
        };
        assert_eq!(resolve_api_key(&missing), None);
        let error = missing_api_key(&missing);
        assert!(error.contains("未配置 API key"), "{error}");
        assert!(error.contains("IGNIS_T3_ABSENT_KEY"), "{error}");
    }

    /// 空串/纯空白一律视为没配 key（`IGNIS_API_KEY=""`、配置里写了空串、`model.api_key_env`
    /// 指向空串）：`run` 的密钥解析与 doctor 的结论必须一致——否则 doctor 报「未配置」（native
    /// 时 ✗）而 `run` 拿一把空 key 上路，直到第一次请求才 401。
    #[test]
    fn empty_api_key_values_count_as_missing_on_both_sides() {
        let dir = TempDir::new("empty-key");
        // 环境变量名取一个不存在的（不走默认的 `OPENAI_API_KEY`），断言与运行者环境无关
        let model = ignis_runtime::ModelConfig {
            api_key_env: "IGNIS_T8_ABSENT_KEY".into(),
            ..ignis_runtime::ModelConfig::default()
        };

        // `IGNIS_API_KEY=""`：core 的加载把空白值视为未设置，`config.api_key` 不会变成 `Some("")`
        let loaded = ignis_core::config::load_config_with_env(dir.path(), |key| {
            (key == "IGNIS_API_KEY").then(String::new)
        })
        .unwrap();
        assert_eq!(loaded.api_key, None, "空串不该变成一把密钥");
        assert_eq!(
            resolve_api_key_with(&loaded, |_| None),
            None,
            "模型环境变量也没有：`run` 视为没有 key"
        );

        // 落在 `Some("")`/纯空白的两个来源：配置里的空串与设成空串的环境变量都不算
        let blank = Config {
            api_key: Some("   ".into()),
            model: model.clone(),
            ..Config::default()
        };
        assert_eq!(resolve_api_key_with(&blank, |_| None), None);
        let no_key = Config {
            model: model.clone(),
            ..Config::default()
        };
        assert_eq!(resolve_api_key_with(&no_key, |_| Some("  ".into())), None);

        // doctor 的结论与上面一致：native 下同一条「未配置 API key」是 ✗
        let results = ignis_core::doctor::run_checks(
            dir.path(),
            &Ok(blank),
            ignis_core::doctor::NATIVE_KERNEL,
        );
        let api_key = results
            .iter()
            .find(|result| result.name == ignis_core::doctor::CHECK_API_KEY)
            .expect("api_key 行必在");
        assert_eq!(
            api_key.level,
            ignis_core::doctor::CheckLevel::Fail,
            "{api_key:?}"
        );
        assert!(api_key.detail.contains("未配置 API key"), "{api_key:?}");
    }

    /// `/kernel` 的切换语义：校验不过只报错、当前内核不动；过了才换（换完只影响后续任务）。
    #[test]
    fn set_kernel_switches_only_when_the_name_and_availability_pass() {
        // 从外部内核起步：「失败后还停在 codex」才是可观测的（native 起步时不动与切回 native 分不清）
        let (_dir, mut session) = session_fixture("switch", "codex");

        for bad in ["gemini", "Native", ""] {
            let error = session.set_kernel(bad).unwrap_err();
            assert!(error.contains(&format!("未知内核 '{bad}'")), "{error}");
            assert_eq!(session.kernel(), "codex", "校验不过不该换内核");
        }

        // native 不查 PATH（会话本身有凭据），切过去并回生效名，且没有能力提示行
        assert_eq!(session.set_kernel("native").unwrap(), "native");
        assert_eq!(session.kernel(), "native");
        assert_eq!(kernel_note(session.kernel()), None);
    }

    /// native 的凭据门槛在**切换点**也成立：只有外部内核凭据的会话切到 native 时给人话错误、
    /// 当前内核不动（否则切过去只会拿一把空 key 在第一次请求时 401）。
    #[test]
    fn set_kernel_refuses_native_without_an_ignis_key() {
        let (_dir, mut session) = keyless_session_fixture("switch-keyless", "codex");

        let error = session.set_kernel("native").unwrap_err();
        assert!(error.contains("未配置 API key"), "{error}");
        assert!(error.contains("IGNIS_T3_ABSENT_KEY"), "{error}");
        assert_eq!(session.kernel(), "codex", "缺凭据不该换到 native");

        // 外部内核之间切换不受影响：没有 Ignis 凭据照样切（各自 CLI 的凭据管）
        let (dir, mut session) = keyless_session_fixture("switch-keyless-external", "codex");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("claude"), "#!/bin/sh\n").unwrap();
        set_executable(&bin.join("claude"));
        assert_eq!(
            session
                .set_kernel_with_path("claude", Some(bin.as_os_str()))
                .unwrap(),
            "claude"
        );
        assert_eq!(session.kernel(), "claude");
    }

    /// `/kernel` 的**可用性**失败路径（与上一条的「名字非法」并列）：PATH 上没有这个 CLI 时
    /// 错误原样交回、当前内核不动；装上之后同一份 PATH 注入就能切过去。
    #[test]
    fn set_kernel_keeps_the_current_kernel_when_the_binary_is_missing() {
        let (dir, mut session) = session_fixture("switch-missing", "native");
        // 空目录当 PATH：codex 没装
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let path = Some(bin.as_os_str());

        let error = session
            .set_kernel_with_path("codex", path)
            .expect_err("PATH 上没有 codex 时应报错");
        assert_eq!(error, "找不到 codex CLI：请先安装 codex 并确保它在 PATH 中");
        assert_eq!(session.kernel(), "native", "不可用的内核不切换");

        // 装上就能切：可用性只看传进来的这份 PATH，进程环境一个字节都没动
        fs::write(bin.join("codex"), "#!/bin/sh\n").unwrap();
        set_executable(&bin.join("codex"));
        assert_eq!(
            session.set_kernel_with_path("codex", path).unwrap(),
            "codex"
        );
        assert_eq!(session.kernel(), "codex");
        // 同一次切换后，claude 仍然没装：再失败一次，内核停在 codex
        let error = session
            .set_kernel_with_path("claude", path)
            .expect_err("PATH 上没有 claude 时应报错");
        assert_eq!(
            error,
            "找不到 claude CLI：请先安装 claude 并确保它在 PATH 中"
        );
        assert_eq!(session.kernel(), "codex");
    }

    /// 渲染选项的三个来源各管一段：输出模式来自**会话状态**（`/details` 只改会话），主题来自
    /// `config.ui`，`ColorChoice` + tty + `NO_COLOR` 定颜色，stdout 不是 tty 时宽度是「不折行」。
    #[test]
    fn renderer_mode_comes_from_the_session() {
        let defaults = Config::default();
        // 参数里的模式压过配置里的 `ui.output_mode`（配置是 normal）
        let options = renderer_options(&defaults, OutputMode::Debug, false, None);
        assert_eq!(options.mode, OutputMode::Debug);
        assert_eq!(options.theme, Theme::Dark);
        assert!(!options.color, "Auto + 非 tty 不上色");
        assert_eq!(options.width, usize::MAX, "非 tty 不插入物理换行");

        let loud = Config {
            ui: UiConfig {
                output_mode: OutputMode::Debug,
                color: ColorChoice::Always,
                theme: Theme::Light,
            },
            ..Config::default()
        };
        // 配置里的模式只在会话没切过时生效：会话切到 minimal，渲染就按 minimal 走
        let options = renderer_options(&loud, OutputMode::Minimal, false, None);
        assert_eq!(options.mode, OutputMode::Minimal);
        assert_eq!(options.theme, Theme::Light);
        assert!(options.color);
        // `Always` 即使非 tty 也上色；`NO_COLOR` 非空恒无色（含 `Always`）
        assert!(!renderer_options(&loud, OutputMode::Debug, false, Some("1")).color);
        assert!(renderer_options(&loud, OutputMode::Debug, false, Some("")).color);
    }

    /// 会话访问器：`model` 覆盖优先、`is_native` 跟内核、`home` 指数据目录（history 路径的落点）。
    #[test]
    fn session_accessors_mirror_the_kernel_and_the_model_override() {
        let (dir, mut session) = session_fixture("accessors", "native");
        assert!(session.is_native());
        assert_eq!(session.home(), dir.path());
        assert_eq!(session.output_mode(), OutputMode::Normal, "初值跟随配置");
        let configured = session.model().to_string();
        assert_eq!(session.last_usage(), None, "还没跑过任务");

        session.set_model("test-model-2");
        assert_eq!(session.model(), "test-model-2");
        assert_ne!(session.model(), configured);
        session.set_output_mode(OutputMode::Debug);
        assert_eq!(session.output_mode(), OutputMode::Debug);

        let (_dir, codex) = session_fixture("accessors-codex", "codex");
        assert!(!codex.is_native());
    }

    /// 本轮配置：只有 native + 会话级模型覆盖才克隆一份换掉 `model.model`；其余情形与
    /// `self.config` 是同一个 `Arc`（外部内核用各自默认模型，覆盖对它们不生效）。
    #[test]
    fn effective_config_swaps_the_model_only_for_native_overrides() {
        // 无覆盖：同一个 Arc（不必为每轮任务克隆配置）
        let (_dir, session) = session_fixture("effective-none", "native");
        assert!(Arc::ptr_eq(&session.effective_config(), &session.config));

        // native + 覆盖：换模型、其它字段原样、且是一份新配置
        let (_dir, mut session) = session_fixture("effective-native", "native");
        session.set_model("test-model-2");
        let config = session.effective_config();
        assert_eq!(config.model.model, "test-model-2");
        assert_eq!(config.api_key, session.config.api_key);
        assert!(
            !Arc::ptr_eq(&config, &session.config),
            "克隆出来的是一份新配置"
        );

        // 外部内核 + 覆盖：原样（覆盖只对 native 生效，不静默改别家内核的模型）
        let (_dir, mut session) = session_fixture("effective-codex", "codex");
        session.set_model("test-model-2");
        assert!(Arc::ptr_eq(&session.effective_config(), &session.config));
        assert_eq!(
            session.effective_config().model.model,
            session.config.model.model
        );
    }

    /// F3（评审遗留）：`NO_COLOR` 只看「存在且非空」，不看值本身——非 UTF-8 的取值有损解码后
    /// 仍非空，必须算「已设置」（否则一个乱码值会把颜色悄悄放回来，违反 no-color.org 的约定）。
    #[cfg(unix)]
    #[test]
    fn no_color_value_treats_non_utf8_values_as_set() {
        use std::os::unix::ffi::OsStringExt;

        // 非 UTF-8 字节：有损解码得到替换字符（非空），颜色照关
        let value = no_color_value(Some(OsString::from_vec(vec![0xff, 0xfe])));
        assert!(value.as_deref().is_some_and(|v| !v.is_empty()), "{value:?}");
        assert!(!resolve_color(ColorChoice::Always, true, value.as_deref()));

        // 未设置 / 空串：按 NO_COLOR 的约定不关颜色（空串走到 `resolve_color` 里按未设置处理）
        assert_eq!(no_color_value(None), None);
        assert_eq!(
            no_color_value(Some(OsString::from(""))).as_deref(),
            Some("")
        );
        // 普通取值原样透传
        assert_eq!(
            no_color_value(Some(OsString::from("1"))).as_deref(),
            Some("1")
        );
    }
}
