//! 会话十次启停恢复（设计 §6.6 的验收判据；P6 Task 6 的新指标「会话启停 10 次」）。
//!
//! 一条契约用例：**同一个临时 `IGNIS_HOME` 与同一份工作区上连起十次 REPL**——第 1 次不带 `-c`
//! （新会话，落第一行会话与第一轮消息），第 2–10 次都以 `ignis -c` 恢复同一条会话；每一轮
//! 「起 REPL → 提交第 i 句 → 断言脚本第 i 步的答复 → `/exit` → 退出码 0」。
//!
//! 判据不用近似：
//!
//! - **送进内核的请求体 vs 库行**：第 i 轮的请求（去掉 system）必须逐条等于「前 i−1 轮的库行 +
//!   本轮输入」，而每轮退出后又直读 `session_messages` 断言逐条等于十轮的全量 canonical 序列
//!   （角色、内容、顺序）。两侧对照，恢复出来的有效历史就是库里那一份；
//! - **会话不换**：banner 的 `Session:` 行第 1 轮是 `new`，第 2 轮起是 13 字符短 id 且十轮同一个
//!   （`-c` 恢复的是同一条会话），库里的 `sessions` 恒为一行、id 前缀与它一致；
//! - **不重启模型**：一个 [`ScriptedModelHandle`] 贯穿十轮，脚本十步按请求顺序消耗——多发的请求
//!   会让后续轮次的答复错位，因此「答复 = 脚本第 i 步」本身就是请求序列的判别式；
//! - **落库齐全**：十轮结束后 `session_messages` 恰好 `2 × 10` 条，`tasks.session_id` 全部指向
//!   同一条会话（十轮 = 十条任务行）。
//!
//! 夹具沿用 `common` 的 [`TempDir`] 与 [`common::pty`]（真 pty + 真终端宽度），模型用
//! `ignis_runtime::test_support` 的脚本化假模型——不碰网络、不碰真实的 `~/.ignis`。

#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use common::TempDir;
use common::pty::{PTY_TIMEOUT, Pty, PtyOptions};
use common::{read_session_messages, read_sessions};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel, ScriptedModelHandle};
use serde_json::{Value, json};

/// 启停循环次数（契约：10 次）。
const CYCLES: usize = 10;

/// 位置敏感断言的复查间隔（与 pty 夹具的 `expect` 同一条 20ms 轮询口径）。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 第 `cycle` 轮提交的句子（逐轮不同，便于钉住请求与库行的对应关系）。
fn sentence(cycle: usize) -> String {
    format!("第 {cycle:02} 句")
}

/// 假模型对第 `cycle` 轮的答复（脚本第 `cycle` 步）。
fn answer(cycle: usize) -> String {
    format!("答复 {cycle:02}")
}

/// 前 `cycles` 轮的 canonical 行：`(seq, role, content)`，与 `session_messages` 的形状一致。
///
/// 一轮落两行（user 在前、assistant 在后），`seq` 从 1 起连续。
fn expected_rows(cycles: usize) -> Vec<(i64, String, String)> {
    let mut rows = Vec::with_capacity(cycles * 2);
    for cycle in 1..=cycles {
        rows.push((2 * cycle as i64 - 1, "user".to_string(), sentence(cycle)));
        rows.push((2 * cycle as i64, "assistant".to_string(), answer(cycle)));
    }
    rows
}

/// 在**既有**目录上起一个 REPL：home/workspace 与假模型句柄都由调用方给，`args` 原样交给 CLI
/// （传 `&["-c"]` 就是恢复路径）。
///
/// 与 `apps/cli/tests/pty.rs` 的同名助手同一份装配（窗口走夹具默认 200×50）：本条用例要的是
/// 「同一份目录上连起十次」，不是另一套夹具。
fn spawn_repl_in(dir: &TempDir, model: &ScriptedModelHandle, args: &[&str]) -> Pty {
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
        env: &[],
        window: None,
    })
}

/// 等 `needle` 出现在 `marker` **最后一次出现之后**（`Pty::expect` 每次从流头重扫，拿已经出现过的
/// 串当断言是同义反复；口径与 `apps/cli/tests/pty.rs` 的同名助手一致）。
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

