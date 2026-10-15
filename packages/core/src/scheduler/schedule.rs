//! 调度表达式：interval（`<n><s|m|h>`）与 5 字段 cron 的解析与匹配。
//!
//! cron 只认 `分 时 日 月 周` 五个字段——**没有秒、没有年**，也不接受 `jan`/`mon` 之类英文名：
//! 每个字段是 `*`、单值、`a-b` 区间、`a,b,c` 列表或 `*/N` 步长（`a/N`、`a-b/N` 同样接受）。
//! 区间必须递增且落在界内：分 0-59、时 0-23、日 1-31、月 1-12、周 0-6（周日 = 0）。
//!
//! 日匹配按**本实现钉死的字面口径**：`日`、`周` 两个字段里，只要有任意一个不是字面 `*`（写成
//! 单值、区间、列表或步长形式都算「受限」）就按「或」（任一命中即命中），两个都是字面 `*` 时按
//! 「与」——`0 0 1 * 1` 因此是「每月 1 日或每周一」。
//!
//! 注意这与 Vixie cron 源码**不同**：Vixie 按「字段是否以 `*` 开头」判定未受限，所以 `*/2` 在
//! Vixie 里是 `DOM_STAR`，`0 0 */2 * 1` 会变成「奇数日且周一」；本实现认字面口径，同一个表达式
//! 是「奇数日或周一」。钉死字面口径是因为解析与匹配都由本层负责，两处必须共用同一份语义。

use serde::{Deserialize, Serialize};

use super::SchedulerError;

/// 调度类型（小写序列化：`interval` / `cron`），与 `jobs.schedule_kind` 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleKind {
    /// 固定间隔：`<正整数><s|m|h>`。
    Interval,
    /// 5 字段 cron 表达式。
    Cron,
}

impl ScheduleKind {
    /// 写入 `jobs.schedule_kind` 的字面量。
    pub fn as_str(&self) -> &'static str {
        match self {
            ScheduleKind::Interval => "interval",
            ScheduleKind::Cron => "cron",
        }
    }

    /// 反解 `jobs.schedule_kind`；未知取值返回中文错误。
    fn parse(kind: &str) -> Result<ScheduleKind, SchedulerError> {
        match kind.trim() {
            "interval" => Ok(ScheduleKind::Interval),
            "cron" => Ok(ScheduleKind::Cron),
            other => Err(SchedulerError::UnknownKind {
                kind: other.to_owned(),
            }),
        }
    }
}

/// 解析后的调度规格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSpec {
    /// 固定间隔，换算成秒。
    Interval { every_secs: u64 },
    /// 5 字段 cron 表达式。
    Cron(CronExpr),
}

/// 按 `jobs.schedule_kind` 的取值解析表达式。
///
/// interval 侧只认 `<正整数><s|m|h>`（至少 1 秒、最大 `i64::MAX` 秒），cron 侧只认 5 个子段；
/// 两侧的错误都带上原表达式。
pub fn parse_schedule(kind: &str, expr: &str) -> Result<ScheduleSpec, SchedulerError> {
    match ScheduleKind::parse(kind)? {
        ScheduleKind::Interval => Ok(ScheduleSpec::Interval {
            every_secs: parse_interval(expr)?,
        }),
        ScheduleKind::Cron => Ok(ScheduleSpec::Cron(CronExpr::parse(expr)?)),
    }
}

