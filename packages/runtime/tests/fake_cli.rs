//! codex 适配器的真实进程测试：`tests/fixtures/fake-codex.sh` 逐字回放真机探测到的
//! `codex exec --json` 事件形状（形状来源：`docs/spikes/2026-10-15-external-kernels-probe.md`）。
//!
//! 这里只走公共 API：`CodexRuntime` + `Runtime::run` 的事件流；夹具脚本经
//! [`CodexRuntime::with_command`] 这个测试接缝注入，不依赖 PATH 上有没有真 codex。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use ignis_runtime::codex::CodexRuntime;
use ignis_runtime::test_support::MockExecutor;
use ignis_runtime::{
    ChatMessage, Limits, ModelConfig, Role, RunRequest, RunStatus, Runtime, RuntimeEvent,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// 夹具脚本的绝对路径（`CARGO_MANIFEST_DIR` = `packages/runtime`）。
fn fake_codex() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-codex.sh")
}

/// 每个用例一个独立工作区：夹具脚本把收到的 argv 写在这里，测试据此断言命令行。
///
/// 用 Drop 兜底删除（与 claude 套件同款）：用例失败 panic 也不会在临时目录里留下垃圾。
struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ignis-codex-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> PathBuf {
        self.path.clone()
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `STDERR_ORPHAN` 的释放开关：夹具留下的后代进程一直攥着 stderr 写端，直到工作区里出现这个
/// 文件（另有 60s 自愈上限）。Drop 兜底删除（用例失败 panic 也会释放），免得后代拖住测试。
struct StderrHolder {
    release: PathBuf,
}

impl StderrHolder {
    fn new(workspace: &Path) -> Self {
        Self {
            release: workspace.join(".fake-codex-stderr-release"),
        }
    }
}

impl Drop for StderrHolder {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, b"release");
    }
}

/// 夹具脚本记下的命令行：开关逐行放在 `.fake-codex-argv`（最后一个是 prompt 的位置参数），
/// prompt 原样放在 `.fake-codex-prompt`（它自带换行，不能按行读）。
fn invocation(workspace: &Path) -> (Vec<String>, String) {
    let args =
        std::fs::read_to_string(workspace.join(".fake-codex-argv")).expect("夹具脚本应写下命令行");
    let prompt = std::fs::read_to_string(workspace.join(".fake-codex-prompt"))
        .expect("夹具脚本应写下 prompt");
    (args.lines().map(str::to_owned).collect(), prompt)
}

