//! 守护进程生命周期的真实进程用例（真二进制 + 真子进程）：
//!
//! 1. `ignis daemon start --foreground`（spec §6.9 的 `ExecStart`）**真的前台运行**：pidfile 里
//!    写着刚拉起的那个直接子进程自己的 pid，进程在断言期间一直是本进程的直接子进程（自守护实现会
//!    让父进程立刻退掉、pidfile 里是别人的 pid），SIGTERM 后自己退出 0 并摘掉 pidfile。
//! 2. `ignis daemon stop` 的等待窗口要盖过调度循环的优雅收尾（`SHUTDOWN_GRACE` = 10s）：
//!    一轮在飞运行停在 `sleep 7` 上，`stop` 必须等它自然收尾——运行行收成 `success`、守护进程
//!    退出码 0、pidfile 摘掉。旧行为（5s 就升级 SIGKILL）会让这一轮留在 `running`、进程被信号
//!    打死，所以这条用例同时是「STOP_TIMEOUT > SHUTDOWN_GRACE」的守护。
//!
//! 两条用例都直读用例自己的临时库（绝不碰真实 `~/.ignis`），收尾一律 SIGTERM → 必要 SIGKILL，
//! 任何断言失败都不留后台守护进程。

mod common;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use common::{TempDir, ignis_cmd, latest_task, output_text, read_job_runs, tool_call};
use ignis_gateway::daemon::{DaemonStatus, status};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel};
use serde_json::json;

/// 调度任务名（断言落库用）。
const JOB_NAME: &str = "disk-watch";
/// 创建调度任务的一轮输入。
const CREATE_PROMPT: &str = "每隔 2 秒检查测试服务器";
/// 调度任务自己的 prompt（内容不与挂起命令逐字重合：断言要能在日志里区分「prompt 提到了」与
/// 「真的调用了」）。
const JOB_PROMPT: &str = "按脚本挂住这一轮，然后收尾";
/// 调度间隔：任务书钉死 2 秒，启动后最快 2 秒内到点。
const INTERVAL: &str = "2s";
/// 挂起命令：跨过旧的 5s SIGKILL 窗口、又在 10s 优雅收尾之内。
const HANG_COMMAND: &str = "sleep 7";
/// 轮询步长。
const POLL: Duration = Duration::from_millis(100);

/// 轮询等条件成立（100ms 一次）；超时 panic 并说明在等什么。
fn wait_for(timeout: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return;
        }
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(POLL);
    }
}

/// 给进程发信号：走 `sh -c 'kill -<sig> <pid>'`（`kill` 是 shell 内建，测试侧不引 libc）。
fn signal(pid: u32, sig: &str) {
    let sent = Command::new("sh")
        .args(["-c", &format!("kill -{sig} {pid}")])
        .status()
        .expect("sh 应可执行");
    assert!(sent.success(), "给 pid {pid} 发 SIG{sig} 失败：{sent}");
}

/// `{home}/logs` 下的任意一份任务日志里是否出现过 `needle`。
///
/// 按内容找而不是按「最新一份」找：创建 job 的那次 CLI 运行也会写日志，谁先谁后不必假设。
fn any_task_log_contains(home: &Path, needle: &str) -> bool {
    let Ok(entries) = fs::read_dir(home.join("logs")) else {
        return false;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        let path = entry.path();
        path.extension().is_some_and(|ext| ext == "jsonl")
            && fs::read_to_string(&path).is_ok_and(|text| text.contains(needle))
    })
}

/// 前台守护进程子进程 + 收尾守卫：`ignis daemon start --foreground` 的直接子进程。
///
/// 析构时 SIGTERM 收尾，2s 没退就 SIGKILL——任何断言失败都不留后台守护进程。
struct ForegroundDaemon {
    child: Child,
    pid: u32,
}

