//! 文件工具：读（行区间 + 截断）、列目录、写（自动建父目录）、编辑（唯一匹配）。
//!
//! 四个工具都先经 [`resolve_in_workspace`] 解析路径：越界、不可解析与 IO 错误一律转
//! [`ToolError`] 返回，不 panic。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ignis_runtime::ToolSpec;
use serde_json::{Value, json};

use super::util::{optional_str, required_str, truncate_output};
use super::{Tool, ToolCtx, ToolError};
use crate::security::{SecurityError, resolve_in_workspace};

/// 读取文件内容，可按行区间分页。
pub(super) struct FsRead;

#[async_trait]
impl Tool for FsRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs_read".into(),
            description: "Read a file inside the workspace. Output is plain content; use offset/limit to page through long files.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the workspace root"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-based first line to read (default 1)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read (default: to the end of the file)"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
        let user_path = required_str(args, "path")?;
        let path = resolve_path(ctx, user_path)?;
        let bytes = fs::read(&path).map_err(|err| io_failure("读取", user_path, &path, &err))?;
        // 非 UTF-8 字节按 lossy 解码：二进制文件也给内容，不 panic、不失败
        let text = String::from_utf8_lossy(&bytes);
        let offset = optional_usize(args, "offset")?;
        let limit = optional_usize(args, "limit")?;
        let output_limit = ctx.limits.tool_output_limit;
        if offset.is_none() && limit.is_none() {
            return Ok(truncate_output(&text, output_limit));
        }
        line_range(&text, offset.unwrap_or(1), limit, user_path, output_limit)
    }
}

/// 列出目录条目；目录带 `/` 后缀，按名排序。
pub(super) struct FsList;

#[async_trait]
impl Tool for FsList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs_list".into(),
            description: "List a directory inside the workspace. Directories are marked with a trailing '/'; entries are sorted by name.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path relative to the workspace root (default \".\")"
                    }
                }
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
        let user_path = optional_str(args, "path")?.unwrap_or(".");
        let path = resolve_path(ctx, user_path)?;
        let entries =
            fs::read_dir(&path).map_err(|err| io_failure("列目录", user_path, &path, &err))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| io_failure("列目录", user_path, &path, &err))?;
            let file_type = entry
                .file_type()
                .map_err(|err| io_failure("列目录", user_path, &path, &err))?;
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if file_type.is_dir() {
                name.push('/');
            }
            names.push(name);
        }
        names.sort();
        Ok(truncate_output(
            &names.join("\n"),
            ctx.limits.tool_output_limit,
        ))
    }
}

/// 写入文件；缺失的父目录自动创建（沙箱已保证整条路径在工作区内）。
pub(super) struct FsWrite;

#[async_trait]
impl Tool for FsWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs_write".into(),
            description: "Write a file inside the workspace, creating missing parent directories and overwriting an existing file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the workspace root"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full file content to write"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
        let user_path = required_str(args, "path")?;
        let content = required_str(args, "content")?;
        let path = resolve_path(ctx, user_path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| io_failure("创建父目录", user_path, parent, &err))?;
        }
        fs::write(&path, content).map_err(|err| io_failure("写入", user_path, &path, &err))?;
        Ok(format!("已写入 {} 字节：{user_path}", content.len()))
    }
}

/// 替换文件内容；默认要求 `old_string` 恰好匹配一处。
pub(super) struct FsEdit;

#[async_trait]
impl Tool for FsEdit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs_edit".into(),
            description: "Replace text in a file inside the workspace. By default old_string must occur exactly once; set replace_all=true to replace every occurrence.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the workspace root"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact text to replace (must be non-empty)"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence instead of requiring a unique match (default false)"
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
        let user_path = required_str(args, "path")?;
        let old_string = required_str(args, "old_string")?;
        let new_string = required_str(args, "new_string")?;
        let replace_all = optional_bool(args, "replace_all")?;
        if old_string.is_empty() {
            return Err(ToolError::message(format!(
                "old_string 不能为空：{user_path}"
            )));
        }
        let path = resolve_path(ctx, user_path)?;
        let content =
            fs::read_to_string(&path).map_err(|err| io_failure("读取", user_path, &path, &err))?;
        let matches = content.matches(old_string).count();
        if matches == 0 {
            return Err(ToolError::message(format!(
                "old_string 在 {user_path} 中匹配 0 处，未做修改"
            )));
        }
        if !replace_all && matches > 1 {
            return Err(ToolError::message(format!(
                "old_string 在 {user_path} 中匹配 {matches} 处，需要恰好 1 处（可设 replace_all=true 全部替换）"
            )));
        }
        let updated = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };
        fs::write(&path, updated).map_err(|err| io_failure("写入", user_path, &path, &err))?;
        Ok(format!("已在 {user_path} 中替换 {matches} 处"))
    }
}

