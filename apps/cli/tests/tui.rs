//! TUI 的 pty 端到端用例（五条）：备用屏 + 流式答复 + `/exit` 还原、任务运行中的 Ctrl+C 取消、
//! 开机品牌块（含 `--no-banner` 收掉它）、`TERM=dumb` 与过小终端两条降级。
//!
//! 与 `pty.rs` 同一条纪律（助手在本套里自持一份，不借用那边的任何符号）：
//!
//! - **home/workspace 都建在用例自己的 `TempDir` 里**，绝不碰真实的 `~/.ignis`；模型走
//!   `ScriptedModel`（夹具把它注入成 `IGNIS_BASE_URL`），`TERM`/`NO_COLOR` 也由夹具钉死，零网络；
//! - **打字前先等一个只可能由 CLI 写出的串**：TUI 在画出第一帧之前就已经进了备用屏 + raw mode
//!   （`\x1b[?1049h`、顶部状态行），等到它们再发按键，解码出来的才是编辑器语义（`\r` 是 Enter，
//!   而不是行规范下的「插换行」）；两条降级是经典 REPL，canonical 下由终端自己缓冲与回显；
//! - **`NO_COLOR=1` 由夹具注入**：TUI 的帧里零 SGR（`color=false`），断言一律按纯文本找；备用屏
//!   与光标定位不是颜色，照常出现在流里（`\x1b[?1049h`/`\x1b[?1049l` 正是两条判据）；
//! - **开机品牌块只进滚动区**：字模字形（`█▀▄`）出现在帧里就是「品牌块已经落进滚动区」的证据；
//!   经典**状态块**不进 TUI（`● Ready` 是那条判据的反面），`--no-banner` 收掉的是品牌块本身；
//! - **位置敏感的断言看「标记之后」**：`Pty::expect` 每次从流头重扫，而每一帧都会把同一块滚动区
//!   重画一遍——取消后的空闲输入区只认 `● Aborted` **之后**新写出的字节（[`expect_after`]）；
//! - **假模型句柄与 `TempDir` 都持有到用例结束**：`ScriptedModelHandle::drop` 停服务，
//!   `Pty::drop` 兜底杀子进程（用例不主动退出的那几条靠它收尾）。

#![cfg(unix)]

mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::TempDir;
use common::pty::{PTY_TIMEOUT, Pty, PtyOptions};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel, ScriptedModelHandle};
use serde_json::{Value, json};

/// 位置敏感断言与请求计数的复查间隔（与夹具的 `expect` 同一条 20ms 轮询口径）。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 起一个 `--tui` 会话：临时 home/workspace + 假模型脚本 + 从端窗口尺寸，返回
/// `(pty, 临时目录, 假模型)`。三个返回值都要活到用例结束（见模块文档）。
fn spawn_tui(
    label: &str,
    script: Vec<ScriptStep>,
    args: &[&str],
    window: Option<(u16, u16)>,
) -> (Pty, TempDir, ScriptedModelHandle) {
    spawn_tui_with_env(label, script, args, window, &[])
}

/// [`spawn_tui`] 的环境覆盖变体：`env` 逐项盖掉夹具的默认取值（本套里给 `TERM=dumb` 用）。
///
/// 与 `pty.rs` 的 `spawn_repl`/`spawn_repl_with_env` 同一形状：主助手保持四个参数的签名，需要改
/// 环境的用例走这一层；`window` 为 `None` 时从端就是夹具默认的 200×50。
fn spawn_tui_with_env(
    label: &str,
    script: Vec<ScriptStep>,
    args: &[&str],
    window: Option<(u16, u16)>,
    env: &[(&str, &str)],
) -> (Pty, TempDir, ScriptedModelHandle) {
    let model = ScriptedModel::start(script);
    let dir = TempDir::new(label);
    let home = dir.join("home");
    let workspace = dir.join("workspace");
    // home 由 CLI 自己建（0700），工作区必须预先存在：CLI 启动时会校验它是目录
    fs::create_dir_all(&workspace).expect("工作区应可创建");

    let pty = Pty::spawn(&PtyOptions {
        home: &home,
        workspace: &workspace,
        base_url: &model.base_url(),
        args,
        // 默认环境（模型地址、密钥、NO_COLOR、TERM）由夹具注入
        env,
        window,
    });
    (pty, dir, model)
}