/// 等编辑器重新接管提示符：任务运行期间终端是 canonical（raw 只在读取区间里生效），这期间送进去
/// 的字节由行规范处理——看到 `marker` 之后的新提示符帧，`/exit` 才会被当输入行读走。
fn wait_for_prompt_after(pty: &Pty, marker: &str) {
    expect_after(pty, marker, "❯ ");
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

/// 请求体里去掉 `role == "system"` 之后的消息数组（system 是本机提示词，逐轮重算，不是这段对话的
/// 历史；判据是「内核看到的历史」）。
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

/// banner 的 `Session:` 行上的会话标签（`new` 或 13 字符短 id）。
///
/// 这一行由 `println!` 在进入 raw mode 之前打出（`Session: …\r\n`），按行取到空白/换行为止即
/// 标签本身；已经等到 `Session: ` 才调用（见用例里的 `expect`），拿不到就是夹具问题。
fn banner_session_label(pty: &Pty) -> String {
    let text = pty.output();
    let at = text.rfind("Session: ").expect("banner 应有 Session: 行") + "Session: ".len();
    text[at..]
        .split(|ch: char| ch.is_whitespace())
        .next()
        .expect("Session: 之后应有标签")
        .to_string()
}

/// 全部任务行的 `(id, session_id)`，按开始时间升序（`session_id` 为 NULL 时给空串）。
///
/// 直读 `tasks`：这条用例要证明的是「十轮任务全部绑在同一条会话上」，读路径会把 NULL 与空串
/// 抹平、也看不到别的轮次。
fn read_task_session_ids(db_path: &Path) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(db_path).expect("任务表应可直读");
    let mut stmt = conn
        .prepare("SELECT id, COALESCE(session_id, '') FROM tasks ORDER BY started_at ASC, id ASC")
        .expect("任务查询应可编译");
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("任务查询应可执行");
    rows.map(|row| row.expect("任务行应可读取")).collect()
}