impl ForegroundDaemon {
    /// 拉起并等它把 pidfile 写成自己的 pid（就是「前台运行」的判据：自守护会把父进程退掉）。
    fn start(home: &Path, workspace: &Path, base_url: &str) -> Self {
        let child = ignis_cmd(home, workspace, base_url)
            .args(["daemon", "start", "--foreground"])
            .stdin(Stdio::null())
            .spawn()
            .expect("ignis 二进制应可执行");
        let pid = child.id();
        let pid_path = home.join("ignis.pid");
        wait_for(
            Duration::from_secs(10),
            "前台守护进程写下自己的 pidfile",
            || {
                fs::read_to_string(&pid_path)
                    .ok()
                    .is_some_and(|raw| raw.trim() == pid.to_string())
            },
        );
        Self { child, pid }
    }

    fn is_running(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("查询守护进程状态失败")
            .is_none()
    }

    /// 等进程退出并读走 stdout/stderr（SIGTERM 之后调用）。
    fn wait_for_exit(&mut self, timeout: Duration) -> (ExitStatus, String) {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("查询守护进程状态失败") {
                let stdout = read_stream(self.child.stdout.take());
                let stderr = read_stream(self.child.stderr.take());
                return (status, format!("stdout:\n{stdout}\nstderr:\n{stderr}"));
            }
            assert!(
                Instant::now() < deadline,
                "守护进程没有在 {timeout:?} 内退出"
            );
            std::thread::sleep(POLL);
        }
    }
}

/// 读空一个子进程输出管道；进程已退出，内容在管道缓冲里，不会阻塞。
fn read_stream(stream: Option<impl io::Read>) -> String {
    let mut text = String::new();
    if let Some(mut stream) = stream {
        let _ = stream.read_to_string(&mut text);
    }
    text
}

impl Drop for ForegroundDaemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = Command::new("sh")
                .args(["-c", &format!("kill -TERM {}", self.pid)])
                .status();
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

/// 用例环境：临时 home / 工作区 + 假模型（宿主的 [`TempDir`] 由用例自己持有，析构时清理）。
struct Env {
    home: PathBuf,
    workspace: PathBuf,
    model: ignis_runtime::test_support::ScriptedModelHandle,
}

impl Env {
    fn new(dir: &TempDir, script: Vec<ScriptStep>) -> Self {
        let home = dir.join("home");
        let workspace = dir.join("workspace");
        fs::create_dir_all(&workspace).expect("工作区应可创建");
        Self {
            home,
            workspace,
            model: ScriptedModel::start(script),
        }
    }

    fn base_url(&self) -> String {
        self.model.base_url()
    }

    /// 一次 `ignis …` 调用（同一套环境变量）。
    fn ignis(&self, args: &[&str]) -> std::process::Output {
        ignis_cmd(&self.home, &self.workspace, &self.model.base_url())
            .args(args)
            .output()
            .expect("ignis 二进制应可执行")
    }

    fn db(&self) -> PathBuf {
        self.home.join("ignis.db")
    }

    fn pid_path(&self) -> PathBuf {
        self.home.join("ignis.pid")
    }
}

/// `ignis daemon start --foreground` 前台运行：pidfile 是直接子进程自己的 pid，SIGTERM 后自己
/// 退出 0 并摘掉 pidfile（而不是「父进程退掉、后台留一个 daemon run」的自守护行为）。
#[test]
fn daemon_start_foreground_runs_in_the_foreground_until_sigterm() {
    let dir = TempDir::new("daemon-foreground");
    let env = Env::new(&dir, Vec::new());

    let mut daemon = ForegroundDaemon::start(&env.home, &env.workspace, &env.base_url());
    // 前台运行：进程一直在，且 pidfile 里就是它（`start` 的自守护实现会让这条断言落空——
    // 父进程带着 stdout 立刻退出，pidfile 里是 re-exec 出来的另一个 pid）
    assert!(
        daemon.is_running(),
        "`daemon start --foreground` 不该把父进程退掉（那是自守护）"
    );
    assert_eq!(
        fs::read_to_string(env.pid_path()).unwrap().trim(),
        daemon.pid.to_string()
    );
    assert_eq!(
        status(&env.home),
        DaemonStatus::Running {
            pid: daemon.pid as i32
        }
    );

    signal(daemon.pid, "TERM");
    let (exit, text) = daemon.wait_for_exit(Duration::from_secs(15));

    assert!(exit.success(), "SIGTERM 后应正常退出：{exit}\n{text}");
    // 前台形态的自我证明：装配与就绪行打在自己（被继承的）stdout 上
    assert!(text.contains("daemon: ready"), "{text}");
    assert!(text.contains("received SIGTERM"), "{text}");
    assert!(!env.pid_path().exists(), "优雅退出应摘掉 pidfile：{text}");
    assert_eq!(status(&env.home), DaemonStatus::Stopped);
}

