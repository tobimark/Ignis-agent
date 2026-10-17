//! REPL 的 pty 冒烟用例：真二进制 + 真终端（[`common::pty`]）+ 脚本化假模型。
//!
//! 五条纪律：
//!
//! - **工具调用只出现在交互审批那一例**（[`pty_interactive_approval_prompt_works_under_the_editor`]）：
//!   pty 里 stdin 是 tty，带工具调用的脚本会走交互审批提问——别的用例一律纯文本步，免得把
//!   审批输出混进要断言的流里；
//! - **home/workspace 都建在用例自己的 `TempDir` 里**，绝不碰真实的 `~/.ignis`；
//! - **假模型句柄持有到用例结束**：`ScriptedModelHandle::drop` 停服务，`Pty::drop` 杀子进程
//!   （用例不主动退出 REPL 的那几条靠它收尾）；
//! - **打字前先等提示符**：raw mode 生效之前，`\r` 会被行规范译成 `\n`（Enter 变成「插换行」），
//!   而且输入行会由终端回显、与 `ignis` 自己的重绘叠在一起——这几条用例断言的是编辑器画出的
//!   那一份。raw 只在**读取区间**里生效（任务运行与审批期间终端回到 canonical + echo），
//!   所以任务结束后也要先等新的提示符帧（[`wait_for_prompt_after`]）再发按键：任务期间送进
//!   去的字节由行规范处理——未成行的输入要等换行、Ctrl-D 直接被吃成 EOF 不留给编辑器；
//! - **位置敏感的断言看「标记之后」**：`Pty::expect` 每次从流头重扫，拿已经出现过的串当断言会
//!   假命中（按 ↑ 召回的「❯ 第二次」在提交那一帧就出现过），所以那一类走 [`expect_after`]；
//!   补全的候选列表与 `/help` 的输出**逐字相同**，认不出标记，那一类走 [`expect_after_offset`]
//!   ——按发送前的流长切窗口。

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

/// 起一个 REPL 会话：临时 home/workspace + 假模型脚本，返回 `(pty, 临时目录, 假模型)`。
///
/// 返回值都要活到用例结束：`TempDir` 管目录、句柄管服务线程、`Pty` 管子进程。
fn spawn_repl(label: &str, script: Vec<ScriptStep>) -> (Pty, TempDir, ScriptedModelHandle) {
    spawn_repl_with_env(label, script, &[])
}

