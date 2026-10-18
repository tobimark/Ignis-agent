//! REPL 的 pty 冒烟用例：真二进制 + 真终端（[`common::pty`]）+ 脚本化假模型。
//!
//! 五条纪律：
//!
//! - **工具调用只出现在交互审批那一例**（[`pty_interactive_approval_prompt_works_under_the_editor`]）：
//!   pty 里 stdin 是 tty，带工具调用的脚本会走交互审批提问——别的用例一律纯文本步，免得把
//!   审批输出混进要断言的流里；
//! - **home/workspace 都建在用例自己的 `TempDir` 里**，绝不碰真实的 `~/.ignis`；两条要跨进程 /
//!   跨轮次看库的用例（`-c` 恢复、`/compact`）经 [`spawn_repl_in`] 在同一份目录上再起进程，并
//!   直读同一份 `home` 下的 `ignis.db` 钉住库里的现场（[`common::read_session_messages`] 一组）；
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
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::TempDir;
use common::pty::{PTY_TIMEOUT, Pty, PtyOptions};
use common::{read_session_compactions, read_session_messages, read_sessions};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel, ScriptedModelHandle};
use serde_json::{Value, json};

/// 位置敏感断言与请求计数的复查间隔（与夹具的 `expect` 同一条 20ms 轮询口径）。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// `/learn` 三条用例共用的固定草稿（模型对草稿请求的回复）：合法 frontmatter + 一段正文。
const DRAFT_MD: &str = "---\nname: demo-skill\ndescription: Demo draft.\n---\n# 步骤\n1. 做事\n";

/// `/learn` 的确认提示词（逐字，与生产侧的 `commands::learn::LEARN_PROMPT` 同一份文案）。
///
/// 这一行读完之前终端是 canonical（与审批同一时机）：答案要在提示词**出现之后**再发——多次
/// `/learn` 的提示词逐字相同，`Pty::expect` 会拿上一轮那一份假命中，所以按位置切窗口（见各
/// 用例里的 `expect_after`）。
const LEARN_PROMPT: &str = "save this skill draft? [s]ave [d]iscard> ";

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
    let pty = spawn_repl_in_with_env(&dir, &model, &[], env);
    (pty, dir, model)
}

/// 在**既有**目录上起一个 REPL：home/workspace 与假模型句柄都由调用方给，`args` 原样交给 CLI
/// （传 `&["-c"]` 就是恢复路径）。
///
/// 拆出这一层是为了让「同一份 home/workspace 跑两个进程」的用例（`-c` 恢复：第一个进程跑一轮退出、
/// 第二个进程接着上一段会话）复用同一份装配；[`spawn_repl`] 与 [`spawn_repl_with_env`] 走的也是
/// 它，既有用例的装配行为逐字不变。
fn spawn_repl_in(dir: &TempDir, model: &ScriptedModelHandle, args: &[&str]) -> Pty {
    spawn_repl_in_with_env(dir, model, args, &[])
}

/// [`spawn_repl_in`] 的环境覆盖变体：`env` 逐项盖掉夹具的默认取值。
fn spawn_repl_in_with_env(
    dir: &TempDir,
    model: &ScriptedModelHandle,
    args: &[&str],
    env: &[(&str, &str)],
) -> Pty {
    let home = dir.join("home");
    let workspace = dir.join("workspace");
    // home 由 CLI 自己建（0700），工作区必须预先存在：CLI 启动时会校验它是目录
    fs::create_dir_all(&workspace).expect("工作区应可创建");

    Pty::spawn(&PtyOptions {
        home: &home,
        workspace: &workspace,
        base_url: &model.base_url(),
        args,
        // 默认环境（模型地址、密钥、NO_COLOR、TERM）由夹具注入
        env,
        // 窗口尺寸走夹具的默认 200×50（本套没有需要改尺寸的用例）
        window: None,
    })
}

/// 技能文件的落点：`{IGNIS_HOME}/skills/{name}/SKILL.md`（`spawn_repl*` 建出来的 home 就是
/// `dir.join("home")`）。
fn skill_path(dir: &TempDir, name: &str) -> PathBuf {
    dir.join("home").join("skills").join(name).join("SKILL.md")
}

