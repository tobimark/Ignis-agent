//! 告警层：[`AlertSink`] 是「校验 → 落库 → 去重 → 投递」的唯一出口，`notify_user` 元工具与
//! 守护进程的失败告警都从这里走。
//!
//! 去重键 = `(来源, 标题)`：来源取 job id，其次 task id，两者都没有（CLI 交互）就是 `cli`。
//! 同一键在冷却窗口（默认 [`DEDUP_WINDOW_SECS`]）内只落一行、只投递一次；窗口外是新告警。
//! 投递是 best-effort：渠道失败只往 stderr 打一行警告，告警行照常落库——`alerts` 表的未读
//! 收件箱（`ignis alerts`）始终是兜底渠道。
//!
//! 标题与正文都是模型可控文本：标题先裁掉首尾空白、再按 [`MAX_TITLE_CHARS`] 截断，去重键与
//! 落库标题都取这一份（`"磁盘满了 "` 与 `"磁盘满了"` 因此是同一条告警，不会因为空格多落一行）；
//! 正文按 [`MAX_BODY_CHARS`] 截断。超长文本静默截断（截断点落在字符边界，末尾补 `…`），与记忆
//! 层事实字段的收口同一口径。
//!
//! 校验错误（级别不在册、标题为空）与存储错误都是中文，调用方原样回填给模型。

mod notifier;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ignis_storage::{AlertRecord, Db, NewAlert, StorageError};

pub use notifier::{MultiNotifier, Notifier, NotifyError};

/// 去重冷却窗口：900 秒（brief 钉死的 15 分钟）。
pub const DEDUP_WINDOW_SECS: i64 = 900;

/// 标题的字符上限：标题会进收件箱列表、去重键与工具回执（模型看得到），落库前静默截断到
/// 前 200 个字符 + `…`（截断点落在字符边界）。
const MAX_TITLE_CHARS: usize = 200;
/// 正文的字符上限：正文只进收件箱，同样在落库前截断——前 2000 个字符 + `…`。
const MAX_BODY_CHARS: usize = 2000;

/// 告警层的错误：入参校验（级别、标题）与 `alerts` 表的读写。
#[derive(Debug, thiserror::Error)]
pub enum AlertError {
    /// `severity` 不在册。
    #[error("告警级别只能是 info、warning 或 critical，收到「{0}」")]
    UnknownSeverity(String),
    /// 标题为空或只有空白。
    #[error("告警标题不能为空")]
    EmptyTitle,
    /// `alerts` 表的读写失败。
    #[error("告警存储失败：{0}")]
    Storage(#[from] StorageError),
}

/// 一次 `raise` 的结果。
#[derive(Debug)]
pub enum RaiseOutcome {
    /// 新告警：已落库并已尝试投递（渠道失败只警告）。
    Sent(AlertRecord),
    /// 窗口内已有同键告警：不落新行、不投递，回命中的旧行。
    Deduped(AlertRecord),
}

/// 告警出口：一行告警从入参校验到落库、去重、投递的完整路径。
pub struct AlertSink {
    db: Arc<Db>,
    notifier: Arc<dyn Notifier>,
    window_secs: i64,
}

impl AlertSink {
    /// 生产构造：15 分钟去重窗口。
    pub fn new(db: Arc<Db>, notifier: Arc<dyn Notifier>) -> Self {
        Self::with_window_secs(db, notifier, DEDUP_WINDOW_SECS)
    }

    /// 指定去重窗口（秒）；测试用它把冷却期收到秒级。
    ///
    /// 非正窗口没有意义（窗口内永远命中不了），构造期就在 debug 构建里拦下。
    pub fn with_window_secs(db: Arc<Db>, notifier: Arc<dyn Notifier>, window_secs: i64) -> Self {
        debug_assert!(window_secs > 0);
        Self {
            db,
            notifier,
            window_secs,
        }
    }

