//! 流式渲染：把内核事件写成终端可读的输出。
//!
//! `out` 可注入（测试传可读回的缓冲区），`color` 由命令层按 `std::io::stdout().is_terminal()`
//! 决定，模块内不做 tty 探测。

use std::io::Write;
use std::time::Duration;

use ignis_runtime::{RunStatus, RuntimeEvent, Usage};

/// ANSI：暗色（工具执行行）。
const DIM: &str = "\x1b[2m";
/// ANSI：红色（工具失败行）。
const RED: &str = "\x1b[31m";
/// ANSI：重置。
const RESET: &str = "\x1b[0m";

/// 工具失败输出最多展示的字符数。
const ERROR_PREVIEW_CHARS: usize = 200;

/// 事件渲染器：文本增量原样输出，工具执行/失败各写一行。
pub struct Renderer {
    /// 渲染目标：命令层给 stdout，测试给可读回的缓冲区。
    pub out: Box<dyn Write + Send>,
    /// 是否输出 ANSI 颜色。
    pub color: bool,
}

impl Renderer {
    /// 渲染一个事件。
    ///
    /// `Usage`、成功的 `ToolResult` 与 `Done` 都不在此渲染：结束行由命令层用
    /// [`format_task_end`] 打印，用量与成功结果对用户无信息量。
    pub fn on_event(&mut self, ev: &RuntimeEvent) {
        match ev {
            RuntimeEvent::TextDelta { text } => {
                let _ = self.out.write_all(text.as_bytes());
            }
            RuntimeEvent::ToolExec { summary, .. } => {
                let line = self.style(&format!("→ {summary}"), DIM);
                let _ = writeln!(self.out, "{line}");
            }
            RuntimeEvent::ToolResult {
                ok: false, output, ..
            } => {
                let preview = error_preview(output.as_deref().unwrap_or_default());
                let line = self.style(&format!("  ✗ {preview}"), RED);
                let _ = writeln!(self.out, "{line}");
            }
            RuntimeEvent::ToolResult { .. }
            | RuntimeEvent::Usage { .. }
            | RuntimeEvent::Done { .. } => {}
        }

        // 流式输出必须逐事件 flush，否则增量会卡在行缓冲里（写失败多为 EPIPE，忽略）。
        let _ = self.out.flush();
    }

    /// 按 `color` 决定是否包一层 ANSI 颜色。
    fn style(&self, text: &str, color: &str) -> String {
        if self.color {
            format!("{color}{text}{RESET}")
        } else {
            text.to_string()
        }
    }
}

/// 失败输出的首行，按**字符**截断到 [`ERROR_PREVIEW_CHARS`]（按字节切会切坏 UTF-8）。
fn error_preview(output: &str) -> String {
    output
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(ERROR_PREVIEW_CHARS)
        .collect()
}

