//! 命令层：`run` 与 REPL 共用的会话装配（配置/存储/策略/内核/审批）与单次任务执行，
//! 以及 `task`/`config`/`memory`/`skills`/`doctor`/`daemon`/`schedule`/`alerts` 八组辅助命令。

pub mod alerts;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod kernel;
pub mod learn;
pub mod memory;
pub mod run;
pub mod schedule;
pub mod skills;
pub mod slash;
pub mod task;

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Result, bail};
use ignis_core::alerts::{MultiNotifier, Notifier};
use ignis_core::config::{Config, OutputMode, ensure_home, ignis_home, load_config};
use ignis_core::learn as core_learn;
use ignis_core::learn::{LearnError, SkillDraft};
use ignis_core::memory::MemoryStore;
use ignis_core::security::{ApprovalMode, Policy, Redactor};
use ignis_core::session::{
    COMPACT_KEEP_TURNS, HistoryMessage, SessionStore, render_transcript, split_turns,
    summarize_turns, summary_message,
};
use ignis_core::skills::SkillCatalog;
use ignis_core::task::{PromptStats, TaskKind, TaskOutcome, TaskRequest, TaskRunner};
use ignis_runtime::{ApprovalHandler, ChatMessage, Role, RunStatus, Usage};
use ignis_storage::{Db, SessionSummary};
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

/// `/sessions` 列出的条数上限（本工作区最近 N 条；只影响展示，不删数据）。
const SESSIONS_LIMIT: usize = 10;

/// `/learn` 选不到可学任务时的错误：默认目标没有成功任务、显式 id 命中但不是成功状态共用这一句
/// （逐字钉住；与 `task not found` 分家是刻意的——任务在不在与任务能不能学是两回事）。
const NO_COMPLETED_TASK: &str = "learn: no completed task to learn from";

/// 确认面的未来：`BoxFuture<'static, bool>` 的口径。CLI crate 不依赖 `futures`，用 std 拼同一个类型
/// （零新增依赖）。
pub type ConfirmLearn = Pin<Box<dyn Future<Output = bool> + Send + 'static>>;

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
    /// 与运行器同一份记忆存储（`Arc` 克隆）：banner 的 active 事实计数从这里取，不重新开库。
    memory: Arc<MemoryStore>,
    /// 本会话当前的技能目录快照：banner 与 `/skills` 都读这一份，不每次重新扫描。
    ///
    /// 锁里放 `Arc`、读方克隆一次就走（[`Session::skills`]）——`Session` 本身是 `&mut` 独占的，
    /// 加锁只为了 `/skills reload` 整份替换时不让读者看到半份目录；不加外层 `Arc`。
    skills: Mutex<Arc<SkillCatalog>>,
    /// 告警投递渠道：每轮任务的运行器由它投递（CLI 侧暂时是空组合）。
    notifier: Arc<dyn Notifier>,
    /// 当前内核名：`/kernel` 只改它，切换只影响后续任务。
    kernel: String,
    /// 本会话的输出模式：初值取自 `config.ui.output_mode`，`/details` 只改这一份（不写回配置文件）。
    output_mode: OutputMode,
    /// 会话级模型覆盖（`/model <name>`）：只在 native 内核上生效，也不写回配置文件；
    /// `None` 表示照配置里的 `model.model` 走。
    model_override: Option<String>,
    /// 跨轮 canonical 上下文（从 `repl_loop` 搬进来）：每轮成功后换成运行器交回的
    /// `outcome.messages`，运行器返回 `Err` 时不动。
    history: Vec<ChatMessage>,
    /// 本进程已绑定的会话 id（懒创建）：`None` = 还没有会话行，首个 REPL 任务才落库。
    current_session: Option<String>,
    /// 最近一轮任务的用量（`/status` 的 `last turn usage` 行）：`None` = 本会话还没跑过任务。
    last_usage: Option<Usage>,
    /// 最近一轮任务的组装统计（`/context` 的 `chars` 与 `prompt` 行）：`None` = 本会话还没跑过
    /// 任务、或最近一次会话切换（`-c` / `/new` / `/resume`）把它清掉了。
    last_prompt_stats: Option<PromptStats>,
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

/// 审批处理器选择：交互模式用注入的那个（没有则 [`InteractiveApproval`]），其余模式一律
/// [`PolicyOnlyApproval`]——`--yes`/`--unsafe` 的预授权语义不该被 TUI 的提问面绕开。
///
/// 私有：选择规则由内联用例直测（[`Session::build`] 是唯一调用点）。
fn approvals_for(
    mode: ApprovalMode,
    injected: Option<Arc<dyn ApprovalHandler>>,
) -> Arc<dyn ApprovalHandler> {
    match mode {
        ApprovalMode::Interactive => injected.unwrap_or_else(|| Arc::new(InteractiveApproval)),
        ApprovalMode::Preauthorized { .. } | ApprovalMode::Unattended => {
            Arc::new(PolicyOnlyApproval)
        }
    }
}

impl Session {
    /// 装配会话：配置 → 工作区 → 选定内核 → 存储 → 策略 → 凭据 → 审批。
    ///
    /// 缺 API key（只有 native 需要）、配置解析失败、工作区不存在、建库失败、选定内核不可用都是
    /// 预期内的失败，返回人话 `Err` 即可，调用方不会因此 panic 或打印堆栈。
    ///
    /// 审批处理器按模式选（见 [`approvals_for`]）：交互模式是 [`InteractiveApproval`]；TUI 用
    /// [`Session::new_with_handler`] 把内联审批注入进来。
    pub fn new(cli: &Cli) -> Result<Self> {
        Self::build(cli, None)
    }

    /// 与 [`Session::new`] 相同，但审批处理器由调用方注入（只对交互模式生效，见 [`approvals_for`]）。
    pub fn new_with_handler(cli: &Cli, approvals: Arc<dyn ApprovalHandler>) -> Result<Self> {
        Self::build(cli, Some(approvals))
    }