    /// 升一条告警。
    ///
    /// `severity` 只认 `info` / `warning` / `critical`，标题不能为空——两者都是模型递进来的
    /// 输入，一律在落库前以中文拒绝。标题先 `trim()` 再截到 [`MAX_TITLE_CHARS`]，去重键、落库
    /// 标题与工具回执取的是同一份（键 = `(job 或 task 或 "cli", 裁过的标题)`；来源两者都有时
    /// 以 job 为准，同一次调度运行里的 `notify_user` 与失败告警因此共用一把键）；正文截到
    /// [`MAX_BODY_CHARS`]。
    pub async fn raise(
        &self,
        severity: &str,
        title: &str,
        body: Option<&str>,
        source_job_id: Option<&str>,
        source_task_id: Option<&str>,
    ) -> Result<RaiseOutcome, AlertError> {
        if !matches!(severity, "info" | "warning" | "critical") {
            return Err(AlertError::UnknownSeverity(severity.to_string()));
        }
        // 标题只在这一处收口：裁空白 → 按上限截断 → 空值拒绝；后面的键、落库与回执都用它
        let title = truncate_chars(title.trim(), MAX_TITLE_CHARS);
        if title.is_empty() {
            return Err(AlertError::EmptyTitle);
        }
        let body = body.map(|text| truncate_chars(text, MAX_BODY_CHARS));
        let owner = source_job_id.or(source_task_id).unwrap_or("cli");
        let dedup_key = format!("{owner}:{title}");
        let now = now_millis();
        // 冷却窗口内已有同键告警：命中的旧行就是这次的结果，不落新行、不投递
        if let Some(record) = self
            .db
            .find_recent_alert(&dedup_key, self.window_secs, now)?
        {
            return Ok(RaiseOutcome::Deduped(record));
        }
        let record = self.db.insert_alert(&NewAlert {
            severity: severity.to_string(),
            title,
            body,
            source_job_id: source_job_id.map(str::to_owned),
            source_task_id: source_task_id.map(str::to_owned),
            dedup_key,
        })?;
        // 投递 best-effort：渠道失败只留一行警告，告警行已经落库（未读收件箱是兜底渠道）
        if let Err(error) = self.notifier.notify(&record).await {
            eprintln!("warning: 告警投递失败：{error}");
        }
        Ok(RaiseOutcome::Sent(record))
    }
}

/// epoch 毫秒；时钟早于 epoch（人工设置过系统时间）时退化成 0，保证告警仍能落库。
fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// 把模型递进来的文本截到 `max_chars` 个字符：截断点落在字符边界，末尾补一个 `…`。
///
/// 与记忆层事实字段的收口同一口径（前 `max_chars` 个字符 + `…`，不改动中段内容）。
fn truncate_chars(value: &str, max_chars: usize) -> String {
    let Some((end, _)) = value.char_indices().nth(max_chars) else {
        return value.to_owned();
    };
    let mut truncated = String::with_capacity(end + '…'.len_utf8());
    truncated.push_str(&value[..end]);
    truncated.push('…');
    truncated
}

/// 测试用的记录型渠道；`tools::notify` 的用例也复用它。
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ignis_storage::AlertRecord;

    use super::{Notifier, NotifyError};

    /// 记录投递内容（含失败的那次尝试）的渠道；`failing` 时每次投递都返回错误。
    pub(crate) struct RecordingNotifier {
        alerts: Mutex<Vec<AlertRecord>>,
        failing: bool,
    }

    impl RecordingNotifier {
        /// 恒成功的渠道。
        pub(crate) fn new() -> Self {
            Self {
                alerts: Mutex::new(Vec::new()),
                failing: false,
            }
        }

        /// 恒失败的渠道：验证「投递失败不影响落库」。
        pub(crate) fn failing() -> Self {
            Self {
                alerts: Mutex::new(Vec::new()),
                failing: true,
            }
        }

        /// 收到的告警，按投递顺序。
        pub(crate) fn alerts(&self) -> Vec<AlertRecord> {
            self.alerts.lock().unwrap().clone()
        }