/// **契约用例**：同一份 home/workspace 十次启停，恢复出来的有效历史与库里的 canonical 行逐条相等。
#[test]
fn session_restore_keeps_the_canonical_history_across_ten_cycles() {
    // 十步脚本 + 一个句柄贯穿十轮：**不重启模型**（每轮的答复必须是脚本第 i 步，多发/漏发请求
    // 都会让答复错位而当场失败）。
    let model = ScriptedModel::start(
        (1..=CYCLES)
            .map(|cycle| ScriptStep::Text(answer(cycle)))
            .collect(),
    );
    let dir = TempDir::new("sessions-ten-cycles");
    let workspace = dir.join("workspace");
    let db_path = dir.join("home").join("ignis.db");

    // 上一轮结束时的库行（第 i 轮的请求必须恰好带上这一份）与会话身份（第 2 轮起必须不变）
    let mut db_rows: Vec<(i64, String, String)> = Vec::new();
    let mut session_id: Option<String> = None;
    let mut short_label: Option<String> = None;

    for cycle in 1..=CYCLES {
        // 第 1 轮不带 `-c`（新会话）；其余轮次都在同一份目录上 `-c` 恢复
        let args: &[&str] = if cycle == 1 { &[] } else { &["-c"] };
        let mut pty = spawn_repl_in(&dir, &model, args);

        // banner 的 Session 行：第 1 轮是 `new`，第 2 轮起是同一个 13 字符短 id。
        // note（恢复未命中）打在 banner 之前，看到 banner 就等于它没出现。
        pty.expect("Session: ");
        let label = banner_session_label(&pty);
        if cycle == 1 {
            assert_eq!(
                label,
                "new",
                "第 1 轮不带 -c，banner 的会话行必须是 new：\n{}",
                pty.output()
            );
            assert!(
                db_rows.is_empty(),
                "第 1 轮之前不该有任何会话消息行：{db_rows:#?}"
            );
        } else {
            assert_ne!(
                label,
                "new",
                "第 {cycle} 轮带 -c，会话行不该是 new（说明 -c 没命中）：\n{}",
                pty.output()
            );
            assert!(
                !pty.output().contains("note: no previous session"),
                "第 {cycle} 轮 -c 未命中上一轮的会话：\n{}",
                pty.output()
            );
            assert_eq!(
                label.chars().count(),
                13,
                "banner 的会话标签应是 13 字符短 id（本轮 {label:?}）：\n{}",
                pty.output()
            );
            match &short_label {
                None => short_label = Some(label),
                Some(first) => assert_eq!(
                    &label,
                    first,
                    "第 {cycle} 轮的会话短 id 必须与第 2 轮一致（会话不许换）：\n{}",
                    pty.output()
                ),
            }
        }

        // 提示符出现 = 编辑器已接管终端（raw mode 生效），此时回车才会被当 Enter 提交
        pty.expect("❯ ");
        pty.send(&format!("{}\r", sentence(cycle)));
        // 本轮脚本答复（脚本按请求顺序消耗：本轮答复必须是第 cycle 步）
        pty.expect(&answer(cycle));
        // 结束行：本轮任务收尾（渲染器已冲刷完正文）
        expect_after(&pty, &answer(cycle), "● Done");
        // 任务结束后终端回到 canonical：等编辑器重新接管提示符，`/exit` 才会被当输入行处理
        wait_for_prompt_after(&pty, "● Done");
        pty.send("/exit\r");
        assert_eq!(
            pty.wait(),
            0,
            "第 {cycle} 轮 /exit 应以 0 退出：\n{}",
            pty.output()
        );
        drop(pty); // 收尸后再读库：落库与收尾都已兑现

        // ① 送进内核的请求体：第 cycle 轮（去掉 system）逐条 = 之前各轮的库行 + 本轮输入。
        //    第 1 轮 db_rows 为空 —— 新会话不许带任何历史，这一断言同时钉住它。
        let requests = wait_for_requests(&model, cycle);
        assert_eq!(
            requests.len(),
            cycle,
            "第 {cycle} 轮为止应恰好送过 {cycle} 份请求（每轮一份）：{requests:#?}"
        );
        let mut expected: Vec<Value> = db_rows
            .iter()
            .map(|(_, role, content)| json!({"role": role, "content": content}))
            .collect();
        expected.push(json!({"role": "user", "content": sentence(cycle)}));
        assert_eq!(
            messages_without_system(&requests[cycle - 1]),
            Value::Array(expected),
            "第 {cycle} 轮送进内核的历史必须与库里的 canonical 行逐条相等（角色、内容、顺序）：{:#?}",
            requests[cycle - 1]["messages"]
        );

        // ② 库里的 canonical 行：逐条等于「前 cycle 轮」的全量序列（与 ① 双向核对）
        let sessions = read_sessions(&db_path, &workspace);
        assert_eq!(
            sessions.len(),
            1,
            "本工作区只能有一条会话（十轮是同一段对话）：{sessions:#?}"
        );
        let (id, title) = &sessions[0];
        assert_eq!(title, &sentence(1), "标题是首条用户消息：{sessions:#?}");
        match &session_id {
            None => session_id = Some(id.clone()),
            Some(first) => assert_eq!(
                id, first,
                "会话 id 从第 1 轮起就应保持不变（-c 恢复的是同一条）：{sessions:#?}"
            ),
        }
        if let Some(label) = short_label.as_deref() {
            assert!(
                id.starts_with(label),
                "banner 的短 id 应是会话 id 的前缀：label={label} id={id}"
            );
        }
        db_rows = read_session_messages(&db_path, id);
        assert_eq!(
            db_rows,
            expected_rows(cycle),
            "第 {cycle} 轮结束后库里的 canonical 历史必须逐条相等（角色、内容、顺序）"
        );
    }

    // 十轮收尾：库里恰好 2 × 10 条消息、十条任务行全部绑在同一条会话上
    let session_id = session_id.expect("十轮之后应有一条会话行");
    assert_eq!(
        read_session_messages(&db_path, &session_id).len(),
        2 * CYCLES,
        "十轮之后 session_messages 的条数必须是 2 × {CYCLES}"
    );
    let tasks = read_task_session_ids(&db_path);
    assert_eq!(tasks.len(), CYCLES, "十轮 = 十条任务行：{tasks:#?}");
    for (task_id, task_session) in &tasks {
        assert_eq!(
            task_session, &session_id,
            "任务 {task_id} 的 session_id 必须指向同一条会话：{tasks:#?}"
        );
    }
    // 十轮之后脚本恰好被消耗十步（一个句柄贯穿全程，没有重启也没有多发的请求）
    assert_eq!(
        model.requests().len(),
        CYCLES,
        "十轮共 10 份请求（脚本恰好十步）：{:#?}",
        model.requests()
    );
}
