//! Demo C 端到端：真二进制（CLI + `daemon run --foreground` 子进程）+ 脚本化假模型，
//! 验证「CLI 退出后守护进程照常调度 → 告警 → 去重 → 暂停/恢复 → 重启不补跑且不重复」。
//!
//! 六步与任务书一一对应：
//! 1. 创建：`ignis run … --yes` 里模型调 `schedule_create`，落一条 active job + 一条审批留痕；
//! 2. 守护进程接管：CLI 进程早已退出，运行行由守护进程写出（`tasks.kind = schedule`）；
//! 3. 告警：`notify_user` 落 `warning` 行，`ignis alerts` 看得到，桌面渠道降级写日志；
//! 4. 去重：窗口内第二次同标题只落一行（第二轮回执是「已去重」，不去打扰第二次）；
//! 5. 暂停/恢复：暂停后不再新起运行，恢复后继续；
//! 6. 重启：被 SIGKILL 打断的运行在重启时被清成 `failed`/`interrupted`，不补跑也不重跑。
//!
//! 关键时序：调度循环在「没有 active job 到期」时最多睡 `RESCAN_SECS`（30s）才重扫一遍，所以
//! 暂停一旦被循环看到，恢复最迟要在那 30s 窗口的前 1/3 落下才能在 10s 内被观察到。测试按守护
//! 进程自己留下的那行「暂停后迟到的一轮」对齐恢复时刻：那一轮之后循环把 30s 窗口睡满，再等
//! 2s 间隔才到下一轮，恢复放在它前 9s 处（≤10s 的断言留 1s 余量）。整条用例实测约 42s：其中
//! 暂停/恢复那一步就占了 ~25s（30s 重扫窗口不可压缩），其余五步 ~17s，断言一条没削。
//!
//! 假模型脚本按请求顺序消费（一次运行恰好三步），所以「第几轮拿哪几步」是确定的：前两轮是
//! 「跑夹具 → notify_user → 收尾文本」，第三轮（暂停后迟到的那轮）与第四轮（恢复后的第一轮）
//! 落在 `sleep 8` 上——第四轮就是要被 SIGKILL 打断的那一轮。

mod common;

use std::fs::{self, OpenOptions};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{
    TempDir, copy_dir, ignis_cmd, latest_task, output_text, read_alerts, read_approvals,
    read_job_runs, tool_call,
};
use ignis_gateway::RESCAN_SECS;
use ignis_gateway::daemon::{DaemonStatus, status};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel};
use serde_json::json;

/// 创建工作流里用户说的话（任务书逐字）。
const CREATE_PROMPT: &str = "每隔 2 秒检查测试服务器，磁盘超过 85% 就通知我";
/// 调度任务名（断言落库用）。
const JOB_NAME: &str = "disk-watch";
/// 调度任务自己的 prompt：告诉它跑哪个脚本、什么阈值告警（与脚本里的命令逐字对上）。
const JOB_PROMPT: &str =
    "运行 examples/test-server/check-health.sh --simulate 92，磁盘超过 85% 就用 notify_user 告警";
/// 调度间隔：任务书钉死 2 秒。
const INTERVAL: &str = "2s";
/// 夹具里那条检查命令（脚本里的工具调用逐字用它）。
const HEALTH_COMMAND: &str = "examples/test-server/check-health.sh --simulate 92";
/// 告警标题（脚本与断言共用）。
const ALERT_TITLE: &str = "磁盘使用率过高";
/// 挂起命令：让一轮运行卡在 shell 里等着被打断（任务书建议的 `sleep 8`）。
const HANG_COMMAND: &str = "sleep 8";
/// 轮询步长（任务书钉死 200ms）。
const POLL: Duration = Duration::from_millis(200);
/// 恢复时刻的对齐量（毫秒）：从迟到那轮的起点算起。
///
/// 推导自守护进程自己那个重扫窗口（[`RESCAN_SECS`]，不再写死一个副本）：迟到那轮之后循环把整段
/// 窗口睡满，恢复点落在**窗口结束前 7s**，所以唤醒时立刻看见 active job，恢复后 7s（≤10s 断言
/// 留 3s 余量）就见到新一轮。窗口一旦调整，这个量跟着走。
///
/// 注意不能落在窗口结束之后：那要等下一个整窗口（`RESCAN_SECS` + 2s 间隔 ≈ 32s）才被观察到，
/// 10s 的断言会失败——「恢复至今 ≤10s」的语义本身就要求恢复落在当前窗口内。
const RESUME_LEAD_MS: i64 = RESCAN_SECS * 1000 - 7_000;