fn pgrep(pattern: &str) -> String {
    let output = Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .output()
        .expect("pgrep 应可用");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// 夹具脚本留下的休眠哨兵（数字由工作区路径派生，见 `fake-codex.sh`）：逐个断言没有残留进程。
///
/// 哨兵每次运行都不同，所以上一次被 SIGKILL 的运行留下的孤儿不会误伤本次断言（评审 Minor #5）；
/// 重试窗口是给刚被 SIGKILL / 刚被释放的后代留一点退出与回收的时间。
async fn assert_no_sleepers(workspace: &Path) {
    let tag = std::fs::read_to_string(workspace.join(".fake-codex-sleepers"))
        .expect("夹具脚本应写下休眠哨兵");
    for sentinel in tag.lines() {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pgrep(sentinel).is_empty() {
            assert!(
                Instant::now() < deadline,
                "休眠哨兵 {sentinel} 还有残留进程：{}",
                pgrep(sentinel)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn request(system_prompt: &str, content: &str, timeout_secs: u64) -> RunRequest {
    RunRequest {
        messages: vec![ChatMessage {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
            tool_calls: vec![],
        }],
        system_prompt: system_prompt.into(),
        model: ModelConfig::default(),
        limits: Limits {
            task_timeout_secs: timeout_secs,
            ..Limits::default()
        },
        executor: MockExecutor::new(vec![]),
    }
}

/// 收全整条流（含唯一的 `Done`）。
async fn collect(
    runtime: &CodexRuntime,
    req: RunRequest,
    cancel: CancellationToken,
) -> Vec<RuntimeEvent> {
    runtime.run(req, cancel).await.collect().await
}

/// 恰好一条 `Done`，且是最后一条——契约里「一条流只有一条 Done」在这里钉住。
fn terminal(events: &[RuntimeEvent]) -> &RuntimeEvent {
    let done: Vec<&RuntimeEvent> = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::Done { .. }))
        .collect();
    assert_eq!(done.len(), 1, "应恰好一条 Done：{events:?}");
    assert_eq!(
        events.last(),
        Some(done[0]),
        "Done 必须是最后一条：{events:?}"
    );
    done[0]
}

fn failure(events: &[RuntimeEvent]) -> (RunStatus, Option<String>) {
    match terminal(events) {
        RuntimeEvent::Done { status, error, .. } => (*status, error.clone()),
        other => panic!("末条应为 Done，实际 {other:?}"),
    }
}

#[test]
fn id_and_capabilities_are_pinned() {
    let runtime = CodexRuntime::new(PathBuf::from("."), None);
    assert_eq!(runtime.id(), "codex");
    assert_eq!(
        runtime.capabilities(),
        ignis_runtime::Capabilities {
            tools: false,
            approvals: false,
            usage: false,
        }
    );
}

/// 命令行与 prompt 渲染（T3 的共享 `render_external_prompt`）逐字钉住：`-m` 只在给了模型时才出现。
#[tokio::test]
async fn spawns_codex_exec_with_pinned_flags_and_rendered_prompt() {
    let workspace = TempWorkspace::new("flags");
    let runtime =
        CodexRuntime::with_command(fake_codex(), workspace.path(), Some("gpt-5-codex".into()));
    let events = collect(
        &runtime,
        request("你是 Ignis 的助手", "说一句 ok", 60),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(
        invocation(&workspace.path()),
        (
            vec![
                "exec".to_string(),
                "--json".into(),
                "-C".into(),
                workspace.path().display().to_string(),
                "--sandbox".into(),
                "workspace-write".into(),
                "--skip-git-repo-check".into(),
                "-m".into(),
                "gpt-5-codex".into(),
            ],
            "你是 Ignis 的助手\n\n---\nUser: 说一句 ok\n".to_string(),
        )
    );
    // 只映射文本与终局：reasoning 条目、`turn.completed.usage` 都不出事件
    assert_eq!(
        events,
        vec![
            RuntimeEvent::TextDelta { text: "OK.".into() },
            RuntimeEvent::Done {
                status: RunStatus::Success,
                summary: Some("OK.".into()),
                error: None,
            },
        ]
    );
}

/// 没给模型时不带 `-m`（参数可缺省），且默认分支照样跑通。
#[tokio::test]
async fn omits_model_flag_when_no_model_was_configured() {
    let workspace = TempWorkspace::new("no-model");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);
    let events = collect(
        &runtime,
        request("sys", "说一句 ok", 60),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(
        invocation(&workspace.path()),
        (
            vec![
                "exec".to_string(),
                "--json".into(),
                "-C".into(),
                workspace.path().display().to_string(),
                "--sandbox".into(),
                "workspace-write".into(),
                "--skip-git-repo-check".into(),
            ],
            "sys\n\n---\nUser: 说一句 ok\n".to_string(),
        )
    );
    assert_eq!(events.len(), 2, "{events:?}");
}

/// 未知事件类型、未知条目类型、非 JSON 行一律静默跳过（容忍 schema 漂移），流照常收束。
#[tokio::test]
async fn unknown_lines_are_skipped_silently() {
    let workspace = TempWorkspace::new("noise");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);
    let events = collect(
        &runtime,
        request("sys", "NOISE", 60),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(
        events,
        vec![
            RuntimeEvent::TextDelta {
                text: "noise ok".into()
            },
            RuntimeEvent::Done {
                status: RunStatus::Success,
                summary: Some("noise ok".into()),
                error: None,
            },
        ]
    );
}

/// 命令事件只观察：`command_execution` 的每个生命周期行映射成一条 `ToolExec`，不发 `ToolResult`
/// （外部内核的工具执行结果不回流到 Ignis 的执行器）。
#[tokio::test]
async fn command_execution_maps_to_observation_only_tool_exec() {
    let workspace = TempWorkspace::new("tool");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);
    let events = collect(
        &runtime,
        request("sys", "TOOL", 60),
        CancellationToken::new(),
    )
    .await;

    let tool_execs: Vec<&RuntimeEvent> = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::ToolExec { .. }))
        .collect();
    assert_eq!(tool_execs.len(), 2, "{events:?}");
    for event in tool_execs {
        let RuntimeEvent::ToolExec {
            name,
            summary,
            args,
            ..
        } = event
        else {
            unreachable!()
        };
        assert_eq!(name, "command_execution");
        assert_eq!(summary, "bash -lc ls");
        assert_eq!(args["command"], "bash -lc ls");
    }
    // 生命周期差别留在原始条目里（`in_progress` → `completed` + exit_code）
    let RuntimeEvent::ToolExec { args, .. } = &events[0] else {
        panic!("首条应为 ToolExec：{events:?}")
    };
    assert_eq!(args["status"], "in_progress");
    let RuntimeEvent::ToolExec { args, .. } = &events[1] else {
        panic!("次条应为 ToolExec：{events:?}")
    };
    assert_eq!(args["status"], "completed");
    assert_eq!(args["exit_code"], 0);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ToolResult { .. })),
        "外部内核不回流工具结果：{events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&RuntimeEvent::Done {
            status: RunStatus::Success,
            summary: Some("列完了".into()),
            error: None,
        })
    );
}