/// 解析 interval 表达式：`<正整数><s|m|h>`，换算成秒（至少 1 秒，上限 `i64::MAX` 秒）。
fn parse_interval(expr: &str) -> Result<u64, SchedulerError> {
    let invalid = |reason: String| SchedulerError::Interval {
        expr: expr.to_owned(),
        reason,
    };
    let value = expr.trim();
    let Some(unit) = value.chars().next_back() else {
        return Err(invalid("表达式为空".into()));
    };
    let seconds_per_unit = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        other => {
            return Err(invalid(format!(
                "未知的时间单位「{other}」，只支持 s / m / h"
            )));
        }
    };
    // 单位占一个 ASCII 字节，按它的长度切出数字部分
    let digits = &value[..value.len() - unit.len_utf8()];
    if digits.is_empty() {
        return Err(invalid("数字部分为空".into()));
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid(format!("数字部分「{digits}」不是正整数")));
    }
    let count: u64 = digits
        .parse()
        .map_err(|_| invalid(format!("数值「{digits}」过大")))?;
    if count == 0 {
        return Err(invalid("间隔至少为 1 秒".into()));
    }
    let seconds = count
        .checked_mul(seconds_per_unit)
        .ok_or_else(|| invalid(format!("数值「{digits}{unit}」过大")))?;
    // 秒数上限收到 `i64::MAX`：`next_due` 的时间算术全在 i64 上，放不进去的间隔没有意义，
    // 在这里当场拒绝——而不是让它悄悄退化成「永不到期」哨兵，两者必须可区分。
    if seconds > i64::MAX as u64 {
        return Err(invalid(format!(
            "间隔「{digits}{unit}」过大：最大只能是 {} 秒",
            i64::MAX
        )));
    }
    Ok(seconds)
}

/// 搜索上限：4 年（按 366 天/年取上界）。越过它仍无命中即视为「永不到期」，返回 `None`。
const MAX_SEARCH_MINUTES: i64 = 4 * 366 * 24 * 60;

/// 解析后的 5 字段 cron 表达式：每个字段是一张位掩码，匹配只做几次位测试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    /// 第 i 位 = 第 i 分钟被选中。
    minutes: u64,
    /// 第 i 位 = 第 i 小时被选中。
    hours: u64,
    /// 第 i 位 = 第 i 日（1-31）被选中。
    days_of_month: u64,
    /// 第 i 位 = 第 i 月（1-12）被选中。
    months: u64,
    /// 第 i 位 = 星期 i（0 = 周日）被选中。
    days_of_week: u64,
    /// `日` 字段不是字面 `*`。
    dom_restricted: bool,
    /// `周` 字段不是字面 `*`。
    dow_restricted: bool,
}

impl CronExpr {
    /// 解析 5 字段 cron 表达式；字段名、越界值、非递增区间都报中文错误。
    pub fn parse(expr: &str) -> Result<CronExpr, SchedulerError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        let [minute, hour, day_of_month, month, day_of_week] = fields.as_slice() else {
            return Err(SchedulerError::Cron {
                expr: expr.to_owned(),
                reason: format!(
                    "需要 5 个子段（分 时 日 月 周），实际 {} 个（不支持秒与年字段）",
                    fields.len()
                ),
            });
        };
        Ok(CronExpr {
            minutes: parse_field(minute, "分", 0, 59, expr)?,
            hours: parse_field(hour, "时", 0, 23, expr)?,
            days_of_month: parse_field(day_of_month, "日", 1, 31, expr)?,
            months: parse_field(month, "月", 1, 12, expr)?,
            days_of_week: parse_field(day_of_week, "周", 0, 6, expr)?,
            dom_restricted: *day_of_month != "*",
            dow_restricted: *day_of_week != "*",
        })
    }

    /// 严格晚于 `after`（unix 秒）的第一个命中分钟；4 年搜索窗内不可达时返回 `None`。
    pub fn next_after(&self, after: i64) -> Option<i64> {
        // 从 after 之后的第一个整分钟起逐分钟推进，整分钟对齐由 `div_euclid` 保证
        let mut at = after.div_euclid(60).saturating_add(1).saturating_mul(60);
        for _ in 0..MAX_SEARCH_MINUTES {
            if self.matches(at) {
                return Some(at);
            }
            at = at.checked_add(60)?;
        }
        None
    }

    /// `at`（整分钟）是否命中本表达式。
    fn matches(&self, at: i64) -> bool {
        let minute = at.div_euclid(60).rem_euclid(60) as u32;
        let hour = at.div_euclid(3_600).rem_euclid(24) as u32;
        let days = at.div_euclid(86_400);
        let (_, month, day) = civil_from_days(days);
        // 1970-01-01 是周四 → 0 = 周日
        let weekday = (days + 4).rem_euclid(7) as u32;
        if (self.minutes >> minute) & 1 == 0
            || (self.hours >> hour) & 1 == 0
            || (self.months >> month) & 1 == 0
        {
            return false;
        }
        let dom_ok = (self.days_of_month >> day) & 1 == 1;
        let dow_ok = (self.days_of_week >> weekday) & 1 == 1;
        // 两边都受限时按「或」，否则（未受限的一边位掩码全 1）按「与」
        if self.dom_restricted && self.dow_restricted {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }
}

