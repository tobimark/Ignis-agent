//! REPL 的 pty 冒烟用例：真二进制 + 真终端（[`common::pty`]）+ 脚本化假模型。
//!
//! 四条纪律：
//!
//! - **脚本一律纯文本步**：pty 里 stdin 是 tty，带上工具调用就会把审批变成交互提问；
//! - **home/workspace 都建在用例自己的 `TempDir` 里**，绝不碰真实的 `~/.ignis`；
//! - **假模型句柄持有到用例结束**：`ScriptedModelHandle::drop` 停服务，`Pty::drop` 杀子进程
//!   （用例不主动退出 REPL 的那几条靠它收尾）；
//! - **打字前先等提示符**：raw mode 生效之前，`\r` 会被行规范译成 `\n`（Enter 变成「插换行」），
//!   而且输入行会由终端回显、与 `ignis` 自己的重绘叠在一起——这几条用例断言的是编辑器画出的
//!   那一份。

#![cfg(unix)]

mod common;

use std::fs;

use common::TempDir;
use common::pty::{Pty, PtyOptions};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel, ScriptedModelHandle};
use serde_json::Value;

/// 起一个 REPL 会话：临时 home/workspace + 假模型脚本，返回 `(pty, 临时目录, 假模型)`。
///
/// 返回值都要活到用例结束：`TempDir` 管目录、句柄管服务线程、`Pty` 管子进程。
fn spawn_repl(label: &str, script: Vec<ScriptStep>) -> (Pty, TempDir, ScriptedModelHandle) {
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
        // 无子命令 = 交互式 REPL
        args: &[],
        // 默认环境（模型地址、密钥、NO_COLOR、TERM）由夹具注入
        env: &[],
    });

    (pty, dir, model)
}

/// 请求体里最后一条 user 消息的正文（REPL 把整段输入行原样交给内核）。
fn last_user_message(request: &Value) -> &str {
    let messages = request["messages"]
        .as_array()
        .expect("请求体里的 messages 应为数组");
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .expect("请求里应有一条带正文的 user 消息")
}

/// 中文提示词提交一次：输入行原样留在屏幕上、答复与结束行都渲染到同一个终端，
/// 假模型恰好收到一次请求、正文就是那行输入。
#[test]
fn pty_submits_a_chinese_prompt_and_renders_the_answer() {
    let (mut pty, _dir, model) = spawn_repl(
        "pty-submit",
        vec![ScriptStep::Text("答复四十二".to_string())],
    );

    // 提示符出现 = 编辑器已接管终端（raw mode 生效），此时回车才会被当 Enter 提交
    pty.expect("❯ ");
    pty.send("你好\r");

    pty.expect("❯ 你好"); // 宽字符按显示宽度画回来，文本本身原样
    pty.expect("答复四十二"); // 模型答复
    pty.expect("● Done"); // 结束行

    let requests = model.requests();
    assert_eq!(requests.len(), 1, "一轮交互只该发一次请求：{requests:#?}");
    assert_eq!(last_user_message(&requests[0]), "你好");
}

/// 未知斜杠命令只在本地报错、**绝不送模型**：pty 里敲它然后 Ctrl-D 退出。
#[test]
fn pty_unknown_command_is_rejected_locally_and_never_reaches_the_model() {
    // 脚本一条不给：真发了请求只会拿到 500，同时被记进 `requests()`
    let (mut pty, _dir, model) = spawn_repl("pty-unknown-command", Vec::new());

    pty.expect("❯ ");
    pty.send("/nope\r");
    pty.expect("Unknown command: /nope — /help lists commands");

    pty.send("\x04"); // 空缓冲的 Ctrl-D = EOF，干净退出
    let code = pty.wait();
    assert_eq!(code, 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());

    assert!(
        model.requests().is_empty(),
        "未知命令绝不送模型：{:#?}",
        model.requests()
    );
}

/// 空闲 Ctrl-C：SIGINT 退出 REPL，且退出时把终端从 raw mode 还原（`ECHO`/`ICANON` 都回来）。
///
/// 后半条是本期最危险的回归——没还原的话用户的 shell 会没回显。
#[test]
fn pty_ctrl_c_at_the_prompt_exits_and_restores_the_terminal() {
    let (mut pty, _dir, _model) = spawn_repl("pty-ctrl-c", Vec::new());

    // 提示符出现 = 编辑器已经接管终端，而 raw mode 刻意保留 ISIG：0x03 仍是 SIGINT。
    // 先在 raw 期间读一次从端：`(false, false)` 说明父进程这份 fd 看到的确实是子进程的终端，
    // 否则下面那条 `(true, true)` 只是「pty 初值恰好如此」的同义反复。
    pty.expect("❯ ");
    assert_eq!(
        pty.echo_and_canonical(),
        (false, false),
        "提示符出来时 raw mode 应已生效：\n{}",
        pty.output()
    );
    pty.send("\x03");

    let code = pty.wait();
    assert_eq!(code, 0, "空闲 Ctrl-C 应干净退出：\n{}", pty.output());
    assert_eq!(
        pty.echo_and_canonical(),
        (true, true),
        "退出后终端必须回到行规范 + 回显：\n{}",
        pty.output()
    );
}

/// 空输入上的 Ctrl-D：与 v0.1 一致，以 0 退出。
#[test]
fn pty_ctrl_d_on_empty_input_exits_with_zero() {
    let (mut pty, _dir, _model) = spawn_repl("pty-ctrl-d", Vec::new());

    pty.expect("❯ ");
    pty.send("\x04");

    let code = pty.wait();
    assert_eq!(code, 0, "空输入上的 Ctrl-D 应以 0 退出：\n{}", pty.output());
}
