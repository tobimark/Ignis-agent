//! Ignis Gateway：守护进程生命周期、调度循环与通知渠道。
//!
//! 三个模块各管一段：
//! - [`daemon`]：`ignis daemon start|stop|status` 与隐藏的前台入口（pidfile + `libc::flock`
//!   单实例锁 + `setsid` re-exec 自守护）；
//! - [`r#loop`]：调度循环——按 job 的 interval/cron 到期时刻发起完整 `TaskRunner` 运行
//!   （`kind=schedule` + profile 白名单 + 预授权策略），保证「不补跑、不重叠、失败必通知」；
//! - [`notify`]：`Notifier` 的 v0.1 渠道（日志行、`notify-send` 桌面通知），调用方用
//!   `MultiNotifier` 串起来。
//!
//! 依赖方向只有 `gateway → core`（`core → {runtime, storage}`）：gateway 不反向被任何 crate
//! 依赖，也不引入新依赖——`libc::flock` / `setsid` / `notify-send` 全部手搓。

pub mod daemon;
pub mod r#loop;
pub mod notify;

use std::io;
use std::path::{Path, PathBuf};

use ignis_core::config::ConfigError;
use ignis_storage::StorageError;

pub use daemon::{DaemonStatus, StartOutcome, StopOutcome};
pub use r#loop::SchedulerLoop;

/// gateway 的失败原因：进程生命周期与调度循环里「预期内」的失败，消息一律中文人话
/// （CLI 只把它们打成 `error: …`，终端不出现堆栈）。
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// pidfile 的独占锁已被另一个守护进程持有。
    #[error("另一个守护进程已在运行（pid {pid}）")]
    AlreadyRunning { pid: i32 },

    /// 自守护拉起后，子进程在就绪前退出了（配置/密钥/库的问题写在 daemon 日志里）。
    #[error("守护进程启动失败：子进程提前退出（{status}），详见 {log}")]
    ChildExited { status: String, log: PathBuf },

    /// 自守护拉起后，等 pidfile 超时。
    #[error("守护进程启动超时：{secs} 秒内未出现可用的 pidfile（{path}）")]
    StartTimeout { secs: u64, path: PathBuf },

    /// 数据目录或配置文件读不出来。
    #[error("配置不可用：{0}")]
    Config(#[from] ConfigError),

    /// `ignis.db` 读写失败。
    #[error("存储不可用：{0}")]
    Storage(#[from] StorageError),

    /// 文件或进程操作失败；`context` 自带路径/pid 上下文。
    #[error("{context}：{source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },

    /// pidfile 存在（锁被持有）但内容不是合法 pid：被改坏、写入未完成。
    ///
    /// `stop`（没有可信的 pid 可发信号）、`start`（认不出在跑的是谁）与前台抢锁失败共用这一条，
    /// 与 `status` 的 [`daemon::DaemonStatus::BadPidfile`] 是同一个情形、同一套措辞。
    #[error(
        "pidfile 不可读：内容不是合法 pid（{path}）——文件被改坏或写入未完成，先检查（必要时删除）它再重试"
    )]
    BadPidfile { path: PathBuf },

    /// 找不到可用的模型密钥。
    #[error("未配置 API key（设置 {env} 或 config.json 的 api_key）")]
    MissingApiKey { env: String },

    /// 模型客户端装不起来（baseUrl 之类的参数不合法）。
    #[error("模型客户端不可用：{0}")]
    Model(String),
}

impl GatewayError {
    /// 带路径/操作上下文的 io 错误。
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// 「锁被持有但 pidfile 读不出合法 pid」的唯一构造（`start`/`stop`/前台抢锁共用同一句措辞）。
    pub(crate) fn bad_pidfile(path: &Path) -> Self {
        Self::BadPidfile {
            path: path.to_path_buf(),
        }
    }
}

/// gateway 的结果别名。
pub type Result<T, E = GatewayError> = std::result::Result<T, E>;
