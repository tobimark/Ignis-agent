//! 通知渠道：告警的日志行与桌面通知（[`ignis_core::alerts::Notifier`] 的 v0.1 实现）。
//!
//! 两个渠道都是 **best-effort**：投递失败只往 stderr 打一行警告，告警行本身已经落进 `alerts`
//! 表（未读收件箱是兜底渠道），调用方不该因为渠道故障以为「整条告警没送出去」。守护进程用
//! [`ignis_core::alerts::MultiNotifier`] 把它们串起来，顺序即渠道顺序。
//!
//! [`LogNotifier`] 每次追加一行 `{RFC3339} [{severity}] {title} {body}`（没有正文时不留行尾
//! 空白）；[`DesktopNotifier`] spawn `notify-send`（不等待、由独立任务收尾），程序不存在或
//! spawn 失败只警告一次就降级——非 unix 平台整体是 no-op。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ignis_core::alerts::{Notifier, NotifyError};
use ignis_storage::AlertRecord;

/// 日志渠道：把每条告警追加成日志文件里的一行。
pub struct LogNotifier {
    path: PathBuf,
}

impl LogNotifier {
    /// 指向日志文件；目录还不存在时在首次投递前按需创建。
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 日志文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl Notifier for LogNotifier {
    async fn notify(&self, alert: &AlertRecord) -> Result<(), NotifyError> {
        let line = format_line(alert, now_secs());
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                NotifyError::message(format!(
                    "创建告警日志目录失败（{}）：{error}",
                    parent.display()
                ))
            })?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| {
                NotifyError::message(format!(
                    "打开告警日志失败（{}）：{error}",
                    self.path.display()
                ))
            })?;
        file.write_all(line.as_bytes()).map_err(|error| {
            NotifyError::message(format!(
                "写入告警日志失败（{}）：{error}",
                self.path.display()
            ))
        })
    }
}

/// 一行告警：`{RFC3339} [{severity}] {title} {body}`（正文缺省时不补行尾空格）。
fn format_line(alert: &AlertRecord, at_secs: i64) -> String {
    let mut line = format!(
        "{} [{}] {}",
        format_rfc3339(at_secs),
        alert.severity,
        alert.title
    );
    if let Some(body) = &alert.body {
        line.push(' ');
        line.push_str(body);
    }
    line.push('\n');
    line
}

/// 桌面通知渠道：spawn `notify-send`，失败只警告一次。
pub struct DesktopNotifier {
    /// 要执行的程序；测试把它换成不存在的程序来验证降级路径。
    program: std::ffi::OsString,
    /// 「已警告过」标志：`notify-send` 不存在时不要每条告警都往日志里刷一行警告。
    warned: AtomicBool,
}

impl Default for DesktopNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopNotifier {
    /// 用系统 `PATH` 里的 `notify-send`。
    pub fn new() -> Self {
        Self {
            program: "notify-send".into(),
            warned: AtomicBool::new(false),
        }
    }

    /// 一次桌面通知的命令行：`-u <low|normal|critical> <title> [body]`，stdio 全部丢弃。
    fn command(&self, alert: &AlertRecord) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .arg("-u")
            .arg(urgency(&alert.severity))
            .arg(&alert.title);
        if let Some(body) = &alert.body {
            command.arg(body);
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }

    /// 降级：只警告一次，然后按「送达不了但不是错误」处理。
    fn warn_once(&self, error: &std::io::Error) {
        if !self.warned.swap(true, Ordering::Relaxed) {
            eprintln!(
                "warning: 桌面通知不可用（{}），后续降级为仅落库与日志：{error}",
                self.program.to_string_lossy()
            );
        }
    }
}

#[cfg(unix)]
#[async_trait]
impl Notifier for DesktopNotifier {
    async fn notify(&self, alert: &AlertRecord) -> Result<(), NotifyError> {
        match self.command(alert).spawn() {
            Ok(mut child) => {
                // 不阻塞投递：等子进程退出即可，顺手把它收掉（长跑守护进程不该攒僵尸）
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                Ok(())
            }
            Err(error) => {
                self.warn_once(&error);
                Ok(())
            }
        }
    }
}

#[cfg(not(unix))]
#[async_trait]
impl Notifier for DesktopNotifier {
    async fn notify(&self, _alert: &AlertRecord) -> Result<(), NotifyError> {
        Ok(())
    }
}

/// `severity` → `notify-send -u` 的 urgency（级别在告警层已校验过，未知值按最低档处理）。
fn urgency(severity: &str) -> &'static str {
    match severity {
        "critical" => "critical",
        "warning" => "normal",
        _ => "low",
    }
}