/// 结束行：`● Finished: success · 1.2s · tokens in=1234 out=567`；`estimated` 时 token 数前加 `~`。
pub fn format_task_end(status: RunStatus, elapsed: Duration, usage: &Usage) -> String {
    let status = match status {
        RunStatus::Success => "success",
        RunStatus::Failed => "failed",
        RunStatus::Aborted => "aborted",
        RunStatus::LimitReached => "limit_reached",
    };
    let elapsed = format!("{:.1}s", elapsed.as_secs_f64());
    let mark = if usage.estimated { "~" } else { "" };
    let input = format!("{mark}in={}", usage.input_tokens);
    let output = format!("{mark}out={}", usage.output_tokens);

    format!("● Finished: {status} · {elapsed} · tokens {input} {output}")
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ignis_runtime::{RunStatus, RuntimeEvent, Usage};
    use serde_json::json;

    use super::*;

    /// 写入落到共享缓冲区的测试写入器：`Renderer` 持有 `out`，需要从外部读回内容。
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).expect("UTF-8 渲染输出")
        }
    }

    fn tool_exec(summary: &str) -> RuntimeEvent {
        RuntimeEvent::ToolExec {
            name: "fs_read".to_string(),
            summary: summary.to_string(),
            args: json!({ "path": "a.txt" }),
        }
    }

    #[test]
    fn renderer_prints_tool_lines_and_text() {
        let buf = SharedBuf::default();
        let mut renderer = Renderer {
            out: Box::new(buf.clone()),
            color: false,
        };

        renderer.on_event(&RuntimeEvent::TextDelta {
            text: "已读取".to_string(),
        });
        renderer.on_event(&tool_exec("Reading a.txt"));
        renderer.on_event(&RuntimeEvent::ToolResult {
            name: "fs_read".to_string(),
            ok: true,
            output: Some("文件内容".to_string()),
        });
        renderer.on_event(&RuntimeEvent::ToolResult {
            name: "shell_run".to_string(),
            ok: false,
            output: Some("not ok\n附加的第二行".to_string()),
        });
        renderer.on_event(&RuntimeEvent::Usage {
            input_tokens: 10,
            output_tokens: 5,
            estimated: false,
        });
        renderer.on_event(&RuntimeEvent::Done {
            status: RunStatus::Success,
            summary: Some("done".to_string()),
            error: None,
        });

        let text = buf.text();
        assert!(text.contains("→ Reading a.txt"), "{text}");
        assert!(text.contains("已读取"), "{text}");
        // 文本增量原样输出：不额外补换行
        assert!(text.starts_with("已读取→ Reading a.txt\n"), "{text}");
        // 失败的工具结果只取首行
        assert!(text.contains("  ✗ not ok\n"), "{text}");
        assert!(!text.contains("附加的第二行"), "{text}");
        // 成功的工具结果、用量与 Done 不在此渲染
        assert!(!text.contains("文件内容"), "{text}");
        assert!(!text.contains("● Finished"), "{text}");
        // color=false 时不得出现任何 ANSI 转义
        assert!(!text.contains('\u{1b}'), "{text}");
    }

    #[test]
    fn color_wraps_tool_lines_in_ansi() {
        let buf = SharedBuf::default();
        let mut renderer = Renderer {
            out: Box::new(buf.clone()),
            color: true,
        };

        renderer.on_event(&tool_exec("Reading a.txt"));
        renderer.on_event(&RuntimeEvent::ToolResult {
            name: "shell_run".to_string(),
            ok: false,
            output: Some("boom".to_string()),
        });

        let text = buf.text();
        assert!(
            text.contains("\u{1b}[2m→ Reading a.txt\u{1b}[0m\n"),
            "{text}"
        );
        assert!(text.contains("\u{1b}[31m  ✗ boom\u{1b}[0m\n"), "{text}");
    }

    #[test]
    fn tool_error_preview_takes_first_line_and_truncates_to_200_chars() {
        let long = "你".repeat(300);
        for output in [long.clone(), format!("{long}\n尾巴")] {
            let buf = SharedBuf::default();
            let mut renderer = Renderer {
                out: Box::new(buf.clone()),
                color: false,
            };
            renderer.on_event(&RuntimeEvent::ToolResult {
                name: "shell_run".to_string(),
                ok: false,
                output: Some(output),
            });

            let text = buf.text();
            assert!(text.starts_with("  ✗ 你"), "{text}");
            // `  ✗ ` 四个字符 + 200 个字符
            assert_eq!(text.trim_end().chars().count(), 4 + 200, "{text}");
            assert!(!text.contains("尾巴"), "{text}");
        }
    }

    #[test]
    fn task_end_line_marks_estimated_tokens() {
        let exact = Usage {
            input_tokens: 1234,
            output_tokens: 567,
            estimated: false,
        };
        assert_eq!(
            format_task_end(RunStatus::Success, Duration::from_millis(1234), &exact),
            "● Finished: success · 1.2s · tokens in=1234 out=567"
        );

        let estimated = Usage {
            input_tokens: 1234,
            output_tokens: 567,
            estimated: true,
        };
        let line = format_task_end(RunStatus::Success, Duration::from_millis(1234), &estimated);
        assert!(line.contains("~in=1234"), "{line}");
        assert!(line.contains("~out=567"), "{line}");
        assert!(!line.contains(" in="), "{line}");
        assert!(!line.contains(" out="), "{line}");
    }

    #[test]
    fn task_end_line_uses_status_names() {
        let usage = Usage::default();
        let cases = [
            (RunStatus::Success, "success"),
            (RunStatus::Failed, "failed"),
            (RunStatus::Aborted, "aborted"),
            (RunStatus::LimitReached, "limit_reached"),
        ];

        for (status, name) in cases {
            assert_eq!(
                format_task_end(status, Duration::from_secs(2), &usage),
                format!("● Finished: {name} · 2.0s · tokens in=0 out=0"),
                "{status:?}"
            );
        }
    }
}
