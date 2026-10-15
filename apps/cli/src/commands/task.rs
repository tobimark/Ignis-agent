//! `ignis task show|list`：任务详情与任务列表。
//!
//! `show` 的渲染顺序是钉死的：任务头 → 审批记录表 → 事件流。事件流按行读
//! [`TaskLog`] 写的 JSONL：能解析成 `RuntimeEvent` 的行渲染成 `HH:MM:SS 类型 …`，
//! 解析不了的行（`task_start`/`task_end` 这类运行器自己的行、被手工改坏的行）只展示
//! `type` 字段——任何一种行都不能让 `task show` 失败。

use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Result, bail};
use ignis_core::config::ignis_home;
use ignis_runtime::{RunStatus, RuntimeEvent};
use ignis_storage::{ApprovalRecord, Db, TaskLog, TaskRecord};
use serde_json::Value;

use crate::cli::TaskCommand;

/// 事件详情与兜底行的展示上限（按字符数，不按字节）。
const DETAIL_CHARS: usize = 120;
/// `task list` 的 prompt 列宽。
const PROMPT_CHARS: usize = 40;
/// 兜底事件行最多展示的标量字段数与其取值长度。
const FALLBACK_FIELDS: usize = 3;
const FALLBACK_VALUE_CHARS: usize = 40;

/// `ignis task`：`show` 看一条任务，`list` 看最近若干条。
pub fn run(command: &TaskCommand) -> Result<ExitCode> {
    match command {
        TaskCommand::Show { id } => show(id),
        TaskCommand::List { limit } => list(*limit),
    }
}

/// 打印一条任务的详情；任务不存在时报 `task not found: {id}`。
fn show(id: &str) -> Result<ExitCode> {
    print!("{}", show_at(&ignis_home(), id)?);
    Ok(ExitCode::SUCCESS)
}

/// 打印最近的任务表。
fn list(limit: usize) -> Result<ExitCode> {
    let db = Db::open(&ignis_home().join("ignis.db"))?;
    print!("{}", render_list(&db.list_tasks(limit)?));
    Ok(ExitCode::SUCCESS)
}

/// 从 `home` 下读出任务详情文本；`home` 可注入，便于测试。
fn show_at(home: &Path, id: &str) -> Result<String> {
    let db = Db::open(&home.join("ignis.db"))?;
    let Some(task) = db.get_task(id)? else {
        // 未找到是命令层的错误：`main` 打成 `error: task not found: {id}` 并给退出码 1
        bail!("task not found: {id}");
    };
    let approvals = db.list_approvals(id)?;
    // 日志路径与写入方同源（`TaskLog::path`）
    let log_path = TaskLog::new(&home.join("logs"))?.path(id);
    Ok(render_task(&task, &approvals, &log_path)?)
}

