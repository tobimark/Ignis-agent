//! `remember` 确认行的端到端回归：成功的工具结果恰好一行 `  ✓ memory: {key} ({scope}) {text}`，
//! 失败（`ok = false`）一行都不打。
//!
//! 回归锚点来自 P1（决策 #22）与 P4 规划：`remember` 是 Safe 元工具，交互审批永远问不到它，所以
//! 「被拒」路径落在渲染器边界——这里走真二进制 `ignis run … --yes`（stdin 非 tty → 非交互模式），
//! 断言 stdout 的确认行形状与 `facts` 表的落库现场；`ok = false` 的单元用例在
//! `render/mod.rs`，两处合起来钉住「成功必须打、失败不许打」。

mod common;

use std::fs;

use common::{TempDir, ignis_cmd, latest_task, output_text, read_facts, tool_call};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel};
use serde_json::json;

/// 成功路径：模型先调 `remember`，再给一句答复（两步脚本）。stdout 里确认行恰好一行，库里一行
/// active 事实，任务照常成功收尾。
#[test]
fn remember_success_prints_the_confirmation_line() {
    let dir = TempDir::new("remember-success");
    let home = dir.join("home");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("工作区应可创建");

    let model = ScriptedModel::start(vec![
        tool_call(
            "remember",
            json!({"scope": "project", "key": "pkg_manager", "text": "这个项目用 uv"}),
        ),
        ScriptStep::Text("已记住。".into()),
    ]);

    let output = ignis_cmd(&home, &workspace, &model.base_url())
        .args(["run", "记住这个项目的包管理器", "--yes"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // 确认行逐字（2 空格缩进 + 结尾换行），且恰好一行
    assert!(
        stdout.contains("  ✓ memory: pkg_manager (project) 这个项目用 uv\n"),
        "{text}"
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.contains("memory:"))
            .count(),
        1,
        "确认行必须恰好一行：{text}"
    );

    // 库里一行 active 事实：key、文本与落库口径都对上
    let db = home.join("ignis.db");
    let facts = read_facts(&db, "pkg_manager");
    assert_eq!(facts.len(), 1, "{facts:?}");
    assert_eq!(facts[0].1, "active");
    assert_eq!(facts[0].2, None, "首条事实没有 superseded_by");
    assert_eq!(facts[0].3, "这个项目用 uv");
    // 工具调用不翻任务：这一轮照常成功收尾
    let (_id, status, error) = latest_task(&db);
    assert_eq!(status, "success", "{error}\n{text}");
}

/// 失败路径：`scope` 写成 `Project`（非法取值）→ 工具结果 `ok = false`。任务仍然成功（工具失败不
/// 翻任务），但输出里没有任何确认行，库里也没有任何事实。
#[test]
fn remember_failure_prints_no_confirmation_line() {
    let dir = TempDir::new("remember-failure");
    let home = dir.join("home");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("工作区应可创建");

    let model = ScriptedModel::start(vec![
        tool_call(
            "remember",
            json!({"scope": "Project", "key": "k", "text": "值"}),
        ),
        ScriptStep::Text("参数不对。".into()),
    ]);

    let output = ignis_cmd(&home, &workspace, &model.base_url())
        .args(["run", "记一条", "--yes"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // 一行确认都不许出现（`✓` 与 `memory:` 都不在输出里）；失败行走既有的 `✗` 形状
    assert!(!text.contains("memory:"), "{text}");
    assert!(!text.contains('✓'), "{text}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("  ✗ "),
        "失败行照旧：{text}"
    );

    // 库里没有任何事实行，任务仍成功收尾
    let db = home.join("ignis.db");
    assert!(read_facts(&db, "k").is_empty(), "失败不落事实");
    let (_id, status, error) = latest_task(&db);
    assert_eq!(status, "success", "{error}\n{text}");
}
