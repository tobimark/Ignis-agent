//! 命令层：`run` 与 REPL 共用的会话装配（配置/存储/策略/内核/审批）与单次任务执行，
//! 以及 `task`/`config`/`memory`/`skills`/`doctor` 五组辅助命令。

pub mod config;
pub mod doctor;
pub mod memory;
pub mod run;
pub mod skills;
pub mod task;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use ignis_core::alerts::MultiNotifier;
use ignis_core::config::{Config, ensure_home, ignis_home, load_config};
use ignis_core::memory::MemoryStore;
use ignis_core::security::{ApprovalMode, Policy};
use ignis_core::skills::SkillCatalog;
use ignis_core::task::{TaskKind, TaskOutcome, TaskRequest, TaskRunner};
use ignis_runtime::native::NativeRuntime;
use ignis_runtime::openai::OpenAiClient;
use ignis_runtime::{ApprovalHandler, ChatMessage, RunStatus, Runtime};
use ignis_storage::Db;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approval::{InteractiveApproval, PolicyOnlyApproval, resolve_mode};
use crate::cli::Cli;
use crate::render::{Renderer, format_task_end};

/// 一次 CLI 会话的共享依赖：`run` 与 REPL 都从这里取运行器。
pub struct Session {
    /// 本次任务的工作区（`--workspace`，否则配置里的 `workspace_root`）。
    workspace: PathBuf,
    runner: TaskRunner,
    /// 与运行器同一份记忆存储与技能目录（`Arc` 克隆）：banner 计数从这里取，
    /// 不重新开库、也不重新扫描技能目录。
    memory: Arc<MemoryStore>,
    skills: Arc<SkillCatalog>,
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

impl Session {
    /// 装配会话：配置 → 工作区 → 存储 → 策略 → 内核 → 审批 → 运行器。
    ///
    /// 缺 API key、配置解析失败、工作区不存在、建库失败都是预期内的失败，返回人话 `Err` 即可，
    /// 调用方不会因此 panic 或打印堆栈。
    pub fn new(cli: &Cli) -> Result<Self> {
        // 配置里的 api_key 已被 IGNIS_API_KEY 覆盖过，这里只是读出生效值
        let home = ignis_home();
        // 数据目录先落地（新建时 0700）：密钥、任务库与日志都在它下面
        ensure_home(&home)?;
        let mut config: Config = load_config(&home)?;

        let workspace = resolve_workspace(cli, &config)?;

        // stdin 是否为 tty 只有命令层知道，策略规则本身在 `resolve_mode` 里
        let mode = resolve_mode(cli.yes, cli.unsafe_ops, std::io::stdin().is_terminal());
        let db = Arc::new(Db::open(&home.join("ignis.db"))?);
        let policy = Arc::new(Policy::new(mode));

        // key 来源：配置文件里的 api_key（含 IGNIS_API_KEY 覆盖）优先，其次配置指定的环境变量
        let api_key = config
            .api_key
            .clone()
            .or_else(|| std::env::var(&config.model.api_key_env).ok());
        let Some(api_key) = api_key else {
            bail!(
                "未配置 API key（设置 {} 或 config.json 的 api_key）",
                config.model.api_key_env
            );
        };
        // 解析出的生效密钥回填进 config：`TaskRunner` 与每轮任务的 `ToolRuntime` 都从
        // `config.api_key` 取脱敏密钥，只把它交给 HTTP 客户端会让环境变量这条路完全绕过
        // `Redactor`——工具输出里的密钥会明文进 JSONL、也明文回填给模型。构造函数签名保持原样。
        config.api_key = Some(api_key.clone());

        let client =
            OpenAiClient::new(&config.model, api_key)?.with_max_retries(config.limits.max_retries);
        let runtime: Arc<dyn Runtime> = Arc::new(NativeRuntime::new(client));
        // 只有交互模式才该问到人：其余模式策略已经给出结论，被问到说明策略漏判 → 一律拒绝
        let approvals: Arc<dyn ApprovalHandler> = match mode {
            ApprovalMode::Interactive => Arc::new(InteractiveApproval),
            ApprovalMode::Preauthorized { .. } | ApprovalMode::Unattended => {
                Arc::new(PolicyOnlyApproval)
            }
        };

        let config = Arc::new(config);
        // 记忆与技能目录都落在 `~/.ignis` 下：运行器在组装期把已知事实与技能目录注入系统提示，
        // 也把 `remember`/`load_skill` 两个元工具挂进注册表
        let memory = MemoryStore::new(Arc::clone(&db), home.join("memory"));
        let skills = SkillCatalog::load(&home.join("skills"));
        let runner = TaskRunner::new(
            Arc::clone(&db),
            Arc::clone(&config),
            runtime,
            policy,
            approvals,
            home.join("logs"),
            Arc::clone(&memory),
            Arc::clone(&skills),
            // 告警投递渠道：CLI 侧先留空组合——告警照常落库与去重（`ignis alerts` 的未读收件箱
            // 就是渠道①），真实渠道（notify-send / 日志 / IM）由 T5 的 gateway 在这里注入
            Arc::new(MultiNotifier::new(Vec::new())),
            // CLI 交互运行没有 job profile：执行器不包白名单（调度运行由守护进程注入）
            None,
        );

        Ok(Self {
            workspace,
            runner,
            memory,
            skills,
        })
    }

    /// 本次会话的工作区（REPL banner 用）。
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// banner 第 4 行的计数：`(技能数, active 事实数)`。
    ///
    /// 事实数读不出来时只打一行警告并退回 0（banner 显示 `Memory: off`）：状态行不该因为
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

    /// 跑一次任务：渲染任务消费 sink 边跑边输出，Ctrl-C 监听任务负责中止本次运行。
    ///
    /// 结束后打印结束行并把结果交回调用方。渲染任务与监听任务都必须随本次运行结束，
    /// 否则 REPL 会攒下一串永远不会触发的监听器。
    pub async fn run_task(
        &self,
        prompt: String,
        history: Vec<ChatMessage>,
        kind: TaskKind,
    ) -> Result<TaskOutcome> {
        let (sink, mut events) = mpsc::unbounded_channel();
        let mut renderer = Renderer {
            out: Box::new(std::io::stdout()),
            color: std::io::stdout().is_terminal(),
        };
        let renderer_task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                renderer.on_event(&event);
            }
        });

        let cancel = CancellationToken::new();
        let watcher = spawn_ctrl_c_watcher(cancel.clone());

        let started = Instant::now();
        let result = self
            .runner
            .run(
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
            )
            .await;
        // 本次运行已经结束：监听任务先收掉，sink 随 `run` 一起析构后渲染任务也会自行结束
        watcher.abort();
        let _ = renderer_task.await;

        let outcome = result?;
        println!(
            "{}",
            format_task_end(
                &outcome.id,
                outcome.status,
                started.elapsed(),
                &outcome.usage,
                &outcome.skills_used
            )
        );
        // 非成功终态不能只留一个状态词：原因（内核给出、已过 Redactor）打到 stderr，与
        // `main` 的错误行同形。任务 id 已在结束行里，随后可直接 `ignis task show {id}`。
        if outcome.status != RunStatus::Success
            && let Some(error) = &outcome.error
        {
            eprintln!("error: {error}");
        }
        Ok(outcome)
    }
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
