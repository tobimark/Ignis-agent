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

use super::ScheduleSpec;

/// cron 在搜索窗内不可达时的哨兵值：`next_due` 返回它表示「永不到期」。
const NEVER_DUE: i64 = i64::MAX;

/// 计算 `spec` 的下一次到期时刻（unix 秒）。
///
/// interval 取「`last_fired + every` 仍晚于 now 就按它，否则 now + every」；cron 取严格晚于
/// `now` 的下一个命中分钟。cron 表达式在 4 年搜索窗内不可达（如 `0 0 30 2 *`）时返回
/// `i64::MAX`——调用方据此视为「永不到期」，而不是编造一个时点。
pub fn next_due(spec: &ScheduleSpec, now_secs: i64, last_fired: Option<i64>) -> i64 {
    match spec {
        ScheduleSpec::Interval { every_secs } => {
            let every = i64::try_from(*every_secs).unwrap_or(i64::MAX);
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
    use crate::scheduler::{CronExpr, ScheduleSpec};

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
        assert_eq!(next_due(&spec, 1_704_067_200, None), i64::MAX);
    }
}
