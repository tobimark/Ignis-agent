//! `notify_user` 元工具：模型按条件「通知用户」的唯一出口。
//!
//! 工具只做参数解析与文案组装——落库、去重与渠道投递都在 [`AlertSink`] 里。告警来源取自
//! [`ToolCtx`]：`task_id` 是本次任务，`job_id` 是调度运行透传过来的来源 job（CLI 交互为
//! `None`），模型无需（也无法）把这两个 id 塞进参数里。

use std::sync::Arc;

use async_trait::async_trait;
use ignis_runtime::ToolSpec;
use serde_json::{Value, json};

use super::util::{optional_str, required_str, truncate_output};
use super::{Tool, ToolCtx, ToolError};
use crate::alerts::{AlertError, AlertSink, RaiseOutcome};

/// 告警工具：与 `remember` / `load_skill` 同属元工具，只写 agent 自有状态。
pub(super) struct NotifyUser {
    alerts: Arc<AlertSink>,
}

impl NotifyUser {
    pub(super) fn new(alerts: Arc<AlertSink>) -> Self {
        Self { alerts }
    }
}

#[async_trait]
impl Tool for NotifyUser {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "notify_user".into(),
            description: "Send the user an alert about something that needs their attention. \
                          The alert is stored in the user's inbox and delivered through the \
                          configured channels; a repeated (source, title) pair is deduplicated \
                          within a 15-minute cooldown."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "severity": {
                        "type": "string",
                        "enum": ["info", "warning", "critical"],
                        "description": "How urgent the alert is"
                    },
                    "title": {
                        "type": "string",
                        "description": "Short one-line title shown in the alert list"
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional details, e.g. the measured value and the threshold"
                    }
                },
                "required": ["severity", "title"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
        let severity = required_str(args, "severity")?;
        let title = required_str(args, "title")?;
        let body = optional_str(args, "body")?;
        // 来源取自 ctx（本次任务 + 调度运行透传的 job），模型不必也无法自己填这两个 id
        let outcome = self
            .alerts
            .raise(
                severity,
                title,
                body,
                ctx.job_id.as_deref(),
                ctx.task_id.as_deref(),
            )
            .await
            .map_err(alert_failure)?;
        // 回显落库/命中的那一行：去重时也让模型知道这次没有重复打扰用户
        let notice = match &outcome {
            RaiseOutcome::Sent(record) => {
                format!("已发送告警（{}）：{}", record.severity, record.title)
            }
            RaiseOutcome::Deduped(record) => format!("告警已去重（{}，冷却中）", record.title),
        };
        Ok(truncate_output(&notice, ctx.limits.tool_output_limit))
    }
}