/// 非零退出：`Done{Failed}` 的人话里同时给退出码与 stderr 首行（第二行不进消息）。
#[tokio::test]
async fn nonzero_exit_maps_to_failed_with_exit_code_and_first_stderr_line() {
    let workspace = TempWorkspace::new("exit");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);
    let events = collect(
        &runtime,
        request("sys", "FAIL", 60),
        CancellationToken::new(),
    )
    .await;

    let (status, error) = failure(&events);
    assert_eq!(status, RunStatus::Failed);
    let error = error.expect("失败必须带人话");
    assert!(error.contains("退出码 3"), "{error}");
    assert!(error.contains("codex: fake failure line 1"), "{error}");
    assert!(!error.contains("line 2"), "stderr 只取首行：{error}");
}

/// `turn.failed` 是终局失败：即使进程退出码是 0 也按失败收束。
#[tokio::test]
async fn turn_failed_maps_to_failed_even_with_zero_exit() {
    let workspace = TempWorkspace::new("turnfail");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);
    let events = collect(
        &runtime,
        request("sys", "TURNFAIL", 60),
        CancellationToken::new(),
    )
    .await;

    let (status, error) = failure(&events);
    assert_eq!(status, RunStatus::Failed);
    let error = error.expect("失败必须带人话");
    assert!(
        error.contains("model response stream ended unexpectedly"),
        "{error}"
    );
}

/// 终局事件即刻收束读循环：CLI 打完 `turn.completed` 后不退出（后台/前台 sleep 挂住），
/// 结论必须是 `Success`，而不是等到 EOF 或 `task_timeout` 才收场。
#[tokio::test]
async fn turn_completed_short_circuits_a_cli_that_does_not_exit() {
    let workspace = TempWorkspace::new("complete-hang");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);

    let started = Instant::now();
    let events = collect(
        &runtime,
        request("sys", "COMPLETE_HANG", 60),
        CancellationToken::new(),
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(
        events,
        vec![
            RuntimeEvent::TextDelta {
                text: "完成了但进程不退出".into()
            },
            RuntimeEvent::Done {
                status: RunStatus::Success,
                summary: Some("完成了但进程不退出".into()),
                error: None,
            },
        ]
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "终局一到手就该收束，不该等满超时，实际 {elapsed:?}"
    );
    assert_no_sleepers(&workspace.path()).await;
}

/// 后代进程攥住继承的 stderr 写端（评审 #1）：CLI 正常退出、`turn.completed` 已到手，stderr 却
/// 等不到 EOF——收尾必须有上限，本轮必须以 `Success` 收束，而不是把调用方无限期地挂住。
/// 夹具用 `setsid` 把后代送出进程组，所以只有有界的 stderr 收尾能救它（进程组 SIGKILL 追不到）。
#[tokio::test]
async fn bounded_stderr_reap_finishes_a_run_whose_descendant_holds_stderr() {
    let workspace = TempWorkspace::new("stderr-orphan");
    let holder = StderrHolder::new(&workspace.path());
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);

    let started = Instant::now();
    let events = timeout(
        Duration::from_secs(6),
        collect(
            &runtime,
            request("sys", "STDERR_ORPHAN", 60),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("stderr 收尾无上限时事件流永远不会结束（评审 #1）：6s 内必须收全");
    let elapsed = started.elapsed();

    // 恰好一条 `Done`、是最后一条，且结论是 `Success`：终局已经产出，不该被后代拖成失败或超时
    assert_eq!(
        events,
        vec![
            RuntimeEvent::TextDelta {
                text: "完成了但 stderr 写端还开着".into()
            },
            RuntimeEvent::Done {
                status: RunStatus::Success,
                summary: Some("完成了但 stderr 写端还开着".into()),
                error: None,
            },
        ]
    );
    assert_eq!(terminal(&events), &events[1]);
    assert!(
        elapsed < Duration::from_secs(6),
        "有界收尾必须在上限内收束，实际 {elapsed:?}"
    );

    // 释放后代（不释放的话它会攥着 stderr 到 60s 自愈上限），随后断言没有残留进程
    drop(holder);
    assert_no_sleepers(&workspace.path()).await;
}

/// `turn.failed` 不带 `error.message`（schema 漂移）：仍按失败收束并给通用中文人话，
/// 不能退回「流结束但未见 turn.completed」——终局确实到了。
#[tokio::test]
async fn turn_failed_without_reason_reports_a_generic_failure() {
    let workspace = TempWorkspace::new("turnfail-bare");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);

    let started = Instant::now();
    let events = collect(
        &runtime,
        request("sys", "TURNFAIL_BARE", 60),
        CancellationToken::new(),
    )
    .await;
    let elapsed = started.elapsed();

    let (status, error) = failure(&events);
    assert_eq!(status, RunStatus::Failed);
    let error = error.expect("失败必须带人话");
    assert!(error.contains("codex 报告失败"), "{error}");
    assert!(error.contains("未提供原因"), "{error}");
    assert!(
        !error.contains("未见 turn.completed"),
        "终局到了就不能说没见到终局：{error}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "失败终局一到手就该收束，不该等满超时，实际 {elapsed:?}"
    );
    assert_no_sleepers(&workspace.path()).await;
}