/// `daemon stop` 等满在飞运行的收尾（跨过旧的 5s SIGKILL 窗口、不超过 12.5s）：运行行收成
/// `success`，守护进程自己退出 0。
#[test]
fn daemon_stop_waits_out_an_in_flight_run_instead_of_sigkilling_at_five_seconds() {
    let dir = TempDir::new("daemon-stop-grace");
    let env = Env::new(
        &dir,
        vec![
            // 第一次 CLI 运行：创建调度任务
            tool_call(
                "schedule_create",
                json!({"name": JOB_NAME, "prompt": JOB_PROMPT, "schedule": INTERVAL}),
            ),
            ScriptStep::Text("已创建调度任务".into()),
            // 守护进程的第一轮：挂在 `sleep 7` 上（收尾时它还在飞）
            tool_call("shell_run", json!({"command": HANG_COMMAND})),
            ScriptStep::Text("挂起的一轮正常收尾".into()),
        ],
    );

    let created = env.ignis(&["run", CREATE_PROMPT, "--yes"]);
    assert_eq!(
        created.status.code(),
        Some(0),
        "创建 job 的 CLI 运行应成功：{}",
        output_text(&created)
    );
    let db = env.db();

    let mut daemon = ForegroundDaemon::start(&env.home, &env.workspace, &env.base_url());
    // 等第一轮调度运行起跑（运行行由守护进程写出），再等它真的进到 shell 里（任务日志里钉着
    // 工具调用的实参）
    wait_for(
        Duration::from_secs(15),
        "第一轮调度运行起跑",
        || !read_job_runs(&db, None).is_empty(),
    );
    wait_for(
        Duration::from_secs(15),
        "第一轮调度运行进入 sleep",
        || any_task_log_contains(&env.home, HANG_COMMAND),
    );
    assert_eq!(
        read_job_runs(&db, None).len(),
        1,
        "此刻应恰好有一行在飞运行"
    );
    assert!(daemon.is_running(), "守护进程应在跑");

    let started = Instant::now();
    let stopped = env.ignis(&["daemon", "stop"]);
    let elapsed = started.elapsed();
    let text = output_text(&stopped);

    assert_eq!(stopped.status.code(), Some(0), "{text}");
    assert!(
        String::from_utf8_lossy(&stopped.stdout).contains("daemon stopped"),
        "{text}"
    );
    // 旧行为（STOP_TIMEOUT = 5s）会在这里 SIGKILL：等待必须跨过 5s（在飞运行自然收尾），
    // 又不能超过 12s 的 STOP_TIMEOUT 加收尾余量
    assert!(
        elapsed >= Duration::from_millis(5_200),
        "stop 没等满在飞运行的收尾（{elapsed:?}）——5s 就 SIGKILL 了？"
    );
    assert!(
        elapsed <= Duration::from_millis(12_500),
        "stop 等过头了（{elapsed:?}）"
    );

    let (exit, daemon_text) = daemon.wait_for_exit(Duration::from_secs(5));
    assert!(
        exit.success(),
        "守护进程应自己（优雅）退出，而不是被信号打死：{exit}\n{daemon_text}"
    );
    assert!(!env.pid_path().exists(), "优雅退出应摘掉 pidfile");
    assert_eq!(status(&env.home), DaemonStatus::Stopped);

    // 在飞运行正常收尾：运行行与任务行都是终态（被 5s SIGKILL 打断的话两行都会留在 running）
    let runs = read_job_runs(&db, None);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].1, "success", "{runs:?}");
    let (task_id, task_status, task_error) = latest_task(&db);
    assert_eq!(
        (task_status.as_str(), task_error.as_str()),
        ("success", ""),
        "任务行 {task_id} 应收成终态"
    );
}
