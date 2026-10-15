//! 上限与失败可见：`limits.max_steps = 1` 时第二步不再请求模型，任务以 `limit_reached` 收尾。
//!
//! 断言退出码 1、`tasks` 行的状态与 `error`、以及 JSONL 里的内核 `done` 行。

mod common;

use std::fs;

use common::{TempDir, ignis_cmd, latest_task, output_text, tool_call};
use ignis_runtime::test_support::ScriptedModel;
use serde_json::{Value, json};

#[test]
fn max_steps_reaches_limit_and_stays_visible() {
    // 两步都是工具调用（恒定多步脚本）：第 1 步执行完，第 2 步在预检处撞上 max_steps
    let model = ScriptedModel::start(vec![
        tool_call("fs_list", json!({})),
        tool_call("fs_list", json!({})),
    ]);

    let dir = TempDir::new("limits");
    let home = dir.join("home");
    let project = dir.join("project");
    fs::create_dir_all(&home).expect("home 应可创建");
    fs::create_dir_all(&project).expect("工作区应可创建");
    fs::write(home.join("config.json"), "{\"limits\":{\"max_steps\":1}}").expect("配置应可写入");

    let output = ignis_cmd(&home, &project, &model.base_url())
        .args(["run", "列出项目里的文件"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(text.contains("limit_reached"), "{text}");
    // 结束行留在 stdout（渲染流），原因行留在 stderr：两条流各看各的
    assert!(stdout.contains("● Finished: limit_reached"), "{text}");
    assert!(!stdout.contains("error: "), "{text}");
    assert_eq!(model.requests().len(), 1, "触限后不得再请求模型");

    // 任务行：状态是终态 `limit_reached`，原因可查
    let db = home.join("ignis.db");
    let (task_id, status, error) = latest_task(&db);
    assert_eq!(status, "limit_reached", "{text}");
    assert!(error.contains("max_steps"), "{error}");
    // 终端必须自己说清为什么没成功（I2）：与 `tasks.error` 同一份文本打到 stderr
    assert!(stderr.contains(&format!("error: {error}")), "{text}");
    // 结束行带任务 id：失败/触限的运行不必翻库就能接着 `ignis task show {id}`
    assert!(stdout.contains(&format!("· task {task_id}")), "{text}");

    // JSONL：内核的 `done` 行带着 limit_reached；收尾行是 `task_end`（不是第二个 `done`）
    let raw = fs::read_to_string(home.join("logs").join(format!("{task_id}.jsonl")))
        .expect("任务日志应存在");
    assert!(raw.contains("limit_reached"), "{raw}");
    let lines: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("日志行应是 JSON"))
        .collect();
    let done = lines
        .iter()
        .find(|line| line["type"] == "done")
        .expect("内核 done 行应落盘");
    assert_eq!(done["status"], json!("limit_reached"), "{done}");
    assert_eq!(lines.last().unwrap()["type"], json!("task_end"), "{raw}");
}
