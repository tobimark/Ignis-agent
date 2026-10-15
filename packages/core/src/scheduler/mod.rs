//! 调度层：解析任务的时间节奏与执行边界，并算出下一次到期时刻。
//!
//! 这一层是**纯函数**的：不碰数据库、不读时钟、不起后台任务，时间一律由调用方以 unix 秒
//! （`i64`）传入。`schedule_create` 元工具用它拦住非法输入，守护进程用它决定「下一次什么
//! 时候跑」——两边共用同一份语义，不存在第二套实现。
//!
//! 三条硬规则：
//! - **无补跑**：到期时刻永远从「现在」或「上次运行」往后算，停机错过的时间点不会被补齐
//!   （见 `due` 模块）。
//! - **cron 走 Vixie 语义**：`日` 与 `周` 都不是字面 `*` 时按「或」匹配（见 [`CronExpr`]）。
//! - **执行边界显式声明**：job 的 [`JobProfile`] 必须给出工具白名单与风险等级，
//!   [`validate_job`] 再核对白名单里的名字确实存在于内核注册的工具里。
//!
//! 所有错误消息都是中文，并带上出错的原文（表达式、字段名或工具名）。

mod due;
mod schedule;

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub use due::next_due;
pub use schedule::{CronExpr, ScheduleKind, ScheduleSpec, parse_schedule};

/// job 的执行限额，复用运行时 [`ignis_runtime::Limits`]（字段可逐个缺省）。
pub type JobLimits = ignis_runtime::Limits;

/// 调度层错误：解析（interval / cron）、job 边界（profile / limits）与可用性校验。
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    /// `kind` 不是 `interval` / `cron`。
    #[error("未知的调度类型「{kind}」，只支持 interval 或 cron")]
    UnknownKind { kind: String },
    /// interval 表达式不合法。
    #[error("无法解析调度间隔「{expr}」：{reason}")]
    Interval { expr: String, reason: String },
    /// cron 表达式不合法。
    #[error("无法解析 cron 表达式「{expr}」：{reason}")]
    Cron { expr: String, reason: String },
    /// job profile 的 JSON 形状或取值不合法。
    #[error("job profile 不合法：{reason}")]
    Profile { reason: String },
    /// job 限额的 JSON 不合法。
    #[error("job 限额不合法：{reason}")]
    Limits { reason: String },
    /// profile 的工具白名单里出现了当前内核没有的工具名。
    #[error("job profile 引用了不可用的工具：{names}")]
    UnavailableTools { names: String },
}

/// job 的执行边界：工具白名单 + 风险等级，即 `jobs.profile_json` 的反序列化形状。
///
/// 反序列化**拒绝未知字段**（字段名写错要当场报错，不能悄悄退化成默认值）；取值校验
/// （非空、非空白、不重名）见 [`parse_profile`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobProfile {
    /// 该 job 允许调用的工具名。
    pub tools: Vec<String>,
    /// 该 job 的风险等级。
    pub risk: ProfileRisk,
}

/// job 的风险等级（小写序列化：`safe` / `elevated`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileRisk {
    /// 只读类工具，无需审批。
    Safe,
    /// 会改动系统或外发数据的工具；无人值守时无人可批，按拒绝处理。
    Elevated,
}

/// 解析 `jobs.profile_json`。
///
/// JSON 形状不合法（未知字段、缺字段、`risk` 取值不在册）与取值不合法（`tools` 为空、项为
/// 空白、重名）都返回中文错误。`tools` 里的名字先裁掉首尾空白，落回来的也是裁过的那份——
/// 否则「 fs_read 」这种多一个空格的写法会在 [`validate_job`] 那里变成「不可用工具」。
pub fn parse_profile(json: &str) -> Result<JobProfile, SchedulerError> {
    let invalid = |reason: String| SchedulerError::Profile { reason };
    let mut profile: JobProfile =
        serde_json::from_str(json).map_err(|err| invalid(err.to_string()))?;
    if profile.tools.is_empty() {
        return Err(invalid("tools 不能为空".into()));
    }
    let mut tools = Vec::with_capacity(profile.tools.len());
    let mut seen = HashSet::with_capacity(profile.tools.len());
    for tool in &profile.tools {
        let name = tool.trim();
        if name.is_empty() {
            return Err(invalid("工具名不能为空或只有空白".into()));
        }
        if !seen.insert(name) {
            return Err(invalid(format!("工具名「{name}」重复")));
        }
        tools.push(name.to_owned());
    }
    profile.tools = tools;
    Ok(profile)
}

/// 解析 `jobs.limits_json`（[`JobLimits`] 的 serde 形状，字段可逐个缺省）。
///
/// 与 profile 不同，这里**不拒绝未知字段**：限额的形状由运行时类型定义，多出来的键按 serde
/// 口径忽略，与配置文件、命令行一致。
pub fn parse_limits(json: &str) -> Result<JobLimits, SchedulerError> {
    serde_json::from_str(json).map_err(|err| SchedulerError::Limits {
        reason: err.to_string(),
    })
}