/// [`spawn_repl`] 的可覆盖环境变体：`env` 逐项盖掉夹具的默认取值（本套里给 `TERM=dumb` 用）。
fn spawn_repl_with_env(
    label: &str,
    script: Vec<ScriptStep>,
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
        // 无子命令 = 交互式 REPL
        args: &[],
        // 默认环境（模型地址、密钥、NO_COLOR、TERM）由夹具注入
        env,
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

/// 等 `needle` 出现在 `marker` **最后一次出现之后**：`Pty::expect` 每次从流头重扫，用已经出现过的串
/// （例如提交那一帧里的「❯ 第二次」）当断言是同义反复；这几条位置敏感的断言只认标记之后的新输出。
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

/// 等编辑器重新接管提示符：任务运行期间终端是 canonical（raw 只在读取区间里生效），这期间
/// 送进去的字节由行规范吃掉——没等到新提示符就发 Ctrl-D 会得到「什么都没发生」。
///
/// 提示符帧由编辑器在进入 raw mode **之后**画出，所以「`marker` 之后出现 `❯ `」= 下一次读取
/// 已经开始、raw 已重新生效。
fn wait_for_prompt_after(pty: &Pty, marker: &str) {
    expect_after(pty, marker, "❯ ");
}

/// `marker` 最后一次出现之后的输出（位置敏感断言的现场；`marker` 没出现就用例失败）。
fn output_after(pty: &Pty, marker: &str) -> String {
    let text = pty.output();
    match text.rfind(marker) {
        Some(at) => text[at + marker.len()..].to_string(),
        None => panic!("输出里没有 {marker:?}：\n{text}"),
    }
}

/// 等 `needle` 出现在 `offset` 之后**新写出的字节**里；`offset` 是发送前取的 `pty.output().len()`。
///
/// 为什么不能用 [`expect_after`]：Tab 的候选列表（`/` 的候选）与 `/help` 的输出**逐字相同**（同一份
/// `group_lines`），认不出任何一个只属于列表的标记——`/help` 那次里已经出现过 `/memory` 与
/// `INTELLIGENCE`，`❯ /` 那类输入行帧也早在打 `/he` 时就画过。于是只能按位置切窗口：窗口里的命中
/// 只可能来自发送那一刻之后写出的内容（pty 的字节按序到达，先写出的只可能落在 `offset` 左侧；
/// 调用点的 `/kernel [name]` 是 `/help` 的最后一行，它之后写出的只有那一行的尾巴与提示符帧，
/// 两者都不含待查的 needle，所以「晚到的旧输出落进窗口」这条假阳性通道也是关着的）。
fn expect_after_offset(pty: &Pty, offset: usize, needle: &str) {
    let deadline = Instant::now() + PTY_TIMEOUT;
    loop {
        let text = pty.output();
        // 快照记的是解码后的字节长：两次快照之间，尾巴上那半截多字节序列补齐后这段的长度会变，
        // 所以从 `offset` 起往后找一个字符边界再切片（越界就取空窗口）——只许往后挪，绝不回退。
        let start = (offset..=text.len())
            .find(|at| text.is_char_boundary(*at))
            .unwrap_or(text.len());
        if text[start..].contains(needle) {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "等待第 {offset} 字节之后的 {needle:?} 超时（{}s）：\n{text}",
                PTY_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 等假模型收满 `count` 条请求并交回它们（有界轮询：挂死的会话在这里失败，而不是把用例拖住）。
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

    wait_for_prompt_after(&pty, "Unknown command: /nope");
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

/// `Ctrl+J`（0x0A）在编辑器里是「插入换行」而不是提交：两行是同一次任务，正文里的换行原样交给模型。
#[test]
fn pty_ctrl_j_edits_a_multi_line_prompt_and_submits_once() {
    let (mut pty, _dir, model) =
        spawn_repl("pty-multi-line", vec![ScriptStep::Text("ok".to_string())]);

    pty.expect("❯ ");
    pty.send("第一行\x0a第二行\r");

    // 续行提示符：两行都画在屏幕上（首行 `❯ `、其余行 `… `）
    pty.expect("… 第二行");
    pty.expect("● Done");

    let requests = model.requests();
    assert_eq!(
        requests.len(),
        1,
        "Ctrl+J 只插换行、不提交：一轮只该发一次请求：{requests:#?}"
    );
    assert_eq!(
        last_user_message(&requests[0]),
        "第一行\n第二行",
        "整段两行文本原样交给内核"
    );

    wait_for_prompt_after(&pty, "● Done");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());
}

/// 长粘贴（bracketed paste）：整段内容一次进缓冲，区间内的换行一个都不提交；等回车才送模型。
///
/// 解码契约（设计 §6.4）：区间内 ESC/DEL 丢弃、Tab 与其余可见字节按字面保留——原文里的
/// `\x1b[2m` 以 `[2m` 形态出现在正文里，Tab 仍是 `\t`。
#[test]
fn pty_long_paste_does_not_submit_until_enter() {
    let (mut pty, _dir, model) = spawn_repl("pty-paste", vec![ScriptStep::Text("ok".to_string())]);

    // 三段各 ≥40 字符、按换行分隔；第二段嵌一个着色转义串，第三段含一个制表符
    let first = "第一段：把这三段文字当成一个任务一次粘进编辑器，验证长粘贴不会在中间提交，也不会提前送给模型\n";
    let second = "第二段：\x1b[2m这一段的原文里带着一个终端着色转义串，粘贴区间内裸 ESC 会被丢弃，可见的 [2m 会留下\n";
    let third =
        "第三段：制表符\t必须按字面留在粘贴内容里，否则粘进来的缩进会全部消失，Tab 与空格都要在";
    let pasted = format!("{first}{second}{third}");
    // 模型该收到的正文 = 发送的正文剔除 ESC 控制字节（标记本身不算内容，不在 pasted 里）
    let expected = pasted.replace('\x1b', "");

    pty.expect("❯ ");
    pty.send("\x1b[200~");
    pty.send(first);
    pty.send(second);
    pty.send(third);
    pty.send("\x1b[201~");

    // 最后一段的尾巴画到屏幕上 = 整段粘贴已经进缓冲；再等 300ms，这段时间一个请求都不该出现
    pty.expect("Tab 与空格都要在");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        model.requests().is_empty(),
        "粘贴区间里的换行绝不提交：{:#?}",
        model.requests()
    );

    pty.send("\r");
    pty.expect("● Done");

    let requests = model.requests();
    assert_eq!(requests.len(), 1, "整段粘贴只该提交一次：{requests:#?}");
    assert_eq!(
        last_user_message(&requests[0]),
        expected,
        "正文 = 粘贴内容剔除 ESC 控制字节后逐字相等"
    );

    wait_for_prompt_after(&pty, "● Done");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());
}

/// ↑ 召回上一条提交、↓ 把进历史前打的草稿原样还回来；Ctrl+U 清行后第三次提交照常送模型。
#[test]
fn pty_up_arrow_recalls_history_and_restores_the_draft() {
    let (mut pty, _dir, model) = spawn_repl(
        "pty-history",
        vec![
            ScriptStep::Text("a".to_string()),
            ScriptStep::Text("b".to_string()),
        ],
    );

    pty.expect("❯ ");
    pty.send("第一次\r");
    pty.expect("● Done");

    pty.send("第二次\r");
    wait_for_requests(&model, 2); // 第二次已经在模型侧留下请求

    pty.send("草稿");
    pty.expect("❯ 草稿");
    pty.send("\x1b[A");
    // 「❯ 第二次」在提交那一帧就出现过：这里只认草稿帧之后新画的召回帧
    expect_after(&pty, "❯ 草稿", "❯ 第二次");
    pty.send("\x1b[B");
    expect_after(&pty, "❯ 第二次", "❯ 草稿"); // ↓ 回到草稿

    pty.send("\x15"); // Ctrl+U 清掉草稿
    pty.send("第三次\r");

    // 脚本到此用尽：第三次请求会以 500 结束（重试退避合计约 4s）：断言的是这次提交发出去的正文
    let requests = wait_for_requests(&model, 3);
    assert_eq!(
        last_user_message(&requests[2]),
        "第三次",
        "Ctrl+U 之后提交的是新输入，不是召回的历史"
    );

    // 第三次任务还在跑（终端是 canonical），等它结束、编辑器重新接管提示符再退出
    wait_for_prompt_after(&pty, "第三次");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());
}