        /// 投递尝试次数（失败也算一次）。
        pub(crate) fn calls(&self) -> usize {
            self.alerts.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Notifier for RecordingNotifier {
        async fn notify(&self, alert: &AlertRecord) -> Result<(), NotifyError> {
            self.alerts.lock().unwrap().push(alert.clone());
            if self.failing {
                return Err(NotifyError::message("渠道故障（测试）"));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ignis_storage::{AlertRecord, Db};

    use super::test_support::RecordingNotifier;
    use super::{
        AlertError, AlertSink, DEDUP_WINDOW_SECS, MAX_BODY_CHARS, MAX_TITLE_CHARS, Notifier,
        RaiseOutcome,
    };

    /// 观察环境：真实内存库 + 记录型渠道。
    struct Env {
        db: Arc<Db>,
        notifier: Arc<RecordingNotifier>,
        sink: AlertSink,
    }

    impl Env {
        fn new() -> Self {
            Self::with_window(DEDUP_WINDOW_SECS)
        }

        fn with_window(window_secs: i64) -> Self {
            let db = Arc::new(Db::open_in_memory().unwrap());
            let notifier = Arc::new(RecordingNotifier::new());
            let channel: Arc<dyn Notifier> = notifier.clone();
            let sink = AlertSink::with_window_secs(Arc::clone(&db), channel, window_secs);
            Self { db, notifier, sink }
        }

        /// 全部告警行（含已读）。
        fn rows(&self) -> Vec<AlertRecord> {
            self.db.list_alerts(true, 100).unwrap()
        }

        /// 把某条告警的 `created_at` 往前挪 `millis` 毫秒：测试不必真的等冷却窗口。
        fn age(&self, id: i64, millis: i64) {
            self.db
                .with_conn(|conn| {
                    conn.execute_batch(&format!(
                        "UPDATE alerts SET created_at = created_at - {millis} WHERE id = {id}"
                    ))
                })
                .unwrap();
        }
    }

    /// 窗口内第二次同键告警：不落新行、不投递，回命中的旧行。
    #[tokio::test]
    async fn a_second_raise_inside_the_window_is_deduped() {
        let env = Env::new();

        let first = env
            .sink
            .raise(
                "warning",
                "磁盘 92%",
                Some("根分区"),
                Some("job-1"),
                Some("task-1"),
            )
            .await
            .unwrap();
        let RaiseOutcome::Sent(sent) = first else {
            panic!("首次应落库：{first:?}")
        };
        assert_eq!(sent.severity, "warning");
        assert_eq!(sent.body.as_deref(), Some("根分区"));
        assert_eq!(sent.dedup_key, "job-1:磁盘 92%");
        assert_eq!(sent.source_job_id.as_deref(), Some("job-1"));
        assert_eq!(sent.source_task_id.as_deref(), Some("task-1"));
        assert_eq!(env.rows().len(), 1);
        assert_eq!(env.notifier.calls(), 1);
        // 投递给渠道的就是落库的那一行（id / created_at 与库里一致）
        let delivered = env.notifier.alerts();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0], sent);

        let second = env
            .sink
            .raise(
                "warning",
                "磁盘 92%",
                Some("根分区"),
                Some("job-1"),
                Some("task-2"),
            )
            .await
            .unwrap();
        let RaiseOutcome::Deduped(hit) = second else {
            panic!("窗口内第二次应去重：{second:?}")
        };
        assert_eq!(hit, sent, "去重回的是命中的旧行");
        assert_eq!(env.rows().len(), 1, "窗口内不落新行");
        assert_eq!(env.notifier.calls(), 1, "窗口内不投递");

        // 来源两者都有时键以 job 为准：换个 task id 仍是同一条告警
        let third = env
            .sink
            .raise("warning", "磁盘 92%", None, Some("job-1"), None)
            .await
            .unwrap();
        assert!(matches!(third, RaiseOutcome::Deduped(_)), "{third:?}");
        assert_eq!(env.rows().len(), 1);
    }

    /// 窗口外的同键告警是新行，并且重新投递一次。
    #[tokio::test]
    async fn a_raise_outside_the_window_lands_a_new_row_and_notifies_again() {
        let env = Env::new();

        let first = env
            .sink
            .raise("info", "重启完成", None, None, Some("task-1"))
            .await
            .unwrap();
        let RaiseOutcome::Sent(sent) = first else {
            panic!("首次应落库：{first:?}")
        };
        env.age(sent.id, DEDUP_WINDOW_SECS * 1000 + 1000);

        let second = env
            .sink
            .raise("info", "重启完成", None, None, Some("task-1"))
            .await
            .unwrap();
        let RaiseOutcome::Sent(again) = second else {
            panic!("窗口外应落新行：{second:?}")
        };
        assert_ne!(again.id, sent.id);
        assert_eq!(again.dedup_key, sent.dedup_key);
        assert_eq!(env.rows().len(), 2);
        assert_eq!(env.notifier.calls(), 2);
    }

    /// 窗口按秒计，默认 15 分钟：2 秒前的行在默认窗口内去重，1 秒窗口放行。
    #[tokio::test]
    async fn the_window_defaults_to_fifteen_minutes_and_is_honoured() {
        assert_eq!(DEDUP_WINDOW_SECS, 900);

        for (window, deduped) in [(DEDUP_WINDOW_SECS, true), (1, false)] {
            let env = Env::with_window(window);
            let first = env
                .sink
                .raise("warning", "窗口测试", None, Some("job-1"), None)
                .await
                .unwrap();
            let RaiseOutcome::Sent(sent) = first else {
                panic!("首次应落库：{first:?}")
            };
            env.age(sent.id, 2000);

            let second = env
                .sink
                .raise("warning", "窗口测试", None, Some("job-1"), None)
                .await
                .unwrap();
            match second {
                RaiseOutcome::Deduped(hit) => {
                    assert!(deduped, "window={window} 不该去重");
                    assert_eq!(hit.id, sent.id);
                }
                RaiseOutcome::Sent(row) => {
                    assert!(!deduped, "window={window} 应去重");
                    assert_ne!(row.id, sent.id);
                }
            }
            assert_eq!(env.rows().len(), if deduped { 1 } else { 2 });
            assert_eq!(env.notifier.calls(), if deduped { 1 } else { 2 });
        }
    }

    /// 渠道失败不影响落库与结果：告警仍是 `Sent`，只是多一行警告。
    #[tokio::test]
    async fn a_failing_notifier_does_not_fail_the_raise() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let notifier = Arc::new(RecordingNotifier::failing());
        let channel: Arc<dyn Notifier> = notifier.clone();
        let sink = AlertSink::new(Arc::clone(&db), channel);

        let outcome = sink
            .raise("critical", "磁盘满了", None, Some("job-9"), None)
            .await
            .unwrap();

        let RaiseOutcome::Sent(sent) = outcome else {
            panic!("投递失败不该影响结果：{outcome:?}")
        };
        assert_eq!(sent.severity, "critical");
        assert_eq!(db.list_alerts(true, 10).unwrap().len(), 1);
        assert_eq!(notifier.calls(), 1, "失败也算尝试过一次投递");
    }

    /// 级别与标题都是模型输入：级别不在册、标题为空都在落库前被中文拒绝。
    #[tokio::test]
    async fn severity_and_blank_title_are_rejected_in_chinese() {
        let env = Env::new();

        for (severity, title, needle) in [
            (
                "fatal",
                "磁盘满了",
                "告警级别只能是 info、warning 或 critical",
            ),
            ("Warning", "磁盘满了", "告警级别"),
            ("", "磁盘满了", "告警级别"),
            ("info", "", "告警标题不能为空"),
            ("info", "   ", "告警标题不能为空"),
            ("info", "\n\t", "告警标题不能为空"),
        ] {
            let error = env
                .sink
                .raise(severity, title, None, Some("job-1"), None)
                .await
                .unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains(needle),
                "severity={severity:?}, title={title:?}: {message}"
            );
        }
        // 错误类型逐个钉住，调用方据此分辨校验失败与存储失败
        assert!(matches!(
            env.sink.raise("fatal", "x", None, None, None).await,
            Err(AlertError::UnknownSeverity(_))
        ));
        assert!(matches!(
            env.sink.raise("info", " ", None, None, None).await,
            Err(AlertError::EmptyTitle)
        ));

        assert!(env.rows().is_empty(), "校验失败不落行");
        assert_eq!(env.notifier.calls(), 0, "校验失败不投递");
    }