/// 夹具 `examples/test-server` 的绝对路径。
fn test_server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/test-server")
}

/// 当前 epoch 毫秒：与库里 `job_runs.started_at` / `tasks.started_at` 同一把尺子。
fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// 轮询等条件成立（200ms 一次）；超时 panic 并说明在等什么。
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

/// `jobs` 表里按名字取出的那一行（只带断言要看的列）。
#[derive(Debug)]
struct JobRow {
    id: String,
    status: String,
    schedule_kind: String,
    schedule_expr: String,
    kernel: String,
    profile_json: String,
}

/// 按名字读一条 job 行（不存在就 panic：本用例里它必然存在）。
fn read_job(db: &Path, name: &str) -> JobRow {
    rusqlite::Connection::open(db)
        .expect("库文件应可直读")
        .query_row(
            "SELECT id, status, schedule_kind, schedule_expr, kernel, profile_json
             FROM jobs WHERE name = ?1",
            [name],
            |row| {
                Ok(JobRow {
                    id: row.get(0)?,
                    status: row.get(1)?,
                    schedule_kind: row.get(2)?,
                    schedule_expr: row.get(3)?,
                    kernel: row.get(4)?,
                    profile_json: row.get(5)?,
                })
            },
        )
        .expect("本次创建应写入一条 job 行")
}

/// 某个运行行回填的任务 id。
fn run_task_id(db: &Path, run_id: &str) -> Option<String> {
    rusqlite::Connection::open(db)
        .expect("库文件应可直读")
        .query_row(
            "SELECT task_id FROM job_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .expect("运行行应可读")
}

/// 某个 job 最后一个运行行的开始时刻（epoch 毫秒）。
///
/// 暂停/恢复都要等循环下一次醒来才生效，这一行就是「唤醒点」在库里的投影：迟到那轮的起点之后
/// 循环把 30s 重扫窗口睡满，恢复后的第一轮必然从它往后 30s 扫描 + 2s 间隔处出现。
fn last_run_started_at(db: &Path, job_id: &str) -> i64 {
    rusqlite::Connection::open(db)
        .expect("库文件应可直读")
        .query_row(
            "SELECT started_at FROM job_runs WHERE job_id = ?1
             ORDER BY started_at DESC, id DESC LIMIT 1",
            [job_id],
            |row| row.get(0),
        )
        .expect("本次运行应写入一条运行行")
}

/// `tasks.kind`。
fn task_kind(db: &Path, task_id: &str) -> String {
    rusqlite::Connection::open(db)
        .expect("库文件应可直读")
        .query_row("SELECT kind FROM tasks WHERE id = ?1", [task_id], |row| {
            row.get(0)
        })
        .expect("任务行应可读")
}

/// 任务日志（`{home}/logs/{task_id}.jsonl`）的原文。
fn task_log(home: &Path, task_id: &str) -> String {
    fs::read_to_string(home.join("logs").join(format!("{task_id}.jsonl"))).expect("任务日志应存在")
}

/// 最新一份任务日志的原文：文件按任务 id 命名，任务 id 是 uuid v7（时间有序），文件名升序就是
/// 时间升序。
///
/// 运行还在飞的时候 `job_runs.task_id` 还没回填（回填发生在运行收尾），要认领「正在跑的那一轮」
/// 的日志只能按最新的一份找。
fn newest_task_log(home: &Path) -> String {
    let newest = fs::read_dir(home.join("logs"))
        .expect("任务日志目录应存在")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            path.extension()
                .is_some_and(|ext| ext == "jsonl")
                .then_some(path)
        })
        .max()
        .expect("应至少写出一份任务日志");
    fs::read_to_string(newest).expect("任务日志应可读")
}

/// 时间线打点：时序断言（30s 重扫窗口、10s 恢复窗口）出问题时要能一眼看出卡在哪一步。
fn mark(started: Instant, label: &str) {
    eprintln!("demo c @{:>6.2}s {label}", started.elapsed().as_secs_f64());
}

/// 在 PATH 里找可执行文件（找不到返回 `None`）。
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}