/// 任务详情：头部（一字段一行）→ 审批记录表 → 事件流。
///
/// 日志文件缺失按「没有事件流」处理（任务行还在、日志被删）；其它读取失败照常上抛。
fn render_task(
    task: &TaskRecord,
    approvals: &[ApprovalRecord],
    log_path: &Path,
) -> io::Result<String> {
    let ended = task.ended_at.map_or_else(|| "-".to_string(), format_time);
    let error = task.error.as_deref().unwrap_or("-");
    let summary = task.summary.as_deref().unwrap_or("-");
    let mut out = format!(
        "id: {}\nstatus: {}\nkind: {}\nkernel: {}\nworkspace: {}\nstarted: {}\nended: {ended}\n\
         duration: {}\ntokens: {}\nerror: {error}\nsummary: {summary}\n",
        task.id,
        task.status,
        task.kind,
        task.kernel,
        task.workspace,
        format_time(task.started_at),
        format_duration(task.started_at, task.ended_at),
        format_tokens(task),
    );

    out.push_str("\napprovals:\n");
    out.push_str(&render_approvals(approvals));

    out.push_str("\nevents:\n");
    match fs::read_to_string(log_path) {
        Ok(raw) => {
            let lines: Vec<&str> = raw.lines().filter(|line| !line.trim().is_empty()).collect();
            if lines.is_empty() {
                out.push_str("(none)\n");
            }
            for line in lines {
                out.push_str(&render_event_line(line));
                out.push('\n');
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            out.push_str(&format!("(log not found: {})\n", log_path.display()));
        }
        Err(error) => return Err(error),
    }
    Ok(out)
}

/// 最近任务表：id 前 8 位、status、kind、started、prompt 前 40 字符。
fn render_list(tasks: &[TaskRecord]) -> String {
    let mut rows = vec![vec![
        "ID".to_string(),
        "STATUS".to_string(),
        "KIND".to_string(),
        "STARTED".to_string(),
        "PROMPT".to_string(),
    ]];
    for task in tasks {
        rows.push(vec![
            id_prefix(&task.id),
            task.status.clone(),
            task.kind.clone(),
            format_time(task.started_at),
            truncate(&one_line(&task.prompt), PROMPT_CHARS),
        ]);
    }
    render_table(&rows)
}

/// 审批记录表：tool / risk / decision / mode / time。
fn render_approvals(approvals: &[ApprovalRecord]) -> String {
    if approvals.is_empty() {
        return "(none)\n".to_string();
    }
    let mut rows = vec![vec![
        "TOOL".to_string(),
        "RISK".to_string(),
        "DECISION".to_string(),
        "MODE".to_string(),
        "TIME".to_string(),
    ]];
    for approval in approvals {
        rows.push(vec![
            approval.tool.clone(),
            approval.risk.clone(),
            approval.decision.clone(),
            approval.mode.clone(),
            format_clock(approval.decided_at),
        ]);
    }
    render_table(&rows)
}

/// 极简表格：按列宽左对齐、列间两个空格、行尾不留空格。
fn render_table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            if index + 1 < row.len() {
                line.push_str(&" ".repeat(widths[index] - cell.chars().count()));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// 一行 JSONL → `HH:MM:SS 类型 …`。
///
/// 能解析成 `RuntimeEvent` 的行渲染成事件详情；解析不了的行（`task_start`/`task_end` 这类
/// 运行器自己的行、被手工改坏的行）只展示 `type` 与少量标量字段——任何一行都不能让
/// `task show` 失败。
fn render_event_line(raw: &str) -> String {
    let Ok(mut record) = serde_json::from_str::<Value>(raw) else {
        return format!(
            "--:--:-- (unparsable) {}",
            truncate(raw.trim(), DETAIL_CHARS)
        );
    };
    let clock = timestamp_of(&record);
    // 时间戳属于日志格式、不属于事件本身：先取出来，再按事件形状解析其余字段
    if let Some(object) = record.as_object_mut() {
        object.remove("ts");
    }
    match serde_json::from_value::<RuntimeEvent>(record.clone()) {
        Ok(event) => format!("{clock} {}", event_detail(&event)),
        Err(_) => format!("{clock} {}", fallback_detail(&record)),
    }
}

/// JSONL 行的 `ts` → `HH:MM:SS`；缺失或形状不对时给 `--:--:--`。
fn timestamp_of(record: &Value) -> String {
    record
        .get("ts")
        .and_then(Value::as_str)
        .and_then(clock_of_rfc3339)
        .unwrap_or_else(|| "--:--:--".to_string())
}

/// 取 RFC 3339 里的时钟部分（`2026-10-15T12:00:00.123+00:00` → `12:00:00`）。
fn clock_of_rfc3339(ts: &str) -> Option<String> {
    let clock = ts.get(11..19)?;
    clock
        .chars()
        .all(|c| c.is_ascii_digit() || c == ':')
        .then(|| clock.to_string())
}

/// `RuntimeEvent` 行的详情：类型名 + 关键字段，压成单行。
fn event_detail(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::TextDelta { text } => {
            format!("text_delta {}", truncate(&one_line(text), DETAIL_CHARS))
        }
        RuntimeEvent::ToolExec { name, summary, .. } => format!(
            "tool_exec {name} {}",
            truncate(&one_line(summary), DETAIL_CHARS)
        ),
        RuntimeEvent::ToolResult { name, ok, output } => {
            let mut detail = format!("tool_result {name} ok={ok}");
            if let Some(output) = output {
                detail.push_str(&format!(" {}", truncate(&one_line(output), DETAIL_CHARS)));
            }
            detail
        }
        RuntimeEvent::Usage {
            input_tokens,
            output_tokens,
            estimated,
        } => format!("usage in={input_tokens} out={output_tokens} estimated={estimated}"),
        RuntimeEvent::Done {
            status,
            summary,
            error,
        } => {
            let mut detail = format!("done status={}", status_name(*status));
            if let Some(summary) = summary {
                detail.push_str(&format!(
                    " summary={}",
                    truncate(&one_line(summary), DETAIL_CHARS)
                ));
            }
            if let Some(error) = error {
                detail.push_str(&format!(
                    " error={}",
                    truncate(&one_line(error), DETAIL_CHARS)
                ));
            }
            detail
        }
    }
}

/// 解析不了的行：`type` 字段 + 最多三个标量字段（如 `task_start kind=run kernel=native …`）。
fn fallback_detail(record: &Value) -> String {
    let Some(object) = record.as_object() else {
        return truncate(&record.to_string(), DETAIL_CHARS);
    };
    let name = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    // 按键名排序，输出稳定；null 与嵌套结构不进摘要
    let mut fields: Vec<(&str, &Value)> = object
        .iter()
        .filter(|(key, value)| key.as_str() != "type" && is_scalar(value))
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    fields.sort_by_key(|(key, _)| *key);

    let mut detail = name.to_string();
    for (key, value) in fields.iter().take(FALLBACK_FIELDS) {
        let text = truncate(&scalar_text(value), FALLBACK_VALUE_CHARS);
        detail.push_str(&format!(" {key}={text}"));
    }
    detail
}

/// 标量（字符串/数字/布尔）才进兜底摘要。
fn is_scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

/// 标量的展示文本：字符串不加引号。
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 用量：与结束行同约定，估算值前加 `~`。
fn format_tokens(task: &TaskRecord) -> String {
    let mark = if task.usage_estimated { "~" } else { "" };
    format!(
        "{mark}in={} {mark}out={}",
        task.input_tokens, task.output_tokens
    )
}

/// 耗时：起止都已知时给 `{:.1}s`，否则 `-`（时钟回拨按 0 处理）。
fn format_duration(started_at: i64, ended_at: Option<i64>) -> String {
    match ended_at {
        Some(ended_at) => format!(
            "{:.1}s",
            ended_at.saturating_sub(started_at) as f64 / 1000.0
        ),
        None => "-".to_string(),
    }
}

/// epoch 毫秒 → RFC 3339 短形（UTC，`2026-10-15T12:00:00Z`）。
fn format_time(millis: i64) -> String {
    let (date, clock) = utc_parts(millis);
    format!("{date}T{clock}Z")
}

/// epoch 毫秒 → 时钟部分 `HH:MM:SS`（UTC）。
fn format_clock(millis: i64) -> String {
    utc_parts(millis).1
}

/// epoch 毫秒 → UTC 的 `(YYYY-MM-DD, HH:MM:SS)`。
///
/// CLI 不为一行时间格式引入日期库（日期库只在 core），这里用民用历算法就地换算；时间一律
/// 标 UTC（`Z`），不做时区转换。
fn utc_parts(millis: i64) -> (String, String) {
    let seconds = millis.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let date = format!("{year:04}-{month:02}-{day:02}");
    let clock = format!(
        "{:02}:{:02}:{:02}",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    );
    (date, clock)
}

/// 自 1970-01-01 起的天数 → `(年, 月, 日)`（Howard Hinnant 的 `civil_from_days`）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_pos = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_pos + 2) / 5 + 1) as u32;
    let month = if month_pos < 10 {
        month_pos + 3
    } else {
        month_pos - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
}

/// id 的展示前缀（8 字符；uuid 前 8 字符必然存在）。
fn id_prefix(id: &str) -> String {
    id.chars().take(8).collect()
}

/// 压成单行：换行/制表/连续空白合成一个空格。
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 按**字符**截断并补 `…`（按字节切会切坏 UTF-8）。
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// `RunStatus` 的跨模块契约字符串（与 `tasks.status`、`done.status` 一致）。
fn status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Success => "success",
        RunStatus::Failed => "failed",
        RunStatus::Aborted => "aborted",
        RunStatus::LimitReached => "limit_reached",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_storage::{NewApproval, NewTask, TaskLog};
    use serde_json::json;

    use super::*;

    /// 单个测试独占的临时 home，析构时清理。
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-cli-task-{}-{label}-{}",
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

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const TASK_ID: &str = "019a1b2c-3d4e-7f00-8000-000000000001";
    /// `2026-10-15T12:00:00Z` 的 epoch 毫秒。
    const STARTED_AT: i64 = 1_792_065_600_000;

    /// 造一份真实的任务记录：临时 home 里的 DB 行 + 审批行 + JSONL 日志。
    fn fixture(home: &Path) -> String {
        let id = TASK_ID.to_string();
        let db = Db::open(&home.join("ignis.db")).unwrap();
        db.insert_task(&NewTask {
            id: id.clone(),
            kind: "run".into(),
            prompt: "修复配置路径".into(),
            kernel: "native".into(),
            workspace: "/tmp/project".into(),
            started_at: STARTED_AT,
        })
        .unwrap();
        db.finish_task(
            &id,
            "success",
            Some("已修复"),
            None,
            12,
            34,
            false,
            STARTED_AT + 5_000,
        )
        .unwrap();
        db.insert_approval(&NewApproval {
            task_id: id.clone(),
            tool: "fs_write".into(),
            summary: "Writing src/config.js".into(),
            risk: "elevated".into(),
            decision: "allow".into(),
            mode: "interactive".into(),
            decided_at: STARTED_AT + 2_000,
        })
        .unwrap();

        let log = TaskLog::new(&home.join("logs")).unwrap();
        let ts = |second: u32| format!("2026-10-15T12:00:{second:02}+00:00");
        log.append(
            &id,
            &json!({"type":"task_start","task_id":id,"kind":"run","kernel":"native","ts":ts(0)}),
        )
        .unwrap();
        log.append(
            &id,
            &json!({"type":"text_delta","text":"已读取","ts":ts(1)}),
        )
        .unwrap();
        log.append(
            &id,
            &json!({"type":"tool_exec","name":"fs_read","summary":"Reading a.txt","args":{"path":"a.txt"},"ts":ts(2)}),
        )
        .unwrap();
        log.append(
            &id,
            &json!({"type":"task_end","task_id":id,"status":"success","summary":"已修复","error":null,"ts":ts(3)}),
        )
        .unwrap();
        // 手工塞一行坏 JSON：`task show` 必须容忍无法解析的行
        let path = log.path(&id);
        let mut raw = fs::read_to_string(&path).unwrap();
        raw.push_str("{oops\n");
        fs::write(&path, raw).unwrap();
        id
    }

    fn record(id: &str, status: &str, kind: &str, started_at: i64, prompt: &str) -> TaskRecord {
        TaskRecord {
            id: id.into(),
            kind: kind.into(),
            prompt: prompt.into(),
            kernel: "native".into(),
            workspace: "/tmp/project".into(),
            status: status.into(),
            summary: None,
            error: None,
            input_tokens: 0,
            output_tokens: 0,
            usage_estimated: false,
            started_at,
            ended_at: None,
        }
    }

    #[test]
    fn task_show_output_contains_status_and_events() {
        let home = TempHome::new("show");
        let id = fixture(home.path());

        let out = show_at(home.path(), &id).unwrap();

        // 任务头：一字段一行
        assert!(out.contains(&format!("id: {id}\n")), "{out}");
        assert!(out.contains("status: success\n"), "{out}");
        assert!(out.contains("kind: run\n"), "{out}");
        assert!(out.contains("kernel: native\n"), "{out}");
        assert!(out.contains("workspace: /tmp/project\n"), "{out}");
        assert!(out.contains("started: 2026-10-15T12:00:00Z\n"), "{out}");
        assert!(out.contains("ended: 2026-10-15T12:00:05Z\n"), "{out}");
        assert!(out.contains("duration: 5.0s\n"), "{out}");
        assert!(out.contains("tokens: in=12 out=34\n"), "{out}");
        assert!(out.contains("error: -\n"), "{out}");
        assert!(out.contains("summary: 已修复\n"), "{out}");

        // 审批记录表：一行一条，时间取 HH:MM:SS
        assert!(out.contains("approvals:\n"), "{out}");
        assert!(out.contains("fs_write"), "{out}");
        assert!(out.contains("elevated"), "{out}");
        assert!(out.contains("allow"), "{out}");
        assert!(out.contains("interactive"), "{out}");
        assert!(out.contains("12:00:02"), "{out}");

        // 事件流：`RuntimeEvent` 行带类型与详情，运行器自己的行只展示 type 与少量标量字段
        assert!(out.contains("events:\n"), "{out}");
        assert!(
            out.contains("12:00:00 task_start kernel=native kind=run"),
            "{out}"
        );
        assert!(out.contains("12:00:01 text_delta 已读取\n"), "{out}");
        assert!(
            out.contains("12:00:02 tool_exec fs_read Reading a.txt\n"),
            "{out}"
        );
        assert!(out.contains("12:00:03 task_end status=success"), "{out}");
        // 无法解析的行不能让 show 失败，也不能被静默丢掉
        assert!(out.contains("--:--:-- (unparsable) {oops\n"), "{out}");
    }

    #[test]
    fn task_show_reports_missing_task() {
        let home = TempHome::new("missing");

        let error = show_at(home.path(), "nope").unwrap_err().to_string();

        assert_eq!(error, "task not found: nope");
    }

    #[test]
    fn task_list_shows_id_prefix_and_truncates_prompt() {
        let long_prompt = "改".repeat(50);
        let tasks = vec![
            record(TASK_ID, "success", "run", STARTED_AT, &long_prompt),
            record(
                "019a1b2c-3d4e-7f00-8000-000000000002",
                "running",
                "repl",
                STARTED_AT + 10_000,
                "短提示",
            ),
        ];

        let out = render_list(&tasks);

        // 表头 + 两行
        assert_eq!(out.lines().count(), 3, "{out}");
        assert!(out.contains("ID"), "{out}");
        assert!(out.contains("019a1b2c "), "{out}");
        assert!(!out.contains("3d4e-7f00"), "id 只展示前 8 位：{out}");
        assert!(out.contains("success"), "{out}");
        assert!(out.contains("running"), "{out}");
        assert!(out.contains("2026-10-15T12:00:00Z"), "{out}");
        assert!(out.contains(&format!("{}…", "改".repeat(40))), "{out}");
        assert!(!out.contains(&"改".repeat(41)), "{out}");
    }
}