    /// 去重键的来源兜底：没有 job 也没有 task（CLI 交互）用 `cli`，只有 task 用 task id。
    #[tokio::test]
    async fn the_dedup_key_falls_back_from_job_to_task_to_cli() {
        let env = Env::new();

        let cli = env
            .sink
            .raise("info", "手工提醒", None, None, None)
            .await
            .unwrap();
        let RaiseOutcome::Sent(cli) = cli else {
            panic!("首次应落库：{cli:?}")
        };
        assert_eq!(cli.dedup_key, "cli:手工提醒");
        assert_eq!(cli.source_job_id, None);
        assert_eq!(cli.source_task_id, None);

        let by_task = env
            .sink
            .raise("info", "任务提醒", None, None, Some("task-7"))
            .await
            .unwrap();
        let RaiseOutcome::Sent(by_task) = by_task else {
            panic!("首次应落库：{by_task:?}")
        };
        assert_eq!(by_task.dedup_key, "task-7:任务提醒");
        assert_eq!(by_task.source_task_id.as_deref(), Some("task-7"));

        // 标题不同就是不同的键：两条都落行、都投递
        assert_eq!(env.rows().len(), 2);
        assert_eq!(env.notifier.calls(), 2);
    }

    /// 标题先裁掉首尾空白：去重键、落库标题与投递内容都取裁过的那一份。
    #[tokio::test]
    async fn a_padded_title_is_trimmed_before_the_key_and_the_row() {
        let env = Env::new();

        let first = env
            .sink
            .raise("warning", "  磁盘满了  ", None, Some("job-1"), None)
            .await
            .unwrap();
        let RaiseOutcome::Sent(sent) = first else {
            panic!("首次应落库：{first:?}")
        };
        assert_eq!(sent.title, "磁盘满了", "落库/投递的是裁过空白的标题");
        assert_eq!(sent.dedup_key, "job-1:磁盘满了", "键也取裁过的标题");
        assert_eq!(env.notifier.alerts()[0], sent);

        // 同一条告警换个写法（这次不带空白）：同键 → 去重，不给空格多落一行
        let second = env
            .sink
            .raise("warning", "磁盘满了", None, Some("job-1"), None)
            .await
            .unwrap();
        assert!(matches!(second, RaiseOutcome::Deduped(_)), "{second:?}");
        assert_eq!(env.rows().len(), 1);
        assert_eq!(env.notifier.calls(), 1);

        // 只有空白的标题仍是空标题（裁完就没了）
        assert!(matches!(
            env.sink
                .raise("info", " \n ", None, Some("job-1"), None)
                .await,
            Err(AlertError::EmptyTitle)
        ));
    }