/// 等 `needle` 出现在 `marker` **最后一次出现之后**：`Pty::expect` 每次从流头重扫，用已经出现过的
/// 串当断言是同义反复；TUI 的每一帧都会重画同一块滚动区，取消后的空闲输入区必须只认结束行之后
/// 新写出的那一份。
///
/// 轮询口径与夹具的 `expect` 一致：20ms 复查一次、10s 超时并把现场贴出来。
fn expect_after(pty: &Pty, marker: &str, needle: &str) {
    let deadline = Instant::now() + PTY_TIMEOUT;
    loop {
        let text = pty.output();
        let found = text
            .rfind(marker)
            .is_some_and(|at| text[at + marker.len()..].contains(needle));
        if found {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "等待 {marker:?} 之后的 {needle:?} 超时（{}s）：\n{text}",
                PTY_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 等假模型收满 `count` 条请求并交回它们（有界轮询：挂死的会话在这里失败，而不是把用例拖住）。
///
/// 本套四条用例的判据全在终端流上（模型请求数不是判据），这一份与 `pty.rs` 同形的助手照约定
/// 留在这里，供后续用例直接用（`--tui` 与经典路径共用同一个会话面，请求数只在需要比对时才看）。
#[allow(dead_code)]
fn wait_for_requests(model: &ScriptedModelHandle, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + PTY_TIMEOUT;
    loop {
        let requests = model.requests();
        if requests.len() >= count {
            return requests;
        }
        if Instant::now() >= deadline {
            panic!(
                "等待第 {count} 条模型请求超时（{}s）：已收到 {:#?}",
                PTY_TIMEOUT.as_secs(),
                requests
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// `--tui`：进备用屏、流式答复、`/exit` 还原备用屏与 termios；开机品牌块进滚动区，经典状态块
/// （`● Ready`）**不打印**。
#[test]
fn pty_tui_enters_the_alt_screen_streams_an_answer_and_restores_on_exit() {
    let (mut pty, _dir, _model) = spawn_tui(
        "tui-alt-screen",
        vec![ScriptStep::Text("答复一".to_string())],
        &["--tui"],
        None,
    );

    // 进屏 + 第一帧（顶行 `IGNIS v{版本} … · kernel native …`）都写完了才打字：raw mode 在进屏
    // 之前就已经生效，此时回车才会被当 Enter 提交
    pty.expect("\x1b[?1049h");
    pty.expect("IGNIS v");
    pty.expect("kernel native");

    pty.send("你好\r");
    pty.expect("答复一"); // 流式答复落进滚动区（每 100ms 的 tick 排空捕获并重绘）
    pty.expect("● Done"); // 结束行

    pty.send("/exit\r");
    pty.expect("\x1b[?1049l");
    let code = pty.wait();
    assert_eq!(code, 0, "`/exit` 应以 0 退出：\n{}", pty.output());
    assert_eq!(
        pty.echo_and_canonical(),
        (true, true),
        "退出后终端必须回到行规范 + 回显：\n{}",
        pty.output()
    );

    let output = pty.output();
    assert!(
        output.contains('█') || output.contains('▀'),
        "开机品牌块（火焰字模）必须真的落进滚动区：\n{output}"
    );
    assert!(
        !output.contains("● Ready"),
        "经典状态块不进 TUI（那一行只属于经典 REPL）：\n{output}"
    );
}

/// 任务运行中的 Ctrl+C：任务收成 `● Aborted`、TUI **不退出**（重绘出空闲输入区），随后 `/exit`
/// 照常还原备用屏并以 0 退出。
#[test]
fn pty_tui_ctrl_c_cancels_a_running_task_without_leaving() {
    // `--yes`：预授权普通操作（`shell_run` 是 Elevated），工具因此直接开跑、不弹审批
    let (mut pty, _dir, _model) = spawn_tui(
        "tui-ctrl-c",
        vec![
            common::tool_call("shell_run", json!({"command": "sleep 5"})),
            ScriptStep::Text("答复二".to_string()),
        ],
        &["--tui", "--yes"],
        None,
    );

    pty.expect("\x1b[?1049h");
    pty.send("跑一个长命令\r");

    // 工具开始行 = 任务确实在跑（`sleep 5` 还早），此时 Ctrl+C 才是「打断运行中的任务」；
    // raw mode 保留了 ISIG，这个字节走的是真 SIGINT（在 `run_task_with_out` 自己的 watcher 里收）
    pty.expect("Running: sleep 5");
    pty.send("\x03");

    pty.expect("● Aborted");
    // 任务结束后 TUI 仍然活着：取消之后重绘的那一帧把空闲输入区（`❯ `）画了回来。每一帧都会
    // 重画同一块滚动区，`● Aborted` 在上一帧里就出现过——只认它**之后**新写出的 `❯ `
    expect_after(&pty, "● Aborted", "❯ ");

    pty.send("/exit\r");
    pty.expect("\x1b[?1049l");
    let code = pty.wait();
    assert_eq!(
        code,
        0,
        "取消不带走会话：`/exit` 仍以 0 退出：\n{}",
        pty.output()
    );

    // 品牌块在这条路径上同样只进滚动区：字模字形在，经典状态块不在
    let output = pty.output();
    assert!(
        output.contains('█') || output.contains('▀'),
        "开机品牌块（火焰字模）必须真的落进滚动区：\n{output}"
    );
    assert!(
        !output.contains("● Ready"),
        "经典状态块不进 TUI（那一行只属于经典 REPL）：\n{output}"
    );
}

/// `--tui --no-banner`：开机品牌块整块不推（滚动区里一个火焰字形都没有），但顶行的字标照旧
/// ——`--no-banner` 只管品牌块，顶行 `IGNIS v…` 是帧的 chrome，与品牌块无关。
#[test]
fn pty_tui_no_banner_drops_the_brand_block_but_keeps_the_top_line() {
    // 空脚本：这一例不发任何模型请求（`/exit` 即退）
    let (mut pty, _dir, _model) =
        spawn_tui("tui-no-banner", Vec::new(), &["--tui", "--no-banner"], None);

    // 进屏 + 顶行（字标 + 内核）都写完了，再等输入区的标记：此时滚动区与输入区都已落地，
    // 下面两条断言看的是同一份完整现场
    pty.expect("\x1b[?1049h");
    pty.expect("IGNIS v");
    pty.expect("kernel native");
    pty.expect("❯ ");

    let output = pty.output();
    for glyph in ['█', '▀', '▄'] {
        assert!(
            !output.contains(glyph),
            "`--no-banner` 下品牌块不该进滚动区（{glyph:?}）：\n{output}"
        );
    }
    assert!(
        output.contains("IGNIS v"),
        "顶行的字标文案仍在（收掉的只是品牌块）：\n{}",
        pty.output()
    );

    pty.send("/exit\r");
    pty.expect("\x1b[?1049l");
    let code = pty.wait();
    assert_eq!(code, 0, "`/exit` 应以 0 退出：\n{}", pty.output());
    assert!(
        !pty.output().contains("● Ready"),
        "经典状态块照旧不进 TUI：\n{}",
        pty.output()
    );
}

/// `TERM=dumb`：`--tui` 只多打一行 note，一个字节都不进备用屏；经典 REPL 照常跑完一轮、Ctrl+D
/// 以 0 退出。
#[test]
fn pty_tui_falls_back_to_the_classic_repl_on_a_dumb_terminal() {
    let (mut pty, _dir, _model) = spawn_tui_with_env(
        "tui-dumb",
        vec![ScriptStep::Text("答复三".to_string())],
        &["--tui"],
        None,
        &[("TERM", "dumb")],
    );

    // 降级只有这一行 note（逐字），随后走的就是经典 REPL 的原路径：banner 照打
    pty.expect("note: TUI unavailable (TERM is dumb); falling back to the classic REPL");
    pty.expect("● Ready");
    assert!(
        !pty.output().contains("\x1b[?1049h"),
        "降级时一个字节都不许进备用屏：\n{}",
        pty.output()
    );

    pty.send("你好\r");
    // 回退路径全程 canonical：终端自己回显输入行，答复与它落在同一条流里
    pty.expect("答复三");

    // Ctrl-D 必须落到一次真正的读上才是 EOF：回退路径的提示符由读循环在自己开读之前写出，
    // 等到它再发（任务运行期间终端是 canonical、没有读在等，发早了会被行规范吃掉）
    expect_after(&pty, "答复三", "❯ ");
    pty.send("\x04");
    let code = pty.wait();
    assert_eq!(code, 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());
}

/// 过小终端（10×4）：`--tui` 只多打一行 note（逐字带上真实尺寸），一个字节都不进备用屏；经典
/// REPL 照常起，Ctrl+D 以 0 退出。
#[test]
fn pty_tui_falls_back_when_the_terminal_is_too_small() {
    // 空脚本：这一例不该发任何模型请求（真发了只会拿到 500 并被记进 `requests()`）
    let (mut pty, _dir, _model) = spawn_tui("tui-too-small", Vec::new(), &["--tui"], Some((10, 4)));

    pty.expect(
        "note: TUI unavailable (terminal too small (10x4)); falling back to the classic REPL",
    );
    pty.expect("● Ready");
    assert!(
        !pty.output().contains("\x1b[?1049h"),
        "降级时一个字节都不许进备用屏：\n{}",
        pty.output()
    );

    // Ctrl-D 必须落到一次真正的读上才是 EOF：编辑器把提示符画在读之前、画的时候 raw mode 已经
    // 生效；这之前终端是 canonical（`choose_reader` 的探针还会进一次 raw 再还原），字节会被吃掉
    expect_after(&pty, "● Ready", "❯ ");
    pty.send("\x04");
    let code = pty.wait();
    assert_eq!(code, 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());
}