/// 告警层错误转工具错误：级别/标题的校验消息本身就是中文人话，原样回填给模型。
fn alert_failure(error: AlertError) -> ToolError {
    ToolError::message(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use ignis_runtime::Limits;
    use ignis_storage::{AlertRecord, Db};
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::NotifyUser;
    use crate::alerts::test_support::RecordingNotifier;
    use crate::alerts::{AlertSink, DEDUP_WINDOW_SECS, Notifier};
    use crate::tools::{Tool, ToolCtx, ToolError};

    /// 工具直调环境：内存库 + 记录型渠道；`notify_user` 不碰工作区，故它固定为 `.`。
    struct Env {
        db: Arc<Db>,
        notifier: Arc<RecordingNotifier>,
        sink: Arc<AlertSink>,
        task_id: Option<String>,
        job_id: Option<String>,
        limits: Limits,
    }

    impl Env {
        /// 交互运行：只有 task id。
        fn new() -> Self {
            Self::build(RecordingNotifier::new(), Some("task-1"), None)
        }

        /// 调度运行：task id 与 job id 都有（job 优先）。
        fn with_job_id(job_id: &str) -> Self {
            Self::build(RecordingNotifier::new(), Some("task-1"), Some(job_id))
        }

        /// 渠道恒失败：验证「投递失败仍在结果里算已发送」。
        fn with_failing_notifier() -> Self {
            Self::build(RecordingNotifier::failing(), Some("task-1"), None)
        }

        fn build(notifier: RecordingNotifier, task_id: Option<&str>, job_id: Option<&str>) -> Self {
            let db = Arc::new(Db::open_in_memory().unwrap());
            let notifier = Arc::new(notifier);
            let channel: Arc<dyn Notifier> = notifier.clone();
            let sink = Arc::new(AlertSink::with_window_secs(
                Arc::clone(&db),
                channel,
                DEDUP_WINDOW_SECS,
            ));
            Self {
                db,
                notifier,
                sink,
                task_id: task_id.map(str::to_string),
                job_id: job_id.map(str::to_string),
                limits: Limits::default(),
            }
        }

        fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx {
                workspace: Path::new("."),
                cancel: CancellationToken::new(),
                limits: &self.limits,
                task_id: self.task_id.clone(),
                job_id: self.job_id.clone(),
            }
        }

        async fn call(&self, args: Value) -> Result<String, ToolError> {
            NotifyUser::new(Arc::clone(&self.sink))
                .run(&args, &self.ctx())
                .await
        }

        fn rows(&self) -> Vec<AlertRecord> {
            self.db.list_alerts(true, 10).unwrap()
        }
    }

    /// 成功路径：结果文案逐字，告警行带上 ctx 里的来源身份。
    #[tokio::test]
    async fn a_sent_alert_carries_the_ctx_provenance() {
        let env = Env::new();

        let output = env
            .call(json!({"severity": "warning", "title": "磁盘 92%", "body": "根分区 92%（阈值 85%）"}))
            .await
            .unwrap();

        assert_eq!(output, "已发送告警（warning）：磁盘 92%");
        let rows = env.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].severity, "warning");
        assert_eq!(rows[0].title, "磁盘 92%");
        assert_eq!(rows[0].body.as_deref(), Some("根分区 92%（阈值 85%）"));
        assert_eq!(rows[0].source_task_id.as_deref(), Some("task-1"));
        assert_eq!(rows[0].source_job_id, None);
        assert_eq!(rows[0].dedup_key, "task-1:磁盘 92%");
        assert_eq!(env.notifier.calls(), 1);
    }

    /// `body` 可省；job id 就位时同时进落库的来源列与去重键（优先于 task id）。
    #[tokio::test]
    async fn the_job_id_wins_over_the_task_id_for_provenance() {
        let env = Env::with_job_id("job-9");

        let output = env
            .call(json!({"severity": "critical", "title": "磁盘满了"}))
            .await
            .unwrap();

        assert_eq!(output, "已发送告警（critical）：磁盘满了");
        let rows = env.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body, None);
        assert_eq!(rows[0].source_job_id.as_deref(), Some("job-9"));
        assert_eq!(rows[0].source_task_id.as_deref(), Some("task-1"));
        assert_eq!(rows[0].dedup_key, "job-9:磁盘满了");
    }

    /// 冷却窗口内第二次同标题：结果文案变成「已去重」，不落新行、不重复投递。
    #[tokio::test]
    async fn a_repeated_title_reports_the_dedup_in_the_result() {
        let env = Env::new();

        let first = env
            .call(json!({"severity": "info", "title": "部署完成"}))
            .await
            .unwrap();
        assert_eq!(first, "已发送告警（info）：部署完成");

        let second = env
            .call(json!({"severity": "warning", "title": "部署完成"}))
            .await
            .unwrap();
        assert_eq!(second, "告警已去重（部署完成，冷却中）");
        assert_eq!(env.rows().len(), 1);
        assert_eq!(env.notifier.calls(), 1);
    }

    /// 标题的首尾空白不进库、不进键、也不进回执：`"  磁盘满了  "` 与 `"磁盘满了"` 是同一条告警。
    #[tokio::test]
    async fn a_padded_title_is_trimmed_in_the_result_and_the_row() {
        let env = Env::new();

        let first = env
            .call(json!({"severity": "warning", "title": "  磁盘满了  "}))
            .await
            .unwrap();
        assert_eq!(first, "已发送告警（warning）：磁盘满了");
        let rows = env.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "磁盘满了");
        assert_eq!(rows[0].dedup_key, "task-1:磁盘满了");

        // 换一种写法（不带空白）再报一次：同键 → 去重，回执里也是裁过的标题
        let second = env
            .call(json!({"severity": "warning", "title": "磁盘满了"}))
            .await
            .unwrap();
        assert_eq!(second, "告警已去重（磁盘满了，冷却中）");
        assert_eq!(env.rows().len(), 1);
        assert_eq!(env.notifier.calls(), 1);
    }

    /// 渠道失败不改结果文案：告警已经落进收件箱，模型看到的就是「已发送」。
    #[tokio::test]
    async fn a_failing_channel_still_reports_the_alert_as_sent() {
        let env = Env::with_failing_notifier();

        let output = env
            .call(json!({"severity": "critical", "title": "磁盘满了"}))
            .await
            .unwrap();

        assert_eq!(output, "已发送告警（critical）：磁盘满了");
        assert_eq!(env.rows().len(), 1);
        assert_eq!(env.notifier.calls(), 1);
    }

    /// 参数错误一律中文，且不落行、不投递。
    #[tokio::test]
    async fn arguments_are_validated_in_chinese() {
        let env = Env::new();

        for (args, needle) in [
            (json!({"title": "磁盘满了"}), "缺少必需的参数 `severity`"),
            (json!({"severity": "info"}), "缺少必需的参数 `title`"),
            (json!({"severity": 3, "title": "磁盘满了"}), "必须是字符串"),
            (json!({"severity": "info", "title": 3}), "必须是字符串"),
            (
                json!({"severity": "info", "title": "x", "body": 3}),
                "必须是字符串",
            ),
            (
                json!({"severity": "fatal", "title": "磁盘满了"}),
                "告警级别只能是 info、warning 或 critical",
            ),
            (
                json!({"severity": "info", "title": "   "}),
                "告警标题不能为空",
            ),
        ] {
            let error = env.call(args.clone()).await.unwrap_err();
            let message = error.to_string();
            assert!(message.contains(needle), "{args}: {message}");
        }

        assert!(env.rows().is_empty(), "参数不对就不落库");
        assert_eq!(env.notifier.calls(), 0, "参数不对就不投递");
    }
}