/// 当前 epoch 秒；时钟早于 epoch（人工设置过系统时间）时退化成 0。
fn now_secs() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// epoch 秒 → `YYYY-MM-DDTHH:MM:SSZ`（UTC）。
///
/// 手写而不引时间库：gateway 的依赖清单里没有 chrono，而这里只要一个日志行前缀；日/月的换算
/// 与 `ignis-core` 的 cron 实现同一套整数算法（Howard Hinnant 的 civil_from_days，1970 年前
/// 同样正确）。
fn format_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// 「自 1970-01-01 起的天数」→ (年, 月, 日)。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // 平移到以 0000-03-01 为原点的「时代」（400 年一轮）计数
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = (shifted - era * 146_097) as u32; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153; // 从 3 月起的月序 [0, 11]
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = i64::from(year_of_era) + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_core::alerts::Notifier;
    use ignis_storage::{AlertRecord, Db, NewAlert};

    use super::{DesktopNotifier, LogNotifier, format_line, format_rfc3339, urgency};

    /// 单个测试独占的临时目录，析构时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-gateway-notify-{}-{label}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 一条已落库的告警（投递渠道拿到的就是这种行）。
    fn alert(body: Option<&str>) -> AlertRecord {
        let db = Db::open_in_memory().unwrap();
        db.insert_alert(&NewAlert {
            severity: "warning".into(),
            title: "任务失败：disk-watch".into(),
            body: body.map(str::to_string),
            source_job_id: Some("job-1".into()),
            source_task_id: None,
            dedup_key: "job-1:任务失败：disk-watch".into(),
        })
        .unwrap()
    }

    /// 日志渠道：一行 `{RFC3339} [severity] title body`，追加写入，目录不存在也照写。
    #[tokio::test]
    async fn log_notifier_appends_one_formatted_line_per_alert() {
        let dir = TempDir::new("log");
        // 目录还不存在：投递时才建
        let path = dir.path().join("nested/logs/alerts.log");
        let notifier = LogNotifier::new(path.clone());
        assert_eq!(notifier.path(), path.as_path());

        notifier.notify(&alert(Some("磁盘 92%"))).await.unwrap();
        notifier.notify(&alert(None)).await.unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2, "{raw}");
        // 时间戳是投递时刻：只钉格式（20 字符的 `YYYY-MM-DDTHH:MM:SSZ`），不钉具体秒
        let (stamp, rest) = lines[0].split_once(" [").unwrap();
        assert!(is_rfc3339(stamp), "{lines:?}");
        assert_eq!(rest, "warning] 任务失败：disk-watch 磁盘 92%");
        assert!(lines[1].starts_with("2"), "{raw}");
        assert!(
            lines[1].ends_with("Z [warning] 任务失败：disk-watch"),
            "{raw}"
        );
        // 没有正文时不留行尾空白
        assert!(!lines[1].ends_with(' '), "{raw}");
    }

    /// `YYYY-MM-DDTHH:MM:SSZ` 的形状校验（本模块的测试自用）。
    fn is_rfc3339(stamp: &str) -> bool {
        let bytes = stamp.as_bytes();
        bytes.len() == 20
            && stamp.bytes().enumerate().all(|(index, byte)| match index {
                4 | 7 => byte == b'-',
                10 => byte == b'T',
                13 | 16 => byte == b':',
                19 => byte == b'Z',
                _ => byte.is_ascii_digit(),
            })
    }

    /// 时间戳换算（不引时间库）：用一个已知时点与一处跨日边界钉住。
    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        // 1704067200 = 2024-01-01T00:00:00Z（P2/P3 的基准时点）
        assert_eq!(format_rfc3339(1_704_067_200), "2024-01-01T00:00:00Z");
        // 跨日边界：-1 秒落在 1969-12-31，闰日 2024-02-29 同样正确
        assert_eq!(format_rfc3339(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(
            format_rfc3339(1_704_067_200 + 86_399),
            "2024-01-01T23:59:59Z"
        );
    }

    /// 行格式：正文可缺，severity 原样透传。
    #[test]
    fn line_format_is_pinned() {
        let mut row = alert(Some("根分区 92%"));
        assert_eq!(
            format_line(&row, 1_704_067_200),
            "2024-01-01T00:00:00Z [warning] 任务失败：disk-watch 根分区 92%\n"
        );
        row.body = None;
        assert_eq!(
            format_line(&row, 1_704_067_200),
            "2024-01-01T00:00:00Z [warning] 任务失败：disk-watch\n"
        );
    }

    /// urgency 映射：info → low、warning → normal、critical → critical（未知按低档）。
    #[test]
    fn urgency_maps_the_three_severities() {
        assert_eq!(urgency("info"), "low");
        assert_eq!(urgency("warning"), "normal");
        assert_eq!(urgency("critical"), "critical");
        assert_eq!(urgency("unknown"), "low");
    }

    /// 桌面通知的命令行：`-u <urgency> <title> [body]`。
    #[test]
    fn desktop_command_assembles_notify_send_arguments() {
        let notifier = DesktopNotifier::new();
        let mut row = alert(Some("根分区 92%"));

        let args: Vec<String> = notifier
            .command(&row)
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-u", "normal", "任务失败：disk-watch", "根分区 92%"]);

        row.severity = "critical".into();
        row.body = None;
        let args: Vec<String> = notifier
            .command(&row)
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-u", "critical", "任务失败：disk-watch"]);
    }

    /// 程序不存在：投递仍然成功（best-effort），只记一次「降级」警告。
    #[cfg(unix)]
    #[tokio::test]
    async fn desktop_notifier_degrades_without_complaining_to_the_caller() {
        let notifier = DesktopNotifier {
            program: "ignis-notify-send-definitely-missing".into(),
            warned: std::sync::atomic::AtomicBool::new(false),
        };

        notifier.notify(&alert(None)).await.unwrap();
        // 第二次仍然成功，但不再重复警告（标志已经翻过）
        notifier.notify(&alert(None)).await.unwrap();
        assert!(notifier.warned.load(Ordering::Relaxed));
    }

    /// 程序存在时的成功路径：spawn 成功、投递仍返回 `Ok`、不记警告。用 `/bin/true` 当程序，
    /// 顺便避免跑测试时真的弹一条桌面通知。
    #[cfg(unix)]
    #[tokio::test]
    async fn desktop_notifier_spawns_the_program_and_returns_ok() {
        let notifier = DesktopNotifier {
            program: "/bin/true".into(),
            warned: std::sync::atomic::AtomicBool::new(false),
        };

        notifier.notify(&alert(Some("ignis test"))).await.unwrap();
        notifier.notify(&alert(None)).await.unwrap();

        assert!(!notifier.warned.load(Ordering::Relaxed));
    }
}
