//! 命令层：`run` 与 REPL 共用的会话装配（配置/存储/策略/内核/审批）与单次任务执行。

pub mod run;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use ignis_core::config::{Config, ignis_home, load_config};
use ignis_core::security::{ApprovalMode, Policy};
use ignis_core::task::{TaskKind, TaskOutcome, TaskRequest, TaskRunner};
use ignis_runtime::native::NativeRuntime;
use ignis_runtime::openai::OpenAiClient;
use ignis_runtime::{ApprovalHandler, ChatMessage, Runtime};
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
}

impl Session {
    /// 装配会话：配置 → 工作区 → 存储 → 策略 → 内核 → 审批 → 运行器。
    ///
    /// 缺 API key、配置解析失败、工作区不存在、建库失败都是预期内的失败，返回人话 `Err` 即可，
    /// 调用方不会因此 panic 或打印堆栈。
    pub fn new(cli: &Cli) -> Result<Self> {
        // 配置里的 api_key 已被 IGNIS_API_KEY 覆盖过，这里只是读出生效值
        let home = ignis_home();
        let config: Config = load_config(&home)?;

        let workspace = cli
            .workspace
            .clone()
            .unwrap_or_else(|| config.workspace_root.clone());
        if !workspace.is_dir() {
            bail!("工作区不存在或不是目录：{}", workspace.display());
        }

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
        let runner = TaskRunner::new(
            Arc::clone(&db),
            Arc::clone(&config),
            runtime,
            policy,
            approvals,
            home.join("logs"),
        );

        Ok(Self { workspace, runner })
    }

    /// 本次会话的工作区（REPL banner 用）。
    pub fn workspace(&self) -> &Path {
        &self.workspace
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
            format_task_end(outcome.status, started.elapsed(), &outcome.usage)
        );
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