/// 造一个只放夹具要用的几条命令的 PATH 目录。
///
/// 守护进程（以及它 spawn 的 shell）只认这个目录：`notify-send` 不在里面，桌面通知渠道必然走
/// 「降级为仅落库与日志」那条路——真机上装了 notify-send 时 spawn 会成功、这行日志就不出现了，
/// 那条断言会变成环境相关的。
fn tools_bin_dir(dir: &Path) {
    fs::create_dir_all(dir).expect("PATH 目录应可创建");
    for name in [
        "sh", "sleep", "df", "awk", "grep", "cut", "head", "tr", "sed",
    ] {
        if let Some(tool) = which(name) {
            let _ = symlink(&tool, dir.join(name));
        }
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

/// 前台守护进程子进程 + 收尾守卫。
///
/// 生产入口是隐藏的 `ignis daemon run --foreground`（`daemon start` 是自守护包装，会把输出丢进
/// `{home}/logs/daemon.log`；这里要自己拿进程句柄，所以直接前台跑）。stdout/stderr 追加到用例
/// 自己的日志文件，绝不与断言读的流混在一起。析构时 SIGTERM 收尾，到点没退就 SIGKILL——任何
/// 断言失败都不留后台守护进程。
struct Daemon {
    child: Child,
    pid: u32,
}

impl Daemon {
    /// 以测试环境（与 `ignis_cmd` 同一套 `IGNIS_*`，另把 PATH 收窄到 [`tools_bin_dir`]）拉起
    /// 守护进程，并等它把 pidfile 写成自己的 pid。
    fn start(
        home: &Path,
        workspace: &Path,
        base_url: &str,
        bin_dir: &Path,
        log_dir: &Path,
    ) -> Daemon {
        let child = Command::new(env!("CARGO_BIN_EXE_ignis"))
            .args(["daemon", "run", "--foreground"])
            .env("IGNIS_HOME", home)
            .env("IGNIS_WORKSPACE", workspace)
            .env("IGNIS_BASE_URL", base_url)
            .env("IGNIS_MODEL", "test-model")
            .env("IGNIS_API_KEY", "test-key")
            .env("PATH", bin_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(open_log(&Self::stdout_log(log_dir))))
            .stderr(Stdio::from(open_log(&Self::stderr_log(log_dir))))
            .spawn()
            .expect("ignis 二进制应可执行");
        let pid = child.id();
        // pidfile 里必须出现「本次拉起的那个子进程自己的 pid」：重启时文件还在（上一次被
        // SIGKILL 没摘），所以不能只看文件存在，得看内容是不是新 pid
        let pid_path = home.join("ignis.pid");
        wait_for(
            Duration::from_secs(10),
            "守护进程写下自己的 pidfile",
            || {
                fs::read_to_string(&pid_path)
                    .ok()
                    .is_some_and(|raw| raw.trim() == pid.to_string())
            },
        );
        Daemon { child, pid }
    }

    fn stdout_log(log_dir: &Path) -> PathBuf {
        log_dir.join("daemon.stdout.log")
    }

    fn stderr_log(log_dir: &Path) -> PathBuf {
        log_dir.join("daemon.stderr.log")
    }

    fn is_running(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("查询守护进程状态失败")
            .is_none()
    }

    /// SIGTERM 优雅停（守护进程会先等在飞运行收尾），等它退出并断言退出成功。
    fn stop(&mut self) -> ExitStatus {
        signal(self.pid, "TERM");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(exit) = self.child.try_wait().expect("查询守护进程状态失败") {
                return exit;
            }
            assert!(
                Instant::now() < deadline,
                "守护进程收到 SIGTERM 后没有在 15s 内退出"
            );
            std::thread::sleep(POLL);
        }
    }

    /// SIGKILL 硬杀：模拟「守护进程在运行途中被打断」，在飞运行行会留在 `running`。
    fn kill_hard(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
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

/// 打开（必要时创建）追加模式的日志文件。
fn open_log(path: &Path) -> fs::File {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("日志文件应可创建")
}

/// 假模型脚本：创建 → 两轮「检查 + 告警」→ 两轮挂起 → 重启后的一轮 → 尾部空转兜底。
///
/// 每个请求消耗一步：一次调度运行恰好三步（工具调用 + 收尾文本），所以第几轮拿哪几步是确定的
/// （守护进程侧没有别的消费者）。尾部留足纯文本步：晚到的运行拿到它就收尾，不会把脚本耗尽
/// （耗尽时假模型回 500，晚到的运行会以失败收场，那是假模型的噪声、不是被测行为）。
fn script() -> Vec<ScriptStep> {
    let mut steps = vec![
        tool_call(
            "schedule_create",
            json!({"name": JOB_NAME, "prompt": JOB_PROMPT, "schedule": INTERVAL}),
        ),
        ScriptStep::Text(format!(
            "已创建调度任务 {JOB_NAME}（每 {INTERVAL} 检查一次）。"
        )),
    ];
    // 两轮完整运行：第一轮落告警，第二轮同标题命中去重窗口
    for _ in 0..2 {
        steps.push(tool_call("shell_run", json!({"command": HEALTH_COMMAND})));
        steps.push(tool_call(
            "notify_user",
            json!({"severity": "warning", "title": ALERT_TITLE}),
        ));
        steps.push(ScriptStep::Text("已检查并告警".into()));
    }
    // 两轮挂起：暂停后迟到的那一轮、恢复后的第一轮（后者要被 SIGKILL 打断）
    for _ in 0..2 {
        steps.push(tool_call("shell_run", json!({"command": HANG_COMMAND})));
        steps.push(ScriptStep::Text("ok".into()));
    }
    // 重启后的第一轮：正常的检查 + 告警（同标题，仍在去重窗口内）
    steps.push(tool_call("shell_run", json!({"command": HEALTH_COMMAND})));
    steps.push(tool_call(
        "notify_user",
        json!({"severity": "warning", "title": ALERT_TITLE}),
    ));
    steps.push(ScriptStep::Text("已检查并告警".into()));
    steps.extend((0..30).map(|_| ScriptStep::Text("ok".into())));
    steps
}

#[test]
fn demo_c_daemon_keeps_scheduling_after_the_cli_exits() {
    let started = Instant::now();
    let dir = TempDir::new("demo-c");
    let home = dir.join("home");
    let project = dir.join("project");
    let log_dir = dir.join("logs");
    fs::create_dir_all(&log_dir).expect("日志目录应可创建");
    // 工作区是夹具的独立副本：模型脚本里的相对命令 `examples/test-server/check-health.sh`
    // 必须在运行的工作区里解析得到（调度运行的命令 cwd 就是 job 的 workspace）
    copy_dir(&test_server_dir(), &project.join("examples/test-server"));
    let bin_dir = dir.join("bin");
    tools_bin_dir(&bin_dir);

    let model = ScriptedModel::start(script());
    let db = home.join("ignis.db");
    let env = || ignis_cmd(&home, &project, &model.base_url());

    // ---- 1. 创建：`ignis run … --yes` 走真创建流程，模型调 `schedule_create` ----
    let created = env()
        .args(["run", CREATE_PROMPT, "--yes"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&created);
    assert_eq!(created.status.code(), Some(0), "{text}");

    let job = read_job(&db, JOB_NAME);
    assert_eq!(job.status, "active", "新 job 应 active：{job:?}");
    assert_eq!(job.schedule_kind, "interval");
    assert_eq!(job.schedule_expr, INTERVAL);
    assert_eq!(job.kernel, "native");
    // profile 是默认白名单：含调度运行的告警出口 notify_user（还有查文件与跑脚本的两件）
    for tool in ["fs_read", "fs_list", "shell_run", "notify_user"] {
        assert!(
            job.profile_json.contains(&format!("\"{tool}\"")),
            "默认 profile 应含 {tool}：{}",
            job.profile_json
        );
    }
    // 创建动作留了一条审批留痕：`--yes` 预授权放行（schedule_create 的审计）
    let (create_task, create_status, create_error) = latest_task(&db);
    assert_eq!(
        (create_status.as_str(), create_error.as_str()),
        ("success", ""),
        "{text}"
    );
    assert_eq!(
        read_approvals(&db, &create_task),
        [(
            "schedule_create".to_owned(),
            "allow".to_owned(),
            "preauthorized".to_owned()
        )],
        "{text}"
    );
    mark(started, "1 创建完成（job 落库 + 审批留痕）");

    // ---- 2. 守护进程接管：CLI 进程已退出（`output()` 已收尸），跑的人只能是守护进程 ----
    let mut daemon = Daemon::start(&home, &project, &model.base_url(), &bin_dir, &log_dir);
    let runs = || read_job_runs(&db, Some(&job.id));
    wait_for(
        Duration::from_secs(10),
        "守护进程的第一次调度运行收尾",
        || {
            runs()
                .first()
                .is_some_and(|(_, run_status, _)| run_status == "success")
        },
    );
    let first_run = runs().remove(0);
    assert!(daemon.is_running(), "守护进程应在跑");
    // 决定性的证据：这一轮的任务行 kind = schedule——只有守护进程的调度运行会这么写，
    // `ignis run` 写的是 run
    let first_task = run_task_id(&db, &first_run.0).expect("运行行应回填 task_id");
    assert_eq!(task_kind(&db, &first_task), "schedule", "{text}");
    // 守护进程自己的日志也点名了这一轮
    wait_for(
        Duration::from_secs(5),
        "守护进程日志里的 run started",
        || {
            fs::read_to_string(Daemon::stdout_log(&log_dir))
                .unwrap_or_default()
                .contains(&format!("run started: {JOB_NAME}"))
        },
    );
    mark(started, "2 首次调度运行收尾（kind=schedule）");

    // ---- 3. 告警：notify_user 落 warning 行，CLI 看得到，桌面渠道降级写日志 ----
    let alerts = || read_alerts(&db);
    let titled = |rows: &[(String, String, i64, Option<i64>)]| {
        rows.iter()
            .filter(|(_, title, ..)| title == ALERT_TITLE)
            .cloned()
            .collect::<Vec<_>>()
    };
    wait_for(Duration::from_secs(10), "warning 告警落库", || {
        alerts()
            .iter()
            .any(|(severity, title, ..)| severity == "warning" && title == ALERT_TITLE)
    });
    let row = titled(&alerts()).remove(0);
    assert_eq!((row.0.as_str(), row.1.as_str()), ("warning", ALERT_TITLE));
    assert_eq!(row.3, None, "刚落库的告警应未读");
    // `ignis alerts`（同一套环境）把它列出来
    let listed = env().arg("alerts").output().expect("ignis 二进制应可执行");
    let listed_text = output_text(&listed);
    assert_eq!(listed.status.code(), Some(0), "{listed_text}");
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains(ALERT_TITLE),
        "`ignis alerts` 应列出这条告警：{listed_text}"
    );
    // 桌面渠道在这台机器上不可用（PATH 里没有 notify-send）：守护进程日志里是那条「降级为
    // 仅落库与日志」的警告
    let daemon_err =
        fs::read_to_string(Daemon::stderr_log(&log_dir)).expect("守护进程 stderr 日志应存在");
    assert!(
        daemon_err.contains("notify-send") && daemon_err.contains("降级"),
        "守护进程日志应留下桌面通知的降级行：{daemon_err}"
    );
    mark(started, "3 告警落库、CLI 可见、桌面渠道降级");

    // ---- 4. 去重：窗口内第二次同标题只留一行 ----
    wait_for(
        Duration::from_secs(10),
        "第二次调度运行收尾",
        || runs().len() >= 2 && runs()[1].1 == "success",
    );
    let second_run = runs().remove(1);
    let second_task = run_task_id(&db, &second_run.0).expect("运行行应回填 task_id");
    // 第二轮真的又调了一次 notify_user：工具结果里是去重回执（没有第二次打扰）
    wait_for(
        Duration::from_secs(5),
        "第二轮的去重回执落盘",
        || task_log(&home, &second_task).contains("告警已去重"),
    );
    // 落库的仍只有一行（同一标题、同一把 15 分钟去重键）
    assert_eq!(
        titled(&alerts()).len(),
        1,
        "去重窗口内同标题只该留一行：{:?}",
        titled(&alerts())
    );
    mark(started, "4 去重确认（第二次 notify_user 只回执、不落新行）");

    // ---- 5. 暂停/恢复 ----
    let before_pause = runs().len();
    let paused = env()
        .args(["schedule", "pause", &job.id])
        .output()
        .expect("ignis 二进制应可执行");
    let paused_text = output_text(&paused);
    assert_eq!(paused.status.code(), Some(0), "{paused_text}");
    assert!(
        String::from_utf8_lossy(&paused.stdout).contains(&job.id),
        "回执应点名完整 id：{paused_text}"
    );
    let pause_ms = now_millis();
    mark(started, "5a 暂停已发出");
    // 暂停在循环下一次醒来时生效：先把「暂停前已经排好、还是会跑一次」的那轮等出来，
    // 再开始数数——这一轮也正是循环把 30s 重扫窗口睡满的起点
    wait_for(
        Duration::from_secs(8),
        "暂停后迟到的那一轮",
        || runs().len() > before_pause,
    );
    let settled = runs().len();
    let tick_ms = last_run_started_at(&db, &job.id);
    assert!(
        tick_ms >= pause_ms,
        "迟到的那轮应在暂停之后 {pause_ms} -> {tick_ms}"
    );
    mark(started, "5b 暂停后迟到的那轮（30s 重扫窗口的起点）");
    // 暂停期间不再新起运行（任务书口径：≥2 个间隔）
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(runs().len(), settled, "暂停期间不得新起运行");
    // 恢复要落在「30s 重扫窗口 + 1 个间隔」的下一轮之前，才能被 10s 的断言观察到：
    // 按迟到那轮 +23s 对齐（见模块头），这段等待同样是「暂停期间一行不落」的继续
    let resume_at = tick_ms + RESUME_LEAD_MS;
    std::thread::sleep(Duration::from_millis(
        (resume_at - now_millis()).max(0) as u64
    ));
    assert_eq!(runs().len(), settled, "等恢复的整段时间里也不得新起运行");
    let resume_ms = now_millis();
    let resumed = env()
        .args(["schedule", "resume", &job.id])
        .output()
        .expect("ignis 二进制应可执行");
    let resumed_text = output_text(&resumed);
    assert_eq!(resumed.status.code(), Some(0), "{resumed_text}");
    mark(started, "5c 恢复已发出");
    // 恢复后 ≤10s 新起一轮：守护进程在下一个唤醒点重读 active job
    wait_for(Duration::from_secs(11), "恢复后的新一轮", || {
        runs().len() > settled
    });
    let fresh_ms = last_run_started_at(&db, &job.id);
    assert!(
        fresh_ms >= resume_ms && fresh_ms - resume_ms <= 10_000,
        "恢复应在 10s 内被观察到：resume={resume_ms} run={fresh_ms}"
    );
    mark(started, "5d 恢复后的新一轮（≤10s 窗口内）");

    // ---- 6. 重启：先打断一轮，再看重启怎么收场 ----
    // 恢复后的第一轮落在脚本的 `sleep 8` 上：正常运行 0.3s 内收尾，挂住的这轮会一直 running
    wait_for(
        Duration::from_secs(12),
        "挂住的那一轮（running 超过 1.2s）",
        || {
            runs()
                .last()
                .is_some_and(|(_, run_status, _)| run_status == "running")
                && now_millis() - last_run_started_at(&db, &job.id) >= 1_200
        },
    );
    let interrupted = runs().last().expect("至少有一轮运行").0.clone();
    // 它真的在跑脚本里那一轮挂起（正在跑那轮的日志里钉着工具调用的实参）
    wait_for(
        Duration::from_secs(5),
        "挂起那轮的工具调用落盘",
        || newest_task_log(&home).contains(HANG_COMMAND),
    );
    mark(started, "6a 挂起那轮已在 shell 里（sleep 8）");

    // 硬杀：运行行留在 running，pidfile 也留在原地（下一次启动自己覆盖它）
    daemon.kill_hard();
    assert_eq!(
        runs()
            .iter()
            .find(|(id, ..)| id == &interrupted)
            .map(|(_, run_status, _)| run_status.as_str()),
        Some("running"),
        "被 SIGKILL 打断的运行应留在 running"
    );
    let runs_before_restart = runs().len();

    let restart_ms = now_millis();
    let mut daemon = Daemon::start(&home, &project, &model.base_url(), &bin_dir, &log_dir);
    // (b) 清场：遗留的 running 被收成 failed / interrupted——不是被重跑一遍
    wait_for(
        Duration::from_secs(5),
        "遗留运行被清成 failed/interrupted",
        || {
            runs().iter().any(|(id, run_status, error)| {
                id == &interrupted
                    && run_status == "failed"
                    && error.as_deref() == Some("interrupted")
            })
        },
    );
    mark(
        started,
        "6b 重启并清场（遗留 running -> failed/interrupted）",
    );
    // (a) 不补跑：重启后头 1.5s 内一行都不许新增（含被清场那次的「重跑」）
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(
        runs().len(),
        runs_before_restart,
        "重启后不得立刻补跑错过的时点"
    );
    assert!(daemon.is_running(), "重启后的守护进程应在跑");
    // 自然轮次照跑到：第一次新运行不早于重启后 1.5s，且只新增一行（被打断的那次不重跑）
    wait_for(Duration::from_secs(10), "重启后的第一轮", || {
        runs().len() > runs_before_restart
    });
    let after_restart_ms = last_run_started_at(&db, &job.id);
    assert!(
        after_restart_ms - restart_ms >= 1_500,
        "重启后的首次执行必须在一个间隔之后：restart={restart_ms} run={after_restart_ms}"
    );
    assert_eq!(
        runs().len(),
        runs_before_restart + 1,
        "重启后只该有自然的那一轮，被打断的运行不得重复执行"
    );
    mark(started, "6c 重启后的第一轮（不补跑、不重复）");

    // ---- 收尾：SIGTERM 优雅停，pidfile 摘掉，不留进程 ----
    let stopped = daemon.stop();
    assert!(stopped.success(), "SIGTERM 后应正常退出：{stopped}");
    assert!(!home.join("ignis.pid").exists(), "优雅退出应摘掉 pidfile");
    assert_eq!(status(&home), DaemonStatus::Stopped, "库侧状态应是 Stopped");
    mark(started, "收尾完成（SIGTERM 优雅退出、pidfile 摘掉）");
}

// ---- 夹具契约小测（brief ②）：不拉起任何进程，直接钉脚本自己的出口 ----

/// 用 `sh` 跑夹具脚本（不依赖脚本的执行位）。
fn run_script(script: &Path, args: &[&str]) -> Output {
    Command::new("sh")
        .arg(script)
        .args(args)
        .output()
        .expect("sh 应可执行")
}

/// 把夹具的一行 JSON 输出解析成对象（解析不了直接失败并贴出原文）。
fn health_json(output: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|error| panic!("夹具输出应是 JSON（{error}）：{text:?}"));
    assert!(value.is_object(), "夹具输出应是 JSON 对象：{value}");
    value
}

/// `examples/test-server/check-health.sh` 是 Demo C 与手工联调共用的出口，三种用法各钉一条：
/// 无参输出可解析、`--simulate 92` 如实回填、非法值退出 2（而不是 1 或 0）。
///
/// 直接在仓库夹具上跑（不复制、不写文件）：脚本只读磁盘与 `/proc`，没有副作用。
#[test]
fn check_health_fixture_contract_is_pinned() {
    let script = test_server_dir().join("check-health.sh");

    // 无参：真实读数，形状必须是三键 JSON（两个百分比是 0-100 的整数，docker 是布尔）
    let plain = run_script(&script, &[]);
    assert_eq!(plain.status.code(), Some(0), "{}", output_text(&plain));
    let value = health_json(&plain);
    for key in ["disk_pct", "mem_pct"] {
        let pct = value[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{key} 应是整数：{value}"));
        assert!(pct <= 100, "{key}={pct} 越界：{value}");
    }
    assert!(value["docker"].is_boolean(), "docker 应是布尔：{value}");
    assert_eq!(
        value.as_object().expect("对象").len(),
        3,
        "只该有 disk_pct / mem_pct / docker 三个键：{value}"
    );

    // --simulate 92：演示用读数如实回填（Demo C 的 85% 阈值场景与 README 都依赖它）
    let simulated = run_script(&script, &["--simulate", "92"]);
    assert_eq!(
        simulated.status.code(),
        Some(0),
        "{}",
        output_text(&simulated)
    );
    assert_eq!(health_json(&simulated)["disk_pct"], 92);

    // 非法值：退出 2（不是 1），stderr 说清是哪一项坏了
    let bad = run_script(&script, &["--simulate", "abc"]);
    assert_eq!(bad.status.code(), Some(2), "{}", output_text(&bad));
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("--simulate"),
        "{}",
        output_text(&bad)
    );
    // 越界值走同一条拒绝路径（100 是边界，101 不是）
    let over = run_script(&script, &["--simulate", "101"]);
    assert_eq!(over.status.code(), Some(2), "{}", output_text(&over));
}