/// 解析路径：越界与不可解析都转 [`ToolError`]，并保证消息里带上用户给的路径。
fn resolve_path(ctx: &ToolCtx<'_>, user_path: &str) -> Result<PathBuf, ToolError> {
    resolve_in_workspace(ctx.workspace, user_path).map_err(|err| match &err {
        // 越界文案本身已含用户路径与工作区路径
        SecurityError::OutsideWorkspace { .. } => ToolError::message(err.to_string()),
        other @ (SecurityError::WorkspaceUnavailable { .. }
        | SecurityError::Unresolvable { .. }) => {
            ToolError::message(format!("{user_path}：{other}"))
        }
    })
}

/// 统一 IO 失败文案：动作、用户路径、解析后的路径与底层原因。
fn io_failure(action: &str, user_path: &str, path: &Path, err: &io::Error) -> ToolError {
    ToolError::message(format!(
        "{action}失败：{user_path}（解析为 {}）：{err}",
        path.display()
    ))
}

/// 截取 `[offset, offset + limit)` 行（1-based）；只展示片段时在尾部提示总行数，
/// 便于模型继续翻页。
///
/// 正文先截断、分页提示后追加：提示必须活过 [`truncate_output`]，否则大页会把总行数吞掉。
fn line_range(
    text: &str,
    offset: usize,
    limit: Option<usize>,
    user_path: &str,
    output_limit: usize,
) -> Result<String, ToolError> {
    if offset == 0 {
        return Err(ToolError::message(format!(
            "offset 从 1 开始计数：{user_path}"
        )));
    }
    if limit == Some(0) {
        return Err(ToolError::message(format!("limit 必须大于 0：{user_path}")));
    }
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if offset > total {
        return Err(ToolError::message(format!(
            "offset {offset} 超出文件总行数 {total}：{user_path}"
        )));
    }
    // 模型可给出任意整数（如 limit = u64::MAX）：饱和加法避免 offset - 1 + limit 溢出
    let end = limit.map_or(total, |limit| (offset - 1).saturating_add(limit).min(total));
    let body = lines[offset - 1..end].join("\n");
    if offset == 1 && end == total {
        // 整页：没有分页提示，正常截断
        return Ok(truncate_output(&body, output_limit));
    }
    let footer = format!("\n\n[显示第 {offset}-{end} 行，共 {total} 行]");
    // 给提示预留配额，保证正文被截断时提示仍然出现
    let budget = output_limit.saturating_sub(footer.len());
    Ok(format!("{}{footer}", truncate_output(&body, budget)))
}

fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|number| usize::try_from(number).ok())
            .map(Some)
            .ok_or_else(|| ToolError::message(format!("参数 `{key}` 必须是非负整数"))),
    }
}

