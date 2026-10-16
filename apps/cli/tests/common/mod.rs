//! 三个 e2e 用例共用的脚手架：临时目录、夹具复制、审批/任务直读、`npm test` 与命令装配。
//!
//! 本机离线，cargo 缓存里没有 `assert_cmd` / `predicates` / `tempfile`，所以一律用 std：
//! 二进制路径取 `CARGO_BIN_EXE_ignis`（本包的集成测试可用），进程用 [`std::process::Command`]，
//! 临时目录自己管。

#![allow(dead_code)] // 每个测试二进制各自 `mod common;`，用不到全部助手是正常的

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use ignis_runtime::test_support::ScriptStep;
use serde_json::Value;

/// 同一测试进程内的目录序号：并行执行的用例之间互不撞名。
static SEQ: AtomicU64 = AtomicU64::new(0);

/// 用例独占的临时目录（`$TMPDIR/ignis-e2e-{进程}-{标签}-{序号}`），析构时递归清理。
///
/// `home`（IGNIS_HOME）与工作区都必须建在它里面：任何用例都不许碰真实的 `~/.ignis`。
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ignis-e2e-{}-{label}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("临时目录应可创建");
        Self(dir)
    }

    /// 目录内的子路径（不要求已存在）。
    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 夹具项目 `examples/broken-project` 的绝对路径。
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/broken-project")
}

/// 递归复制目录；夹具里没有任何需要跳过的构建产物。
pub fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("目标目录应可创建");
    for entry in fs::read_dir(src).expect("源目录应可读取") {
        let entry = entry.expect("目录项应可读取");
        let target = dst.join(entry.file_name());
        if entry.file_type().expect("目录项类型应可读取").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("文件应可复制");
        }
    }
}