/// 解析一个 cron 字段，返回位掩码（第 i 位 = 取值 i 被选中）。
///
/// 字段是逗号分隔的列表，每项为 `*`、`a`、`a-b`、`*/N`、`a/N` 或 `a-b/N`；`a/N` 按自然推广
/// 理解为「从 a 到上界、步长 N」。步长不受字段上界约束（`u32` 全范围都收，只有 0 被拒）：
/// 比区间跨度还大时只标记起点那一位，推进用 `saturating_add`，绝不上溢。
fn parse_field(
    field: &str,
    name: &str,
    min: u32,
    max: u32,
    expr: &str,
) -> Result<u64, SchedulerError> {
    let invalid = |reason: String| SchedulerError::Cron {
        expr: expr.to_owned(),
        reason,
    };
    let mut mask = 0u64;
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step = parse_number(step, name, "步长", expr)?;
                if step == 0 {
                    return Err(invalid(format!("字段「{name}」的步长不能为 0")));
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((lo, hi)) = range.split_once('-') {
            (
                parse_number(lo, name, "取值", expr)?,
                parse_number(hi, name, "取值", expr)?,
            )
        } else {
            let value = parse_number(range, name, "取值", expr)?;
            if part.contains('/') {
                (value, max)
            } else {
                (value, value)
            }
        };
        if lo < min || lo > max || hi < min || hi > max {
            return Err(invalid(format!(
                "字段「{name}」的取值「{range}」超出 {min}-{max} 的范围"
            )));
        }
        if lo > hi {
            return Err(invalid(format!(
                "字段「{name}」的区间「{range}」不是递增的"
            )));
        }
        let mut value = lo;
        while value <= hi {
            mask |= 1u64 << value;
            // 步长可以是任意 u32（只拒 0）：`value + step` 会溢出 u32（debug panic / release
            // 回绕成错误掩码），饱和推进让循环在下一轮自然退出，只留起点那一位。
            value = value.saturating_add(step);
        }
    }
    Ok(mask)
}

/// 解析一个十进制数值；`position` 说明它在字段里的位置（取值 / 步长），用于错误消息。
fn parse_number(raw: &str, name: &str, position: &str, expr: &str) -> Result<u32, SchedulerError> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SchedulerError::Cron {
            expr: expr.to_owned(),
            reason: format!("字段「{name}」的{position}「{raw}」不是数字"),
        });
    }
    raw.parse().map_err(|_| SchedulerError::Cron {
        expr: expr.to_owned(),
        reason: format!("字段「{name}」的{position}「{raw}」过大"),
    })
}