/// 校验 job 的工具白名单：`profile.tools` 必须都在 `available`（内核真正注册的工具名）里。
///
/// 不在册的名字**一次全部列出**（按 profile 里的先后顺序），避免逐个试错。
pub fn validate_job(profile: &JobProfile, available: &[String]) -> Result<(), SchedulerError> {
    let known: HashSet<&str> = available.iter().map(String::as_str).collect();
    let unknown: Vec<&str> = profile
        .tools
        .iter()
        .map(String::as_str)
        .filter(|tool| !known.contains(tool))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(SchedulerError::UnavailableTools {
        names: unknown.join("、"),
    })
}

#[cfg(test)]
mod tests {
    use super::{JobProfile, ProfileRisk, parse_limits, parse_profile, validate_job};
    use ignis_runtime::Limits;

    #[test]
    fn profile_risk_round_trips_as_lowercase_json() {
        for (risk, literal) in [
            (ProfileRisk::Safe, "safe"),
            (ProfileRisk::Elevated, "elevated"),
        ] {
            assert_eq!(
                serde_json::to_string(&risk).unwrap(),
                format!("\"{literal}\"")
            );
            assert_eq!(
                serde_json::from_str::<ProfileRisk>(&format!("\"{literal}\"")).unwrap(),
                risk
            );
        }
        assert!(serde_json::from_str::<ProfileRisk>("\"Elevated\"").is_err());
    }

    #[test]
    fn profile_parses_tools_and_risk() {
        let profile =
            parse_profile(r#"{"tools":["fs_read","shell_run"],"risk":"elevated"}"#).unwrap();
        assert_eq!(profile.tools, ["fs_read", "shell_run"]);
        assert_eq!(profile.risk, ProfileRisk::Elevated);

        // 名字两边多出来的空白在解析时就裁掉
        let padded = parse_profile(r#"{"tools":[" fs_read "],"risk":"safe"}"#).unwrap();
        assert_eq!(padded.tools, ["fs_read"]);

        // 形状与 `jobs.profile_json` 一致，可整体往返
        let round_tripped: JobProfile =
            serde_json::from_str(&serde_json::to_string(&profile).unwrap()).unwrap();
        assert_eq!(round_tripped, profile);
    }

    #[test]
    fn profile_rejects_unknown_fields_and_bad_values() {
        for (json, needle) in [
            // 未知字段：不许悄悄忽略
            (r#"{"tools":["fs_read"],"risk":"safe","extra":1}"#, "extra"),
            // 空列表
            (r#"{"tools":[],"risk":"safe"}"#, "tools"),
            // 空白项
            (r#"{"tools":[""],"risk":"safe"}"#, "工具名"),
            (r#"{"tools":["  "],"risk":"safe"}"#, "工具名"),
            // 裁空白后重名
            (r#"{"tools":["fs_read"," fs_read"],"risk":"safe"}"#, "重复"),
            // risk 只认 safe / elevated
            (r#"{"tools":["fs_read"],"risk":"dangerous"}"#, "dangerous"),
            // 缺字段
            (r#"{"risk":"safe"}"#, "tools"),
            (r#"{"tools":["fs_read"]}"#, "risk"),
            // 根本不是 JSON
            ("not json", "job profile"),
        ] {
            let err = parse_profile(json).unwrap_err().to_string();
            assert!(err.contains(needle), "{json}: {err}");
        }
    }

    #[test]
    fn limits_parse_uses_the_runtime_shape() {
        // 未知键按 serde 口径忽略（与 `jobs` 表里既有的 `{"max_turns":8}` 写法兼容）
        let limits = parse_limits(r#"{"max_steps":5,"max_turns":8}"#).unwrap();
        assert_eq!(limits.max_steps, 5);
        assert_eq!(
            limits,
            Limits {
                max_steps: 5,
                ..Limits::default()
            }
        );

        assert_eq!(parse_limits("{}").unwrap(), Limits::default());

        let err = parse_limits(r#"{"max_steps":"x"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("限额"), "{err}");
    }

    #[test]
    fn validate_job_lists_every_name_outside_the_available_tools() {
        let available = ["fs_read".to_owned(), "shell_run".to_owned()];

        let profile =
            parse_profile(r#"{"tools":["fs_read","nope","also_nope"],"risk":"safe"}"#).unwrap();
        let err = validate_job(&profile, &available).unwrap_err().to_string();
        assert!(err.contains("nope、also_nope"), "{err}");
        assert!(!err.contains("fs_read"), "{err}");

        let ok = parse_profile(r#"{"tools":["fs_read","shell_run"],"risk":"safe"}"#).unwrap();
        assert!(validate_job(&ok, &available).is_ok());
    }
}
