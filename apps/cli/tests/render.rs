//! 渲染 e2e：内联推理分道、工具完成行与 NO_COLOR 覆盖（v0.2 §6.3）。
//!
//! 三个用例都只认 stdout：终端渲染写的是 stdout，stderr 是另一条流，用它替渲染背书会把
//! 「行没渲染到 stdout」放过去。非 tty 下零 ANSI、不插入物理换行（每行可 grep），所以断言按
//! `contains` 找行，而不是比对整段输出。

mod common;

use std::fs;

use common::{TempDir, ignis_cmd, output_text, tool_call};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel};
use serde_json::json;

/// v0.2 之前的结束行文案：三个用例都钉住它不再出现（回归锚点）。
const OLD_END_LINE: &str = "● Finished";

/// 推理行的行首标记（normal 模式）：推理与正文分道后，推理块独占一行。
const REASONING_PREFIX: &str = "· ";

/// 内联 `<think>` 块由内核分道：正文通道只留块外的两段，标记不许漏进终端。
#[test]
fn inline_reasoning_is_split_out_of_the_answer_channel() {
    let model = ScriptedModel::start(vec![ScriptStep::Text(
        "先说明<think>内心戏</think>结论".into(),
    )]);

    let dir = TempDir::new("render-reasoning");
    let home = dir.join("home");
    let project = dir.join("project");
    fs::create_dir_all(&project).expect("工作区应可创建");

    let output = ignis_cmd(&home, &project, &model.base_url())
        .args(["run", "说一句话", "--yes"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // 标记是分道记法，不是正文：终端上一处都不该看到
    assert!(!stdout.contains("<think>"), "{text}");
    assert!(!stdout.contains("</think>"), "{text}");

    // 正文通道：推理块前后的两段正文都在，合起来正是 `先说明结论`。推理块在 normal 模式下
    // 独占一行（行首见 REASONING_PREFIX），所以按「摘掉推理行再拼接」的口径还原正文通道。
    let answer: String = stdout
        .lines()
        .filter(|line| !line.starts_with(REASONING_PREFIX))
        .collect();
    assert!(answer.contains("先说明结论"), "{text}");

    // 不加输出模式旗标 ⇒ 默认 normal；stdio 都是管道 ⇒ 非 tty（零 ANSI）；结束行也不再是旧文案
    assert!(!stdout.contains('\u{1b}'), "{text}");
    assert!(!stdout.contains(OLD_END_LINE), "{text}");
    assert!(stdout.contains("● Done"), "{text}");
}

/// 工具完成行：开始行 `→ Reading …` 与完成行 `  ✓ Reading … · {耗时}` 配对出现。
#[test]
fn tool_completion_line_pairs_with_the_exec_line_and_carries_the_elapsed() {
    // 两步：读工作区里的文件（Safe，不需要审批放行）→ 文本收尾
    let model = ScriptedModel::start(vec![
        tool_call("fs_read", json!({"path": "a.txt"})),
        ScriptStep::Text("读完了".into()),
    ]);

    let dir = TempDir::new("render-tool");
    let home = dir.join("home");
    let project = dir.join("project");
    fs::create_dir_all(&project).expect("工作区应可创建");
    fs::write(project.join("a.txt"), "内容\n").expect("夹具文件应可写入");

    let output = ignis_cmd(&home, &project, &model.base_url())
        .args(["run", "读一下 a.txt", "--yes"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // 开始行与完成行共用同一条摘要（`fs_read` 的 `Reading {path}`）；完成行带实测耗时
    assert!(stdout.contains("→ Reading a.txt"), "{text}");
    assert!(stdout.contains("  ✓ Reading a.txt · "), "{text}");
    assert!(stdout.contains("读完了"), "{text}");

    // normal 模式的结束行是 `● Done …`；非 tty 零 ANSI 与旧文案的回归锚点同上一例
    assert!(stdout.contains("● Done"), "{text}");
    assert!(!stdout.contains(OLD_END_LINE), "{text}");
    assert!(!stdout.contains('\u{1b}'), "{text}");
}

/// `NO_COLOR` 压过配置里的 `color: always`：环境变量存在且非空时零 ANSI，即便配置要求上色；
/// 正文与结束行照旧输出（压的是颜色，不是渲染本身）。
#[test]
fn no_color_env_overrides_always_color_in_config() {
    // 加粗正文在 `color: always` 下会带 ANSI（`\u{1b}[1m…`）：没有 NO_COLOR 覆盖时这条断言
    // 会真的看见转义序列，测试不是空转
    let model = ScriptedModel::start(vec![ScriptStep::Text("**结论已定**".into())]);

    let dir = TempDir::new("render-no-color");
    let home = dir.join("home");
    let project = dir.join("project");
    fs::create_dir_all(&home).expect("home 应可创建");
    fs::create_dir_all(&project).expect("工作区应可创建");
    fs::write(home.join("config.json"), r#"{"ui":{"color":"always"}}"#).expect("配置应可写入");

    let output = ignis_cmd(&home, &project, &model.base_url())
        // 只给这一例加 `NO_COLOR`：其余用例保持进程环境的原样（`ignis_cmd` 不碰它）
        .env("NO_COLOR", "1")
        .args(["run", "说一句话", "--yes"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // `color: always` 被 NO_COLOR 压回去：stdout 一个 ANSI 字节都不许有
    assert!(!stdout.contains('\u{1b}'), "{text}");
    // 正文与结束行照旧（回归锚点同前两例）
    assert!(stdout.contains("结论已定"), "{text}");
    assert!(stdout.contains("● Done"), "{text}");
    assert!(!stdout.contains(OLD_END_LINE), "{text}");
}