    /// 两个会话入口的公共实现：`injected` 只对交互模式生效（预授权/无人值守下策略结论优先）。
    fn build(cli: &Cli, injected: Option<Arc<dyn ApprovalHandler>>) -> Result<Self> {
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

        // 只有交互模式才该问到人：其余模式策略已经给出结论，被问到说明策略漏判 → 一律拒绝。
        // 注入的处理器（TUI 的内联审批）只在这里生效——`--yes`/`--unsafe` 的预授权语义不可绕开
        let approvals = approvals_for(mode, injected);

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
            skills: Mutex::new(skills),
            // 告警投递渠道：CLI 侧先留空组合——告警照常落库与去重（`ignis alerts` 的未读收件箱
            // 就是渠道①），真实渠道（notify-send / 日志 / IM）由 T5 的 gateway 在这里注入
            notifier: Arc::new(MultiNotifier::new(Vec::new())),
            kernel: kernel.to_string(),
            output_mode,
            model_override: None,
            history: Vec::new(),
            current_session: None,
            last_usage: None,
            last_prompt_stats: None,
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

    /// 最近一轮任务的组装统计（`/context` 的 `chars` 与 `prompt` 位）：本会话还没跑过任务时是
    /// `None`，`/new` 与两个恢复路径之后也是 `None`。
    pub fn last_prompt_stats(&self) -> Option<&PromptStats> {
        self.last_prompt_stats.as_ref()
    }

    /// 字符预算（`config.limits.context_char_budget`，默认 200_000）：`/context` 的预算位。
    pub fn context_budget(&self) -> usize {
        self.config.limits.context_char_budget
    }

    /// 当前会话 id（banner 与 `/status` 的 Session 行读它）：本进程还没落过会话行时是 `None`。
    pub fn current_session(&self) -> Option<&str> {
        self.current_session.as_deref()
    }

    /// 跨轮 canonical 上下文：每轮成功后是运行器交回的 `outcome.messages`，`-c` 恢复后是
    /// 库里读出的有效历史。
    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    /// 本会话有没有 Ignis 自己的凭据（native 的起步门槛；外部内核用各自 CLI 的凭据）。
    pub fn has_credentials(&self) -> bool {
        self.config.api_key.is_some()
    }

    /// 会话存储：与任务库共用同一个 `Arc<Db>`，只是每次按需包一层薄封装。
    pub(crate) fn store(&self) -> SessionStore {
        SessionStore::new(Arc::clone(&self.db))
    }

    /// 会话面的脱敏器：与每轮任务的 `TaskRunner` / `ToolRuntime` 同一来源（`config.api_key`，
    /// 已在 `Session::new` 里回填生效值）。
    ///
    /// 标题、轮次落库与压缩摘要都先过它——命令层写下的会话行因此与运行器写的历史同源，
    /// 用户粘贴过的已知密钥不会从 REPL 这一侧泄漏。
    fn redactor(&self) -> Redactor {
        Redactor::new(self.config.api_key.iter().cloned().collect())
    }

    /// 首个 REPL 任务：懒创建会话行（标题 = 首条用户消息）并记住 id；已有会话直接返回。
    ///
    /// 错误只交回人话文本（调用方 fail-soft：警告一行、本轮不绑会话，任务照跑）。
    /// `model` 只在 native 时落库：外部内核用各自的默认模型，Ignis 这边的模型名对它们没有意义。
    fn ensure_session(&mut self, prompt: &str) -> Result<String, String> {
        if let Some(id) = &self.current_session {
            return Ok(id.clone());
        }
        let model = self.is_native().then(|| self.model().to_string());
        let record = self
            .store()
            .create_session(&self.workspace, &self.kernel, model.as_deref(), prompt)
            .map_err(|error| error.to_string())?;
        self.current_session = Some(record.id.clone());
        Ok(record.id)
    }

    /// 清掉「最近一轮」的观察值（用量与组装统计）：`/new` 与两个恢复路径共用这一处。
    ///
    /// 两个值都是**上一段运行**的观察值，跟过来只会让 `/status` 与 `/context` 说谎。集中在这里
    /// 是因为会话状态的清空点有三个（[`Session::resume_latest`] / [`Session::start_new_session`] /
    /// [`Session::resume_session`]）：将来再加轮次观察值只补这一行，不会各抄一遍、漏一处。
    fn clear_turn_state(&mut self) {
        self.last_usage = None;
        self.last_prompt_stats = None;
    }

    /// 换上一段会话（`-c` 与 `/resume` 共用）：当前会话指针与 canonical 历史成对更新，最近一轮的
    /// 观察值跟着清掉（见 [`Session::clear_turn_state`]）。
    ///
    /// 指针与历史必须一起换：只换一处就会让下一轮任务把旧会话的历史送进内核，或在旧会话上追加
    /// 新一轮消息。库里读出的 [`HistoryMessage`] 在这里统一还原成送进内核的 canonical 消息。
    fn adopt_history(&mut self, id: String, history: Vec<HistoryMessage>) {
        self.current_session = Some(id);
        self.history = history
            .into_iter()
            .map(|message| message.to_chat_message())
            .collect();
        self.clear_turn_state();
    }

    /// `-c`：装载本工作区最近一次会话的有效历史并设为当前会话；没有会话给 `Ok(None)`。
    ///
    /// 命中时清掉最近一轮的观察值（用量与组装统计，见 [`Session::clear_turn_state`]）：那是上一段
    /// 运行的观察值，恢复后 `last turn usage` 与 `/context` 的统计行都应回到起点。未命中什么都不改。
    pub fn resume_latest(&mut self) -> Result<Option<usize>, String> {
        let store = self.store();
        let Some(record) = store
            .latest_for_workspace(&self.workspace)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let history = store
            .effective_history(&record.id)
            .map_err(|error| error.to_string())?;
        let count = history.len();
        self.adopt_history(record.id, history);
        Ok(Some(count))
    }

    /// `/new`：清会话指针与历史，回到「本进程还没绑会话」的起点。
    ///
    /// 只清内存里的会话状态——模式、模型覆盖、内核一律不碰，数据也一行不删（旧会话留在
    /// `sessions` 表里，之后可用 `/sessions` + `/resume` 回去）。最近一轮的观察值（用量与组装
    /// 统计）也一并清空（与 [`Session::resume_latest`] 同一口径）。
    pub fn start_new_session(&mut self) {
        self.history.clear();
        self.current_session = None;
        self.clear_turn_state();
    }

    /// `/resume <id|前缀>`：按前缀解析本工作区的一条会话，装载有效历史并设为当前会话；
    /// 返回装载的消息条数。
    ///
    /// 先过 [`task::is_id_prefix`] 的准入（≥8 字符、只含 uuid 字符集）：拼错或过短的参数直接报
    /// `session not found`，不去库里翻无关行。命中唯一一条时换成它的有效历史（最近一次压缩的
    /// 摘要 + 其后原始消息），并清掉最近一轮的观察值（用量与组装统计，见
    /// [`Session::clear_turn_state`]）；命中多条把候选（新近在前、短 id、与 `/sessions` 同一口径）
    /// 交给用户自己挑。错误一律人话 `Err`，由 REPL 原样打成 `error: {…}`。
    pub fn resume_session(&mut self, id: &str) -> Result<usize, String> {
        if !task::is_id_prefix(id) {
            return Err(format!("session not found: {id}"));
        }
        let store = self.store();
        let matches = store
            .sessions_with_prefix(&self.workspace, id, task::CANDIDATES)
            .map_err(|error| error.to_string())?;
        match matches.as_slice() {
            [] => Err(format!("session not found: {id}")),
            [record] => {
                let history = store
                    .effective_history(&record.id)
                    .map_err(|error| error.to_string())?;
                let count = history.len();
                self.adopt_history(record.id.clone(), history);
                Ok(count)
            }
            many => Err(format!(
                "ambiguous session id: {id}（候选：{}）",
                many.iter()
                    .map(|record| crate::render::short_id(&record.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// `/sessions`：本工作区最近 [`SESSIONS_LIMIT`] 条会话（新近在前，含 canonical 消息条数）。
    ///
    /// `message_count` 是 `session_messages` 的**原始行数**（含已被压缩覆盖、不在有效历史里的
    /// 前缀）；展示层知情即可，与 Task 2 的文档口径一致。
    pub fn sessions(&self) -> Result<Vec<SessionSummary>, String> {
        self.store()
            .list_for_workspace(&self.workspace, SESSIONS_LIMIT)
            .map_err(|error| error.to_string())
    }

    /// `/compact`：把有效历史里较早的轮次摘要化（设计 §6.7、决策 #20）。
    ///
    /// 检查顺序即错误优先级（测试锁定）：
    /// 1. 当前内核不是 native、或没有 Ignis 凭据 → 凭据错误（外置内核不提供摘要所需的凭据）；
    /// 2. 还没绑定会话 → `compact: no session yet`；
    /// 3. 有效历史的 user 消息 ≤ [`COMPACT_KEEP_TURNS`] 条（没有可压段）→
    ///    `Ok(["compact: nothing to compact"])`——普通回显，不是错误；
    /// 4. 否则把被压缩段渲染成一次**无工具**的 native 请求交给模型（正文与交回的摘要都先过
    ///    会话脱敏器），成功后落一条 compaction 记录、把内存历史重建为「摘要消息 + 保留段」、
    ///    清掉组装统计（下一次组装会重算；用量保持最近一轮的真实值），返回回显行。
    ///
    /// 切分读的是 `store().effective_history(id)`——库里的 canonical 视图：某一轮的 append 被
    /// fail-soft 跳过时，内存历史可能比库里更富，压缩**有意**以这份持久化视图为准重建——重建
    /// 会把内存历史整体换成库里的 durable 视图（内存里未落库的富余消息随之消失，没有别处再
    /// 补齐），否则切点算错会让恢复后读到的历史与压缩时的不一致。
    ///
    /// 摘要请求带调用方给的取消令牌（REPL 在 `Dispatch::Compact` 期间挂 Ctrl-C 监听）：内核因
    /// 取消收成 `Done{Aborted}` 时按 `compact: summarization failed: aborted` 交回 `Err`。
    /// 失败（含取消）→ `Err`（REPL 打 `error: …`）且**不改任何状态**：不落 compaction、不动
    /// `history`；`session_messages` 的原始行**永不删除**——有效历史只是重建出来的视图。
    pub async fn compact(&mut self, cancel: CancellationToken) -> Result<Vec<String>, String> {
        // 1. 摘要要用一次 native 请求：外置内核与没有 Ignis 凭据的会话都不具备
        if !self.is_native() || !self.has_credentials() {
            return Err(
                "compact requires native kernel credentials (current kernel does not provide them)"
                    .to_string(),
            );
        }
        // 2. 没有会话就没有可压的历史
        let Some(id) = self.current_session.clone() else {
            return Err("compact: no session yet".to_string());
        };
        // 3. 库里的 canonical 视图切出「较旧轮次」；不足 keep 轮时没有可压段
        let messages = self
            .store()
            .effective_history(&id)
            .map_err(|error| error.to_string())?;
        let (older, retained) = split_turns(&messages, COMPACT_KEEP_TURNS);
        let Some(last_compacted) = older.last() else {
            return Ok(vec![slash::compact_nothing_line()]);
        };
        // 被压缩段的上界：摘要消息的 seq 与后续切点都按它算
        let upto_seq = last_compacted.seq;

        // 4. 一次无工具的 native 请求（会话级模型覆盖经 `effective_config` 对摘要同样生效）。
        //    C1：送给模型的旧轮次正文与模型交回的摘要各过一道会话脱敏器（这条路径不经过
        //    `TaskRunner`）——落库的摘要与重建后的历史因此与运行器写下的会话面同源。
        let config = self.effective_config();
        let transcript = self.redactor().redact(&render_transcript(&older));
        let runtime = mk_runtime(NATIVE_KERNEL, &config, &self.workspace)?;
        // 取消令牌直达摘要请求：Ctrl-C → 内核 `Done{Aborted}` → `Summarize("aborted")`，
        // 由下面的 `map_err` 收成 `compact: summarization failed: aborted`，状态零变化。
        let summary = summarize_turns(runtime, &config, &transcript, cancel)
            .await
            .map_err(|error| error.to_string())?;
        let summary = self.redactor().redact(&summary);

        // 成功才动状态：先落库（重建视图的起点），再把内存历史换成「摘要消息 + 保留段」。
        // 摘要消息的正文用与 `effective_history` 同一份拼法（脱敏后的摘要文本）。
        self.store()
            .record_compaction(&id, upto_seq, &summary)
            .map_err(|error| error.to_string())?;
        let mut history = Vec::with_capacity(retained.len() + 1);
        history.push(ChatMessage {
            role: Role::User,
            content: summary_message(&summary),
            tool_call_id: None,
            tool_calls: Vec::new(),
        });
        history.extend(
            retained
                .into_iter()
                .map(|message| message.to_chat_message()),
        );
        self.history = history;
        self.last_prompt_stats = None;
        Ok(slash::compact_lines(older.len(), summary.chars().count()))
    }

    /// `/learn [task-id]`：生成草稿 → 确认（由调用方给确认面）→ 落盘 + reload / `discarded`。
    ///
    /// `confirm` 收到草稿与 `draft.to_skill_md()`（头行/预览与落盘取的是同一份文本）；闭包要
    /// `'static`（[`ConfirmLearn`]），所以名字得先从草稿里取出来。确认回 `true` 才落盘：成功返回
    /// `["saved skill {name}", "reloaded skills ({n})"]` 并刷新会话里的技能目录快照，下一轮任务的
    /// 系统提示立即带上新技能（运行中的轮次仍持自己那一份旧快照）；回 `false` 一律
    /// `["discarded"]`（磁盘零变化）。生成阶段的检查顺序与错误文案见
    /// [`Session::draft_for_learn`]；命令本身不新建任务行、不动会话。
    pub async fn learn_with<F>(
        &mut self,
        arg: Option<&str>,
        cancel: CancellationToken,
        confirm: F,
    ) -> Result<Vec<String>, String>
    where
        F: FnOnce(&SkillDraft, String) -> ConfirmLearn,
    {
        let draft = self.draft_for_learn(arg, cancel).await?;
        if !confirm(&draft, draft.to_skill_md()).await {
            return Ok(vec!["discarded".to_string()]);
        }
        self.commit_draft(&draft)
    }

    /// 经典 REPL 的 `/learn`：确认面是真 stdin（行为逐字不变）。
    ///
    /// 确认面在 [`self::learn::confirm_save_owned`]：头行 + 预览（截断按 120 行 / 8 KiB）+ 提示词
    /// **先**打到 stdout，再从 stdin 读一行；只有 `s`（trim + 大小写归一）才落盘，EOF、`d`、其它
    /// 输入与读取失败一律交回 `["discarded"]`（磁盘零变化）。
    pub async fn learn(
        &mut self,
        arg: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Vec<String>, String> {
        self.learn_with(arg, cancel, |draft, skill_md| {
            let name = draft.name.clone();
            Box::pin(self::learn::confirm_save_owned(name, skill_md))
        })
        .await
    }

    /// 生成草稿（不落盘、不打预览）：检查顺序 = 凭据 → 目标任务 → 转录 → 一次无工具请求 → 校验；
    /// 返回前用会话脱敏器过一遍 `description` / `body`（预览与落盘因此是同一份文本）。
    ///
    /// 目标：`arg == None` 取本工作区最近一条**成功**任务
    /// （[`Db::latest_successful_task_for_workspace`]）；显式 id 走 [`task::find_task`] 的既有前缀
    /// 规则（≥8 字符、`[0-9a-f-]`、0/1/多命中的三种人话结果），命中但状态不是 `success` 与「没有
    /// 任务」共用同一句。全程只读任务行与任务 JSONL，不新建任务行、不动会话状态。
    async fn draft_for_learn(
        &mut self,
        arg: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<SkillDraft, String> {
        // 1. 草稿要用一次 native 请求：外置内核与没有 Ignis 凭据的会话都不具备
        if !self.is_native() || !self.has_credentials() {
            return Err(
                "learn requires native kernel credentials (current kernel does not provide them)"
                    .to_string(),
            );
        }
        // 2. 目标：显式 id 的错误（not found / ambiguous）原样交回；默认只认本工作区的成功任务
        let task = match arg {
            Some(id) => task::find_task(self.db(), id).map_err(|error| error.to_string())?,
            None => self
                .db()
                .latest_successful_task_for_workspace(&self.workspace.display().to_string())
                .map_err(|error| error.to_string())?
                .ok_or_else(|| NO_COMPLETED_TASK.to_string())?,
        };
        if task.status != RunStatus::Success.as_str() {
            return Err(NO_COMPLETED_TASK.to_string());
        }
        // 3. 任务日志 + 转录：读失败带出问题的路径；坏 JSONL 行在转录里跳过，不 panic
        let log =
            core_learn::read_task_log(self.home(), &task.id).map_err(|error| error.to_string())?;
        let transcript = core_learn::render_transcript(&task, &log);
        // 4. 一次无工具的 native 请求（会话级模型覆盖经 `effective_config` 同样生效）
        let config = self.effective_config();
        let runtime = mk_runtime(NATIVE_KERNEL, &config, &self.workspace)?;
        let draft = core_learn::draft_skill(runtime, &config, &transcript, cancel)
            .await
            .map_err(|error| error.to_string())?;
        // 5. C1 口径：预览与落盘取同一份文本——模型从转录里复述出的已知密钥在这里被抹掉
        let redactor = self.redactor();
        let SkillDraft {
            name,
            description,
            body,
        } = draft;
        Ok(SkillDraft {
            name,
            description: redactor.redact(&description),
            body: redactor.redact(&body),
        })
    }

    /// 落盘（`s` 那一支）：查重 → `save_skill` → `reload_skills` →
    /// `["saved skill {name}", "reloaded skills ({n})"]`。
    ///
    /// 同名（目标文件已存在，或目录里已有同名技能，含内置）一律拒绝、一个字节都不写：`Exists`
    /// 的展示文案逐字是 `skill {name} already exists`；其它写失败带 `learn: ` 前缀原样交回。
    /// 落盘成功后重扫 `~/.ignis/skills`，返回的第二行与 `/skills reload` 同一份文案。
    fn commit_draft(&mut self, draft: &SkillDraft) -> Result<Vec<String>, String> {
        let skills_dir = self.home.join("skills");
        match core_learn::save_skill(&skills_dir, draft, &self.skills()) {
            Ok(_path) => {}
            Err(LearnError::Exists(name)) => {
                return Err(format!("skill {name} already exists"));
            }
            Err(other) => return Err(other.to_string()),
        }
        let count = self.reload_skills();
        Ok(vec![
            format!("saved skill {}", draft.name),
            self::skills::reloaded_line(count),
        ])
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

    /// 当前技能目录快照（每轮任务与 `/skills` 都读这一份）。
    ///
    /// 克隆的是 `Arc`：调用方拿到的那一刻起就是一份不会再变的目录——运行中的轮次、正在渲染的
    /// `/skills` 都不会被随后的 [`Session::reload_skills`] 改到。锁中毒只取回内部值，不 panic。
    pub(crate) fn skills(&self) -> Arc<SkillCatalog> {
        let guard = self
            .skills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(&guard)
    }

    /// 重扫 `~/.ignis/skills` + 内置技能，换掉会话里的那一份；返回新目录的技能条数
    /// （`metas().len()`，覆盖后仍是合并后的条数）。
    ///
    /// 目录来源与会话构造同一处（`self.home.join("skills")`）：`/skills reload` 认的就是这份
    /// 安装的数据目录。已经在跑的那一轮任务握着自己那份快照（[`Session::run_task`] 每轮取一次），
    /// 换掉的只影响后续轮次与之后的 `/skills`。锁中毒同样只取回内部值。
    pub(crate) fn reload_skills(&mut self) -> usize {
        let catalog = SkillCatalog::load(&self.home.join("skills"));
        let count = catalog.metas().len();
        let mut guard = self
            .skills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = catalog;
        count
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
        let skills = self.skills().metas().len();
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

    /// 经典 REPL 的轮次（行为逐字不变）：渲染与结束行写 stdout，`error:`/`warning:` 行写 stderr。
    pub async fn run_task(&mut self, prompt: String, kind: TaskKind) -> Result<TaskOutcome> {
        self.run_task_with_sinks(
            prompt,
            kind,
            Box::new(std::io::stdout()),
            Box::new(std::io::stderr()),
            // 经典路径的「问答」只发生在交互终端上：管道 / 脚本里只打提示行
            std::io::stdin().is_terminal(),
        )
        .await
    }

    /// 同一轮次，输出面由调用方给（TUI 传 `CaptureSink`）：渲染器输出、结束行、`error:` 行与
    /// `warning: session write failed` 行都写进这一个面；自拉起只打提示行（不问答——TUI 没有
    /// 按行读的交互面）。
    pub async fn run_task_with_out(
        &mut self,
        prompt: String,
        kind: TaskKind,
        out: Box<dyn Write + Send>,
    ) -> Result<TaskOutcome> {
        // 同一个面拆成两个共享它的句柄（写序不变）：渲染器输出与错误行落进同一个捕获缓冲
        let (render_out, error_out) = shared_writer(out);
        self.run_task_with_sinks(prompt, kind, render_out, error_out, false)
            .await
    }

    /// 跑一次任务：渲染与 `runner.run` 在**同一任务内交错消费**事件，Ctrl-C 监听任务负责中止本次运行。
    ///
    /// 上面两个入口的公共实现（本模块私有；`interactive` 决定自拉起是否提问、两个输出面由调用方给：
    /// 经典是 stdout/stderr，TUI 是同一个捕获缓冲的两个句柄）。
    ///
    /// 每轮按当前内核装配运行器（native 建 HTTP 客户端，外部内核只做一次 PATH 检查），
    /// 因此 `/kernel` 的切换只影响后续任务。本轮配置由 [`Session::effective_config`] 给出
    /// （native 的会话级模型覆盖在这里生效）。渲染选项每轮重算：模式来自会话状态（`/details`
    /// 切过就按新的来），颜色/主题/宽度来自 `config.ui` + stdout 的 tty 状态（宽度每轮探测一次
    /// 终端）。结束后先 `finish()` 冲刷挂起内容再打结束行，随后把本轮用量、组装统计与上下文记进
    /// 会话（`/status`、`/context` 与下一轮内核都读它们），最后按本轮事件决定要不要提示自拉起
    /// 守护进程。
    /// 监听任务必须随本次运行结束，否则 REPL 会攒下一串永远不会触发的监听器。
    ///
    /// 会话接线（REPL 轮次）：先懒创建会话（首个任务落一行），`TaskRequest.session_id` 带上它；
    /// 运行结束后把 canonical 的 `user + assistant` 追加进 `session_messages`。两处写库失败都是
    /// fail-soft：只打一行 `warning: session write failed: …`，本轮照跑、上下文照常推进——
    /// 写库失败不该把一轮对话丢掉。`TaskKind::Run`（`ignis run`）不建会话、不追加。
    ///
    /// 提示词在会话面先过一道脱敏器（C1 终审）：会话标题、轮次落库与送给运行器的请求取同一份
    /// 文本，与运行器写下 `tasks.prompt` 的那一份逐字相同。
    ///
    /// 往注入面写失败一律忽略（EPIPE 等）：输出面坏了不该把一轮对话丢掉，也不该 panic。
    ///
    /// 审批提示是审批层对 stdout 的直接写入（不经渲染器），异步管线下它与工具行的硬写序做不到
    /// ——记为已知限制，P2/P5 单一写者时收口。
    async fn run_task_with_sinks(
        &mut self,
        prompt: String,
        kind: TaskKind,
        out: Box<dyn Write + Send>,
        mut err: Box<dyn Write + Send>,
        interactive: bool,
    ) -> Result<TaskOutcome> {
        // 输出面拆成两份共享它的句柄：渲染器拿一份，结束行与自拉起提示拿另一份（写序不变）
        let (render_out, mut out) = shared_writer(out);
        // C1：提示词先过会话面的脱敏器再用——标题（`ensure_session`）、轮次落库（`append_turn`）
        // 与送给运行器的 `TaskRequest` 都取这一份（运行器入口另有一道同源脱敏，重复替换幂等），
        // 三面同源；提示词里的已知密钥因此既到不了模型，也写不进 `sessions.title` /
        // `session_messages`。
        let prompt = self.redactor().redact(&prompt);
        // REPL 轮次先懒创建会话：失败只警告一行，本轮不绑会话照跑（见上文的 fail-soft 口径）
        let session_id = match kind {
            TaskKind::Repl => match self.ensure_session(&prompt) {
                Ok(id) => Some(id),
                Err(error) => {
                    let _ = writeln!(err, "warning: session write failed: {error}");
                    None
                }
            },
            TaskKind::Run | TaskKind::Schedule => None,
        };

        // 本轮配置：native 的会话级模型覆盖在这里生效（外部内核用各自的默认模型）
        let config = self.effective_config();
        let runtime =
            mk_runtime(&self.kernel, &config, &self.workspace).map_err(anyhow::Error::msg)?;
        // 技能目录每轮只取一次快照：这一轮运行期间 `/skills reload` 换掉的是下一轮的目录
        let skills = self.skills();
        let runner = TaskRunner::new(
            Arc::clone(&self.db),
            Arc::clone(&config),
            runtime,
            Arc::clone(&self.policy),
            Arc::clone(&self.approvals),
            self.log_dir.clone(),
            Arc::clone(&self.memory),
            Arc::clone(&skills),
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
        let mut renderer = Renderer::new(render_out, options, Arc::clone(&saw_schedule_create));

        let cancel = CancellationToken::new();
        let watcher = spawn_ctrl_c_watcher(cancel.clone());

        let started = Instant::now();
        // 渲染在同一任务内与运行器交错消费（不另起渲染任务）：写序竞态压到最低，事件不再经第二个
        // 任务中转。`run` 返回后再把已经入队的剩余事件排干（通道随 sink 析构关闭）。
        let run = runner.run(
            TaskRequest {
                prompt: prompt.clone(),
                history: self.history.clone(),
                kind,
                workspace: self.workspace.clone(),
                // 交互运行没有来源 job（调度运行由守护进程填 job.id）
                job_id: None,
                // REPL 轮次绑会话；`ignis run` 与调度运行不绑（迁移 004 的关联列）
                session_id: session_id.clone(),
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
        // 运行成功才换跨轮上下文与最近一轮的观察值（用量 + 组装统计）：运行器返回 `Err` 时
        // 三者都保持原样（本轮没跑成）
        self.history = outcome.messages.clone();
        self.last_usage = Some(outcome.usage);
        self.last_prompt_stats = Some(outcome.stats);
        // 轮次落库：只追加 REPL 的一轮（user + 有最终答复时的 assistant），写库失败 fail-soft
        if kind == TaskKind::Repl
            && let Some(session_id) = session_id.as_deref()
            && let Err(error) =
                self.store()
                    .append_turn(session_id, &prompt, outcome.final_text.as_deref())
        {
            let _ = writeln!(err, "warning: session write failed: {error}");
        }
        let _ = writeln!(
            out,
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
        // 非成功终态不能只留一个状态词：原因（内核给出、已过 Redactor）打到错误面，与
        // `main` 的错误行同形。任务 id 已在结束行里，随后可直接 `ignis task show {id}`。
        if outcome.status != RunStatus::Success
            && let Some(error) = &outcome.error
        {
            let _ = writeln!(err, "error: {error}");
        }
        // 本轮建过调度任务、守护进程又没在跑时提示一次自拉起（每会话至多一次）：
        // `interactive` 由入口给（经典路径 = stdin 是否 tty；TUI 传 `false` 只打提示行）
        daemon::autostart_with(
            self.home(),
            saw_schedule_create.load(Ordering::Relaxed),
            &self.already_prompted,
            interactive,
            &mut *out,
            &mut *err,
        )
        .await;
        Ok(outcome)
    }

    /// TUI 的颜色/主题来源（`ui` 段是 `Copy`）。
    pub(crate) fn ui_config(&self) -> ignis_core::config::UiConfig {
        self.config.ui
    }
}

/// 把一个写入面拆成两个共享它的句柄：TUI 的单一捕获缓冲因此既收渲染输出，也收 `error:`/`warning:` 行。
///
/// 两个句柄持同一把锁，写序就是调用顺序。锁中毒（持锁的写线程 panic 过）取回内部值——生产路径
/// 只许做减法，不许 panic。
fn shared_writer(out: Box<dyn Write + Send>) -> (Box<dyn Write + Send>, Box<dyn Write + Send>) {
    let shared = Arc::new(Mutex::new(out));
    (
        Box::new(SharedSink(Arc::clone(&shared))),
        Box::new(SharedSink(shared)),
    )
}

/// [`shared_writer`] 的句柄：每次写都先取锁，转发给共享的那个面。
struct SharedSink(Arc<Mutex<Box<dyn Write + Send>>>);

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .flush()
    }
}

/// 渲染选项：模式来自**会话状态**（`/details` 只改会话，不写回配置），主题来自 `config.ui`，
/// `ColorChoice` + stdout 的 tty 状态 + `NO_COLOR` 定颜色，宽度按 stdout 类型给
/// （tty → 探测终端列数，非 tty → 不折行）。
///
/// `pub(crate)`：TUI 的帧合成器按同一份口径取宽度（TUI 下 stdout 是 tty，宽度就是面板列数）。
pub(crate) fn renderer_options(
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
///
/// `pub(crate)`：`NO_COLOR` 的约定只有这一处，TUI 的 `refresh_facts` 也从这里读。
pub(crate) fn no_color_value(raw: Option<OsString>) -> Option<String> {
    raw.map(|value| value.to_string_lossy().into_owned())
}

/// REPL 启动时把进程级 SIGINT 处理器**提前装好**（一次），返回的句柄由调用方持有到进程退出。
///
/// 今日的竞态：`tokio::signal::ctrl_c` 的处理器在 future **首次轮询**时装，而
/// `spawn_ctrl_c_watcher` 是「每段现起」——第一段 watcher 被轮询之前到达的 SIGINT 走默认处置，
/// 直接杀掉进程（P5 台账记的偶发信号退出）。这里在进第一段等待之前先装一次，安装先于任何
/// 可交互时机；之后各段 watcher 复用同一个已装好的处理器（监听者各自成流，互不消耗）。
///
/// 实现口径：`tokio::signal::unix::signal(SignalKind::interrupt())` **调用即注册**（不是首次
/// 轮询），把返回的流移进后台任务持续 `recv()` 即完成安装；非 unix 返回 `None`（REPL 本身
/// 也是 unix 门控，行为退化为今日）。安装失败（不支持的平台）同样 `None`，不 panic。
///
/// 前提：必须在 tokio 运行时内调用——这里的常驻排空任务用 `tokio::spawn` 挂（运行时外调用
/// 会 panic）；平台不支持时 `signal()` 先交出 `Err`（随即 `None`），走不到 `spawn`。
#[cfg(unix)]
pub fn install_ctrl_c_handler() -> Option<JoinHandle<()>> {
    use tokio::signal::unix::{SignalKind, signal};

    // `signal(...)` 返回流之前处理器已经装好（调用即注册，不等首次轮询）；装不上（不支持的
    // 平台）给 `None`，调用方按「没有处理器」的今日行为继续。
    let mut stream = signal(SignalKind::interrupt()).ok()?;
    Some(tokio::spawn(async move {
        // 常驻排空：本任务只负责「把信号接住」（不再走默认处置），取消语义仍归各段 watcher
        // ——监听者各自成流、互不消耗，每一段 `ctrl_c()` 都收到属于自己的那一份。
        while stream.recv().await.is_some() {
            // 收到一次就继续排空：这里不取消任何东西
        }
    }))
}

/// 非 unix 上没有 unix 信号流（经典 REPL 本身也按 unix 门控）：交回 `None`，行为退化为今日。
#[cfg(not(unix))]
pub fn install_ctrl_c_handler() -> Option<JoinHandle<()>> {
    None
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
    /// 会话级状态的初值与 [`Session::new`] 一致：输出模式取自配置、模型无覆盖、没有用量、
    /// 没有历史与会话 id（`run_task` 懒创建）。
    pub(crate) fn session_fixture_with_config(
        label: &str,
        kernel: &str,
        config: Config,
    ) -> (TempDir, Session) {
        let dir = TempDir::new(label);
        let session = build_session(dir.path(), kernel, config);
        (dir, session)
    }

    /// 同一个 home 上的又一个最小会话（调用方负责让 home 活着）：`-c` 的恢复用例要先跑一轮、
    /// 再起一个指向同一份库的会话，空 home 用例则直接拿它当「新进程」。
    pub(crate) fn session_at_home(home: &Path, kernel: &str, config: Config) -> Session {
        build_session(home, kernel, config)
    }

    /// 会话夹具的装配体：临时 home/工作区 + 空记忆与技能目录，绝不碰真实 `~/.ignis`。
    fn build_session(home: &Path, kernel: &str, config: Config) -> Session {
        let home = home.to_path_buf();
        let db = Arc::new(Db::open(&home.join("ignis.db")).unwrap());
        let output_mode = config.ui.output_mode;
        Session {
            home: home.clone(),
            workspace: home.clone(),
            db: Arc::clone(&db),
            config: Arc::new(config),
            policy: Arc::new(Policy::new(ApprovalMode::Unattended)),
            approvals: Arc::new(PolicyOnlyApproval),
            log_dir: home.join("logs"),
            memory: MemoryStore::new(db, home.join("memory")),
            skills: Mutex::new(SkillCatalog::load(&home.join("skills"))),
            notifier: Arc::new(MultiNotifier::new(Vec::new())),
            kernel: kernel.to_string(),
            already_prompted: AtomicBool::new(false),
            output_mode,
            model_override: None,
            last_usage: None,
            last_prompt_stats: None,
            history: Vec::new(),
            current_session: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use ignis_core::config::{ColorChoice, OutputMode, Theme, UiConfig};
    use ignis_core::session::{COMPACT_SYSTEM_PROMPT, EARLIER_SUMMARY_PREFIX};
    use ignis_runtime::test_support::{ScriptStep, ScriptedModel, ScriptedModelHandle};
    use ignis_runtime::{ApprovalDecision, ApprovalRequest, RiskLevel, Role};
    use ignis_storage::NewTask;

    use super::fixtures::{
        keyless_session_fixture, session_at_home, session_fixture, session_fixture_with_config,
    };
    use super::*;
    use crate::commands::kernel::fixtures::{TempDir, set_executable};

    /// 父子两个用例共享的环境变量名：父用例只在拉起子进程时设它，子探针只在它存在时才真的
    /// `raise`——`cargo test -- --ignored`（或 `--include-ignored`）之类的全量扫描即使扫到
    /// 子探针，它也只是一次空转，不会朝任何进程打信号。
    #[cfg(unix)]
    const SIGINT_HELPER_ENV: &str = "IGNIS_CLI_SIGINT_HELPER";

    /// 子进程里的 `raise` 探针（父用例拉起同一个测试二进制、只跑这一条）：装上处理器 →
    /// `raise(SIGINT)` → 打标记后正常返回。
    ///
    /// 与父用例分进程是刻意的：信号投递是**进程级**的，而同一个 bin 测试目标里还有几十条跑
    /// `Session::run_task` 的用例——它们的 watcher 共享同一份投递，同进程 `raise` 会把手气
    /// 变成「并发用例被顺带取消」的偶发抖动。子进程里只有本探针，信号只可能命中它。
    ///
    /// 判据由父用例在进程边界上观察：处理器没装上时本进程按默认处置**死于 SIGINT**（`status`
    /// 里没有退出码），父用例因此拿不到「成功 + 标记」这一对断言。
    #[cfg(unix)]
    #[ignore = "由 install_ctrl_c_handler_arms_the_handler_before_the_first_reader 在子进程里拉起"]
    #[tokio::test]
    async fn sigint_install_helper_child() {
        // 没有父用例设的环境变量就什么都不做（防 `--ignored` 全量扫描把信号打进测试进程）
        if std::env::var_os(SIGINT_HELPER_ENV).is_none() {
            return;
        }
        // 安装先于 raise：返回 `Some` 表示进程级处理器此刻已经就位。句柄活到本用例结束（排空
        // 任务常驻，安装到进程退出都在）；没装上时下面那一句 `raise` 会按默认处置杀掉本进程
        // ——失败方式本身就是这条用例要钉住的东西
        let installed = install_ctrl_c_handler();
        assert!(installed.is_some(), "unix 上必须装得上 SIGINT 处理器");
        // SAFETY: `raise` 只把信号递给本进程自己，不碰内存、也不需要初始化。
        unsafe { libc::raise(libc::SIGINT) };
        // 常驻排空任务把这次信号消费掉（安装已生效，`raise` 不再是默认处置）：给它的这一小段
        // 时间只是把标记的时机定在「信号已被接住」之后
        tokio::time::sleep(Duration::from_millis(100)).await;
        println!("SIGINT-CAUGHT");
    }

    /// 安装时机（P5 台账里的偶发信号退出）：[`install_ctrl_c_handler`] **调用即注册**，此后
    /// 每段 `spawn_ctrl_c_watcher` 都建立在已装好的处理器之上——第一段 watcher 被首次轮询之前
    /// 到达的 SIGINT 不再走默认处置把进程带走，而是由各段监听者各自收成取消。
    ///
    /// `raise` 那半条判据跑在子进程里（[`sigint_install_helper_child`]）：本用例只看「子进程活着
    /// 走完 + 打过标记」，信号因此不会把同二进制里几十条 `run_task` 用例的 watcher 顺带取消。
    #[cfg(unix)]
    #[tokio::test]
    async fn install_ctrl_c_handler_arms_the_handler_before_the_first_reader() {
        // 安装先于任何监听者：返回 `Some` 表示进程级处理器此刻已经就位（句柄不显式 drop——
        // 排空任务常驻，安装到进程退出都在）
        let installed = install_ctrl_c_handler();
        assert!(installed.is_some(), "unix 上必须装得上 SIGINT 处理器");

        // 同一个测试二进制的子进程里只跑探针那一条：`--exact` 钉名字、`--ignored` 让被
        // `#[ignore]` 的它照跑、`--nocapture` 让它的 `println!` 落到我们读的这根管道上
        let exe = std::env::current_exe().expect("测试二进制路径");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "commands::tests::sigint_install_helper_child",
                "--ignored",
                "--nocapture",
            ])
            // 守卫：探针只在父用例设了它时才真的 raise
            .env(SIGINT_HELPER_ENV, "1")
            .output()
            .expect("拉起子进程探针");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 处理器没装上时子进程死于 SIGINT：没有退出码，`success()` 为假
        assert!(
            output.status.success(),
            "子进程必须活着走完（没装处理器会死于 SIGINT）：{:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status
        );
        assert!(
            stdout.contains("SIGINT-CAUGHT"),
            "探针必须打到标记：\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );

        // 重复调用：不 panic、不改变行为（第二次照样是「已装好」的形状）
        assert!(
            install_ctrl_c_handler().is_some(),
            "重复调用不 panic，仍然交回 Some"
        );
    }

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

    /// 脚本化审批处理器：只交回调用方给的那一个决定（用来分辨「注入的那个」与策略占位）。
    struct StubApproval(ApprovalDecision);

    #[async_trait::async_trait]
    impl ignis_runtime::ApprovalHandler for StubApproval {
        async fn decide(&self, _req: ApprovalRequest) -> ApprovalDecision {
            self.0
        }
    }

    /// 审批处理器的选择规则（裁定 2）：只有交互模式用注入的那个（没注入则用默认的交互处理器）；
    /// 预授权 / 无人值守下注入的一律被忽略、恒是策略占位（`--yes`/`--unsafe` 的语义不可被 TUI
    /// 的提问面绕开）。
    #[tokio::test]
    async fn approvals_for_uses_the_injected_handler_only_when_interactive() {
        use ApprovalMode::{Interactive, Preauthorized, Unattended};

        let request = || ApprovalRequest {
            tool: "fs_write".to_string(),
            summary: "Writing a.txt".to_string(),
            args: serde_json::json!({ "path": "a.txt" }),
            risk: RiskLevel::Elevated,
        };
        let injected: Arc<dyn ApprovalHandler> =
            Arc::new(StubApproval(ApprovalDecision::AllowSession));

        // 交互 + 注入：就是注入的那个（同一个 Arc），决定原样交回
        let handler = approvals_for(Interactive, Some(Arc::clone(&injected)));
        assert!(Arc::ptr_eq(&handler, &injected), "交互模式用注入的处理器");
        assert_eq!(
            handler.decide(request()).await,
            ApprovalDecision::AllowSession
        );

        // 交互 + 没注入：默认的交互处理器（按「不是注入那个」断言；**不调 decide**——它会去读
        // 真实 stdin，用例在 tty 下会卡住）
        let plain = approvals_for(Interactive, None);
        assert!(
            !Arc::ptr_eq(&plain, &injected),
            "没注入就轮到默认的交互处理器"
        );

        // 其余三个模式 + 注入：注入的被忽略，策略结论优先（被问到一律拒绝）
        for mode in [
            Preauthorized { unsafe_ops: false },
            Preauthorized { unsafe_ops: true },
            Unattended,
        ] {
            let handler = approvals_for(mode, Some(Arc::clone(&injected)));
            assert!(
                !Arc::ptr_eq(&handler, &injected),
                "{mode:?} 不该用注入的处理器"
            );
            assert_eq!(
                handler.decide(request()).await,
                ApprovalDecision::Reject,
                "{mode:?}：策略已经给出结论，被问到一律拒绝"
            );
        }
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

    /// 指向脚本化假模型的 native 配置：`run_task` 真的会走一遍 native 内核（HTTP → SSE），
    /// 不是挂钩的假内核（本 crate 是二进制，没有注入运行器的接缝）。
    fn scripted_config(model: &ScriptedModelHandle) -> Config {
        Config {
            api_key: Some("test-key".into()),
            model: ignis_runtime::ModelConfig {
                base_url: model.base_url(),
                model: "test-model".into(),
                ..ignis_runtime::ModelConfig::default()
            },
            ..Config::default()
        }
    }

    /// 无脚本模型的最小配置（只恢复历史、不跑任务的用例用它）。
    fn plain_config() -> Config {
        Config {
            api_key: Some("test-key".into()),
            ..Config::default()
        }
    }

    /// REPL 的一轮：懒创建会话（标题 = 首条用户消息）、轮次按 canonical 口径追加
    /// （user + assistant）、跨轮历史与 `tasks.session_id` 都对上。
    #[tokio::test]
    async fn run_task_creates_the_session_and_appends_the_turn() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("答复一".into())]);
        let (dir, mut session) =
            session_fixture_with_config("session-create", "native", scripted_config(&model));
        assert_eq!(
            session.current_session(),
            None,
            "懒创建：没跑任务时没有会话行"
        );
        assert!(session.history().is_empty());

        let outcome = session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .expect("脚本化模型应答应成功");
        assert_eq!(outcome.final_text.as_deref(), Some("答复一"));

        // sessions 恰好一行：title 是首条用户消息，工作区/内核/模型都照会话记
        let session_id = session
            .current_session()
            .expect("首个 REPL 任务应建会话")
            .to_string();
        let rows = session.store().list_for_workspace(dir.path(), 10).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let record = &rows[0].session;
        assert_eq!(record.id, session_id);
        assert_eq!(record.title, "你好");
        assert_eq!(record.workspace, dir.path().display().to_string());
        assert_eq!(record.kernel, "native");
        assert_eq!(record.model.as_deref(), Some("test-model"));

        // session_messages 恰好两条：user + assistant（canonical 口径）
        let messages = session.db().session_messages_from(&session_id, 1).unwrap();
        let actual: Vec<(&str, &str)> = messages
            .iter()
            .map(|message| (message.role.as_str(), message.content.as_str()))
            .collect();
        assert_eq!(actual, [("user", "你好"), ("assistant", "答复一")]);

        // 跨轮上下文：本轮 messages 原样接进会话；最近用量来自本轮
        assert_eq!(session.history().len(), 2);
        assert_eq!(session.history()[0].content, "你好");
        assert_eq!(session.history()[1].content, "答复一");
        assert_eq!(
            session.last_usage().map(|usage| usage.input_tokens),
            Some(12),
            "脚本化模型上报 12/3"
        );

        // 任务行绑到会话上
        let task = session.db().get_task(&outcome.id).unwrap().expect("任务行");
        assert_eq!(task.session_id.as_deref(), Some(session_id.as_str()));

        // 假模型真的收到了这一轮的 user 消息
        let requests = model.requests();
        assert_eq!(requests.len(), 1);
        let sent = requests[0]["messages"].as_array().unwrap();
        assert!(
            sent.iter()
                .any(|message| message["role"] == "user" && message["content"] == "你好"),
            "{sent:#?}"
        );
    }

    /// C1（终审）：REPL 轮次的提示词含已知密钥时，会话标题（`/sessions` 的展示面）、
    /// `session_messages` 的 user 行、跨轮历史与模型请求体里都不许出现明文——命令层与
    /// `TaskRunner` 用的是同一道 `Redactor`、同一份脱敏文本。
    #[tokio::test]
    async fn prompt_secrets_never_reach_the_session_surfaces_or_the_model() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("答复一".into())]);
        let config = Config {
            api_key: Some("sk-test-secret".into()),
            ..scripted_config(&model)
        };
        let (dir, mut session) = session_fixture_with_config("session-redacted", "native", config);

        let outcome = session
            .run_task("把 sk-test-secret 记下来".into(), TaskKind::Repl)
            .await
            .expect("脚本化模型应答应成功");
        assert_eq!(outcome.status, RunStatus::Success);

        // 1. 会话标题（`/sessions` 的 TITLE 列直接打印它）
        let session_id = session
            .current_session()
            .expect("首个 REPL 任务应建会话")
            .to_string();
        let rows = session.store().list_for_workspace(dir.path(), 10).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].session.title, "把 *** 记下来");

        // 2. `session_messages` 的 user 行（恢复会话时读回的就是它）
        let messages = session.db().session_messages_from(&session_id, 1).unwrap();
        let user_row = messages
            .iter()
            .find(|message| message.role == "user")
            .expect("user 行");
        assert_eq!(user_row.content, "把 *** 记下来");
        // 整表都不带明文（assistant 行同样过脱敏）
        let raw = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!raw.contains("sk-test-secret"), "{raw}");

        // 3. 模型请求体：那一次任务真的碰过模型，body 里没有明文
        let body = serde_json::to_string(&model.requests()).unwrap();
        assert!(!body.contains("sk-test-secret"), "{body}");
        assert!(body.contains("把 *** 记下来"), "{body}");

        // 4. 跨轮历史（下一轮送进内核的那一份）
        assert_eq!(session.history()[0].content, "把 *** 记下来");
    }

    /// `TaskKind::Run`（`ignis run`）是一次性运行：不建会话、不追加轮次，任务行的
    /// `session_id` 为 NULL。
    #[tokio::test]
    async fn run_task_is_one_shot_for_taskkind_run() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("答复".into())]);
        let (dir, mut session) =
            session_fixture_with_config("session-one-shot", "native", scripted_config(&model));

        let outcome = session
            .run_task("一次跑完".into(), TaskKind::Run)
            .await
            .expect("一次性运行应成功");

        assert_eq!(session.current_session(), None, "一次性运行不建会话");
        assert!(
            session
                .store()
                .list_for_workspace(dir.path(), 10)
                .unwrap()
                .is_empty(),
            "sessions 表必须是空的"
        );
        let task = session.db().get_task(&outcome.id).unwrap().expect("任务行");
        assert_eq!(task.session_id, None, "一次性运行不绑会话");
        assert_eq!(task.kind, "run");
    }

    /// R2 fail-soft：会话写库失败不丢轮次——建不出会话照跑（本轮不绑会话），
    /// 失败只落一行 `warning: session write failed: …`（stderr 由进程边界观察，这里钉行为）。
    #[tokio::test]
    async fn run_task_survives_a_broken_session_store() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("答复".into())]);
        let (_dir, mut session) =
            session_fixture_with_config("session-broken", "native", scripted_config(&model));
        // 会话表被删（库损坏 / 迁移中途失败）：`ensure_session` 必失败
        session
            .db()
            .with_conn(|conn| conn.execute_batch("DROP TABLE sessions"))
            .unwrap();

        let outcome = session
            .run_task("写库坏了也要跑".into(), TaskKind::Repl)
            .await
            .expect("建不出会话不该把这一轮丢掉");

        assert_eq!(outcome.status, RunStatus::Success);
        assert_eq!(session.current_session(), None, "建不出会话就没有会话可绑");
        assert_eq!(session.history().len(), 2, "跨轮上下文照常推进");
        let task = session.db().get_task(&outcome.id).unwrap().expect("任务行");
        assert_eq!(task.session_id, None);
    }

    /// R2 fail-soft（终审补齐的 append 路径）：会话行已经建过，只有 `append_turn` 的落库失败
    /// （这里把 `session_messages` 整表删掉）——这一轮照样成功、跨轮历史照常推进，只留一行
    /// `warning: session write failed`；不 panic、不把对话丢掉。
    #[tokio::test]
    async fn run_task_survives_a_broken_session_messages_table() {
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("答复一".into()),
            ScriptStep::Text("答复二".into()),
        ]);
        let (_dir, mut session) = session_fixture_with_config(
            "session-messages-broken",
            "native",
            scripted_config(&model),
        );