/// 最近的一条任务行：`(id, status, error)`（`error` 为 NULL 时给空串）。
pub fn latest_task(db_path: &Path) -> (String, String, String) {
    let conn = rusqlite::Connection::open(db_path).expect("库文件应可直读");
    conn.query_row(
        "SELECT id, status, COALESCE(error, '') FROM tasks ORDER BY started_at DESC, id DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .expect("本次运行应写入一条任务行")
}

/// 某个任务的审批记录，按决定先后：`(tool, decision, mode)`。
pub fn read_approvals(db_path: &Path, task_id: &str) -> Vec<(String, String, String)> {
    let conn = rusqlite::Connection::open(db_path).expect("审批表应可直读");
    let mut stmt = conn
        .prepare("SELECT tool, decision, mode FROM approvals WHERE task_id = ?1 ORDER BY id ASC")
        .expect("审批查询应可编译");
    let rows = stmt
        .query_map([task_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("审批查询应可执行");
    rows.map(|row| row.expect("审批行应可读取")).collect()
}

/// 某个 key 的全部事实行，按 id 升序：`(id, status, superseded_by, text)`。
///
/// 直读 `facts` 表而不是走读路径：Demo B 要断言的正是「库里到底留了几行、旧行指向谁」，
/// 而只回 active 的读路径会把 supersede 的现场抹掉（`superseded_by` 为 NULL 时给 `None`）。
pub fn read_facts(db_path: &Path, key: &str) -> Vec<(i64, String, Option<i64>, String)> {
    let conn = rusqlite::Connection::open(db_path).expect("事实表应可直读");
    let mut stmt = conn
        .prepare("SELECT id, status, superseded_by, text FROM facts WHERE key = ?1 ORDER BY id ASC")
        .expect("事实查询应可编译");
    let rows = stmt
        .query_map([key], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("事实查询应可执行");
    rows.map(|row| row.expect("事实行应可读取")).collect()
}

/// 全部告警行，按 `created_at` 升序（同刻按 id 升序）：`(severity, title, created_at, read_at)`。
///
/// 直读 `alerts` 表：Demo C 要断言的是「去重窗口内同标题到底留了几行」——只回未读的读路径会把
/// 「被翻成已读」的现场抹掉，所以 `read_at` 单独给出来（未读为 `None`），读路径的过滤一律绕开。
pub fn read_alerts(db_path: &Path) -> Vec<(String, String, i64, Option<i64>)> {
    let conn = rusqlite::Connection::open(db_path).expect("告警表应可直读");
    let mut stmt = conn
        .prepare(
            "SELECT severity, title, created_at, read_at FROM alerts
             ORDER BY created_at ASC, id ASC",
        )
        .expect("告警查询应可编译");
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("告警查询应可执行");
    rows.map(|row| row.expect("告警行应可读取")).collect()
}

/// 某个 job（`job_id` 为 `None` 时是全部 job）的运行行，按 `started_at` 升序（同刻按 id 升序）：
/// `(id, status, error)`（`error` 为 NULL 时给 `None`）。
///
/// 直读 `job_runs`：Demo C 要按「第几次运行」定位现场（第一次落告警、第二次命中去重、被
/// SIGKILL 遗留的 `running` 被清场），读路径的过滤与截断会把这几次数抹平。
pub fn read_job_runs(
    db_path: &Path,
    job_id: Option<&str>,
) -> Vec<(String, String, Option<String>)> {
    let conn = rusqlite::Connection::open(db_path).expect("运行表应可直读");
    let mut stmt = conn
        .prepare(
            "SELECT id, status, error FROM job_runs
             WHERE (?1 IS NULL OR job_id = ?1) ORDER BY started_at ASC, id ASC",
        )
        .expect("运行查询应可编译");
    let rows = stmt
        .query_map([job_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("运行查询应可执行");
    rows.map(|row| row.expect("运行行应可读取")).collect()
}

/// 在 `dir` 里跑 `npm test`，返回退出状态（输出丢弃，断言只看退出码）。
pub fn run_npm_test(dir: &Path) -> ExitStatus {
    Command::new("npm")
        .arg("test")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("本机应装有 npm 与 node")
}

/// 装配一次 `ignis` 调用：统一注入测试环境变量，stdin 固定接 `/dev/null`。
///
/// stdin 必须是「非 tty」，审批模式才会按无旗标那条路径落到 `Unattended`——不能被 runner
/// 恰好挂在终端上这件事影响。`home` 与 `workspace` 一律指向用例自己的临时目录。
pub fn ignis_cmd(home: &Path, workspace: &Path, base_url: &str) -> Command {
    let mut cmd = base_cmd(home, workspace, base_url);
    cmd.env("IGNIS_API_KEY", "test-key");
    cmd
}

/// 密钥只走 `api_key_env`（默认 `OPENAI_API_KEY`）的变体：**不设** `IGNIS_API_KEY`，并显式把
/// 它从子进程环境里摘掉——运行者环境里恰好有 `IGNIS_API_KEY` 时，这条路径会悄悄变成
/// 「配置文件/覆盖密钥」路径，回归就观测不到了。
pub fn ignis_cmd_with_api_key_env(
    home: &Path,
    workspace: &Path,
    base_url: &str,
    key: &str,
) -> Command {
    let mut cmd = base_cmd(home, workspace, base_url);
    cmd.env_remove("IGNIS_API_KEY").env("OPENAI_API_KEY", key);
    cmd
}

/// 两个变体的共同部分：home、工作区、假模型地址与固定的 stdio。
fn base_cmd(home: &Path, workspace: &Path, base_url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ignis"));
    cmd.env("IGNIS_HOME", home)
        .env("IGNIS_WORKSPACE", workspace)
        .env("IGNIS_BASE_URL", base_url)
        .env("IGNIS_MODEL", "test-model")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// 假模型的一步：发起一次不带文本的工具调用。
pub fn tool_call(name: &str, args: Value) -> ScriptStep {
    ScriptStep::ToolCalls {
        text: String::new(),
        calls: vec![(name.to_string(), args)],
    }
}

/// 进程输出汇总（stdout + stderr），断言失败时贴进消息里便于定位。
pub fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
