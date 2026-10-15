//! 通知渠道契约：[`Notifier`] 与把多个渠道串成一个的 [`MultiNotifier`]。
//!
//! v0.1 的真实渠道（桌面通知、日志行、IM）在 Task 5 的 gateway 里实现；这一层只钉契约与投
//! 递语义——按注册顺序逐个投递，任一渠道失败只记一行警告，不阻断其余渠道。

use std::sync::Arc;

use async_trait::async_trait;
use ignis_storage::AlertRecord;

/// 通知渠道的投递失败原因；消息由渠道自己给出，最终以一行警告落到 stderr。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NotifyError(String);

impl NotifyError {
    /// 用一段可读消息构造错误。
    pub fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// 通知渠道：把一条已落库的告警投递出去。
#[async_trait]
pub trait Notifier: Send + Sync {
    /// 投递一条告警；失败只表示「这个渠道没送到」，告警行本身已经落库。
    async fn notify(&self, alert: &AlertRecord) -> Result<(), NotifyError>;
}

/// 多个渠道的组合：按注册顺序投递，失败逐个转成警告，整体不失败。
pub struct MultiNotifier {
    notifiers: Vec<Arc<dyn Notifier>>,
}

impl MultiNotifier {
    /// 按序组合渠道；空列表合法（只剩 CLI 未读收件箱这一个渠道）。
    pub fn new(notifiers: Vec<Arc<dyn Notifier>>) -> Self {
        Self { notifiers }
    }
}

#[async_trait]
impl Notifier for MultiNotifier {
    async fn notify(&self, alert: &AlertRecord) -> Result<(), NotifyError> {
        // 一个渠道失败不阻断其余渠道：逐个转成警告，整体不返回错误——某个渠道（桌面通知）
        // 不可用不该让调用方以为「整条告警没送出去」。
        for notifier in &self.notifiers {
            if let Err(error) = notifier.notify(alert).await {
                eprintln!("warning: 告警投递失败：{error}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ignis_storage::{AlertRecord, Db, NewAlert};

    use super::{MultiNotifier, Notifier, NotifyError};

    /// 把调用顺序记进共享轨迹的渠道；`failing` 时每次投递都失败。
    struct NamedNotifier {
        name: &'static str,
        trace: Arc<Mutex<Vec<&'static str>>>,
        failing: bool,
    }

    #[async_trait]
    impl Notifier for NamedNotifier {
        async fn notify(&self, _alert: &AlertRecord) -> Result<(), NotifyError> {
            self.trace.lock().unwrap().push(self.name);
            if self.failing {
                return Err(NotifyError::message(format!(
                    "{} 渠道故障（测试）",
                    self.name
                )));
            }
            Ok(())
        }
    }

    /// 一条已落库的告警：本模块只关心投递语义，不关心去重。
    fn alert() -> AlertRecord {
        let db = Db::open_in_memory().unwrap();
        db.insert_alert(&NewAlert {
            severity: "info".into(),
            title: "测试告警".into(),
            body: None,
            source_job_id: None,
            source_task_id: None,
            dedup_key: "cli:测试告警".into(),
        })
        .unwrap()
    }

    /// 渠道按组合顺序被调用，每个渠道都拿到同一条告警。
    #[tokio::test]
    async fn multi_notifier_calls_every_channel_in_order() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let channels: Vec<Arc<dyn Notifier>> = ["first", "second"]
            .into_iter()
            .map(|name| {
                Arc::new(NamedNotifier {
                    name,
                    trace: Arc::clone(&trace),
                    failing: false,
                }) as Arc<dyn Notifier>
            })
            .collect();

        MultiNotifier::new(channels).notify(&alert()).await.unwrap();

        assert_eq!(*trace.lock().unwrap(), ["first", "second"]);
    }

    /// 一个渠道失败不阻断其余渠道：失败的那个只留下警告。
    #[tokio::test]
    async fn a_failing_channel_does_not_block_the_rest() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let channels: Vec<Arc<dyn Notifier>> = [("bad", true), ("good", false), ("worse", true)]
            .into_iter()
            .map(|(name, failing)| {
                Arc::new(NamedNotifier {
                    name,
                    trace: Arc::clone(&trace),
                    failing,
                }) as Arc<dyn Notifier>
            })
            .collect();

        let result = MultiNotifier::new(channels).notify(&alert()).await;

        // 全部渠道都被尝试过，组合本身不返回错误（失败已经打成了警告）
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(*trace.lock().unwrap(), ["bad", "good", "worse"]);
    }

    /// 空组合是合法配置：一个渠道都没有时投递是 no-op。
    #[tokio::test]
    async fn an_empty_multi_notifier_is_a_noop() {
        assert!(
            MultiNotifier::new(Vec::new())
                .notify(&alert())
                .await
                .is_ok()
        );
    }
}