    /// 标题与正文都有字符上限：超长静默截断（前 N 个字符 + `…`，落在字符边界），不多不少。
    #[tokio::test]
    async fn long_titles_and_bodies_are_capped_on_a_char_boundary() {
        let env = Env::new();

        // 超长标题（多字节字符）：前 MAX_TITLE_CHARS 个字符 + `…`
        let long_title = "磁".repeat(MAX_TITLE_CHARS + 50);
        let long_body = "好".repeat(MAX_BODY_CHARS + 500);
        let capped_title = format!("{}…", "磁".repeat(MAX_TITLE_CHARS));
        let capped_body = format!("{}…", "好".repeat(MAX_BODY_CHARS));

        let first = env
            .sink
            .raise(
                "critical",
                &long_title,
                Some(&long_body),
                Some("job-1"),
                None,
            )
            .await
            .unwrap();
        let RaiseOutcome::Sent(sent) = first else {
            panic!("首次应落库：{first:?}")
        };
        assert_eq!(sent.title, capped_title);
        assert_eq!(sent.title.chars().count(), MAX_TITLE_CHARS + 1);
        assert_eq!(sent.dedup_key, format!("job-1:{capped_title}"));
        assert_eq!(sent.body.as_deref(), Some(capped_body.as_str()));
        assert_eq!(
            sent.body.as_deref().unwrap().chars().count(),
            MAX_BODY_CHARS + 1
        );
        // 落库与投递的就是截断后的那一份
        assert_eq!(env.rows()[0], sent);
        assert_eq!(env.notifier.alerts()[0], sent);

        // 同一把（截断后的）键仍然认得出重复
        let repeat = env
            .sink
            .raise("critical", &long_title, None, Some("job-1"), None)
            .await
            .unwrap();
        assert!(matches!(repeat, RaiseOutcome::Deduped(_)), "{repeat:?}");
        assert_eq!(env.rows().len(), 1);
        assert_eq!(env.notifier.calls(), 1);

        // 边界：恰好等于上限的标题不截断、不补 `…`
        let exact = "滿".repeat(MAX_TITLE_CHARS);
        let boundary = env
            .sink
            .raise("info", &exact, None, Some("job-2"), None)
            .await
            .unwrap();
        let RaiseOutcome::Sent(boundary) = boundary else {
            panic!("窗口内不存在同键告警：{boundary:?}")
        };
        assert_eq!(boundary.title, exact);
        assert_eq!(boundary.title.chars().count(), MAX_TITLE_CHARS);
    }

    /// 非正窗口是构造期错误：debug 构建直接 panic（release 里断言被编译掉）。
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "window_secs > 0")]
    fn a_non_positive_dedup_window_is_rejected_in_debug_builds() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let channel: Arc<dyn Notifier> = Arc::new(RecordingNotifier::new());

        let _ = AlertSink::with_window_secs(db, channel, 0);
    }
}
