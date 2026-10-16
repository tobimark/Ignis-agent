//! 命令层：`run` 与 REPL 共用的会话装配（配置/存储/策略/内核/审批）与单次任务执行，
//! 以及 `task`/`config`/`memory`/`skills`/`doctor`/`daemon`/`schedule`/`alerts` 八组辅助命令。

pub mod alerts;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod memory;
pub mod run;
pub mod schedule;
pub mod skills;
pub mod task;

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Result, bail};
use ignis_core::alerts::{MultiNotifier, Notifier};
use ignis_core::config::{Config, ensure_home, ignis_home, load_config};
use ignis_core::memory::MemoryStore;
use ignis_core::security::{ApprovalMode, Policy};
use ignis_core::skills::SkillCatalog;
use ignis_core::task::{TaskKind, TaskOutcome, TaskRequest, TaskRunner};
use ignis_runtime::claude::ClaudeRuntime;
use ignis_runtime::codex::CodexRuntime;
use ignis_runtime::native::NativeRuntime;
use ignis_runtime::openai::OpenAiClient;
use ignis_runtime::{ApprovalHandler, ChatMessage, RunStatus, Runtime};
use ignis_storage::Db;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approval::{InteractiveApproval, PolicyOnlyApproval, resolve_mode};
use crate::cli::{Cli, KERNELS, NATIVE_KERNEL};
use crate::render::{Renderer, format_task_end};

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

/// 未知内核的人话错误：可选值就是 `--kernel` 的 clap 值域（同一份 [`KERNELS`]），不另抄一遍。
fn unknown_kernel(kernel: &str) -> String {
    format!("未知内核 '{kernel}'：可选 {}", KERNELS.join(" / "))
}

/// 内核名白名单校验：`--kernel` 的值域（clap）是同一份口径，`/kernel` 用它给人话错误。
pub fn known_kernel(kernel: &str) -> Result<&'static str, String> {
    KERNELS
        .iter()
        .copied()
        .find(|known| *known == kernel)
        .ok_or_else(|| unknown_kernel(kernel))
}

/// 非 native 内核的能力提示行（决策 #4：工具跑在内核自己的沙箱里，记忆写与技能全文加载不可用）；
/// native 没有降级可提示，返回 `None`。
///
/// 调用方传的都是过了 [`known_kernel`] 的名字，这里只区分 native 与两个外部内核。
pub fn kernel_note(kernel: &str) -> Option<String> {
    if kernel == NATIVE_KERNEL {
        return None;
    }
    Some(format!(
        "note: kernel={kernel} — tools run inside {kernel}'s sandbox; \
         remember/load_skill unavailable; usage may be missing"
    ))
}

/// 按内核名构造执行内核；构造失败（外部 CLI 没装）以人话 `Err` 返回。
///
/// 名字不在值域内也走同一条人话错误（`--kernel` 由 clap 把关，`/kernel` 落到这里）。
pub fn mk_runtime(
    kernel: &str,
    config: &Config,
    workspace: &Path,
) -> Result<Arc<dyn Runtime>, String> {
    mk_runtime_with_path(
        kernel,
        config,
        workspace,
        std::env::var_os("PATH").as_deref(),
    )
}

/// 带 PATH 注入的变体：可用性判定只看传进来的这份 PATH，用例因此不必改进程环境（测试可并行）。
fn mk_runtime_with_path(
    kernel: &str,
    config: &Config,
    workspace: &Path,
    path: Option<&OsStr>,
) -> Result<Arc<dyn Runtime>, String> {
    let kernel = known_kernel(kernel)?;
    match kernel {
        NATIVE_KERNEL => {
            let client =
                OpenAiClient::new(&config.model, config.api_key.clone().unwrap_or_default())
                    .map_err(|error| error.to_string())?
                    .with_max_retries(config.limits.max_retries);
            Ok(Arc::new(NativeRuntime::new(client)))
        }
        // 两个外部内核都用它们自己的默认模型：`config.model` 是给 OpenAI 兼容 relay 的模型名，
        // 塞给 codex/claude 只会让它们去请求一个自家 provider 不认识的模型（模型名各自的配置里管）
        "codex" => {
            ensure_installed("codex", path)?;
            Ok(Arc::new(CodexRuntime::new(workspace.to_path_buf(), None)))
        }
        "claude" => {
            ensure_installed("claude", path)?;
            Ok(Arc::new(ClaudeRuntime::new(workspace.to_path_buf(), None)))
        }
        // 值域与分支同源：将来加了新内核却忘了在这里接上时，报人话而不是静默落到别的实现
        other => Err(format!("内核 '{other}' 尚未接入")),
    }
}

/// 外部内核的可用性检查：PATH 上找不到这个 CLI 就是「没装」。
fn ensure_installed(program: &str, path: Option<&OsStr>) -> Result<(), String> {
    if find_program(program, path).is_some() {
        return Ok(());
    }
    Err(format!(
        "找不到 {program} CLI：请先安装 {program} 并确保它在 PATH 中"
    ))
}

