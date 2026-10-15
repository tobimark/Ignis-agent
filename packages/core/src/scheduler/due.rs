//! 到期时刻计算：从「现在」与「上次运行」推出下一次该在什么时候跑。
//!
//! 口径只有一条——**无补跑**：`next_due` 从不返回已经过去的时刻，停机期间错过的时间点就被
//! 跳过。守护进程重启后因此不会立刻倾泻一批「欠账」运行，重新起跑永远从当前时刻往后算。
//!
//! interval 分两种情况：
//! 1. `last_fired + every` 仍晚于现在 → 就按它（保持原有节奏，一次运行慢了几秒不会让后续
//!    全部漂移）；
//! 2. 否则（从没跑过，或那个时刻已经过去）→ `now + every`（从此刻重新起算）。
//!
//! cron 没有节奏可保持，直接取严格晚于 `now` 的下一个命中分钟；表达式在 4 年搜索窗内不可达
//! （如 `0 0 30 2 *`）时返回 [`NEVER_DUE`]，调用方据此视为「永不触发」，而不是编造一个时点。
//! 同一个哨兵也用于手工构造的、大到 `i64` 秒装不下的 interval——正经的解析入口
//! （[`super::parse_schedule`]）会在解析阶段就用中文错误拒掉这种间隔，两个分支因此可区分。

use super::ScheduleSpec;

/// 不可达时的哨兵值：`next_due` 返回它表示「永不到期」，调用方不要当成一个真实时刻。
///
/// 触发条件有两个：cron 在 4 年搜索窗内不可达（如 `0 0 30 2 *`），或手工构造的 interval
/// 大到 `i64` 秒装不下（`parse_schedule` 已把这种间隔挡在解析阶段）。合法但逼近 `i64::MAX`
/// 的间隔加到 `now` 上也会饱和到这个值——那已经远在 2900 亿年之后。
pub const NEVER_DUE: i64 = i64::MAX;

/// 计算 `spec` 的下一次到期时刻（unix 秒）。
///
/// interval 取「`last_fired + every` 仍晚于 now 就按它，否则 now + every」；cron 取严格晚于
/// `now` 的下一个命中分钟。返回 [`NEVER_DUE`] 表示「永不到期」：`i64::MAX` 不是 2920 亿年后
/// 的时间点，调用方应跳过它而不是拿它去 `sleep`。
pub fn next_due(spec: &ScheduleSpec, now_secs: i64, last_fired: Option<i64>) -> i64 {
    match spec {
        ScheduleSpec::Interval { every_secs } => {
            // 解析入口已保证秒数放得进 i64；这里兜底手工构造的越界值
            let Ok(every) = i64::try_from(*every_secs) else {
                return NEVER_DUE;
            };
            last_fired
                .map(|fired| fired.saturating_add(every))
                .filter(|due| *due > now_secs)
                .unwrap_or_else(|| now_secs.saturating_add(every))
        }
        ScheduleSpec::Cron(expr) => expr.next_after(now_secs).unwrap_or(NEVER_DUE),
    }
}

#[cfg(test)]
mod tests {
    use super::next_due;
    // 从 `scheduler` 重导出的路径取哨兵：顺带钉住它在 mod.rs 的再导出
    use crate::scheduler::{CronExpr, NEVER_DUE, ScheduleSpec};

    fn interval(every_secs: u64) -> ScheduleSpec {
        ScheduleSpec::Interval { every_secs }
    }

    #[test]
    fn interval_due_keeps_the_beat_or_restarts_from_now() {
        let spec = interval(600);
        // 从没跑过 → now + every
        assert_eq!(next_due(&spec, 10_000, None), 10_600);
        // 上次运行很久以前（连 last_fired + every 都过去了）→ 不补跑，从 now 重新起算
        assert_eq!(next_due(&spec, 10_000, Some(1_000)), 10_600);
        // 上次运行就在上一格内（last_fired + every 晚于 now）→ 保持原节奏
        assert_eq!(next_due(&spec, 10_000, Some(9_500)), 10_100);
        // last_fired 在未来（时钟回拨或手工改表）→ 仍从 last_fired 起算
        assert_eq!(next_due(&spec, 10_000, Some(10_500)), 11_100);
    }

    #[test]
    fn interval_due_never_returns_a_past_instant() {
        // 停机一整天后再问：给的是 now + every，而不是把错过的那一格补上
        let spec = interval(3_600);
        let now = 1_704_067_200 + 86_400;
        assert_eq!(next_due(&spec, now, Some(1_704_067_200)), now + 3_600);
    }

    #[test]
    fn cron_due_is_the_next_matching_minute() {
        let spec = ScheduleSpec::Cron(CronExpr::parse("*/5 * * * *").unwrap());
        // 1704067200 落在 5 分钟线上 → 下一次是 +300 秒
        assert_eq!(next_due(&spec, 1_704_067_200, None), 1_704_067_500);
        // cron 的节奏由表达式决定，last_fired 不参与
        assert_eq!(
            next_due(&spec, 1_704_067_200, Some(1_704_067_200)),
            1_704_067_500
        );
    }

    #[test]
    fn unreachable_cron_never_becomes_due() {
        // 2 月 30 日不存在 → 哨兵值，调用方视为永不到期
        let spec = ScheduleSpec::Cron(CronExpr::parse("0 0 30 2 *").unwrap());
        assert_eq!(next_due(&spec, 1_704_067_200, None), NEVER_DUE);
        assert_eq!(NEVER_DUE, i64::MAX);
    }

    #[test]
    fn interval_that_cannot_fit_in_seconds_is_never_due() {
        // 解析入口会拒掉这种间隔（见 schedule.rs 的 `interval_parse_rejects_…`）；
        // 这里钉住手工构造值的兜底：同样返回哨兵，而不是编造一个真实时刻。
        let spec = ScheduleSpec::Interval {
            every_secs: u64::MAX,
        };
        assert_eq!(next_due(&spec, 10_000, None), NEVER_DUE);
        assert_eq!(next_due(&spec, 10_000, Some(1_000)), NEVER_DUE);
    }
}
