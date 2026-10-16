//! 内核切换 e2e：`--kernel codex|claude` 用假 CLI 走完整链路（prompt 渲染、能力门控、落库与
//! 能力提示行），缺二进制与非法取值各钉一例。
//!
//! 外置 CLI 用 PATH 上的假脚本注入（不碰真 codex/claude），因此这些用例离线可跑；native 一例用
//! 脚本化假模型，证明默认路径逐字未变（没有提示行、工具清单还在）。

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{TempDir, ignis_cmd, latest_task, output_text};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel};

/// codex 假 CLI：把收到的 prompt 原样写进工作区（cwd），再回放一轮成功事件。
///
/// 事件形状与 `packages/runtime/tests/fixtures/fake-codex.sh` 同源（探测记录里的真机形状）：
/// 一条 `agent_message` + `turn.completed`。
const FAKE_CODEX: &str = r#"#!/bin/sh
prompt=""
for arg in "$@"; do prompt="$arg"; done
printf '%s' "$prompt" > .fake-codex-prompt
printf '%s\n' '{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}'
printf '%s\n' '{"type":"turn.started"}'
printf '%s\n' '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK from codex"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}'
"#;

/// claude 假 CLI：同上，回放 `assistant` 文本块 + `result` 终局。
const FAKE_CLAUDE: &str = r#"#!/bin/sh
prompt=""
for arg in "$@"; do prompt="$arg"; done
printf '%s' "$prompt" > .fake-claude-prompt
printf '%s\n' '{"type":"system","subtype":"init","permissionMode":"default"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"OK from claude"}]}}'
printf '%s\n' '{"type":"result","is_error":false,"result":"OK from claude"}'
"#;

/// 能力提示行（非 native 时逐字打印，见 `commands::kernel_note`）。
fn note(kernel: &str) -> String {
    format!(
        "note: kernel={kernel} — tools run inside {kernel}'s sandbox; \
         remember/load_skill unavailable; usage may be missing"
    )
}

/// 在用例的 bin 目录里放一个可执行的假 CLI。
fn install_fake_cli(bin: &Path, name: &str, script: &str) {
    fs::create_dir_all(bin).expect("bin 目录应可创建");
    let path = bin.join(name);
    fs::write(&path, script).expect("假 CLI 应可写入");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("假 CLI 应可执行");
    }
}

/// 任务行的内核名（`tasks.kernel`；`latest_task` 不带这一列）。
fn latest_task_kernel(db_path: &Path) -> String {
    let conn = rusqlite::Connection::open(db_path).expect("库文件应可直读");
    conn.query_row(
        "SELECT kernel FROM tasks ORDER BY started_at DESC, id DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .expect("本次运行应写入一条任务行")
}

/// 用例的固定三件套：home、工作区、PATH 上只有假 CLI 的 bin 目录。
struct Layout {
    /// 用例独占的临时目录：只负责析构时递归清理（内容经 home/project/bin 访问）。
    _dir: TempDir,
    home: PathBuf,
    project: PathBuf,
    bin: PathBuf,
}

impl Layout {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let home = dir.join("home");
        let project = dir.join("project");
        let bin = dir.join("bin");
        fs::create_dir_all(&home).expect("home 应可创建");
        fs::create_dir_all(&project).expect("工作区应可创建");
        Self {
            _dir: dir,
            home,
            project,
            bin,
        }
    }

    /// PATH 只留假 CLI 目录：真机上的 codex/claude 不会替用例「把测试跑绿」。
    fn cmd(&self, base_url: &str) -> std::process::Command {
        let mut cmd = ignis_cmd(&self.home, &self.project, base_url);
        cmd.env("PATH", &self.bin);
        cmd
    }

    /// 任务日志原文（每次运行恰好一个 `.jsonl`）。
    fn log_raw(&self, task_id: &str) -> String {
        fs::read_to_string(self.home.join("logs").join(format!("{task_id}.jsonl")))
            .expect("任务日志应存在")
    }
}