        // 第一轮正常：会话行与两条 canonical 消息都落库
        session
            .run_task("第一句".into(), TaskKind::Repl)
            .await
            .expect("第一轮应成功");
        let session_id = session
            .current_session()
            .expect("首个 REPL 任务应建会话")
            .to_string();
        assert_eq!(
            session
                .db()
                .session_messages_from(&session_id, 1)
                .unwrap()
                .len(),
            2
        );

        // `session_messages` 表没了（库损坏 / 迁移中途失败）：此后只可能是 append 这一步失败
        session
            .db()
            .with_conn(|conn| conn.execute_batch("DROP TABLE session_messages"))
            .unwrap();
        assert!(
            session.db().session_messages_from(&session_id, 1).is_err(),
            "表删掉后读消息必须失败（证明破坏生效）"
        );

        // 第二轮：append 落库失败照跑，历史照常推进（user + assistant 各加一条）
        let outcome = session
            .run_task("第二句".into(), TaskKind::Repl)
            .await
            .expect("写库坏了也不该把这一轮丢掉");
        assert_eq!(outcome.status, RunStatus::Success);
        assert_eq!(outcome.final_text.as_deref(), Some("答复二"));
        assert_eq!(session.history().len(), 4, "跨轮上下文照常推进");
        assert_eq!(session.history()[2].content, "第二句");
        assert_eq!(session.history()[3].content, "答复二");
        assert_eq!(session.current_session(), Some(session_id.as_str()));
    }

    /// `-c` 的装载：命中本工作区最近会话 → 设当前会话、有效历史接进 `history`、清掉最近用量，
    /// 返回条数；空工作区什么都不改、给 `None`。
    #[tokio::test]
    async fn resume_latest_loads_the_newest_workspace_session() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("答复一".into())]);
        let (dir, mut session) =
            session_fixture_with_config("session-resume", "native", scripted_config(&model));
        session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .expect("先跑一轮把会话落库");
        let expected = session.history().to_vec();

        // 同一个 home / 同一工作区的又一个会话（等价于进程重启后的 `Session::new`）
        let mut resumed = session_at_home(dir.path(), "native", plain_config());
        assert_eq!(resumed.current_session(), None, "恢复前本进程还没有会话");
        assert_eq!(resumed.last_prompt_stats(), None, "恢复前没有组装统计");
        assert_eq!(resumed.resume_latest().unwrap(), Some(2));
        assert!(resumed.current_session().is_some());
        assert_eq!(resumed.history(), expected.as_slice(), "有效历史逐条相等");
        assert_eq!(resumed.last_usage(), None);

        // 恢复也重置最近一轮的观察值：同进程里再恢复一次，跑过那一轮的旧值必须回落
        assert!(session.last_usage().is_some(), "跑过一轮后有用量");
        assert!(
            session.last_prompt_stats().is_some(),
            "跑过一轮后有组装统计"
        );
        assert_eq!(session.resume_latest().unwrap(), Some(2));
        assert_eq!(session.last_usage(), None);
        assert_eq!(session.last_prompt_stats(), None);

        // 空 home：没有会话，什么都不改
        let empty = TempDir::new("session-resume-empty");
        let mut fresh = session_at_home(empty.path(), "native", plain_config());
        assert_eq!(fresh.resume_latest().unwrap(), None);
        assert_eq!(fresh.current_session(), None);
        assert!(fresh.history().is_empty());
    }

    /// `/new` 与 `/resume` 只动会话状态（`history` / `current_session` / `last_usage`）：
    /// 输出模式、模型覆盖与内核一律不碰；旧会话行在库里一行不动。
    #[test]
    fn start_new_session_and_resume_session_move_only_conversation_state() {
        let (_dir, mut session) = session_fixture("session-switch-state", "native");
        let store = session.store();
        let record = store
            .create_session(session.workspace(), "native", None, "第一句")
            .unwrap();
        assert_eq!(
            store
                .append_turn(&record.id, "第一句", Some("答复一"))
                .unwrap(),
            2
        );
        let id = record.id.clone();

        // 把非会话状态都设成非默认值：只有它们不该被两个命令碰到
        session.set_model("test-model-2");
        session.set_output_mode(OutputMode::Debug);
        session.last_usage = Some(Usage {
            input_tokens: 1,
            output_tokens: 2,
            estimated: false,
        });
        let stats = PromptStats {
            facts_injected: 1,
            skills_listed: 4,
            system_chars: 640,
            history_chars: 128,
        };
        session.last_prompt_stats = Some(stats);

        // `/resume <前缀>`：装载历史、切指针、清最近一轮的观察值（运行前的观察值不该跟过来）
        assert_eq!(session.resume_session(&id[..8]).unwrap(), 2);
        assert_eq!(session.current_session(), Some(id.as_str()));
        assert_eq!(session.history().len(), 2);
        assert_eq!(session.history()[0].content, "第一句");
        assert_eq!(session.history()[1].content, "答复一");
        assert_eq!(session.last_usage(), None, "恢复清掉最近用量");
        assert_eq!(session.last_prompt_stats(), None, "恢复清掉组装统计");
        assert_eq!(session.output_mode(), OutputMode::Debug, "模式不动");
        assert_eq!(session.model(), "test-model-2", "模型覆盖不动");
        assert_eq!(session.kernel(), "native", "内核不动");

        // `/new`：清会话指针/历史/最近一轮的观察值；模式/模型/内核照旧，旧会话行还在库里
        session.last_usage = Some(Usage {
            input_tokens: 1,
            output_tokens: 2,
            estimated: false,
        });
        session.last_prompt_stats = Some(stats);
        session.start_new_session();
        assert_eq!(session.current_session(), None);
        assert!(session.history().is_empty());
        assert_eq!(session.last_usage(), None);
        assert_eq!(session.last_prompt_stats(), None, "`/new` 清掉组装统计");
        assert_eq!(session.output_mode(), OutputMode::Debug, "模式不动");
        assert_eq!(session.model(), "test-model-2", "模型覆盖不动");
        assert_eq!(session.kernel(), "native", "内核不动");
        let rows = session.sessions().unwrap();
        assert_eq!(rows.len(), 1, "旧会话一行不动（不删数据）");
        assert_eq!(rows[0].session.id, id);
        assert_eq!(rows[0].message_count, 2);
    }

    /// 送进内核的一条 canonical 消息（canonical 口径：工具两列恒空）。
    fn chat(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    /// 造一条会话行 + 三轮 canonical 消息，并把会话切到它（有效历史装进 `history`）。
    fn session_with_three_turns(session: &mut Session, label: &str) -> String {
        let record = session
            .store()
            .create_session(session.workspace(), "native", None, "第一句")
            .unwrap();
        for (prompt, reply) in [
            ("第一句", "答复一"),
            ("第二句", "答复二"),
            ("第三句", "答复三"),
        ] {
            assert_eq!(
                session
                    .store()
                    .append_turn(&record.id, prompt, Some(reply))
                    .unwrap(),
                2,
                "{label}"
            );
        }
        assert_eq!(
            session.resume_session(&record.id[..8]).unwrap(),
            6,
            "{label}: 三轮六条"
        );
        record.id
    }

    /// `/compact` 的检查顺序即错误优先级：内核/凭据 → 会话 → 可压段（三种错误都不碰模型）。
    #[tokio::test]
    async fn compact_error_matrix() {
        // codex（外置内核）：凭据错误逐字
        let (_dir, mut codex) = session_fixture("compact-codex", "codex");
        assert_eq!(
            codex.compact(CancellationToken::new()).await.unwrap_err(),
            "compact requires native kernel credentials (current kernel does not provide them)"
        );
        assert_eq!(codex.current_session(), None, "失败的压缩不动会话指针");

        // native 但没有 Ignis 凭据（环境变量名取不存在的，断言与运行者环境无关）：同一句
        let (_dir, mut keyless) = keyless_session_fixture("compact-keyless", "native");
        assert!(keyless.is_native());
        assert!(!keyless.has_credentials());
        assert_eq!(
            keyless.compact(CancellationToken::new()).await.unwrap_err(),
            "compact requires native kernel credentials (current kernel does not provide them)"
        );

        // native + 有凭据但还没绑定会话：第二句（凭据门槛先过）
        let (_dir, mut fresh) = session_fixture("compact-no-session", "native");
        assert!(fresh.has_credentials());
        assert_eq!(
            fresh.compact(CancellationToken::new()).await.unwrap_err(),
            "compact: no session yet"
        );
    }

    /// 两轮（= `keep`）：`Ok` 一行「没有可压的」；库与内存历史一位不动，也不碰模型。
    #[tokio::test]
    async fn compact_with_two_turns_says_nothing_to_compact() {
        // 空脚本：真发了请求只会拿到 500，`requests()` 因此能钉住「没碰模型」
        let model = ScriptedModel::start(Vec::new());
        let (_dir, mut session) =
            session_fixture_with_config("compact-nothing", "native", scripted_config(&model));
        let record = session
            .store()
            .create_session(session.workspace(), "native", None, "第一句")
            .unwrap();
        for (prompt, reply) in [("第一句", "答复一"), ("第二句", "答复二")] {
            session
                .store()
                .append_turn(&record.id, prompt, Some(reply))
                .unwrap();
        }
        assert_eq!(session.resume_session(&record.id[..8]).unwrap(), 4);
        let before = session.history().to_vec();

        let lines = session
            .compact(CancellationToken::new())
            .await
            .expect("「没有可压的」是回显，不是错误");
        assert_eq!(lines, ["compact: nothing to compact"]);
        assert_eq!(session.history(), before.as_slice(), "不动内存历史");
        assert!(
            session
                .db()
                .latest_compaction(&record.id)
                .unwrap()
                .is_none(),
            "不落 compaction"
        );
        assert!(model.requests().is_empty(), "没有可压段就不消耗模型");
    }

    /// 三轮：摘要落库（恰好一行、`upto_seq` = 被覆盖段上界）、内存历史重建成「摘要 + 保留段」、
    /// 原始行一条不删、回显逐字，`last_prompt_stats` 清零而 `last_usage` 保持最近一轮的真实值。
    #[tokio::test]
    async fn compact_stores_the_summary_and_rebuilds_the_history() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("早前对话摘要".into())]);
        let (_dir, mut session) =
            session_fixture_with_config("compact-success", "native", scripted_config(&model));
        let id = session_with_three_turns(&mut session, "compact-success");

        // 压缩前的观察值：用量是最近一轮的真实值（成功后保留），组装统计随压缩清零
        session.last_prompt_stats = Some(PromptStats {
            facts_injected: 1,
            skills_listed: 2,
            system_chars: 3,
            history_chars: 4,
        });
        session.last_usage = Some(Usage {
            input_tokens: 12,
            output_tokens: 3,
            estimated: false,
        });

        let summary = "早前对话摘要";
        let lines = session
            .compact(CancellationToken::new())
            .await
            .expect("脚本化模型应给出摘要");
        let chars = summary.chars().count();
        assert_eq!(chars, 6, "按字符计（按字节是 18）");
        assert_eq!(
            lines,
            [format!("compacted: 2 messages → summary ({chars} chars)")]
        );

        // 压缩记录恰好一行：`upto_seq` 是第一轮 assistant 的 `seq`（= 2）
        let rows: Vec<(i64, String)> = session
            .db()
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT upto_seq, summary FROM session_compactions WHERE session_id = ?1",
                )?;
                let rows = stmt.query_map([&id], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect()
            })
            .unwrap();
        assert_eq!(rows, [(2, summary.to_string())], "压缩记录恰好一行");

        // 内存历史重建：前缀 + 空行 + 摘要（与 `effective_history` 同一份拼法常量）+ 保留段原文
        let expected = [
            chat(
                Role::User,
                &format!("{EARLIER_SUMMARY_PREFIX}\n\n{summary}"),
            ),
            chat(Role::User, "第二句"),
            chat(Role::Assistant, "答复二"),
            chat(Role::User, "第三句"),
            chat(Role::Assistant, "答复三"),
        ];
        assert_eq!(session.history(), expected, "摘要 + 保留段");
        // 与库里的有效历史（重建视图）逐个字段相等：退出再恢复读到的是同一段
        let db_view: Vec<ChatMessage> = session
            .store()
            .effective_history(&id)
            .unwrap()
            .into_iter()
            .map(|message| message.to_chat_message())
            .collect();
        assert_eq!(session.history(), db_view.as_slice(), "内存与库里同口径");
        // 原始行一条不删（压缩只是视图，`/sessions` 的 MESSAGES 仍是 6）
        assert_eq!(
            session.db().session_messages_from(&id, 1).unwrap().len(),
            6,
            "原始消息一条不删"
        );

        // 观察值：组装统计清零（下一次组装重算）、用量保持最近一轮的真实值
        assert_eq!(session.last_prompt_stats(), None);
        assert_eq!(
            session.last_usage().map(|usage| usage.input_tokens),
            Some(12),
            "用量是上一轮的真实用量，不被压缩改动"
        );

        // 摘要请求走会话的生效配置：system 钉死、user 是第一轮的渲染正文、不带工具
        let requests = model.requests();
        assert_eq!(requests.len(), 1, "{requests:#?}");
        assert_eq!(
            requests[0]["messages"][0],
            serde_json::json!({"role": "system", "content": COMPACT_SYSTEM_PROMPT})
        );
        assert_eq!(
            requests[0]["messages"][1]["content"],
            serde_json::json!("user: 第一句\n\nassistant: 答复一")
        );
        assert!(requests[0].get("tools").is_none(), "{:#?}", requests[0]);
    }

    /// C1（终审）：`/compact` 的摘要先过一道脱敏再落库、再进重建后的历史——模型把用户
    /// 粘贴过的已知密钥复述进摘要，也不会被持久化、回显或带给下一轮内核。
    #[tokio::test]
    async fn compact_summary_is_redacted_before_storage_and_history() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("早前提到 sk-test-secret".into())]);
        let config = Config {
            api_key: Some("sk-test-secret".into()),
            ..scripted_config(&model)
        };
        let (_dir, mut session) = session_fixture_with_config("compact-redacted", "native", config);
        let id = session_with_three_turns(&mut session, "compact-redacted");

        let lines = session
            .compact(CancellationToken::new())
            .await
            .expect("脚本化模型应给出摘要");

        // 落库的摘要、重建后的历史与库里的有效历史视图三处都只有脱敏文本
        let latest = session
            .db()
            .latest_compaction(&id)
            .unwrap()
            .expect("compaction 行");
        assert_eq!(latest.summary, "早前提到 ***");
        assert_eq!(latest.upto_seq, 2, "切点仍是被压缩段的上界");
        let expected = format!("{EARLIER_SUMMARY_PREFIX}\n\n早前提到 ***");
        assert_eq!(session.history()[0].content, expected);
        assert!(
            !session
                .history()
                .iter()
                .any(|message| message.content.contains("sk-test-secret")),
            "{:?}",
            session.history()
        );
        let db_view = session.store().effective_history(&id).unwrap();
        assert_eq!(db_view[0].content, expected, "内存与库里同源");
        // 回显按脱敏后的字符数算
        assert_eq!(
            lines,
            [format!(
                "compacted: 2 messages → summary ({} chars)",
                "早前提到 ***".chars().count()
            )]
        );
    }

    /// 二次压缩（ledger 预检 #6 的口径）：可压段只剩上一份摘要消息，`upto_seq` 沿用旧上界、
    /// 保留段不变；原始行照旧一条不删。
    #[tokio::test]
    async fn compact_again_summarizes_the_previous_summary_only() {
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("第一份摘要".into()),
            ScriptStep::Text("第二份摘要".into()),
        ]);
        let (_dir, mut session) =
            session_fixture_with_config("compact-second", "native", scripted_config(&model));
        let id = session_with_three_turns(&mut session, "compact-second");

        assert_eq!(
            session.compact(CancellationToken::new()).await.unwrap(),
            [format!(
                "compacted: 2 messages → summary ({} chars)",
                "第一份摘要".chars().count()
            )]
        );
        // 二次压缩：有效历史 = [摘要(seq 2), 第二句, 答复二, 第三句, 答复三]，
        // 倒数第二条 user 是「第二句」——比它更早的只有上一份摘要
        let lines = session.compact(CancellationToken::new()).await.unwrap();
        assert_eq!(
            lines,
            [format!(
                "compacted: 1 messages → summary ({} chars)",
                "第二份摘要".chars().count()
            )]
        );
        assert_eq!(model.requests().len(), 2);
        // `upto_seq` 沿用旧上界（还是第一轮 assistant 的 seq = 2），保留段原样
        let latest = session
            .db()
            .latest_compaction(&id)
            .unwrap()
            .expect("第二次压缩行");
        assert_eq!(latest.upto_seq, 2);
        assert_eq!(latest.summary, "第二份摘要");
        assert_eq!(
            session.history(),
            [
                chat(
                    Role::User,
                    &format!("{EARLIER_SUMMARY_PREFIX}\n\n第二份摘要")
                ),
                chat(Role::User, "第二句"),
                chat(Role::Assistant, "答复二"),
                chat(Role::User, "第三句"),
                chat(Role::Assistant, "答复三"),
            ]
        );
        // 两次压缩都是「追加一行」：原始 6 行一条不删，摘要行两条都在
        assert_eq!(session.db().session_messages_from(&id, 1).unwrap().len(), 6);
        let count: i64 = session
            .db()
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM session_compactions WHERE session_id = ?1",
                    [&id],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(count, 2);
    }

    /// 模型失败：`Err` 带 `compact: summarization failed` 前缀、状态零变化（不落 compaction、
    /// 历史逐条不变、组装统计不被清），原始行一条不删。
    #[tokio::test]
    async fn compact_model_failure_changes_nothing() {
        // 401 不重试：内核立刻折成 `Done{Failed}`
        let model = ScriptedModel::start(vec![ScriptStep::Error {
            status: 401,
            body: r#"{"error":"bad key"}"#.into(),
        }]);
        let (_dir, mut session) =
            session_fixture_with_config("compact-failed", "native", scripted_config(&model));
        let id = session_with_three_turns(&mut session, "compact-failed");
        let before = session.history().to_vec();
        session.last_prompt_stats = Some(PromptStats::default());

        let error = session.compact(CancellationToken::new()).await.unwrap_err();
        assert!(error.contains("compact: summarization failed"), "{error}");

        assert!(
            session.db().latest_compaction(&id).unwrap().is_none(),
            "失败的压缩不落库"
        );
        assert_eq!(session.history(), before.as_slice(), "失败不动历史");
        assert_eq!(
            session.db().session_messages_from(&id, 1).unwrap().len(),
            6,
            "原始行一条不删"
        );
        assert!(
            session.last_prompt_stats().is_some(),
            "失败的压缩不清组装统计"
        );
        assert_eq!(model.requests().len(), 1, "请求真的发过：失败来自内核");
    }

    /// I2（终审）：`/compact` 可中断——预取消的令牌让摘要请求在首轮请求前就中止（native 每轮
    /// 预检直接给 `Done{Aborted}`），错误逐字是 `compact: summarization failed: aborted`，
    /// 状态零变化、也不消耗模型。
    #[tokio::test]
    async fn compact_with_a_cancelled_token_fails_without_touching_state() {
        // 空脚本：预取消时内核根本不该发起 HTTP 请求（真发了只会拿到 500）
        let model = ScriptedModel::start(Vec::new());
        let (_dir, mut session) =
            session_fixture_with_config("compact-cancelled", "native", scripted_config(&model));
        let id = session_with_three_turns(&mut session, "compact-cancelled");
        let before = session.history().to_vec();
        session.last_prompt_stats = Some(PromptStats::default());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = session.compact(cancel).await.unwrap_err();
        assert_eq!(error, "compact: summarization failed: aborted");

        assert!(
            session.db().latest_compaction(&id).unwrap().is_none(),
            "中止的压缩不落库"
        );
        assert_eq!(session.history(), before.as_slice(), "中止不动历史");
        assert_eq!(
            session.db().session_messages_from(&id, 1).unwrap().len(),
            6,
            "原始行一条不删"
        );
        assert!(session.last_prompt_stats().is_some(), "中止不清组装统计");
        assert!(model.requests().is_empty(), "预取消不该消耗模型");
    }

    /// `/learn` 的草稿文本（两步脚本的第二步）：合法的 SKILL.md。
    fn draft_text() -> String {
        "---\nname: demo-skill\ndescription: Demo draft.\n---\n# 步骤\n1. 做事\n".to_string()
    }

    /// 检查顺序第一档：非 native 或没有 Ignis 凭据都报同一句，一次模型请求都不发（`learn` 与
    /// `draft_for_learn` 同一条门槛）。
    #[tokio::test]
    async fn learn_requires_native_credentials() {
        // 外置内核（有 Ignis 凭据也不具备草稿所需的 native 请求）
        let model = ScriptedModel::start(Vec::new());
        let (_dir, mut codex) =
            session_fixture_with_config("learn-codex", "codex", scripted_config(&model));
        assert!(!codex.is_native());
        assert_eq!(
            codex
                .draft_for_learn(None, CancellationToken::new())
                .await
                .unwrap_err(),
            "learn requires native kernel credentials (current kernel does not provide them)"
        );
        // `learn` 整条路径同一句：门槛在生成草稿之前，根本走不到预览与确认
        assert_eq!(
            codex
                .learn(None, CancellationToken::new())
                .await
                .unwrap_err(),
            "learn requires native kernel credentials (current kernel does not provide them)"
        );

        // native 但没有 Ignis 凭据（环境变量名取不存在的，断言与运行者环境无关）
        let (_dir, mut keyless) = keyless_session_fixture("learn-keyless", "native");
        assert!(keyless.is_native());
        assert!(!keyless.has_credentials());
        assert_eq!(
            keyless
                .learn(None, CancellationToken::new())
                .await
                .unwrap_err(),
            "learn requires native kernel credentials (current kernel does not provide them)"
        );

        assert!(
            model.requests().is_empty(),
            "凭据门槛先于模型：{:?}",
            model.requests()
        );
    }

    /// 默认目标 = 本工作区最近一条**成功**任务：转录里带上它的 id 与提示词；其后往库里插一条更旧
    /// 的成功与一条更新的失败，仍取「最近的成功」。
    #[tokio::test]
    async fn learn_defaults_to_the_latest_successful_task_of_the_workspace() {
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("答复一".into()),
            ScriptStep::Text(draft_text()),
            ScriptStep::Text(draft_text()),
        ]);
        let (_dir, mut session) =
            session_fixture_with_config("learn-default", "native", scripted_config(&model));

        // 先跑出一轮成功任务（消耗脚本第 1 步）
        let outcome = session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .expect("脚本化模型应答应成功");
        let task_id = outcome.id.clone();

        let draft = session
            .draft_for_learn(None, CancellationToken::new())
            .await
            .expect("第二步是合法草稿");
        assert_eq!(draft.name, "demo-skill");
        assert_eq!(draft.description, "Demo draft.");

        let requests = model.requests();
        assert_eq!(requests.len(), 2, "一轮任务 + 一次草稿请求：{requests:#?}");
        let sent = requests[1]["messages"][1]["content"]
            .as_str()
            .expect("草稿请求的 user 正文");
        assert!(sent.starts_with("Task: "), "{sent}");
        assert!(sent.contains(&format!("Task: {task_id}")), "{sent}");
        assert!(sent.contains("Prompt: 你好"), "{sent}");
        assert!(sent.contains("Answer: 答复一"), "{sent}");
        assert!(requests[1].get("tools").is_none(), "{:#?}", requests[1]);

        // 更旧的成功 + 更新的失败：默认目标仍是「最近的成功」
        let workspace = session.workspace().display().to_string();
        for (id, started_at) in [
            ("019a1b2c-3d4e-7f00-8000-000000000002", 1),
            ("019a1b2c-3d4e-7f00-8000-000000000003", i64::MAX),
        ] {
            session
                .db()
                .insert_task(&NewTask {
                    id: id.into(),
                    kind: "run".into(),
                    prompt: format!("prompt {id}"),
                    kernel: "native".into(),
                    workspace: workspace.clone(),
                    started_at,
                    session_id: None,
                })
                .unwrap();
        }
        session
            .db()
            .finish_task(
                "019a1b2c-3d4e-7f00-8000-000000000002",
                "success",
                Some("旧答复"),
                None,
                0,
                0,
                false,
                2,
            )
            .unwrap();
        session
            .db()
            .finish_task(
                "019a1b2c-3d4e-7f00-8000-000000000003",
                "failed",
                None,
                Some("boom"),
                0,
                0,
                false,
                i64::MAX,
            )
            .unwrap();

        let again = session
            .draft_for_learn(None, CancellationToken::new())
            .await
            .expect("第三步是合法草稿");
        assert_eq!(again.name, "demo-skill");
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        let sent = requests[2]["messages"][1]["content"]
            .as_str()
            .expect("草稿请求的 user 正文");
        assert!(
            sent.contains(&format!("Task: {task_id}")),
            "仍取最近的成功：{sent}"
        );
        assert!(
            !sent.contains("prompt 019a1b2c-3d4e-7f00-8000-000000000003"),
            "{sent}"
        );
    }

    /// 显式 id 走 `task show` 的前缀规则：≥8 字符命中同一条、拼错原样报 not found；命中但不是
    /// 成功状态与空库都给「没有可学的任务」；全程不消耗模型之外的会话状态、不落任务行。
    #[tokio::test]
    async fn learn_resolves_an_explicit_task_prefix_and_rejects_other_states() {
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("答复一".into()),
            ScriptStep::Text(draft_text()),
        ]);
        let (_dir, mut session) =
            session_fixture_with_config("learn-prefix", "native", scripted_config(&model));
        let outcome = session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .unwrap();
        let id = outcome.id.clone();
        // `/learn` 不动会话：先记下五个面，跑完再逐个比
        let history = session.history().to_vec();
        let session_id = session.current_session().map(str::to_string);
        let usage = session.last_usage().copied();
        let stats = session.last_prompt_stats().copied();
        let tasks_before = session.db().list_tasks(10).unwrap().len();

        // 前缀（8 字符）命中同一条：转录里是完整 id
        let draft = session
            .draft_for_learn(Some(&id[..8]), CancellationToken::new())
            .await
            .expect("前缀应命中那条成功任务");
        assert_eq!(draft.name, "demo-skill");
        let sent = model.requests()[1]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(sent.contains(&format!("Task: {id}")), "{sent}");

        // 过短 / 拼错：原样交回 `task show` 的报错，不碰模型
        for bad in ["short", "zzzzzzzz"] {
            assert_eq!(
                session
                    .draft_for_learn(Some(bad), CancellationToken::new())
                    .await
                    .unwrap_err(),
                format!("task not found: {bad}")
            );
        }
        assert_eq!(model.requests().len(), 2, "not found 不消耗模型");

        // `/learn` 不新建任务行、不动会话（history / 指针 / 用量 / 组装统计一个都不变）
        assert_eq!(session.db().list_tasks(10).unwrap().len(), tasks_before);
        assert_eq!(session.history(), history.as_slice());
        assert_eq!(session.current_session().map(str::to_string), session_id);
        assert_eq!(session.last_usage().copied(), usage);
        assert_eq!(session.last_prompt_stats().copied(), stats);

        // 命中的任务不是成功状态（改成 aborted）→ 同一句「没有可学的任务」；默认目标也一样
        let model = ScriptedModel::start(vec![ScriptStep::Text("答复一".into())]);
        let (_dir, mut aborted) =
            session_fixture_with_config("learn-aborted", "native", scripted_config(&model));
        let outcome = aborted
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .unwrap();
        aborted
            .db()
            .finish_task(&outcome.id, "aborted", None, None, 0, 0, false, 99)
            .unwrap();
        for arg in [Some(outcome.id.as_str()), None] {
            assert_eq!(
                aborted
                    .draft_for_learn(arg, CancellationToken::new())
                    .await
                    .unwrap_err(),
                "learn: no completed task to learn from"
            );
        }
        assert_eq!(model.requests().len(), 1, "目标解析失败不消耗模型");

        // 空 home：没有任务行 → 同一句
        let model = ScriptedModel::start(Vec::new());
        let (_dir, mut empty) =
            session_fixture_with_config("learn-empty", "native", scripted_config(&model));
        assert_eq!(
            empty
                .draft_for_learn(None, CancellationToken::new())
                .await
                .unwrap_err(),
            "learn: no completed task to learn from"
        );
        assert!(model.requests().is_empty());
    }

    /// 落盘那一支：`saved skill …` + `reloaded skills (4)`、文件 0600 且内容等于草稿文本、reload
    /// 后会话目录里看得见；再存一次被拒（文件存在）且字节不变。夹具技能目录里只有内置三技能。
    #[tokio::test]
    async fn learn_saves_the_draft_and_reloads_the_catalog() {
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("答复一".into()),
            ScriptStep::Text(draft_text()),
        ]);
        let (dir, mut session) =
            session_fixture_with_config("learn-save", "native", scripted_config(&model));
        session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .unwrap();
        assert_eq!(session.skills().metas().len(), 3, "夹具目录只有内置三技能");

        let draft = session
            .draft_for_learn(None, CancellationToken::new())
            .await
            .unwrap();
        let lines = session.commit_draft(&draft).expect("空目录应当能落盘");
        assert_eq!(lines, ["saved skill demo-skill", "reloaded skills (4)"]);

        let path = dir
            .path()
            .join("skills")
            .join("demo-skill")
            .join("SKILL.md");
        assert!(path.is_file(), "{}", path.display());
        assert_eq!(fs::read_to_string(&path).unwrap(), draft.to_skill_md());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
        assert!(
            session
                .skills()
                .metas()
                .iter()
                .any(|meta| meta.name == "demo-skill"),
            "reload 后会话目录里应看得见新技能"
        );

        // 再存一次同一草稿：拒绝（文件存在）且一个字节都不改
        let before = fs::read(&path).unwrap();
        assert_eq!(
            session.commit_draft(&draft).unwrap_err(),
            "skill demo-skill already exists"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    /// C1 口径：草稿的 `description` / `body` 在预览与落盘之前先过一遍会话脱敏器——两处是同一份
    /// 文本，模型复述的密钥既不会进预览，也不会进 SKILL.md。
    #[tokio::test]
    async fn learn_redacts_the_draft_before_preview_and_save() {
        let secret = "sk-learn-secret";
        let text = format!(
            "---\nname: demo-skill\ndescription: 提到 {secret}\n---\n# 步骤\n用 {secret} 做事\n"
        );
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("答复一".into()),
            ScriptStep::Text(text),
        ]);
        let config = Config {
            api_key: Some(secret.into()),
            ..scripted_config(&model)
        };
        let (dir, mut session) = session_fixture_with_config("learn-redacted", "native", config);
        session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .unwrap();

        let draft = session
            .draft_for_learn(None, CancellationToken::new())
            .await
            .unwrap();
        assert!(
            draft.description.contains("***") && !draft.description.contains(secret),
            "{draft:?}"
        );
        assert!(
            draft.body.contains("***") && !draft.body.contains(secret),
            "{draft:?}"
        );

        let lines = session.commit_draft(&draft).unwrap();
        assert_eq!(lines, ["saved skill demo-skill", "reloaded skills (4)"]);
        let saved = fs::read_to_string(dir.path().join("skills/demo-skill/SKILL.md")).unwrap();
        assert_eq!(saved, draft.to_skill_md(), "预览与落盘是同一份文本");
        assert!(!saved.contains(secret), "{saved}");
    }

    /// 非法草稿与模型失败都在落盘之前收口：技能目录一个子项都不多；错误前缀逐字。
    #[tokio::test]
    async fn learn_reports_an_invalid_draft_without_writing() {
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("答复一".into()),
            ScriptStep::Text("不是草稿".into()),
        ]);
        let (dir, mut session) =
            session_fixture_with_config("learn-invalid", "native", scripted_config(&model));
        session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .unwrap();

        let error = session
            .draft_for_learn(None, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "learn: invalid draft (missing or invalid frontmatter)"
        );
        // 技能目录没有被碰过（不存在，或还是空的）
        let entries = fs::read_dir(dir.path().join("skills"))
            .map(Iterator::count)
            .unwrap_or(0);
        assert_eq!(entries, 0, "非法草稿一个目录都不该建");

        // 模型失败：`learn: generation failed: ` 前缀（401 不重试）
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("答复一".into()),
            ScriptStep::Error {
                status: 401,
                body: r#"{"error":"bad key"}"#.into(),
            },
        ]);
        let (_dir, mut session) =
            session_fixture_with_config("learn-generate", "native", scripted_config(&model));
        session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .unwrap();
        let error = session
            .draft_for_learn(None, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.starts_with("learn: generation failed: "), "{error}");
    }

    /// `/learn` 的确认接缝：草稿生成之后才调确认面，`(草稿, to_skill_md)` 就是它收到的两个入参；
    /// 只有回 `true` 才落盘（0600、内容就是确认面收到的那一份、reload 后目录里看得见），回
    /// `false` 只交回 `["discarded"]` 且技能目录一个子项都不多。
    #[tokio::test]
    async fn learn_with_calls_the_injected_confirm_and_saves_only_on_true() {
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("答复一".into()),
            ScriptStep::Text(draft_text()),
            ScriptStep::Text(draft_text()),
        ]);
        let (dir, mut session) =
            session_fixture_with_config("learn-with-confirm", "native", scripted_config(&model));
        session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .expect("先跑一轮成功任务（`/learn` 的目标）");

        // 确认面收到 `(草稿, SKILL.md)`，回 `true`：落盘 + reload
        let seen: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&seen);
        let lines = session
            .learn_with(None, CancellationToken::new(), move |draft, skill_md| {
                *captured.lock().expect("记录锁不该中毒") =
                    Some((draft.name.clone(), skill_md.clone()));
                Box::pin(std::future::ready(true))
            })
            .await
            .expect("确认后应落盘");
        assert_eq!(lines, ["saved skill demo-skill", "reloaded skills (4)"]);
        let (name, previewed) = seen
            .lock()
            .expect("记录锁不该中毒")
            .clone()
            .expect("确认面收到了草稿与正文");
        assert_eq!(name, "demo-skill");

        let path = dir
            .path()
            .join("skills")
            .join("demo-skill")
            .join("SKILL.md");
        assert!(path.is_file(), "{}", path.display());
        assert_eq!(
            fs::read_to_string(&path).expect("落盘文件应可读"),
            previewed,
            "落盘内容与确认面收到的正文是同一份"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
        assert!(
            session
                .skills()
                .metas()
                .iter()
                .any(|meta| meta.name == "demo-skill"),
            "reload 后会话目录里应看得见新技能"
        );

        // 同一夹具再走一次：确认面回 `false` → 只有 `discarded`，技能目录一个子项都不多
        let skills_dir = dir.path().join("skills");
        let snapshot = || {
            let mut names: Vec<_> = fs::read_dir(&skills_dir)
                .expect("技能目录应可读")
                .map(|entry| entry.expect("目录项应可读").file_name())
                .collect();
            names.sort();
            names
        };
        let before = snapshot();
        let called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&called);
        let lines = session
            .learn_with(None, CancellationToken::new(), move |_draft, _skill_md| {
                flag.store(true, Ordering::SeqCst);
                Box::pin(std::future::ready(false))
            })
            .await
            .expect("放弃不是错误");
        assert!(called.load(Ordering::SeqCst), "确认面必须被调用");
        assert_eq!(lines, ["discarded"]);
        assert_eq!(snapshot(), before, "放弃一个目录都不该新建");
    }

    /// 生成失败走经典文案（确认面一次都不调）：`Err` 以 `learn: generation failed: ` 开头。
    #[tokio::test]
    async fn learn_with_reports_the_generation_failure_through_the_classic_copy() {
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("答复一".into()),
            ScriptStep::Error {
                status: 401,
                body: r#"{"error":"bad key"}"#.into(),
            },
        ]);
        let (_dir, mut session) =
            session_fixture_with_config("learn-with-failure", "native", scripted_config(&model));
        session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .expect("先跑一轮成功任务（`/learn` 的目标）");

        let error = session
            .learn_with(None, CancellationToken::new(), |_draft, _skill_md| {
                unreachable!("生成失败不该走到确认面")
            })
            .await
            .unwrap_err();
        assert!(error.starts_with("learn: generation failed: "), "{error}");
    }

    /// 记录写出的字节（注入面的用例夹具）：两个句柄写进同一块 `Vec<u8>`，按写序落账。
    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("记录锁不该中毒").clone())
                .expect("渲染输出、结束行与 error: 行都是合法 UTF-8")
        }
    }

    impl std::io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("记录锁不该中毒")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// TUI 的单一捕获面：`shared_writer` 把一个面拆成两个共享它的句柄，写序不变。
    #[test]
    fn shared_writer_writes_both_ends_into_one_buffer() {
        let buffer = SharedBuffer::default();
        let (mut out, mut err) = shared_writer(Box::new(buffer.clone()));

        out.write_all(b"render").unwrap();
        err.write_all("error: 坏了\n".as_bytes()).unwrap();
        out.write_all(b"more").unwrap();
        err.flush().unwrap();

        assert_eq!(
            buffer.text(),
            "rendererror: 坏了\nmore",
            "两个句柄写进同一块缓冲，顺序即写序"
        );
    }

    /// 注入输出面的轮次：渲染输出、结束行、非成功终态的原因行都进同一个面（经典路径分 stdout/stderr，
    /// 由既有 e2e 覆盖；这里用同一个实现钉死两个入口）。
    #[tokio::test]
    async fn run_task_writes_the_end_line_and_the_error_line_into_the_injected_sink() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("答复一".into())]);
        let (_dir, mut session) =
            session_fixture_with_config("tui-sink", "native", scripted_config(&model));
        let buffer = SharedBuffer::default();

        let outcome = session
            .run_task_with_out("你好".into(), TaskKind::Repl, Box::new(buffer.clone()))
            .await
            .expect("脚本化模型应答应成功");
        assert_eq!(outcome.final_text.as_deref(), Some("答复一"));

        let text = buffer.text();
        assert!(
            text.contains("答复一"),
            "渲染器的正文输出必须进同一个面：{text:?}"
        );
        assert!(text.contains("● Done · "), "结束行进同一个面：{text:?}");

        // 非成功终态：内核给出的原因行也进同一个面（经典路径它走 stderr）
        let model = ScriptedModel::start(vec![ScriptStep::Error {
            status: 401,
            body: r#"{"error":"bad key"}"#.into(),
        }]);
        let (_dir, mut session) =
            session_fixture_with_config("tui-sink-error", "native", scripted_config(&model));
        let buffer = SharedBuffer::default();
        let outcome = session
            .run_task_with_out("你好".into(), TaskKind::Repl, Box::new(buffer.clone()))
            .await
            .expect("401 不重试：轮次本身不返回 Err");

        assert_ne!(outcome.status, RunStatus::Success);
        let text = buffer.text();
        assert!(
            text.lines().any(|line| line.starts_with("error: ")),
            "非成功终态的原因行必须进同一个面：{text:?}"
        );
    }
}
