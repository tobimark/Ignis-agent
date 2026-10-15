//! 创建调度任务后的自拉起提示：真二进制 + 脚本化假模型。
//!
//! `ignis run` 的 stdin 是 `/dev/null`（非 tty）→ 走「非交互只打提示行」那条路。这里钉三件事：
//! 提示行**逐字**出现且只出现一次、任务真的落了库（提示的前提是本轮建过调度任务）、以及
//! 提示只提示——不会替用户把守护进程拉起来。
//!
//! 交互模式（提问 `[y/N]`）与「每会话至多一次」的 REPL 路径需要 tty，交给手动 pty 冒烟；
//! 判定函数的 16 种组合在 `commands::daemon` 的内联单测里全表钉死。

mod common;

use std::fs;

use common::{TempDir, ignis_cmd, output_text, tool_call};
use ignis_gateway::daemon::{DaemonStatus, status};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel};
use serde_json::json;

/// 非交互模式的提示行：与 `commands::daemon::START_HINT` 逐字一致（改字要连那边一起改）。
const HINT: &str = "hint: daemon is not running; run `ignis daemon start` to enable schedules";

/// 这一次运行要建的调度任务名（断言落库用）。
const JOB_NAME: &str = "disk-watch";

#[test]
fn a_created_schedule_hints_once_that_the_daemon_is_not_running() {
    let dir = TempDir::new("autostart-hint");
    let home = dir.join("home");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("工作区应可创建");

    let model = ScriptedModel::start(vec![
        tool_call(
            "schedule_create",
            json!({
                "name": JOB_NAME,
                "prompt": "检查磁盘占用，超过 85% 就通知我",
                "schedule": "10m",
            }),
        ),
        ScriptStep::Text("已创建调度任务。".into()),
    ]);

    let output = ignis_cmd(&home, &workspace, &model.base_url())
        .args(["run", "每隔 10 分钟检查一次磁盘", "--yes"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // 提示行逐字出现，且恰好一次（非交互不打两遍）
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches(HINT).count(), 1, "应恰好提示一次：{text}");

    // 提示的前提成立：本轮真的建了一条调度任务（走真库断言，不看输出文案）
    let jobs: i64 = rusqlite::Connection::open(home.join("ignis.db"))
        .expect("任务库应可直读")
        .query_row(
            "SELECT COUNT(*) FROM jobs WHERE name = ?1 AND status = 'active'",
            [JOB_NAME],
            |row| row.get(0),
        )
        .expect("jobs 表应可查");
    assert_eq!(jobs, 1, "调度任务应落库且 active\n{text}");

    // 只是提示：守护进程没有被「顺手」拉起来，也没有留下 pidfile
    assert_eq!(status(&home), DaemonStatus::Stopped, "{text}");
    assert!(
        !home.join("ignis.pid").exists(),
        "提示不应启动守护进程：{}",
        home.display()
    );
}

/// 没建过调度任务的那一轮不打扰用户：同样非交互、同样没在跑守护进程，stdout 里没有提示行。
#[test]
fn a_run_without_schedule_create_stays_silent() {
    let dir = TempDir::new("autostart-silent");
    let home = dir.join("home");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("工作区应可创建");

    let model = ScriptedModel::start(vec![ScriptStep::Text("好的。".into())]);
    let output = ignis_cmd(&home, &workspace, &model.base_url())
        .args(["run", "随便聊一句", "--yes"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(HINT), "没建调度任务就不该提示：{text}");
    assert!(!home.join("ignis.pid").exists(), "{text}");
}