/// Tab 补全：唯一前缀接受整行（`/he` → `/help`），多候选列出候选、输入行不变；未知命令只在本地报错。
#[test]
fn pty_tab_completes_a_unique_prefix_and_lists_candidates() {
    // 一条脚本都不给：这一例里没有任何输入该走到模型（真发了请求只会拿到 500 并被记进 `requests()`）
    let (mut pty, _dir, model) = spawn_repl("pty-completion", Vec::new());

    pty.expect("❯ ");
    pty.send("/he\t");
    pty.expect("❯ /help"); // 唯一候选：整行被补全成 `/help`

    pty.send("\r");
    pty.expect("INTERFACE"); // `/help` 的分组标题
    pty.expect("/kernel [name]"); // `/help` 的命令行

    pty.send("/\t"); // 行首 `/` 有多个候选：列出行
    // 候选列表的内容与上面 `/help` 的输出逐字相同，`Pty::expect` 从流头重扫就会被那一份满足
    // （列表不渲染也绿）：先记下流长，只在 `/\t` 之后新写出的字节里找（见 [`expect_after_offset`]）。
    // `/kernel [name]` 是 `/help` 的最后一行，它之后 CLI 写过的只有那一行的尾巴与提示符帧，
    // 两者都不含这两个 needle——晚到的旧输出替不了列表。
    let offset = pty.output().len();
    expect_after_offset(&pty, offset, "/memory");
    expect_after_offset(&pty, offset, "INTELLIGENCE");

    pty.send("\x15"); // Ctrl+U 清掉 `/`
    pty.send("/nope\r");
    pty.expect("Unknown command: /nope");

    assert!(
        model.requests().is_empty(),
        "补全与未知命令都只在本地处理：{:#?}",
        model.requests()
    );

    wait_for_prompt_after(&pty, "Unknown command: /nope");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());
}