/// 真机失败轨迹（逐字回放 2026-10-15 的复测：非终局的 `type:"error"` 行 + `turn.failed` +
/// 退出码 1）：`Failed` 的人话是 CLI 给的原因，`error` 行不被误当成终局。
#[tokio::test]
async fn real_failure_trace_reports_the_cli_reason() {
    let workspace = TempWorkspace::new("real-fail");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);
    let events = collect(
        &runtime,
        request("sys", "REAL_FAIL", 60),
        CancellationToken::new(),
    )
    .await;

    let (status, error) = failure(&events);
    assert_eq!(status, RunStatus::Failed);
    let error = error.expect("失败必须带人话");
    assert!(error.contains("code: 2013"), "{error}");
    assert!(error.contains("unknown model"), "{error}");
}

/// 缺失 `codex` 二进制：spawn 错误折成清晰的中文 `Done{Failed}`，不 panic、不静默。
#[tokio::test]
async fn missing_codex_binary_reports_a_clear_failure() {
    let workspace = TempWorkspace::new("missing");
    let runtime = CodexRuntime::with_command(
        PathBuf::from("/nonexistent/ignis-codex-missing"),
        workspace.path(),
        None,
    );
    let events = collect(
        &runtime,
        request("sys", "说一句 ok", 60),
        CancellationToken::new(),
    )
    .await;

    let (status, error) = failure(&events);
    assert_eq!(status, RunStatus::Failed);
    let error = error.expect("失败必须带人话");
    assert!(error.contains("无法启动 codex 内核"), "{error}");
    assert!(error.contains("ignis-codex-missing"), "{error}");
}

/// 取消：杀整个进程组（后台 sleep 也逃不掉），收束成 `Done{Aborted}`，不能拖时间。
#[tokio::test]
async fn cancel_kills_the_process_group_and_reports_aborted() {
    let workspace = TempWorkspace::new("cancel");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });

    let started = Instant::now();
    let events = collect(&runtime, request("sys", "CANCEL_HANG", 60), cancel).await;
    let elapsed = started.elapsed();

    assert_eq!(
        events,
        vec![
            RuntimeEvent::TextDelta {
                text: "挂起前".into()
            },
            RuntimeEvent::Done {
                status: RunStatus::Aborted,
                summary: None,
                error: None,
            },
        ]
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "取消应立即返回，实际 {elapsed:?}"
    );
    assert_no_sleepers(&workspace.path()).await;
}

/// 墙钟超时：与 native 同款标记（`task_timeout`），同样杀整组。
#[tokio::test]
async fn task_timeout_kills_the_process_group_and_reports_limit_reached() {
    let workspace = TempWorkspace::new("timeout");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.path(), None);

    let started = Instant::now();
    let events = collect(
        &runtime,
        request("sys", "TIMEOUT_HANG", 1),
        CancellationToken::new(),
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(
        events,
        vec![
            RuntimeEvent::TextDelta {
                text: "挂起前".into()
            },
            RuntimeEvent::Done {
                status: RunStatus::LimitReached,
                summary: None,
                error: Some("task_timeout".into()),
            },
        ]
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "1 秒超时应很快返回，实际 {elapsed:?}"
    );
    assert_no_sleepers(&workspace.path()).await;
}