/// codex 内核端到端：提示行逐字、`tasks.kernel=codex`、外置 CLI 收到的是 T3 钉死的渲染
/// （`---` 分隔 + `User:` 行）且工具清单已被能力门控顶掉。
#[test]
fn codex_kernel_runs_a_task_with_the_rendered_external_prompt() {
    let layout = Layout::new("codex");
    install_fake_cli(&layout.bin, "codex", FAKE_CODEX);
    let model = ScriptedModel::start(vec![ScriptStep::Text("unused".into())]);

    let output = layout
        .cmd(&model.base_url())
        .args(["run", "--kernel", "codex", "说一句 ok"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // 能力提示行在流式输出之前逐字打印
    assert!(stdout.contains(&note("codex")), "{text}");
    // 假 CLI 的文本事件与结束行照常渲染
    assert!(stdout.contains("OK from codex"), "{text}");
    assert!(stdout.contains("● Finished: success"), "{text}");
    // 假模型一次都没被请求：这一轮跑的是 codex 内核
    assert!(model.requests().is_empty(), "{:#?}", model.requests());

    // 落库的内核名与 JSONL 首行一致
    let db = layout.home.join("ignis.db");
    let (task_id, status, error) = latest_task(&db);
    assert_eq!(status, "success", "{task_id}\n{text}");
    assert!(error.is_empty(), "{error}");
    assert_eq!(latest_task_kernel(&db), "codex");
    let raw = layout.log_raw(&task_id);
    assert!(raw.contains("\"kernel\":\"codex\""), "{raw}");

    // 外置 CLI 收到的 prompt：T3 的共享渲染 + 能力门控（工具清单整体不出现）
    let prompt = fs::read_to_string(layout.project.join(".fake-codex-prompt"))
        .expect("假 codex 应写下 prompt");
    assert!(prompt.starts_with("You are Ignis"), "{prompt}");
    assert!(prompt.contains("\n\n---\nUser: 说一句 ok\n"), "{prompt}");
    assert!(
        prompt.contains("Tools: unavailable in this kernel (Ignis tools do not run here)."),
        "{prompt}"
    );
    assert!(!prompt.contains("Available tools:"), "{prompt}");
    assert!(!prompt.contains("fs_read"), "{prompt}");
}

/// claude 内核走同一条链路（选择、提示行、渲染与落库）。
#[test]
fn claude_kernel_runs_a_task_with_the_rendered_external_prompt() {
    let layout = Layout::new("claude");
    install_fake_cli(&layout.bin, "claude", FAKE_CLAUDE);
    let model = ScriptedModel::start(vec![ScriptStep::Text("unused".into())]);

    let output = layout
        .cmd(&model.base_url())
        .args(["run", "--kernel", "claude", "说一句 ok"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(stdout.contains(&note("claude")), "{text}");
    assert!(stdout.contains("OK from claude"), "{text}");

    let db = layout.home.join("ignis.db");
    let (task_id, status, _) = latest_task(&db);
    assert_eq!(status, "success", "{task_id}\n{text}");
    assert_eq!(latest_task_kernel(&db), "claude");

    let prompt = fs::read_to_string(layout.project.join(".fake-claude-prompt"))
        .expect("假 claude 应写下 prompt");
    assert!(prompt.contains("\n\n---\nUser: 说一句 ok\n"), "{prompt}");
    assert!(!prompt.contains("Available tools:"), "{prompt}");
}

/// 选定内核缺二进制：`error:` 人话 + 退出码 1，且不留下跑过的任务行（构造期就拒绝）。
#[test]
fn missing_external_binary_fails_before_any_task_runs() {
    let layout = Layout::new("missing");
    // bin 目录空着：PATH 上没有 codex
    fs::create_dir_all(&layout.bin).expect("bin 目录应可创建");
    let model = ScriptedModel::start(vec![ScriptStep::Text("unused".into())]);

    let output = layout
        .cmd(&model.base_url())
        .args(["run", "--kernel", "codex", "说一句 ok"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(stderr.contains("error: 找不到 codex CLI"), "{text}");
    assert!(stderr.contains("PATH"), "{text}");
    // 构造期就拒绝：没有任务行，也没有提示行
    let conn = rusqlite::Connection::open(layout.home.join("ignis.db")).expect("库文件应可读");
    let tasks: i64 = conn
        .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
        .expect("任务表应可查");
    assert_eq!(tasks, 0, "不可用的内核不该启动任务：{text}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("note:"),
        "{text}"
    );
}

/// `--kernel` 的值域由 clap 把关：非法取值在解析期就退出（码 2），并列出可选值。
#[test]
fn unknown_kernel_value_is_rejected_by_the_parser() {
    let layout = Layout::new("value");
    let model = ScriptedModel::start(vec![ScriptStep::Text("unused".into())]);

    let output = layout
        .cmd(&model.base_url())
        .args(["run", "--kernel", "gemini", "说一句 ok"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{text}");
    assert!(stderr.contains("invalid value 'gemini'"), "{text}");
    assert!(stderr.contains("native, codex, claude"), "{text}");
}

/// native 一例：默认内核没有提示行，且工具清单仍在 system prompt 里（门控不误伤主路径）。
#[test]
fn native_kernel_keeps_the_tool_list_and_prints_no_note() {
    let layout = Layout::new("native");
    let model = ScriptedModel::start(vec![ScriptStep::Text("native ok".into())]);

    let output = layout
        .cmd(&model.base_url())
        .args(["run", "--kernel", "native", "说一句 ok"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(!stdout.contains("note:"), "{text}");
    assert!(stdout.contains("native ok"), "{text}");

    let requests = model.requests();
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let body = serde_json::to_string(&requests[0]).expect("请求体应可序列化");
    assert!(body.contains("Available tools: fs_read"), "{body}");
    assert!(!body.contains("Tools: unavailable"), "{body}");
}
