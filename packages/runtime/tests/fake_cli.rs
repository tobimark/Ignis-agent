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
use tokio_util::sync::CancellationToken;

/// 夹具脚本的绝对路径（`CARGO_MANIFEST_DIR` = `packages/runtime`）。
fn fake_codex() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-codex.sh")
}

/// 每个用例一个独立工作区：夹具脚本把收到的 argv 写在这里，测试据此断言命令行。
fn temp_workspace(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("ignis-codex-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

/// 命令行与 prompt 渲染（T1 过渡实现）逐字钉住：`-m` 只在给了模型时才出现。
#[tokio::test]
async fn spawns_codex_exec_with_pinned_flags_and_rendered_prompt() {
    let workspace = temp_workspace("flags");
    let runtime =
        CodexRuntime::with_command(fake_codex(), workspace.clone(), Some("gpt-5-codex".into()));
    let events = collect(
        &runtime,
        request("你是 Ignis 的助手", "说一句 ok", 60),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(
        invocation(&workspace),
        (
            vec![
                "exec".to_string(),
                "--json".into(),
                "-C".into(),
                workspace.display().to_string(),
                "--sandbox".into(),
                "workspace-write".into(),
                "--skip-git-repo-check".into(),
                "-m".into(),
                "gpt-5-codex".into(),
            ],
            "你是 Ignis 的助手\n\nuser: 说一句 ok".to_string(),
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
    let workspace = temp_workspace("no-model");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace.clone(), None);
    let events = collect(
        &runtime,
        request("sys", "说一句 ok", 60),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(
        invocation(&workspace),
        (
            vec![
                "exec".to_string(),
                "--json".into(),
                "-C".into(),
                workspace.display().to_string(),
                "--sandbox".into(),
                "workspace-write".into(),
                "--skip-git-repo-check".into(),
            ],
            "sys\n\nuser: 说一句 ok".to_string(),
        )
    );
    assert_eq!(events.len(), 2, "{events:?}");
}

/// 未知事件类型、未知条目类型、非 JSON 行一律静默跳过（容忍 schema 漂移），流照常收束。
#[tokio::test]
async fn unknown_lines_are_skipped_silently() {
    let workspace = temp_workspace("noise");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace, None);
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
    let workspace = temp_workspace("tool");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace, None);
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
    let workspace = temp_workspace("exit");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace, None);
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
    let workspace = temp_workspace("turnfail");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace, None);
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

/// 缺失 `codex` 二进制：spawn 错误折成清晰的中文 `Done{Failed}`，不 panic、不静默。
#[tokio::test]
async fn missing_codex_binary_reports_a_clear_failure() {
    let runtime = CodexRuntime::with_command(
        PathBuf::from("/nonexistent/ignis-codex-missing"),
        temp_workspace("missing"),
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
    let workspace = temp_workspace("cancel");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace, None);
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
    assert_eq!(pgrep("12351"), "", "后台任务逃出了进程组");
    assert_eq!(pgrep("12352"), "", "前台任务未被终止");
}

/// 墙钟超时：与 native 同款标记（`task_timeout`），同样杀整组。
#[tokio::test]
async fn task_timeout_kills_the_process_group_and_reports_limit_reached() {
    let workspace = temp_workspace("timeout");
    let runtime = CodexRuntime::with_command(fake_codex(), workspace, None);

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
    assert_eq!(pgrep("12361"), "", "后台任务逃出了进程组");
    assert_eq!(pgrep("12362"), "", "前台任务未被终止");
}
