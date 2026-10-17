//! Demo A 端到端：真二进制 + 脚本化假模型，把一个坏掉的 Node 项目修到测试通过。
//!
//! 假模型脚本（5 步）：读 `src/config.js` → 跑 `npm test` 看到失败 → 把多退一层的 `'..', '..'`
//! 改成 `'..'` → 重跑 `npm test` → 给最终答复。断言全部落在真文件、真 DB 行、真 stdout 与假模型
//! 实际收到的请求上。

mod common;

use std::fs;

use common::{
    TempDir, copy_dir, fixture_dir, ignis_cmd, latest_task, output_text, read_approvals,
    run_npm_test, tool_call,
};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel};
use serde_json::json;

/// 题目里的任务描述（原样写进任务行）。
const PROMPT: &str = "分析这个项目的测试失败原因，修复它并重新运行测试";

#[test]
fn demo_a_fixes_broken_project_end_to_end() {
    let model = ScriptedModel::start(vec![
        tool_call("fs_read", json!({"path": "src/config.js"})),
        tool_call("shell_run", json!({"command": "npm test"})),
        tool_call(
            "fs_edit",
            json!({
                "path": "src/config.js",
                "old_string": "'..', '..'",
                "new_string": "'..'",
            }),
        ),
        tool_call("shell_run", json!({"command": "npm test"})),
        ScriptStep::Text("已修复配置路径，测试通过。".into()),
    ]);

    // 工作区是夹具的独立副本，home 也在临时目录里：本次运行不碰真实的 ~/.ignis
    let dir = TempDir::new("demo-a");
    let home = dir.join("home");
    let project = dir.join("project");
    copy_dir(&fixture_dir(), &project);

    let output = ignis_cmd(&home, &project, &model.base_url())
        .args(["run", PROMPT, "--yes"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // 终端渲染：两处工具行（DIM 色码在非 tty 下不出现）+ 最终答复。工具行断言只认 stdout：
    // stderr 是另一条流，用它替 CLI 的终端渲染背书会把「行没渲染到 stdout」放过去。
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("→ Reading src/config.js"), "{text}");
    assert!(stdout.contains("→ Running: npm test"), "{text}");
    assert!(text.contains("已修复配置路径，测试通过。"), "{text}");

    // 修复真的落在工作区副本上：这份副本自己跑 npm test 也通过
    assert!(run_npm_test(&project).success(), "修复后 npm test 应退出 0");

    // 假模型收到 5 次请求；第 3 次（索引 2）里带着第一次 npm test 的失败输出（工具结果确实回填）。
    // 失败证据只取 reporter 无关的两个标记：`ENOENT`（Node 报错正文，spec 与 TAP 都照抄）
    // 与 ignis shell 工具自己的结果头 `exit code: 1`——两者都只在工具结果被回填时才存在。
    let requests = model.requests();
    assert_eq!(requests.len(), 5, "{requests:#?}");
    let messages = requests[2]["messages"]
        .as_array()
        .expect("请求体里的 messages 应为数组");
    assert!(
        messages.iter().any(|message| {
            message["role"] == "tool"
                && message["content"].as_str().is_some_and(|content| {
                    content.contains("ENOENT") && content.contains("exit code: 1")
                })
        }),
        "索引 2 的请求应带回第一次 `npm test` 的失败输出（ENOENT + exit code: 1）：{messages:#?}"
    );

    // 审批：只有需要判定的调用留痕（Safe 的 `fs_read` 不记录），全部预授权放行
    let db = home.join("ignis.db");
    let (task_id, status, error) = latest_task(&db);
    assert_eq!(
        (status.as_str(), error.as_str()),
        ("success", ""),
        "{task_id}"
    );
    // 结束行带任务 id（I2）：成功运行同样要能直接从终端复制 id 去查任务。
    // v0.2 起结束行给 13 字符短 id（`task show` 接受 ≥8 字符的唯一前缀），完整 id 照旧在库里。
    assert!(
        stdout.contains(&format!("· task {}", &task_id[..13])),
        "{text}"
    );
    // 成功运行不该出现失败原因行（`Session::run_task` 只对非成功终态打印）
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("error: "),
        "{text}"
    );
    let want = [
        ("shell_run", "allow", "preauthorized"),
        ("fs_edit", "allow", "preauthorized"),
        ("shell_run", "allow", "preauthorized"),
    ];
    assert_eq!(
        read_approvals(&db, &task_id),
        want.map(|(tool, decision, mode)| (tool.to_owned(), decision.to_owned(), mode.to_owned()))
    );

    // JSONL 日志：文件按任务 id 落在 IGNIS_HOME/logs 下，且密钥不落盘
    let raw = fs::read_to_string(home.join("logs").join(format!("{task_id}.jsonl")))
        .expect("任务日志应存在");
    assert!(!raw.contains("test-key"), "{raw}");
    assert!(raw.contains("task_end"), "{raw}");
}