/// PATH 里按目录顺序找可执行文件；`path` 为 `None`（环境变量缺失）时找不到任何东西。
fn find_program(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// 可执行文件判定：普通文件，且（Unix 下）至少有一位执行位。
fn is_executable(candidate: &Path) -> bool {
    let Ok(metadata) = candidate.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

impl Session {
    /// 装配会话：配置 → 工作区 → 存储 → 策略 → 选定内核 → 审批。
    ///
    /// 缺 API key、配置解析失败、工作区不存在、建库失败、选定内核不可用都是预期内的失败，返回
    /// 人话 `Err` 即可，调用方不会因此 panic 或打印堆栈。
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
        // `Redactor`——工具输出里的密钥会明文进 JSONL、也明文回填给模型。
        config.api_key = Some(api_key);
        let config = Arc::new(config);

        // 只有交互模式才该问到人：其余模式策略已经给出结论，被问到说明策略漏判 → 一律拒绝
        let approvals: Arc<dyn ApprovalHandler> = match mode {
            ApprovalMode::Interactive => Arc::new(InteractiveApproval),
            ApprovalMode::Preauthorized { .. } | ApprovalMode::Unattended => {
                Arc::new(PolicyOnlyApproval)
            }
        };

        // 选定内核先构造一次：缺二进制的错误在碰记忆库与第一轮任务之前就变成人话（构造开销小）
        mk_runtime(&cli.kernel, &config, &workspace).map_err(anyhow::Error::msg)?;

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
            kernel: cli.kernel.clone(),
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
        let kernel = known_kernel(kernel)?;
        // 先构造一次：不可用的内核不切换
        mk_runtime(kernel, &self.config, &self.workspace)?;
        self.kernel = kernel.to_string();
        Ok(kernel)
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

    /// 跑一次任务：渲染任务消费 sink 边跑边输出，Ctrl-C 监听任务负责中止本次运行。
    ///
    /// 每轮按当前内核装配运行器（native 建 HTTP 客户端，外部内核只做一次 PATH 检查），
    /// 因此 `/kernel` 的切换只影响后续任务。结束后打印结束行并把结果交回调用方；随后按本轮事件
    /// 决定要不要提示自拉起守护进程。渲染任务与监听任务都必须随本次运行结束，否则 REPL 会攒下
    /// 一串永远不会触发的监听器。
    pub async fn run_task(
        &self,
        prompt: String,
        history: Vec<ChatMessage>,
        kind: TaskKind,
    ) -> Result<TaskOutcome> {
        let runtime =
            mk_runtime(&self.kernel, &self.config, &self.workspace).map_err(anyhow::Error::msg)?;
        let runner = TaskRunner::new(
            Arc::clone(&self.db),
            Arc::clone(&self.config),
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
        // 自拉起的判定看「本轮事件里出现过 schedule_create」：由渲染任务顺手记下
        let saw_schedule_create = Arc::new(AtomicBool::new(false));
        let mut renderer = Renderer {
            out: Box::new(std::io::stdout()),
            color: std::io::stdout().is_terminal(),
            saw_schedule_create: Arc::clone(&saw_schedule_create),
        };
        let renderer_task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                renderer.on_event(&event);
            }
        });

        let cancel = CancellationToken::new();
        let watcher = spawn_ctrl_c_watcher(cancel.clone());

        let started = Instant::now();
        let result = runner
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
        // 本轮建过调度任务、守护进程又没在跑时提示一次自拉起（每会话至多一次）
        daemon::autostart(
            &self.home,
            saw_schedule_create.load(Ordering::Relaxed),
            &self.already_prompted,
        )
        .await;
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// 单个测试独占的临时目录（PATH 夹具），析构时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-cli-kernel-{}-{label}-{}",
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

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 给夹具脚本补上执行位（Unix 之外没有这个概念，测试里另按存在性判定）。
    fn set_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    /// 内核白名单与能力提示文案逐字钉住（`--kernel` 的 clap 值域是同一份口径）。
    #[test]
    fn kernel_whitelist_and_capability_note_are_pinned() {
        for kernel in KERNELS {
            assert_eq!(known_kernel(kernel).unwrap(), kernel);
        }
        for bad in ["", "Native", "gemini", "codex "] {
            let error = known_kernel(bad).unwrap_err();
            assert!(error.contains(&format!("未知内核 '{bad}'")), "{error}");
            assert!(error.contains("native / codex / claude"), "{error}");
        }

        // native 没有降级可提示；两个外部内核同一模板，逐字
        assert_eq!(kernel_note("native"), None);
        assert_eq!(
            kernel_note("codex").as_deref(),
            Some(
                "note: kernel=codex — tools run inside codex's sandbox; \
                 remember/load_skill unavailable; usage may be missing"
            )
        );
        assert_eq!(
            kernel_note("claude").as_deref(),
            Some(
                "note: kernel=claude — tools run inside claude's sandbox; \
                 remember/load_skill unavailable; usage may be missing"
            )
        );
    }

    /// `mk_runtime` 的选择真值表（`/kernel` 与 `--kernel` 同一条路径）：名字合法且可用才构造出内核。
    ///
    /// 可用性走 PATH 注入的变体——不能为测试改进程环境（用例并行，`set_var` 会互相打架）。
    #[test]
    fn mk_runtime_selects_by_name_and_reports_unavailable_external_kernels() {
        let dir = TempDir::new("select");
        let config = Config {
            api_key: Some("test-key".into()),
            ..Config::default()
        };

        // native 永远可（不查 PATH），能力是全套
        let runtime = mk_runtime("native", &config, Path::new("/tmp")).unwrap();
        assert_eq!(runtime.id(), "native");
        assert!(runtime.capabilities().tools);

        // 未知名：人话错误，可选值列全
        let error = mk_runtime("gemini", &config, Path::new("/tmp"))
            .err()
            .expect("未知内核名应报错");
        assert!(error.contains("未知内核 'gemini'"), "{error}");
        assert!(error.contains("native / codex / claude"), "{error}");

        // 已知但 PATH 上没有（目录此刻还是空的）：不上天，给人话错误
        for program in ["codex", "claude"] {
            let error =
                mk_runtime_with_path(program, &config, dir.path(), Some(dir.path().as_os_str()))
                    .err()
                    .expect("PATH 上没有这个 CLI 时应报错");
            assert_eq!(
                error,
                format!("找不到 {program} CLI：请先安装 {program} 并确保它在 PATH 中")
            );
        }

        // 装上就能切：id 与能力声明逐项钉住（外部内核三个能力都是 false）
        for program in ["codex", "claude"] {
            let bin = dir.path().join(program);
            fs::write(&bin, "#!/bin/sh\n").unwrap();
            set_executable(&bin);
        }
        let path = Some(dir.path().as_os_str());
        assert_eq!(
            mk_runtime_with_path("codex", &config, dir.path(), path)
                .unwrap()
                .id(),
            "codex"
        );
        let claude = mk_runtime_with_path("claude", &config, dir.path(), path).unwrap();
        assert_eq!(claude.id(), "claude");
        assert!(!claude.capabilities().tools);
        assert!(!claude.capabilities().approvals);
        assert!(!claude.capabilities().usage);
    }

    /// PATH 扫描：命中第一个可执行文件；非可执行文件、缺失的 PATH 都不算「装上了」。
    #[test]
    fn find_program_scans_path_entries_for_an_executable() {
        let dir = TempDir::new("path-scan");
        let program = "ignis-fake-codex";
        let fixture = dir.path().join(program);
        fs::write(&fixture, "#!/bin/sh\n").unwrap();

        // PATH 缺失（环境变量没设）：找不到任何东西
        assert_eq!(find_program(program, None), None);

        // 存在但没有执行位：不算可执行（非 Unix 平台只看存在性）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&fixture, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(find_program(program, Some(dir.path().as_os_str())), None);
        }
        set_executable(&fixture);
        assert_eq!(
            find_program(program, Some(dir.path().as_os_str())),
            Some(fixture.clone())
        );

        // 多目录按顺序扫描，前面的目录不存在也不影响后面的命中
        let joined =
            std::env::join_paths([dir.path().join("nope"), dir.path().to_path_buf()]).unwrap();
        assert_eq!(find_program(program, Some(&joined)), Some(fixture));
    }

    /// 缺二进制的错误含 CLI 名与安装提示；装上（PATH 里可执行）即返回 `Ok`。
    #[test]
    fn external_kernel_availability_reports_a_missing_binary() {
        let dir = TempDir::new("missing-cli");

        let error = ensure_installed("codex", Some(dir.path().as_os_str())).unwrap_err();
        assert_eq!(error, "找不到 codex CLI：请先安装 codex 并确保它在 PATH 中");

        fs::write(dir.path().join("codex"), "#!/bin/sh\n").unwrap();
        set_executable(&dir.path().join("codex"));
        assert_eq!(
            ensure_installed("codex", Some(dir.path().as_os_str())),
            Ok(())
        );
    }

    /// 最小会话：只装配 `/kernel` 切换要用的字段（临时 home/工作区 + 空记忆与技能目录）。
    fn session_fixture(label: &str, kernel: &str) -> (TempDir, Session) {
        let dir = TempDir::new(label);
        let home = dir.path().to_path_buf();
        let db = Arc::new(Db::open(&home.join("ignis.db")).unwrap());
        let session = Session {
            home: home.clone(),
            workspace: home.clone(),
            db: Arc::clone(&db),
            config: Arc::new(Config::default()),
            policy: Arc::new(Policy::new(ApprovalMode::Unattended)),
            approvals: Arc::new(PolicyOnlyApproval),
            log_dir: home.join("logs"),
            memory: MemoryStore::new(db, home.join("memory")),
            skills: SkillCatalog::load(&home.join("skills")),
            notifier: Arc::new(MultiNotifier::new(Vec::new())),
            kernel: kernel.to_string(),
            already_prompted: AtomicBool::new(false),
        };
        (dir, session)
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

        // native 恒可（不查 PATH）：切过去并回生效名，且没有能力提示行
        assert_eq!(session.set_kernel("native").unwrap(), "native");
        assert_eq!(session.kernel(), "native");
        assert_eq!(kernel_note(session.kernel()), None);
    }
}