fn optional_bool(args: &Value, key: &str) -> Result<bool, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| ToolError::message(format!("参数 `{key}` 必须是布尔值"))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_runtime::Limits;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::tools::{Tool, ToolCtx, ToolError};

    /// 单个测试独占的临时目录，析构时清理（本 crate 不引入 tempfile 依赖）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-core-tools-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 临时工作区 + 默认上限；`limits` 与工作区同生命周期，供 `ToolCtx` 借用。
    struct Env {
        ws: TempDir,
        limits: Limits,
    }

    impl Env {
        fn new() -> Self {
            Self {
                ws: TempDir::new(),
                limits: Limits::default(),
            }
        }

        fn path(&self) -> &Path {
            self.ws.path()
        }

        fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx {
                workspace: self.ws.path(),
                cancel: CancellationToken::new(),
                limits: &self.limits,
                task_id: None,
                job_id: None,
            }
        }

        async fn run(&self, tool: &dyn Tool, args: Value) -> Result<String, ToolError> {
            tool.run(&args, &self.ctx()).await
        }
    }

    #[tokio::test]
    async fn fs_read_returns_content_and_truncates() {
        let env = Env::new();
        std::fs::write(env.path().join("a.txt"), "hello\nworld\n").unwrap();
        let output = env.run(&FsRead, json!({"path": "a.txt"})).await.unwrap();
        assert_eq!(output, "hello\nworld\n");

        // 100KB 文件超过 tool_output_limit：截断到上限内并以尾部标记收尾
        std::fs::write(env.path().join("big.txt"), "x".repeat(100 * 1024)).unwrap();
        let output = env.run(&FsRead, json!({"path": "big.txt"})).await.unwrap();
        let limit = env.limits.tool_output_limit;
        let marker = format!("\n… [truncated: output exceeded {limit} bytes]");
        assert!(
            output.ends_with(&marker),
            "输出未以截断标记结尾，尾部：{}",
            &output[output.len().saturating_sub(80)..]
        );
        assert!(
            output.len() <= limit + marker.len(),
            "截断后长度 {} 超过上限 {} + 标记长度",
            output.len(),
            limit
        );

        // 非 UTF-8 字节按 lossy 解码：给出内容而不是 panic
        std::fs::write(env.path().join("bin.dat"), [0xff, 0xfe, b'a']).unwrap();
        let output = env.run(&FsRead, json!({"path": "bin.dat"})).await.unwrap();
        assert!(
            output.contains('a') && output.contains('\u{fffd}'),
            "{output}"
        );
    }

    #[tokio::test]
    async fn fs_write_creates_parent_dirs() {
        let env = Env::new();
        let output = env
            .run(&FsWrite, json!({"path": "a/b/c.txt", "content": "hi"}))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(env.path().join("a/b/c.txt")).unwrap(),
            "hi"
        );
        assert!(output.contains("a/b/c.txt"), "{output}");
        // 工具结果文案为中文（spec description 才用英文）
        assert!(output.starts_with("已写入 2 字节"), "{output}");

        // 覆盖已存在的文件
        env.run(&FsWrite, json!({"path": "a/b/c.txt", "content": "bye"}))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(env.path().join("a/b/c.txt")).unwrap(),
            "bye"
        );
    }

    #[tokio::test]
    async fn fs_edit_requires_unique_match() {
        let env = Env::new();
        let file = env.path().join("a.txt");
        std::fs::write(&file, "foo bar foo\n").unwrap();

        // 两处匹配且未开 replace_all → 报错并说明匹配数，文件不动
        let err = env
            .run(
                &FsEdit,
                json!({"path": "a.txt", "old_string": "foo", "new_string": "baz"}),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains('2'),
            "错误信息应说明匹配 2 处：{err}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "foo bar foo\n");

        // replace_all=true → 两处都替换
        let output = env
            .run(
                &FsEdit,
                json!({"path": "a.txt", "old_string": "foo", "new_string": "baz", "replace_all": true}),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "baz bar baz\n");
        assert!(output.contains("a.txt"), "{output}");
        assert!(output.contains("替换 2 处"), "{output}");

        // 0 处匹配同样报错并保持文件原样
        let err = env
            .run(
                &FsEdit,
                json!({"path": "a.txt", "old_string": "nope", "new_string": "x"}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains('0'), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "baz bar baz\n");
    }

    #[tokio::test]
    async fn fs_tools_reject_workspace_escape() {
        let env = Env::new();
        // 工作区外的同名候选：词法上位于工作区父目录
        let escape = format!("../ignis-outside-{}.txt", std::process::id());
        let outside = env.path().join(&escape);
        let cases: [(&dyn Tool, Value); 4] = [
            (&FsRead, json!({"path": escape.clone()})),
            (&FsList, json!({"path": escape.clone()})),
            (&FsWrite, json!({"path": escape.clone(), "content": "x"})),
            (
                &FsEdit,
                json!({"path": escape.clone(), "old_string": "x", "new_string": "y"}),
            ),
        ];
        for (tool, args) in cases {
            let err = env.run(tool, args).await.unwrap_err();
            assert!(
                err.to_string().contains(&escape),
                "错误信息应含越界路径：{err}"
            );
        }
        assert!(
            !outside.exists(),
            "工作区外不应产生文件：{}",
            outside.display()
        );
    }

    #[tokio::test]
    async fn fs_list_marks_dirs_and_sorts() {
        let env = Env::new();
        std::fs::create_dir(env.path().join("src")).unwrap();
        std::fs::write(env.path().join("a.txt"), "x").unwrap();
        std::fs::write(env.path().join("src/lib.rs"), "x").unwrap();

        // 默认路径为 "."，按名排序，目录带 "/" 后缀
        let output = env.run(&FsList, json!({})).await.unwrap();
        assert_eq!(output, "a.txt\nsrc/");

        let output = env.run(&FsList, json!({"path": "src"})).await.unwrap();
        assert_eq!(output, "lib.rs");
    }

    #[tokio::test]
    async fn registered_tools_execute_through_lookup() {
        let env = Env::new();
        // 注册表是 Task 9 消费的入口：从 lookup 拿到的工具必须能真实执行
        let registry = crate::tools::default_registry();
        env.run(
            registry.get("fs_write").unwrap(),
            json!({"path": "x.txt", "content": "hi"}),
        )
        .await
        .unwrap();
        let output = env
            .run(registry.get("fs_read").unwrap(), json!({"path": "x.txt"}))
            .await
            .unwrap();
        assert_eq!(output, "hi");
    }

    #[tokio::test]
    async fn fs_read_slices_line_ranges_and_reports_total() {
        let env = Env::new();
        std::fs::write(env.path().join("a.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();

        let output = env
            .run(&FsRead, json!({"path": "a.txt", "offset": 2, "limit": 2}))
            .await
            .unwrap();
        assert_eq!(output, "l2\nl3\n\n[显示第 2-3 行，共 5 行]");

        // 只有 offset、读到末尾 → 尾部仍有总行数提示
        let output = env
            .run(&FsRead, json!({"path": "a.txt", "offset": 4}))
            .await
            .unwrap();
        assert_eq!(output, "l4\nl5\n\n[显示第 4-5 行，共 5 行]");

        // offset 越界 → 错误信息里给出总行数，便于模型改参数
        let err = env
            .run(&FsRead, json!({"path": "a.txt", "offset": 9}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains('5'), "{err}");
    }

    #[tokio::test]
    async fn fs_read_clamps_extreme_offset_and_limit() {
        let env = Env::new();
        std::fs::write(env.path().join("a.txt"), "l1\nl2\nl3\n").unwrap();

        // limit = u64::MAX（模型可给的任意整数）不得触发算术溢出，语义等同「读到末尾」
        let output = env
            .run(
                &FsRead,
                json!({"path": "a.txt", "offset": 2, "limit": u64::MAX}),
            )
            .await
            .unwrap();
        assert_eq!(output, "l2\nl3\n\n[显示第 2-3 行，共 3 行]");

        // 同样极端的 offset：越界只报错、不 panic
        let err = env
            .run(
                &FsRead,
                json!({"path": "a.txt", "offset": u64::MAX, "limit": 1}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains('3'), "{err}");
    }

    #[tokio::test]
    async fn fs_read_keeps_the_total_line_footer_on_a_truncated_page() {
        let env = Env::new();
        // 60 行 × 4KB ≈ 240KB；请求一页 40 行 ≈ 160KB，远超 tool_output_limit
        let line = "x".repeat(4096);
        let content: String = (0..60).map(|_| format!("{line}\n")).collect();
        std::fs::write(env.path().join("big.txt"), content).unwrap();

        let output = env
            .run(
                &FsRead,
                json!({"path": "big.txt", "offset": 1, "limit": 40}),
            )
            .await
            .unwrap();

        // 正文截断到「上限 - 提示长度」，截断标记之后仍带着总行数提示收尾
        let footer = "\n\n[显示第 1-40 行，共 60 行]";
        let budget = env.limits.tool_output_limit - footer.len();
        let marker = format!("\n… [truncated: output exceeded {budget} bytes]");
        assert!(
            output.ends_with(&format!("{marker}{footer}")),
            "分页提示被截断吞掉，尾部：{}",
            &output[output.len().saturating_sub(120)..]
        );
        assert_eq!(output.len(), budget + marker.len() + footer.len());
    }

    #[tokio::test]
    async fn fs_tools_report_missing_arguments() {
        let env = Env::new();
        let err = env.run(&FsRead, json!({})).await.unwrap_err();
        assert!(err.to_string().contains("path"), "{err}");
        let err = env
            .run(&FsWrite, json!({"path": "a.txt"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("content"), "{err}");
        let err = env
            .run(&FsEdit, json!({"path": "a.txt"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("old_string"), "{err}");
    }

    #[tokio::test]
    async fn fs_edit_rejects_empty_old_string() {
        let env = Env::new();
        std::fs::write(env.path().join("a.txt"), "foo\n").unwrap();
        let err = env
            .run(
                &FsEdit,
                json!({"path": "a.txt", "old_string": "", "new_string": "x"}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("old_string"), "{err}");
        assert_eq!(
            std::fs::read_to_string(env.path().join("a.txt")).unwrap(),
            "foo\n"
        );
    }
}