/// 把「自 1970-01-01 起的天数」换算成公历年月日（Howard Hinnant 的 civil_from_days 算法）。
///
/// 手写而不引日历库：这里只要日/月/周几，公式是纯整数运算，1970 年前（负数天）同样正确。
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
    use super::{CronExpr, ScheduleKind, ScheduleSpec, parse_schedule};

    fn interval(expr: &str) -> ScheduleSpec {
        parse_schedule("interval", expr).unwrap()
    }

    fn cron(expr: &str) -> CronExpr {
        CronExpr::parse(expr).unwrap()
    }

    #[test]
    fn schedule_kind_round_trips_as_lowercase_json() {
        for (kind, literal) in [
            (ScheduleKind::Interval, "interval"),
            (ScheduleKind::Cron, "cron"),
        ] {
            assert_eq!(kind.as_str(), literal);
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{literal}\"")
            );
            assert_eq!(
                serde_json::from_str::<ScheduleKind>(&format!("\"{literal}\"")).unwrap(),
                kind
            );
        }
        assert!(serde_json::from_str::<ScheduleKind>("\"Interval\"").is_err());
    }

    #[test]
    fn parse_schedule_maps_kind_to_spec() {
        assert_eq!(interval("10m"), ScheduleSpec::Interval { every_secs: 600 });
        assert!(matches!(
            parse_schedule("cron", "*/5 * * * *").unwrap(),
            ScheduleSpec::Cron(_)
        ));

        // 未知类型：点名它，且不会拿表达式去猜
        let err = parse_schedule("every", "10m").unwrap_err().to_string();
        assert!(err.contains("every"), "{err}");
        assert!(err.contains("interval"), "{err}");
    }

    #[test]
    fn interval_parse_reads_seconds_minutes_hours() {
        assert_eq!(interval("1s"), ScheduleSpec::Interval { every_secs: 1 });
        assert_eq!(interval("30s"), ScheduleSpec::Interval { every_secs: 30 });
        assert_eq!(interval("10m"), ScheduleSpec::Interval { every_secs: 600 });
        assert_eq!(interval("2h"), ScheduleSpec::Interval { every_secs: 7_200 });
        assert_eq!(
            interval("90m"),
            ScheduleSpec::Interval { every_secs: 5_400 }
        );
        // 首尾空白容错
        assert_eq!(interval(" 5s "), ScheduleSpec::Interval { every_secs: 5 });
    }

    #[test]
    fn interval_parse_rejects_bad_input_with_the_expression_in_the_message() {
        for (expr, needle) in [
            ("0s", "至少为 1 秒"),
            ("0m", "至少为 1 秒"),
            ("", "为空"),
            ("   ", "为空"),
            ("10", "单位"),
            ("10x", "单位"),
            ("10S", "单位"),
            ("s", "数字"),
            ("-5s", "数字"),
            ("1.5s", "数字"),
            ("10 m", "数字"),
            ("99999999999999999999h", "过大"),
        ] {
            let err = parse_schedule("interval", expr).unwrap_err().to_string();
            assert!(err.contains(needle), "{expr}: {err}");
            assert!(
                err.contains(&format!("「{expr}」")),
                "{expr}: 错误里必须带原表达式：{err}"
            );
        }
    }

    #[test]
    fn interval_parse_rejects_values_that_do_not_fit_in_i64_seconds() {
        // 上限是 i64::MAX 秒（`next_due` 的时间算术全在 i64 上）
        assert_eq!(
            interval("9223372036854775807s"),
            ScheduleSpec::Interval {
                every_secs: i64::MAX as u64
            }
        );
        for expr in [
            "9223372036854775808s",  // i64::MAX + 1 秒
            "18446744073709551615s", // u64::MAX 秒
            "18446744073709551615m", // 换算成秒连 u64 都装不下
        ] {
            let err = parse_schedule("interval", expr).unwrap_err().to_string();
            assert!(err.contains("过大"), "{expr}: {err}");
            assert!(
                err.contains(&format!("「{expr}」")),
                "{expr}: 错误里必须带原表达式：{err}"
            );
        }
    }

    #[test]
    fn cron_parse_accepts_every_field_form() {
        for expr in [
            "* * * * *",
            "*/5 * * * *",
            "0 9 * * 1-5",
            "30 2 1,15 * *",
            "0 0/6 * * *",
            "5-10/2 * * * *",
            "0 0 * * 0,6",
            "15 3 * * 1,3,5",
            "0 0 * 1,6,12 *",
            "59 23 31 12 6",
        ] {
            assert!(CronExpr::parse(expr).is_ok(), "{expr} 应当可解析");
        }
    }

    #[test]
    fn cron_parse_rejects_malformed_fields_with_field_and_expression_in_the_message() {
        for (expr, needle) in [
            // 字段数不对（含秒/年字段的写法）
            ("* * * *", "5 个子段"),
            ("0 0 1 1 1 1", "5 个子段"),
            ("", "5 个子段"),
            // 越界
            ("60 * * * *", "分"),
            ("* 24 * * *", "时"),
            ("* * 0 * *", "日"),
            ("* * 32 * *", "日"),
            ("* * * 13 *", "月"),
            ("* * * * 7", "周"),
            // 英文名
            ("* * * jan *", "月"),
            ("* * * * MON", "周"),
            // 区间必须递增
            ("30-10 * * * *", "递增"),
            // 步长
            ("*/0 * * * *", "步长"),
            ("*/5x * * * *", "步长"),
            // 列表里的空项
            ("1,,2 * * * *", "分"),
        ] {
            let err = CronExpr::parse(expr).unwrap_err().to_string();
            assert!(err.contains(needle), "{expr}: {err}");
            assert!(
                err.contains(&format!("「{expr}」")),
                "{expr}: 错误里必须带原表达式：{err}"
            );
        }
    }

    #[test]
    fn cron_next_after_steps_five_minute_marks() {
        // 基准 1704067200 = 2024-01-01T00:00:00Z（自 epoch 起 19723 天 × 86400 秒）
        let expr = cron("*/5 * * * *");
        // 它正好落在 5 分钟线上（1704067200 / 300 = 5680224）：严格晚于 → 下一格 +300 秒
        assert_eq!(expr.next_after(1_704_067_200), Some(1_704_067_500));
        // 线后 1 秒 → 同样回到下一格
        assert_eq!(expr.next_after(1_704_067_201), Some(1_704_067_500));
        // 上一格之后 1 秒（1704067140 = 2023-12-31T23:59:00Z）→ 基准点本身
        assert_eq!(expr.next_after(1_704_067_140), Some(1_704_067_200));
    }

    #[test]
    fn cron_next_after_skips_the_weekend_for_weekday_mornings() {
        // 0 9 * * 1-5 = 周一至周五 09:00 UTC
        let expr = cron("0 9 * * 1-5");
        // 2024-01-05（周五）00:00 = 1704067200 + 4 天 = 1704412800 → 当日 09:00（+32400 秒）
        assert_eq!(expr.next_after(1_704_412_800), Some(1_704_445_200));
        // 周五 09:00 就问「下一次」→ 跳过周六周日：2024-01-08（周一）09:00（+3 天 32400 秒）
        assert_eq!(expr.next_after(1_704_445_200), Some(1_704_704_400));
        // 2024-01-06（周六）00:00 = +86400 秒 → 同样落到周一 09:00
        assert_eq!(expr.next_after(1_704_499_200), Some(1_704_704_400));
    }

    #[test]
    fn cron_next_after_handles_day_lists() {
        // 30 2 1,15 * * = 每月 1 日与 15 日的 02:30 UTC
        let expr = cron("30 2 1,15 * *");
        // 2024-01-01 00:00 → 当日 02:30（+9000 秒）
        assert_eq!(expr.next_after(1_704_067_200), Some(1_704_076_200));
        // 1 日 02:30 之后 → 1 月 15 日 02:30（+14 天 +9000 秒）
        assert_eq!(expr.next_after(1_704_076_200), Some(1_705_285_800));
        // 15 日之后 → 2 月 1 日 02:30（2024-02-01 = +31 天 = 1706745600，再加 9000 秒）
        assert_eq!(expr.next_after(1_705_285_800), Some(1_706_754_600));
    }

    #[test]
    fn cron_dom_and_dow_are_ored_when_both_are_restricted() {
        // 0 12 1 * 1 = 每月 1 日 12:00 或每周一 12:00
        let expr = cron("0 12 1 * 1");
        // 2024-02-01 是周四（1706745600 = 当日 00:00）→ 命中「1 日」这条腿
        assert_eq!(expr.next_after(1_706_745_600), Some(1_706_788_800));
        // 2024-02-02 00:00 起本月 1 日已过 → 命中「周一」这条腿：2 月 5 日 12:00
        assert_eq!(expr.next_after(1_706_832_000), Some(1_707_134_400));
        // 若按「与」理解，2 月的周一没有一天是 1 日，会一路跳到 2024-04-01（周一）——
        // 与上面的结果不同，这条断言因此钉死了「或」语义。
        // 只有一边受限（`周` 是 `*`）时按「与」：0 12 * * 1 = 只看周一
        let monday_only = cron("0 12 * * 1");
        assert_eq!(monday_only.next_after(1_706_745_600), Some(1_707_134_400));
    }

    #[test]
    fn cron_dom_step_form_counts_as_restricted_and_is_ored_with_dow() {
        // 0 0 */2 * 1：`日` 取奇数日（1,3,…,31），`周` 取周一。`*/2` 不是字面 `*` → 两边都受限
        // → 按「或」。起点 2024-02-10T00:00Z（周六、10 日，两条腿都不命中）：下一个命中是
        // 2024-02-11T00:00Z（周日、11 日）——只有「日」这条腿命中，结果因此钉死「或」。
        let expr = cron("0 0 */2 * 1");
        // 1707523200 = 2024-02-10T00:00Z；1707609600 = 2024-02-11T00:00Z（+86400 秒）
        assert_eq!(expr.next_after(1_707_523_200), Some(1_707_609_600));
        // 若按 Vixie 源码（字段以 `*` 开头即视为未受限 → 这里退化成「与」），下一个命中要等到
        // 2024-02-19（周一、19 日）00:00 = 1708300800，与本断言不同。
    }

    #[test]
    fn cron_expression_that_never_matches_is_none() {
        // 2 月 30 日不存在：4 年搜索窗内一次都不会命中
        let expr = cron("0 0 30 2 *");
        assert_eq!(expr.next_after(1_704_067_200), None);
    }

    #[test]
    fn cron_next_after_reaches_a_leap_day_inside_the_search_window() {
        // 0 0 29 2 * = 2 月 29 日 00:00；从 2024-03-01（2024 年的 2 月 29 日已过）往后数，
        // 下一个是 2028-02-29 = 1709251200 + 1460 天 × 86400 秒，仍在 4 年（1464 天）窗内
        let expr = cron("0 0 29 2 *");
        assert_eq!(expr.next_after(1_709_251_200), Some(1_835_395_200));
    }

    #[test]
    fn cron_huge_step_in_star_form_marks_only_the_start_value() {
        // `*/N` 落在「日」（下界 1）：1 + u32::MAX 会溢出 u32 —— 步长必须饱和推进，只留第 1 日。
        // 第一行是评审点名的原样输入。
        assert_eq!(cron("* * */4294967295 * *").days_of_month, 1 << 1);
        let expr = cron("0 0 */4294967295 * *");
        assert_eq!(expr.days_of_month, 1 << 1);
        // 每月 1 日 00:00：2024-01-01T00:00Z 之后是 2024-02-01T00:00Z（1706745600）
        assert_eq!(expr.next_after(1_704_067_200), Some(1_706_745_600));
    }

    #[test]
    fn cron_huge_step_in_value_form_marks_only_the_start_value() {
        // `a/N`：只剩第 30 分钟
        let expr = cron("30/4294967295 * * * *");
        assert_eq!(expr.minutes, 1 << 30);
    }

    #[test]
    fn cron_huge_step_in_range_form_marks_only_the_start_value() {
        // `a-b/N`：只剩区间起点第 5 分钟
        let expr = cron("5-10/4294967295 * * * *");
        assert_eq!(expr.minutes, 1 << 5);
    }

    #[test]
    fn cron_next_after_handles_negative_after_before_1970() {
        // 0 0 1 * *：每月 1 日 00:00；整分钟对齐用 `div_euclid`，负数秒同样正确。
        let expr = cron("0 0 1 * *");
        // after = -10000000（1969-09-07T06:13:20Z）→ 1969-10-01T00:00Z（epoch 前 92 天）
        assert_eq!(expr.next_after(-10_000_000), Some(-7_948_800));
        // after = -86400（1969-12-31T00:00Z）→ 跨过 epoch：1970-01-01T00:00Z
        assert_eq!(expr.next_after(-86_400), Some(0));
        // 含 `周` 字段的表达式在 1970 年前同样按 UTC 星期几匹配：1969-12-31 是周三，
        // after = -3600（当日 23:00Z）→ 1970-01-01（周四）09:00Z
        assert_eq!(cron("0 9 * * 1-5").next_after(-3_600), Some(32_400));
    }
}
