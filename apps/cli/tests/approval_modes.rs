//! 审批模式的端到端阶梯（fail-closed 与危险命令）：三个用例各用一个独立临时工作区。
//!
//! 无旗标且 stdin 非 tty → `Unattended` 全拒；`--yes` 放行普通操作但挡住危险命令；`--unsafe`
//! 让危险命令在沙箱工作区里真的执行。

mod common;

use std::fs;

use common::{TempDir, ignis_cmd, latest_task, output_text, read_approvals, tool_call};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel};
use serde_json::json;

#[test]
fn no_flags_denies_everything() {
    let model = ScriptedModel::start(vec![
        tool_call("fs_write", json!({"path": "new.txt", "content": "x"})),
        ScriptStep::Text("已尝试写入。".into()),
    ]);

    let dir = TempDir::new("approval-none");
    let home = dir.join("home");
    let project = dir.join("project");
    fs::create_dir_all(&project).expect("工作区应可创建");

    // 无 `--yes` / `--unsafe`，stdin 非 tty → Unattended：Elevated 的写操作直接拒绝
    let output = ignis_cmd(&home, &project, &model.base_url())
        .args(["run", "在工作区里写一个文件"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // 未授权即无副作用；任务本身照常收尾
    assert!(
        !project.join("new.txt").exists(),
        "拒绝后不得写文件：{text}"
    );
    let db = home.join("ignis.db");
    let (task_id, status, _) = latest_task(&db);
    assert_eq!(status, "success", "{text}");
    assert_eq!(
        read_approvals(&db, &task_id),
        [("fs_write", "deny", "unattended")].map(|(tool, decision, mode)| (
            tool.to_owned(),
            decision.to_owned(),
            mode.to_owned()
        ))
    );
}

#[test]
fn yes_allows_write_but_not_dangerous() {
    let model = ScriptedModel::start(vec![
        tool_call("fs_write", json!({"path": "ok.txt", "content": "x"})),
        tool_call("shell_run", json!({"command": "rm -rf guarded"})),
        ScriptStep::Text("已处理。".into()),
    ]);

    let dir = TempDir::new("approval-yes");
    let home = dir.join("home");
    let project = dir.join("project");
    fs::create_dir_all(project.join("guarded")).expect("工作区与 guarded/ 应可创建");

    let output = ignis_cmd(&home, &project, &model.base_url())
        .args(["run", "写入标记文件并清理缓存目录", "--yes"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // 普通写操作被放行；危险命令被策略挡在审批方之外的 deny
    assert!(
        project.join("ok.txt").exists(),
        "预授权应放行写操作：{text}"
    );
    assert!(
        project.join("guarded").is_dir(),
        "`--yes` 不得放行危险命令：{text}"
    );

    let db = home.join("ignis.db");
    let (task_id, status, _) = latest_task(&db);
    assert_eq!(status, "success", "{text}");
    let want = [
        ("fs_write", "allow", "preauthorized"),
        ("shell_run", "deny", "preauthorized"),
    ];
    assert_eq!(
        read_approvals(&db, &task_id),
        want.map(|(tool, decision, mode)| (tool.to_owned(), decision.to_owned(), mode.to_owned()))
    );
}

#[test]
fn unsafe_executes_dangerous_in_sandbox() {
    let model = ScriptedModel::start(vec![
        tool_call("shell_run", json!({"command": "rm -rf guarded"})),
        ScriptStep::Text("已清理。".into()),
    ]);

    let dir = TempDir::new("approval-unsafe");
    let home = dir.join("home");
    let project = dir.join("project");
    fs::create_dir_all(project.join("guarded")).expect("工作区与 guarded/ 应可创建");
    fs::write(project.join("keep.txt"), "keep").expect("旁观文件应可创建");

    let output = ignis_cmd(&home, &project, &model.base_url())
        .args(["run", "清理缓存目录", "--unsafe"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // 危险命令真的执行了，但作用范围就是这个沙箱工作区
    assert!(
        !project.join("guarded").exists(),
        "`--unsafe` 应真的执行危险命令：{text}"
    );
    assert!(
        project.join("keep.txt").is_file(),
        "删除只应作用于目标目录：{text}"
    );

    let db = home.join("ignis.db");
    let (task_id, status, _) = latest_task(&db);
    assert_eq!(status, "success", "{text}");
    assert_eq!(
        read_approvals(&db, &task_id),
        [("shell_run", "allow", "preauthorized")].map(|(tool, decision, mode)| (
            tool.to_owned(),
            decision.to_owned(),
            mode.to_owned()
        ))
    );
}