/// 在 REPL 运行期间往 home 的技能目录里落一个合法技能（内容与内置技能同一形状：frontmatter +
/// 一段正文），供 `/skills reload` 与直读断言使用。
///
/// 目录按需创建：`/learn` 用例里 home 已由 CLI 启动时建好，但这里不依赖那个时序。
fn write_skill(dir: &TempDir, name: &str, description: &str) {
    let path = skill_path(dir, name);
    fs::create_dir_all(path.parent().expect("技能文件应有父目录")).expect("技能目录应可创建");
    fs::write(
        &path,
        format!("---\nname: {name}\ndescription: {description}\n---\n# 步骤\n1. 做事\n"),
    )
    .expect("技能文件应可写入");
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

/// 请求体里去掉 `role == "system"` 之后的消息数组（保留 JSON 形态，便于与 `json!` 整段比对）。
///
/// 恢复与压缩两条用例的判据都是「内核看到的消息序列」：system（本机提示词）逐轮重算，与这两条
/// 断言的契约无关，先从序列里摘掉。
fn messages_without_system(request: &Value) -> Value {
    Value::Array(
        request["messages"]
            .as_array()
            .expect("请求体里的 messages 应为数组")
            .iter()
            .filter(|message| message["role"] != "system")
            .cloned()
            .collect(),
    )
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

/// 空闲 Ctrl+C 连开 10 次：每次都以 0 结束（不是被默认处置杀掉的信号退出），且退出流里在
/// 「发 `\x03` 之前的偏移之后」出现 bracketed paste 关。
///
/// 两条观察各由这里的一段钉住：①第一段 watcher 被首次轮询之前到达的 SIGINT 走默认处置——
/// 安装必须先于任何可交互时机（提示符出现即意味着安装已完成）；②退出时读取线程仍握着守卫，
/// `Drop` 的关闭序列永远写不出——退出路径要自己补那 6 个字节。
#[test]
fn pty_idle_ctrl_c_exits_cleanly_ten_times() {
    for round in 1..=10 {
        let (mut pty, _dir, _model) = spawn_repl("pty-idle-ctrl-c", Vec::new());

        // 提示符 = 编辑器已接管终端，raw 保留了 ISIG，于是 0x03 是 SIGINT 而不是普通字节
        pty.expect("❯ ");
        // 偏移取在发送**之前**（见 [`expect_after_offset`]）：窗口里的命中只可能来自之后写出的字节
        let offset = pty.output().len();
        pty.send("\x03");

        let code = pty.wait();
        assert_eq!(
            code,
            0,
            "第 {round} 次：空闲 Ctrl-C 应干净退出（不是信号退出）：\n{}",
            pty.output()
        );
        expect_after_offset(&pty, offset, "\x1b[?2004l");
    }
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

    // 候选列表的内容与上面 `/help` 的输出逐字相同，`Pty::expect` 从流头重扫就会被那一份满足
    // （列表不渲染也绿）：**发送前**记下流长，只在 `/\t` 之后新写出的字节里找（见
    // [`expect_after_offset`]；偏移取在发送之前，发送到快照之间写出的候选不会被漏掉）。
    // `/kernel [name]` 是 `/help` 的最后一行，它之后 CLI 写过的只有那一行的尾巴与提示符帧，
    // 两者都不含这两个 needle——晚到的旧输出替不了列表。
    let offset = pty.output().len();
    pty.send("/\t"); // 行首 `/` 有多个候选：列出行
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

/// `-c` 恢复：第一个进程跑完一轮就退出，第二个进程用 `-c` 起在**同一份 home/workspace** 上；
/// 后续一轮送进模型的消息必须带着上一轮的 user/assistant 两条（恢复判据：模型真的看到了它们）。
///
/// 两个进程共用一个假模型句柄（脚本按请求顺序消耗）：第一轮答复「答复一」、第二轮「答复二」。
#[test]
fn pty_continue_restores_the_previous_turn_for_the_model() {
    let model = ScriptedModel::start(vec![
        ScriptStep::Text("答复一".to_string()),
        ScriptStep::Text("答复二".to_string()),
    ]);
    let dir = TempDir::new("pty-continue");

    // 第一个进程：一轮任务（会话行与 user/assistant 两条消息落库）后 Ctrl-D 干净退出
    let mut first = spawn_repl_in(&dir, &model, &[]);
    first.expect("❯ ");
    first.send("第一句\r");
    first.expect("答复一");
    wait_for_prompt_after(&first, "● Done");
    first.send("\x04");
    assert_eq!(first.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", first.output());
    drop(first); // 已收尸：关掉这个 pty，下面在同一份目录上另起一个进程

    // 第二个进程：`-c` 命中本工作区最近一次会话，banner 第 5 行是 `Session: {短 id}`；未命中才会
    // 打「本工作区没有上一段会话」的 note（note 写在 banner 之前，所以看到 banner 就能断言它没出现）
    let mut second = spawn_repl_in(&dir, &model, &["-c"]);
    second.expect("Session: ");
    let banner = second.output();
    assert!(
        !banner.contains("note: no previous session"),
        "-c 未命中上一轮的会话（只有未命中才会打 note）：\n{banner}"
    );

    second.expect("❯ ");
    second.send("第二句\r");
    second.expect("答复二");

    let requests = wait_for_requests(&model, 2);
    assert_eq!(requests.len(), 2, "两个进程各跑一轮：{requests:#?}");
    // 第一个进程是全新会话：请求里只有本轮输入（「恢复」的证据不能来自「第一轮就带历史」）
    assert_eq!(
        messages_without_system(&requests[0]),
        json!([{"role": "user", "content": "第一句"}]),
        "第一个进程不该带任何历史：{:#?}",
        requests[0]["messages"]
    );
    // 恢复判据：去掉 system 之后逐条是「上一轮 user/assistant + 本轮 user」
    assert_eq!(
        messages_without_system(&requests[1]),
        json!([
            {"role": "user", "content": "第一句"},
            {"role": "assistant", "content": "答复一"},
            {"role": "user", "content": "第二句"},
        ]),
        "恢复后模型必须看到上一轮的两条消息：{:#?}",
        requests[1]["messages"]
    );

    wait_for_prompt_after(&second, "答复二");
    second.send("\x04");
    assert_eq!(
        second.wait(),
        0,
        "Ctrl-D 应以 0 退出：\n{}",
        second.output()
    );
}

/// `/compact`：三轮任务后压缩较早轮次——库里原始消息一条不少、压缩记录恰好一行，第四轮送进
/// 模型的是「摘要消息 + 保留的两轮 + 本轮输入」。
///
/// 脚本按顺序消耗：三轮任务的答复、摘要请求的正文（第 4 步）、第四轮的答复。
#[test]
fn pty_compact_summarizes_and_keeps_the_raw_rows() {
    let model = ScriptedModel::start(vec![
        ScriptStep::Text("一".to_string()),
        ScriptStep::Text("二".to_string()),
        ScriptStep::Text("三".to_string()),
        ScriptStep::Text("早前对话摘要".to_string()),
        ScriptStep::Text("答复四".to_string()),
    ]);
    let dir = TempDir::new("pty-compact");
    let mut pty = spawn_repl_in(&dir, &model, &[]);

    // 三轮任务：答复正文（「一」「二」「三」）是下一句提示词的子串，不能拿来当位置标记；结束行
    // 「● Done」在上一轮就出现过。两处断言都只认**本轮输入行之后**新写出的输出。
    pty.expect("❯ ");
    for (turn, prompt) in [(1_usize, "第一句"), (2, "第二句"), (3, "第三句")] {
        pty.send(&format!("{prompt}\r"));
        expect_after(&pty, prompt, "● Done");
        // 请求在答复之前就已送达模型（假模型先记账再响应），等编辑器重新接管提示符时必然已入账
        wait_for_prompt_after(&pty, "● Done");
        assert_eq!(
            model.requests().len(),
            turn,
            "第 {turn} 轮应已送出一份请求：{:#?}",
            model.requests()
        );
    }

    // `/compact`：第 4 步脚本被摘要请求消耗；回显行钉住「进入摘要段 2 条」与摘要的**字符**数（6）
    pty.send("/compact\r");
    pty.expect("compacted: 2 messages → summary (6 chars)");
    // 压缩期间终端是 canonical：等编辑器重新接管提示符（回显行之后）才能发第四句
    wait_for_prompt_after(&pty, "compacted: 2 messages");

    // 直读库（只读用例自己 TempDir 下的 home）：压缩只落一行记录，原始消息行一条不少、不被改写
    let db_path = dir.join("home").join("ignis.db");
    let sessions = read_sessions(&db_path, &dir.join("workspace"));
    assert_eq!(sessions.len(), 1, "本工作区应只有一条会话：{sessions:#?}");
    assert_eq!(
        sessions[0].1, "第一句",
        "标题应是首条用户消息（会话行确实来自这一段对话）：{sessions:#?}"
    );
    let session_id = &sessions[0].0;
    let mut expected_rows: Vec<(i64, String, String)> = vec![
        (1, "user".to_string(), "第一句".to_string()),
        (2, "assistant".to_string(), "一".to_string()),
        (3, "user".to_string(), "第二句".to_string()),
        (4, "assistant".to_string(), "二".to_string()),
        (5, "user".to_string(), "第三句".to_string()),
        (6, "assistant".to_string(), "三".to_string()),
    ];
    assert_eq!(
        read_session_messages(&db_path, session_id),
        expected_rows,
        "压缩后原始消息仍该是 6 行、逐字未被改写"
    );
    let compactions = read_session_compactions(&db_path, session_id);
    assert_eq!(compactions.len(), 1, "压缩记录恰好一行：{compactions:#?}");
    assert_eq!(
        compactions[0].1, 2,
        "upto_seq 应是第一轮 assistant 的 seq：{compactions:#?}"
    );
    assert_eq!(
        compactions[0].2, "早前对话摘要",
        "行里存的摘要正文应是模型给出的那一份：{compactions:#?}"
    );

    // 第四轮：模型看到的是「摘要消息（前缀 + 摘要正文）+ 保留的第二/第三轮四条 + 本轮输入」
    pty.send("第四句\r");
    let requests = wait_for_requests(&model, 5);
    assert_eq!(requests.len(), 5, "三轮 + 摘要 + 第四轮：{requests:#?}");
    assert_eq!(
        messages_without_system(&requests[4]),
        json!([
            {"role": "user", "content": "[Earlier conversation summary]\n\n早前对话摘要"},
            {"role": "user", "content": "第二句"},
            {"role": "assistant", "content": "二"},
            {"role": "user", "content": "第三句"},
            {"role": "assistant", "content": "三"},
            {"role": "user", "content": "第四句"},
        ]),
        "压缩后的请求应是摘要 + 保留的两轮 + 本轮输入：{:#?}",
        requests[4]["messages"]
    );

    pty.expect("答复四");
    wait_for_prompt_after(&pty, "答复四");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());

    // 第四轮也只往原始表追加（seq 7/8）：被压缩的前缀一条不少地留在库里
    expected_rows.push((7, "user".to_string(), "第四句".to_string()));
    expected_rows.push((8, "assistant".to_string(), "答复四".to_string()));
    assert_eq!(
        read_session_messages(&db_path, session_id),
        expected_rows,
        "第四轮之后原始消息只增不减、seq 续写"
    );
}

/// `/learn` 保存路径的端到端：一轮任务 → `/learn` 预览（头行逐字）→ 确认提示词 → `s` →
/// 落盘回显 + reload 回显 → `/skills` 的表里看得见新技能；文件直读（内容前缀 + 0600）。
///
/// 确认读的是一行 **canonical** 模式的 stdin（与审批同一时机），所以 `s\r` 必须等提示词出现
/// 之后再发；`demo-skill` 在草稿头行里早就出现过，表里那一行只认 reload 回显之后的窗口。
#[test]
fn pty_learn_saves_the_draft_after_confirmation() {
    let (mut pty, dir, _model) = spawn_repl(
        "pty-learn-save",
        vec![
            ScriptStep::Text("答复一".to_string()),
            ScriptStep::Text(DRAFT_MD.to_string()),
        ],
    );

    pty.expect("❯ ");
    pty.send("你好\r");
    pty.expect("● Done");
    wait_for_prompt_after(&pty, "● Done");

    pty.send("/learn\r");
    pty.expect("draft skill demo-skill: untested, not deployed or executed automatically");
    pty.expect(LEARN_PROMPT);
    pty.send("s\r");
    pty.expect("saved skill demo-skill");
    pty.expect("reloaded skills (4)");

    // 落盘之后编辑器重新接管提示符：这时发的回车才被当 Enter（canonical 下会被译成 Ctrl+J）
    wait_for_prompt_after(&pty, "reloaded skills (4)");
    pty.send("/skills\r");
    // 草稿头行里已经有 `demo-skill` 了：这一行必须来自 reload 之后的表
    expect_after(&pty, "reloaded skills (4)", "demo-skill");

    wait_for_prompt_after(&pty, "demo-skill");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());

    // 直读落盘现场：frontmatter 逐字开头、文件 0600（与 `save_skill` 的契约一致）
    let path = skill_path(&dir, "demo-skill");
    let text = fs::read_to_string(&path).expect("确认后草稿应已落盘");
    assert!(text.starts_with("---\nname: demo-skill\n"), "{text}");
    let mode = fs::metadata(&path)
        .expect("技能文件应可 stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "{}", path.display());
}

/// `/learn` 的拒绝面：`d` 放弃（磁盘零变化）→ 显式任务前缀再存一次落盘 → 同名再存被拒
/// （`error: skill demo-skill already exists`）且文件字节一个不改。
///
/// 脚本 4 步：1 轮任务 + 3 次 `/learn`，每次都要一次模型往返（草稿在确认之前生成）。
#[test]
fn pty_learn_discard_and_refusal_leave_no_file() {
    let (mut pty, dir, _model) = spawn_repl(
        "pty-learn-refusal",
        vec![
            ScriptStep::Text("答复一".to_string()),
            ScriptStep::Text(DRAFT_MD.to_string()),
            ScriptStep::Text(DRAFT_MD.to_string()),
            ScriptStep::Text(DRAFT_MD.to_string()),
        ],
    );

    pty.expect("❯ ");
    pty.send("你好\r");
    pty.expect("● Done");
    wait_for_prompt_after(&pty, "● Done");

    // 第一次：`d` 放弃。提示词与头行都是流里第一次出现，`expect` 的从流头重扫不会假命中
    pty.send("/learn\r");
    pty.expect("draft skill demo-skill: untested, not deployed or executed automatically");
    pty.expect(LEARN_PROMPT);
    pty.send("d\r");
    pty.expect("discarded");
    assert!(
        !skill_path(&dir, "demo-skill").exists(),
        "放弃不该落盘：\n{}",
        pty.output()
    );

    // 第二次：显式前缀（8 字符 = 前缀规则的下界）取回刚才那条成功任务
    wait_for_prompt_after(&pty, "discarded");
    let home = dir.join("home");
    let (id, status, error) = common::latest_task(&home.join("ignis.db"));
    assert_eq!(
        status,
        "success",
        "默认目标与显式前缀都只认成功任务（error={error:?}）：\n{}",
        pty.output()
    );
    let prefix = &id[..8];
    pty.send(&format!("/learn {prefix}\r"));
    // 提示词与上一轮逐字相同：只认本轮输入行回显之后的那一份
    expect_after(&pty, &format!("❯ /learn {prefix}"), LEARN_PROMPT);
    pty.send("s\r");
    pty.expect("saved skill demo-skill");
    pty.expect("reloaded skills (4)");

    let path = skill_path(&dir, "demo-skill");
    let saved = fs::read(&path).expect("第二次应已落盘");
    assert!(saved.starts_with(b"---\nname: demo-skill\n"), "{saved:?}");

    // 第三次：同一前缀、同一名字 → 拒绝；文件字节逐字不变。
    //
    // 这一轮的门禁不能再用输入行回显当标记：第三轮的回显与第二轮**逐字相同**，`expect_after` 的
    // `rfind` 会先命中第二轮那一份（新回显还没画出来），于是 `s\r` 在本轮输入行提交之前就发出去了
    // ——编辑器还在 raw mode 里读，这两个字节会被它收进「跨提交边界的按键」队列，确认面永远等不
    // 到一行（实测超时）。改用只有第二轮才写过的那行落盘回显当标记：本轮提示词只可能在它之后。
    wait_for_prompt_after(&pty, "reloaded skills (4)");
    pty.send(&format!("/learn {prefix}\r"));
    expect_after(&pty, "reloaded skills (4)", LEARN_PROMPT);
    pty.send("s\r");
    pty.expect("error: skill demo-skill already exists");
    assert_eq!(
        fs::read(&path).expect("被拒之后文件仍该在"),
        saved,
        "被拒时一个字节都不许改：\n{}",
        pty.output()
    );

    wait_for_prompt_after(&pty, "already exists");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());
}

/// `/skills reload` 的端到端：REPL 运行期间由测试直接往 home 里落一个新技能，reload 之后
/// `/skills` 的表里就有它；起步时的 banner 计数（3）钉住「新技能确实是运行期间才出现的」。
#[test]
fn pty_skills_reload_picks_up_a_new_skill_file() {
    // 本轮不提交任务：脚本那一步只是给夹具一个 `base_url`（reload 是纯本地命令，不该碰模型）
    let (mut pty, dir, model) = spawn_repl(
        "pty-skills-reload",
        vec![ScriptStep::Text("答复一".to_string())],
    );

    pty.expect("Session: "); // banner 第 5 行：整屏已写完，此时目录里只有内置三技能
    let banner = pty.output();
    assert!(banner.contains("Skills: 3"), "{banner}");

    pty.expect("❯ "); // raw mode 生效后再打字：canonical 下回车会被译成 Ctrl+J
    write_skill(&dir, "fresh-skill", "Added while running.");

    pty.send("/skills reload\r");
    pty.expect("reloaded skills (4)");
    wait_for_prompt_after(&pty, "reloaded skills (4)");

    pty.send("/skills\r");
    expect_after(&pty, "reloaded skills (4)", "fresh-skill");

    wait_for_prompt_after(&pty, "fresh-skill");
    pty.send("\x04");
    assert_eq!(pty.wait(), 0, "Ctrl-D 应以 0 退出：\n{}", pty.output());

    assert!(
        model.requests().is_empty(),
        "reload 与列表都是本地动作：{:#?}",
        model.requests()
    );
}