/// `/details minimal` 换掉本轮结束行的信息量（`● Done · {耗时}`，没有 `· task …`），
/// `/model` 的会话级覆盖落到下一轮请求体里。
#[test]
fn pty_details_and_model_change_the_next_turn() {
    let (mut pty, _dir, model) = spawn_repl(
        "pty-details-model",
        vec![
            ScriptStep::Text("一".to_string()),
            ScriptStep::Text("二".to_string()),
        ],
    );

    pty.expect("❯ ");
    pty.send("/details minimal\r");
    pty.expect("output mode: minimal");

    pty.send("说一句\r");
    pty.expect("● Done · ");
    // 下一轮提示符出现 = 结束行已经写完：这时才能断言结束行里没有任务 id
    expect_after(&pty, "● Done · ", "❯ ");
    let tail = output_after(&pty, "● Done · ");
    assert!(
        !tail.contains(" · task "),
        "minimal 的结束行不该带任务 id：\n{tail}"
    );

    pty.send("/model test-model-2\r");
    pty.expect("model: test-model-2");

    pty.send("第二句\r");
    let requests = wait_for_requests(&model, 2);
    assert_eq!(requests.len(), 2, "两轮各一次请求：{requests:#?}");
    assert_eq!(
        requests[1]["model"], "test-model-2",
        "会话级模型覆盖要落到下一轮请求体里"
    );

    // 第二轮还在跑（终端是 canonical），等编辑器重新接管提示符再退出
    wait_for_prompt_after(&pty, "第二句");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());
}

/// `TERM=dumb`：编辑器不接管终端，回退 v0.1 的按行输入并说明一句；任务照常跑、Ctrl-D 照常退出。
#[test]
fn pty_dumb_terminal_falls_back_to_line_mode() {
    let (mut pty, _dir, _model) = spawn_repl_with_env(
        "pty-dumb",
        vec![ScriptStep::Text("答复".to_string())],
        &[("TERM", "dumb")],
    );

    pty.expect("note: line editing unavailable");

    pty.send("你好\r");
    // 行规范下由终端回显输入，模型的答复走同一条从端：两个都在这一条流里
    pty.expect("答复");

    // 任务可能还没结束（回退路径全程 canonical），等提示符回来再退出
    wait_for_prompt_after(&pty, "答复");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());
}

/// 交互审批必须在编辑器下可用（终审 Critical 1 的回归门）：编辑器的 raw mode 只在「读取区间」
/// 里生效，任务执行（含审批提问）期间终端回到 canonical + echo，审批的按行读等得到回车。
#[test]
fn pty_interactive_approval_prompt_works_under_the_editor() {
    let (mut pty, dir, model) = spawn_repl(
        "pty-approval",
        vec![
            common::tool_call(
                "fs_write",
                json!({"path":"approval-test.txt","content":"ok"}),
            ),
            ScriptStep::Text("ok".to_string()),
        ],
    );

    pty.expect("❯ ");
    pty.send("写一个文件\r");

    // `fs_write` 是 Ask：交互审批提问 + 读一行；raw mode 若跨读取存活，这一行永远等不到
    pty.expect("Approval required");
    pty.send("a\r");
    pty.expect("● Done");

    let written = dir.join("workspace").join("approval-test.txt");
    assert!(
        written.exists(),
        "审批通过后文件必须写进工作区：{}",
        pty.output()
    );
    assert_eq!(
        model.requests().len(),
        2,
        "工具调用一轮 + 结果回灌一轮：{:#?}",
        model.requests()
    );

    // 任务结束后编辑器重新接管提示符，Ctrl-D 干净退出
    wait_for_prompt_after(&pty, "● Done");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());
}
