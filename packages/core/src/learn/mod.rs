//! 学习（learn）：从一条已完成的任务生成一份 SKILL.md **草稿**。
//!
//! 一次 `/learn` 的流水线：任务行 + 任务 JSONL → 转录（[`render_transcript`]，原文由
//! [`read_task_log`] 读出）→ 一次**无工具**的 native 请求（与 `/compact` 共用
//! `crate::session::run_tool_less`）→ 校验过的 [`SkillDraft`]（[`parse_draft`]）→ 用户确认后才由
//! 命令层调用 [`save_skill`] 落盘。三条边界：
//!
//! - **只生成草稿**：模型产出先在内存里校验（frontmatter、名字形态、正文非空），不合格的草稿一个
//!   字节都不写；
//! - **经用户确认才落盘**：[`save_skill`] 由命令层在用户按 `s` 之后调用，本模块不自己落盘、也不改
//!   任何别的状态；
//! - **永不自动部署或执行**：落盘的只有 `{skills_dir}/{name}/SKILL.md` 一个文件，生成的正文永不
//!   执行、也不注入任何提示（设计 §2.2 的非目标：技能自我改进闭环留 v0.3）。
//!
//! 两条只读 / 失败边界：转录只读任务行与任务 JSONL（坏行跳过，绝不 panic）；生成失败、草稿非法或
//! 同名冲突都在写第一个字节之前返回，磁盘上不留痕迹。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignis_runtime::{Runtime, RuntimeEvent};
use ignis_storage::TaskRecord;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::session::{ToolLessError, run_tool_less};
use crate::skills::{SkillCatalog, frontmatter};

/// 草稿请求的 system 提示（英文，逐字钉住）。
pub const LEARN_SYSTEM_PROMPT: &str = "You write one reusable skill for an autonomous coding agent. \
You are given the transcript of a completed task. Reply with a SKILL.md draft: YAML frontmatter \
with `name` (lowercase letters, digits and hyphens only, at most 60 characters) and a one-line \
`description`, followed by a Markdown body with the concrete steps, commands and checks that made \
the task succeed. Reply with the SKILL.md draft only — no code fences, no commentary.";

/// 技能名的字符数上限（`^[a-z0-9][a-z0-9-]{0,59}$` 的上界）。
pub const SKILL_NAME_MAX_CHARS: usize = 60;
/// 转录里工具清单最多列的行数（超出以一行 `- … [truncated]` 收尾）。
pub const TRANSCRIPT_TOOL_LINES: usize = 60;

/// 任务日志目录名：与写入方 [`TaskLog::new`] 同源（`{home}/logs`）。
const LOGS_DIR: &str = "logs";
/// 技能目录里约定的技能文件名（与 `skills::catalog` 的取值一致）。
const SKILL_FILE: &str = "SKILL.md";
/// 转录里工具清单被截断时的收尾行。
const TOOL_TRUNCATED: &str = "- … [truncated]";
/// 转录里一条工具调用都没有时的占位行。
const TOOL_NONE: &str = "- (none)";

/// 草稿校验的失败理由（喂给 [`LearnError::Draft`]），逐字钉住。
const FRONTMATTER_INVALID: &str = "missing or invalid frontmatter";
const DESCRIPTION_EMPTY: &str = "description is empty";
const BODY_EMPTY: &str = "body is empty";
/// 模型成功但正文全空白：没有可解析的草稿。
const DRAFT_EMPTY: &str = "empty draft";

/// 通过校验的一份 SKILL.md 草稿。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDraft {
    pub name: String,
    pub description: String,
    pub body: String,
}

impl SkillDraft {
    /// 落盘文本：`---\nname: {name}\ndescription: {description}\n---\n{body 去尾空白}\n`。
    ///
    /// 再过一遍 [`frontmatter::parse`] 必须还原出同一组 `(name, description, body.trim_end())`
    /// （见本模块的往返用例）。
    pub fn to_skill_md(&self) -> String {
        format!(
            "---\nname: {}\ndescription: {}\n---\n{}\n",
            self.name,
            self.description,
            self.body.trim_end()
        )
    }
}

/// `/learn` 的失败形态；展示文案与 `session::SessionError` 同一先例（英文、带模块前缀）。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LearnError {
    /// 草稿请求失败：`{0}` 是内核交回的人话原因。
    #[error("learn: generation failed: {0}")]
    Generate(String),
    /// 模型正文不是一份合法草稿：`{0}` 是逐字的失败理由。
    #[error("learn: invalid draft ({0})")]
    Draft(String),
    /// 读写失败（日志读取、技能落盘）：`{0}` 带出问题的路径。
    #[error("learn: {0}")]
    Io(String),
    /// 目标技能已存在（文件在，或目录里已有同名技能，含内置）。
    #[error("skill {0} already exists")]
    Exists(String),
}

/// 模型正文 → 草稿：先按需剥一层整段代码围栏（首行以 ``` 开头且末行是 ```），再走
/// [`frontmatter::parse`]（既有手写解析器，不引 serde_yaml），然后校验名字形态与正文非空。
///
/// 失败理由逐字（喂给 [`LearnError::Draft`]）：`missing or invalid frontmatter` /
/// `invalid skill name: {name}` / `description is empty` / `body is empty`。
pub fn parse_draft(text: &str) -> Result<SkillDraft, LearnError> {
    let Some((name, description, body)) = frontmatter::parse(strip_code_fence(text)) else {
        return Err(LearnError::Draft(FRONTMATTER_INVALID.to_owned()));
    };
    if !valid_skill_name(&name) {
        return Err(LearnError::Draft(format!("invalid skill name: {name}")));
    }
    // 解析器已把空描述按缺失处理（那种情况走 FRONTMATTER_INVALID），这里是契约兜底
    if description.trim().is_empty() {
        return Err(LearnError::Draft(DESCRIPTION_EMPTY.to_owned()));
    }
    if body.trim().is_empty() {
        return Err(LearnError::Draft(BODY_EMPTY.to_owned()));
    }
    Ok(SkillDraft {
        name,
        description,
        body,
    })
}

/// `{home}/logs/{task_id}.jsonl` 的原文（路径与写入方 `TaskLog` 同源：`{home}/logs/{task_id}.jsonl`）。
///
/// 文件缺失给空串（任务行还在、日志被删：转录仍可生成，只是没有工具清单），其它读取失败给
/// [`LearnError::Io`]（`reading {path} failed: {error}`）。**只读**：`logs` 目录不存在时不会被
/// 建出来（写入方按需创建，读路径不产生副作用）。
pub fn read_task_log(home: &Path, task_id: &str) -> Result<String, LearnError> {
    let path = task_log_path(home, task_id);
    match fs::read_to_string(&path) {
        Ok(raw) => Ok(raw),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(LearnError::Io(format!(
            "reading {} failed: {error}",
            path.display()
        ))),
    }
}

/// 任务行 + JSONL → 送给模型的 user 正文：`Task:` / `Prompt:` / `Answer:` / `Tools:` 四段，`\n`
/// 连接、末尾不留空行。
///
/// 工具清单每条 `- {name}: {summary}`，按 JSONL 出现顺序、最多 [`TRANSCRIPT_TOOL_LINES`] 行；超出
/// 上限时以一行 `- … [truncated]` 收尾，一条都没有时是 `- (none)`。答复缺失（失败/中止任务）写
/// `-`；解析不了的行跳过，不 panic。
pub fn render_transcript(task: &TaskRecord, log: &str) -> String {
    let mut lines = vec![
        format!("Task: {} ({})", task.id, task.kind),
        format!("Prompt: {}", task.prompt),
        format!("Answer: {}", task.summary.as_deref().unwrap_or("-")),
        "Tools:".to_owned(),
    ];
    let mut listed = 0usize;
    let mut truncated = false;
    for raw in log.lines() {
        let Some((name, summary)) = tool_exec_of(raw) else {
            continue; // 坏行与别的事件（`task show` 同口径）：跳过这一行
        };
        if listed == TRANSCRIPT_TOOL_LINES {
            truncated = true;
            break;
        }
        lines.push(format!("- {name}: {summary}"));
        listed += 1;
    }
    if truncated {
        lines.push(TOOL_TRUNCATED.to_owned());
    } else if listed == 0 {
        lines.push(TOOL_NONE.to_owned());
    }
    lines.join("\n")
}

/// 一次无工具的 native 请求：system = [`LEARN_SYSTEM_PROMPT`]、user = `transcript`，正文经
/// [`parse_draft`] 校验成草稿。
///
/// 内核没以成功终态收束（`session::ToolLessError::Failed`）→ [`LearnError::Generate`]（内核给的
/// 原因原样带出）；内核成功但正文全空白（`Empty`）→ [`LearnError::Draft`]（`empty draft`：没有
/// 可解析的正文）。任何失败都不改磁盘。
pub async fn draft_skill(
    runtime: Arc<dyn Runtime>,
    config: &Config,
    transcript: &str,
    cancel: CancellationToken,
) -> Result<SkillDraft, LearnError> {
    match run_tool_less(runtime, config, LEARN_SYSTEM_PROMPT, transcript, cancel).await {
        Ok(text) => parse_draft(&text),
        Err(ToolLessError::Failed(reason)) => Err(LearnError::Generate(reason)),
        Err(ToolLessError::Empty) => Err(LearnError::Draft(DRAFT_EMPTY.to_owned())),
    }
}

/// 落盘 `{skills_dir}/{name}/SKILL.md`（目录按需创建；文件 `create_new` + 0600 + `sync_all`）。
///
/// 目标文件已存在，或 `catalog` 里已有同名技能（含 builtin）→ [`LearnError::Exists`]，一个字节都
/// 不写。草稿本身仍要过一遍名字形态（`SkillDraft` 的字段是公开的，落盘面不信任任何来源）。
pub fn save_skill(
    skills_dir: &Path,
    draft: &SkillDraft,
    catalog: &SkillCatalog,
) -> Result<PathBuf, LearnError> {
    // 落盘面不信任任何来源：`SkillDraft` 的字段是公开的，名字形态在这里再查一遍
    if !valid_skill_name(&draft.name) {
        return Err(LearnError::Draft(format!(
            "invalid skill name: {}",
            draft.name
        )));
    }
    // 查重先于任何落盘动作：目录里已有同名技能（含内置）时连目录都不建
    if catalog.get(&draft.name).is_some() {
        return Err(LearnError::Exists(draft.name.clone()));
    }

    let dir = skills_dir.join(&draft.name);
    let path = dir.join(SKILL_FILE);
    fs::create_dir_all(&dir)
        .map_err(|error| LearnError::Io(format!("creating {} failed: {error}", dir.display())))?;

    // 与 `config.rs::write_private` 同一手法：mode 只在新建时生效，umask 无法放宽它
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        // 目标文件（或同名目录）已经在了：拒绝，不截断、不改一个字节
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(LearnError::Exists(draft.name.clone()));
        }
        Err(error) => {
            return Err(LearnError::Io(format!(
                "writing {} failed: {error}",
                path.display()
            )));
        }
    };
    let text = draft.to_skill_md();
    file.write_all(text.as_bytes())
        .map_err(|error| LearnError::Io(format!("writing {} failed: {error}", path.display())))?;
    file.sync_all()
        .map_err(|error| LearnError::Io(format!("writing {} failed: {error}", path.display())))?;
    Ok(path)
}

/// 名字形态：`^[a-z0-9][a-z0-9-]{0,59}$`——首字符是字母/数字，其余只含小写字母、数字与连字符，
/// 长度（字符数，合法字符全在 ASCII 内，等价于字节数）不超过 [`SKILL_NAME_MAX_CHARS`]。
fn valid_skill_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    let mut chars_seen = 1usize;
    for ch in chars {
        if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '-' {
            return false;
        }
        chars_seen += 1;
    }
    chars_seen <= SKILL_NAME_MAX_CHARS
}

/// 剥掉模型偶尔多包的一层整段代码围栏：首行以 ``` 开头、末行是 ``` 且中间有内容时取中间的部分；
/// 其余形状（首行不是围栏、末行不是围栏、只有一行）原样返回，交给 frontmatter 解析去判定。
fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(first_end) = trimmed.find('\n') else {
        return text;
    };
    let Some(last_start) = trimmed.rfind('\n') else {
        return text;
    };
    let first = &trimmed[..first_end];
    let last = &trimmed[last_start + 1..];
    // 首尾都是围栏，且中间确实夹着内容（`last_start > first_end` 保证切片区间合法）
    if first.starts_with("```") && last.trim() == "```" && last_start > first_end {
        // 连同末行之前的那个换行一起取：围栏之间那最后一行的行尾属于内容，不属于闭合围栏
        &trimmed[first_end + 1..=last_start]
    } else {
        text
    }
}

/// 一行 JSONL → 一条 `tool_exec` 的 `(name, summary)`；坏行与别的事件都给 `None`。
///
/// 口径照 `ignis task show`：先按 [`Value`] 解析、去掉行级的 `ts`、再按 [`RuntimeEvent`] 的形状
/// 解析；任何一步失败都跳过这一行（生产路径不 panic）。
fn tool_exec_of(raw: &str) -> Option<(String, String)> {
    let mut record: Value = serde_json::from_str(raw).ok()?;
    // 时间戳属于日志格式、不属于事件本身
    if let Some(object) = record.as_object_mut() {
        object.remove("ts");
    }
    match serde_json::from_value::<RuntimeEvent>(record).ok()? {
        RuntimeEvent::ToolExec { name, summary, .. } => Some((name, summary)),
        _ => None,
    }
}

/// `{home}/logs/{task_id}.jsonl` 的路径（目录名 [`LOGS_DIR`]、文件名 `{task_id}.jsonl` 与写入方
/// `TaskLog` 同源）。
///
/// **直拼、不碰磁盘**：读路径不该有副作用——经 `TaskLog::new` 取路径会顺手在 home 下建出 `logs`
/// 目录（日志被删、从没写过日志时白留一个空目录）。按需创建是写入路径那一侧的契约。
fn task_log_path(home: &Path, task_id: &str) -> PathBuf {
    home.join(LOGS_DIR).join(format!("{task_id}.jsonl"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_runtime::native::NativeRuntime;
    use ignis_runtime::openai::OpenAiClient;
    use ignis_runtime::test_support::{ScriptStep, ScriptedModel, ScriptedModelHandle};
    use ignis_runtime::{ModelConfig, Runtime};
    use ignis_storage::TaskRecord;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::{
        LEARN_SYSTEM_PROMPT, LearnError, SKILL_NAME_MAX_CHARS, SkillDraft, TRANSCRIPT_TOOL_LINES,
        draft_skill, parse_draft, read_task_log, render_transcript, save_skill,
    };
    use crate::config::Config;
    use crate::skills::{SkillCatalog, frontmatter};

    /// 唯一临时目录：测试并发跑，目录名带进程号与进程内自增序号（与 session/skills 同一手法）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("ignis-learn-{name}-{}-{seq}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 一份 SKILL.md 文本（正文原样拼在闭合围栏之后）。
    fn skill_md(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}")
    }

    /// 一条已完成任务的任务行（测试只关心 id / kind / prompt / summary）。
    fn task_record(id: &str, kind: &str, summary: Option<&str>) -> TaskRecord {
        TaskRecord {
            id: id.into(),
            kind: kind.into(),
            prompt: "修复构建失败".into(),
            kernel: "native".into(),
            workspace: "/w".into(),
            status: "success".into(),
            summary: summary.map(str::to_owned),
            error: None,
            input_tokens: 0,
            output_tokens: 0,
            usage_estimated: false,
            started_at: 1,
            ended_at: Some(2),
            session_id: None,
            skills_used: None,
        }
    }

    /// 指向脚本化假模型的 native 配置（草稿请求真的走一遍 HTTP → SSE，不碰网络）。
    fn scripted_config(model: &ScriptedModelHandle) -> Config {
        Config {
            api_key: Some("test-key".into()),
            model: ModelConfig {
                base_url: model.base_url(),
                model: "test-model".into(),
                ..ModelConfig::default()
            },
            ..Config::default()
        }
    }

    /// 与 `commands::kernel::mk_runtime("native", …)` 同形的运行器。
    fn native_runtime(config: &Config) -> Arc<dyn Runtime> {
        Arc::new(NativeRuntime::new(
            OpenAiClient::new(&config.model, "test-key".into()).unwrap(),
        ))
    }

    /// 一段含 `count` 条 `tool_exec` 的 JSONL（`t{index}` / `step {index}`）。
    fn tool_log(count: usize) -> String {
        let mut log = String::new();
        for index in 0..count {
            log.push_str(&format!(
                "{{\"type\":\"tool_exec\",\"name\":\"t{index}\",\"summary\":\"step {index}\",\"args\":{{}}}}\n"
            ));
        }
        log
    }

    /// 草稿校验矩阵：合法草稿三字段逐字；畸形 frontmatter / 非法名字 / 空正文各自的理由逐字。
    #[test]
    fn parse_draft_accepts_a_minimal_draft_and_rejects_every_violation() {
        let parsed = parse_draft(&skill_md(
            "demo-skill",
            "A demo skill.",
            "# 步骤\n1. 做事\n",
        ))
        .expect("合法草稿应当通过");
        assert_eq!(
            parsed,
            SkillDraft {
                name: "demo-skill".into(),
                description: "A demo skill.".into(),
                body: "# 步骤\n1. 做事\n".into(),
            }
        );

        // 名字上界是 60 个字符（闭区间）：正好 60 合法
        assert_eq!(SKILL_NAME_MAX_CHARS, 60);
        let at_limit = "a".repeat(SKILL_NAME_MAX_CHARS);
        assert!(
            parse_draft(&skill_md(&at_limit, "描述", "# 正文\n")).is_ok(),
            "{at_limit}"
        );

        // 无 frontmatter / 缺闭合围栏 / 空名字（解析器按缺失处理）：都报同一句
        for broken in [
            "name: demo\ndescription: 描述\n---\n正文\n",
            "---\nname: demo\ndescription: 描述\n正文\n",
            "---\nname: \"\"\ndescription: 描述\n---\n正文\n",
            "---\nname: demo\n---\n正文\n",
        ] {
            let error = parse_draft(broken).expect_err("畸形 frontmatter 必须被拒");
            assert_eq!(
                error,
                LearnError::Draft("missing or invalid frontmatter".into()),
                "{broken}"
            );
        }

        // 名字形态：大写/下划线、非 ASCII、首字符是连字符、超长都不合法
        for bad in [
            "Bad_Name".to_string(),
            "中文名".to_string(),
            "-lead".to_string(),
            "a".repeat(SKILL_NAME_MAX_CHARS + 1),
        ] {
            let error =
                parse_draft(&skill_md(&bad, "描述", "# 正文\n")).expect_err("非法名字必须被拒");
            assert_eq!(
                error,
                LearnError::Draft(format!("invalid skill name: {bad}")),
                "{bad}"
            );
        }

        // 正文只有空白（空格 / 换行 / 制表）不算正文
        let error = parse_draft(&skill_md("demo", "描述", "  \n\t\n")).expect_err("空正文必须被拒");
        assert_eq!(error, LearnError::Draft("body is empty".into()));

        // 展示文案：`learn: invalid draft ({理由})`
        assert_eq!(
            LearnError::Draft("body is empty".into()).to_string(),
            "learn: invalid draft (body is empty)"
        );
    }

    /// 模型偶尔多包一层整段代码围栏：首行以 ``` 开头且末行是 ``` 时剥掉；不闭合按原样解析。
    #[test]
    fn parse_draft_strips_one_code_fence() {
        let fenced = format!(
            "```markdown\n{}```\n",
            skill_md("demo-skill", "A fenced skill.", "# 步骤\n")
        );
        let draft = parse_draft(&fenced).expect("整段代码围栏应当被剥掉");
        assert_eq!(
            draft,
            SkillDraft {
                name: "demo-skill".into(),
                description: "A fenced skill.".into(),
                body: "# 步骤\n".into(),
            }
        );

        // 不闭合的围栏按原样解析：首行是围栏而不是 `---`，frontmatter 失败
        let unclosed = format!(
            "```markdown\n{}",
            skill_md("demo-skill", "A fenced skill.", "# 步骤\n")
        );
        let error = parse_draft(&unclosed).expect_err("不闭合的围栏不算合法草稿");
        assert_eq!(
            error,
            LearnError::Draft("missing or invalid frontmatter".into())
        );

        // 退化形状（只有一行围栏、围栏之间没有内容）也不 panic
        for degenerate in ["```", "```\n```", "```\n```\n"] {
            assert!(
                matches!(parse_draft(degenerate), Err(LearnError::Draft(_))),
                "{degenerate}"
            );
        }
    }

    /// `to_skill_md` 的文本再过一遍同一份 frontmatter 解析器必须还原出同一组三字段。
    #[test]
    fn to_skill_md_round_trips_through_the_frontmatter_parser() {
        let draft = SkillDraft {
            name: "round-trip".into(),
            description: "落盘文本必须能被同一份解析器读回".into(),
            // 尾部空白在落盘时去掉（解析回来的正文因此只差 `to_skill_md` 补的那一个换行）
            body: "# 步骤\n\n1. 做事  \n\n".into(),
        };

        let text = draft.to_skill_md();

        assert_eq!(
            text,
            "---\nname: round-trip\ndescription: 落盘文本必须能被同一份解析器读回\n---\n# 步骤\n\n1. 做事\n"
        );
        let (name, description, body) =
            frontmatter::parse(&text).expect("落盘文本必须能被同一份解析器读回");
        assert_eq!(name, draft.name);
        assert_eq!(description, draft.description);
        assert_eq!(body.trim_end(), draft.body.trim_end());

        // 落盘文本正好通过草稿校验，再次渲染逐字相同（幂等）
        let again = parse_draft(&text).expect("落盘文本应当正好通过校验");
        assert_eq!(again.name, draft.name);
        assert_eq!(again.description, draft.description);
        assert_eq!(again.body.trim_end(), draft.body.trim_end());
        assert_eq!(again.to_skill_md(), text);
    }

    /// 转录的四段逐字形状：任务行 → 提示词 → 答复 → 工具清单（按 JSONL 出现顺序，坏行跳过）。
    #[test]
    fn render_transcript_lists_prompt_answer_and_tool_calls() {
        let task = task_record(
            "019a1b2c-3d4e-7f00-8000-000000000001",
            "run",
            Some("已修复"),
        );
        let log = concat!(
            "{\"type\":\"task_start\",\"task_id\":\"01\"}\n",
            "{\"type\":\"tool_exec\",\"name\":\"fs_read\",\"summary\":\"Reading Cargo.toml\",",
            "\"args\":{\"path\":\"Cargo.toml\"},\"call_id\":\"call_1\",\"ts\":\"2026-10-16T10:00:00Z\"}\n",
            "这不是 JSON\n",
            "{\"type\":\"text_delta\",\"text\":\"好\"}\n",
            "{\"type\":\"tool_exec\",\"name\":\"shell_run\",\"summary\":\"Running: cargo test\",",
            "\"args\":{\"command\":\"cargo test\"},\"call_id\":\"call_2\"}\n",
        );

        let transcript = render_transcript(&task, log);

        assert_eq!(
            transcript,
            "Task: 019a1b2c-3d4e-7f00-8000-000000000001 (run)\n\
             Prompt: 修复构建失败\n\
             Answer: 已修复\n\
             Tools:\n\
             - fs_read: Reading Cargo.toml\n\
             - shell_run: Running: cargo test"
        );
        // 坏行与 `text_delta` 不出现；末尾不留空行
        assert!(!transcript.contains("text_delta"), "{transcript}");
        assert!(!transcript.contains('好'), "{transcript}");
        assert!(!transcript.ends_with('\n'), "{transcript}");

        // 工具行超过上限：恰好列 TRANSCRIPT_TOOL_LINES 条，末行是截断标记
        let truncated = render_transcript(&task, &tool_log(TRANSCRIPT_TOOL_LINES + 3));
        let tool_lines: Vec<&str> = truncated.lines().skip(4).collect();
        assert_eq!(tool_lines.len(), TRANSCRIPT_TOOL_LINES + 1, "{truncated}");
        assert_eq!(tool_lines[0], "- t0: step 0");
        assert_eq!(
            tool_lines[TRANSCRIPT_TOOL_LINES - 1],
            format!(
                "- t{}: step {}",
                TRANSCRIPT_TOOL_LINES - 1,
                TRANSCRIPT_TOOL_LINES - 1
            )
        );
        assert_eq!(tool_lines[TRANSCRIPT_TOOL_LINES], "- … [truncated]");
        // 超出上限的行一条都不列
        assert!(!truncated.contains("t60"), "{truncated}");

        // 恰好等于上限：不追加截断标记
        let at_limit = render_transcript(&task, &tool_log(TRANSCRIPT_TOOL_LINES));
        let tool_lines: Vec<&str> = at_limit.lines().skip(4).collect();
        assert_eq!(tool_lines.len(), TRANSCRIPT_TOOL_LINES);
        assert_eq!(
            tool_lines[TRANSCRIPT_TOOL_LINES - 1],
            format!(
                "- t{}: step {}",
                TRANSCRIPT_TOOL_LINES - 1,
                TRANSCRIPT_TOOL_LINES - 1
            )
        );
        assert!(!at_limit.contains("[truncated]"), "{at_limit}");

        // 一条工具行都没有（含空日志）：`Tools:` 之后是 `- (none)`
        let bare = render_transcript(&task, "{\"type\":\"task_start\"}\n不是 JSON\n");
        assert_eq!(bare.lines().last().unwrap(), "- (none)");
        assert_eq!(bare.lines().count(), 5, "{bare}");
        assert_eq!(render_transcript(&task, ""), bare);

        // 没有答复与别的任务种类照旧成段
        let pending = task_record(&task.id, "schedule", None);
        let transcript = render_transcript(&pending, "");
        assert_eq!(
            transcript.lines().next().unwrap(),
            format!("Task: {} (schedule)", task.id)
        );
        assert_eq!(transcript.lines().nth(2).unwrap(), "Answer: -");
    }

    /// 任务行还在、日志被删（或 home 还没建）时给空串；其它读取失败带路径报 `Io`。
    #[test]
    fn read_task_log_treats_a_missing_file_as_empty() {
        let dir = TempDir::new("log");
        let home = dir.path().join("ignis-home");

        assert_eq!(read_task_log(&home, "01").unwrap(), "");

        // 有文件：原文逐字交回；别的任务 id 互不串
        // （目录由夹具自己建：读路径不创建 `logs`，见 `read_task_log_does_not_create_the_logs_dir`）
        let logs = home.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let raw = "{\"type\":\"task_start\"}\n{\"type\":\"text_delta\",\"text\":\"hi\"}\n";
        fs::write(logs.join("01.jsonl"), raw).unwrap();
        assert_eq!(read_task_log(&home, "01").unwrap(), raw);
        assert_eq!(read_task_log(&home, "02").unwrap(), "");

        // 目录冒充日志文件：读取失败带上路径
        fs::create_dir_all(logs.join("03.jsonl")).unwrap();
        let error = read_task_log(&home, "03").expect_err("目录不是可读的日志文件");
        assert!(matches!(error, LearnError::Io(_)), "{error:?}");
        let message = error.to_string();
        assert!(message.starts_with("learn: reading "), "{message}");
        assert!(message.contains("03.jsonl"), "{message}");
    }

    /// 读路径不碰磁盘：`home/logs` 不存在（日志被删、任务从没写过日志）时给空串，且**一个目录都
    /// 不创建**——写入路径（`TaskLog::new`）才是按需建目录的那一侧。
    ///
    /// 经 `TaskLog::new` 取路径会在读之前把 `logs` 目录建出来：`/learn`、`task show` 这类只读动作
    /// 白留一个空目录，也让「home 里有什么」变成看谁先跑过。
    #[test]
    fn read_task_log_does_not_create_the_logs_dir() {
        let dir = TempDir::new("log-read-only");
        let home = dir.path().join("ignis-home");
        fs::create_dir_all(&home).unwrap();

        assert_eq!(read_task_log(&home, "01").unwrap(), "");
        assert!(!home.join("logs").exists(), "读路径不该创建 logs 目录");

        // 没有 home 也不该被补建（路径直拼，不碰磁盘）
        let absent = dir.path().join("no-home");
        assert_eq!(read_task_log(&absent, "01").unwrap(), "");
        assert!(!absent.exists(), "读路径不该创建 home");
    }

    /// 落盘：目录按需创建、内容等于 `to_skill_md()`、文件 0600、路径 `{dir}/{name}/SKILL.md`。
    #[test]
    fn save_skill_creates_the_directory_and_writes_0600() {
        let dir = TempDir::new("save");
        let skills_dir = dir.path().join("skills");
        let catalog = SkillCatalog::load(&dir.path().join("no-such-skills"));

        let draft = SkillDraft {
            name: "demo-skill".into(),
            description: "A saved skill.".into(),
            body: "# 步骤\n1. 做事\n".into(),
        };
        let path = save_skill(&skills_dir, &draft, &catalog).expect("空目录应当能落盘");

        assert_eq!(path, skills_dir.join("demo-skill").join("SKILL.md"));
        assert!(path.is_file());
        assert_eq!(fs::read_to_string(&path).unwrap(), draft.to_skill_md());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{path:?}");
        }
    }

    /// 同名一律拒绝：文件已存在（字节不变）或在册（含内置）都不落盘。
    #[test]
    fn save_skill_refuses_an_existing_file_or_a_catalog_name() {
        let dir = TempDir::new("overwrite");
        let skills_dir = dir.path().join("skills");
        let catalog = SkillCatalog::load(&dir.path().join("no-such-skills"));
        let draft = SkillDraft {
            name: "demo-skill".into(),
            description: "First.".into(),
            body: "# 一\n".into(),
        };
        save_skill(&skills_dir, &draft, &catalog).unwrap();

        // 目标文件已存在：拒绝，且一个字节都不改（`create_new` 不截断）
        let path = skills_dir.join("demo-skill").join("SKILL.md");
        let before = fs::read(&path).unwrap();
        let error = save_skill(&skills_dir, &draft, &catalog).expect_err("已存在的技能不可覆盖");
        assert_eq!(error, LearnError::Exists("demo-skill".into()));
        assert_eq!(error.to_string(), "skill demo-skill already exists");
        assert_eq!(fs::read(&path).unwrap(), before);

        // 与内置同名（目录里其实没有这个文件）：同样拒绝，且不产生目录
        let builtin = SkillDraft {
            name: "server-health".into(),
            description: "Mine.".into(),
            body: "# 二\n".into(),
        };
        let error = save_skill(&skills_dir, &builtin, &catalog).expect_err("与内置同名必须被拒");
        assert_eq!(error, LearnError::Exists("server-health".into()));
        assert!(!skills_dir.join("server-health").exists());

        // 目录里只有第一次落盘的那一个技能
        let names: Vec<String> = fs::read_dir(&skills_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["demo-skill"]);
    }

    /// 草稿请求：system 是钉死的英文提示、user 是转录原文、整条请求没有 `tools` 字段。
    #[tokio::test]
    async fn draft_skill_uses_the_pinned_prompt_without_tools() {
        let model = ScriptedModel::start(vec![ScriptStep::Text(
            "---\nname: demo-skill\ndescription: Demo.\n---\n# 步骤\n".into(),
        )]);
        let config = scripted_config(&model);
        let transcript = "Task: 01 (run)\nPrompt: 你好\nAnswer: -\nTools:\n- (none)";

        let draft = draft_skill(
            native_runtime(&config),
            &config,
            transcript,
            CancellationToken::new(),
        )
        .await
        .expect("脚本化模型应给出草稿");
        assert_eq!(
            draft,
            SkillDraft {
                name: "demo-skill".into(),
                description: "Demo.".into(),
                body: "# 步骤\n".into(),
            }
        );

        let requests = model.requests();
        assert_eq!(requests.len(), 1, "{requests:#?}");
        // 请求体的第一条消息就是 system（文本来自钉死的 `LEARN_SYSTEM_PROMPT`）
        assert_eq!(
            requests[0]["messages"][0],
            json!({"role": "system", "content": LEARN_SYSTEM_PROMPT})
        );
        // 第二条是转录原文，且整条请求只有这两条消息
        assert_eq!(requests[0]["messages"][1]["role"], "user");
        assert_eq!(requests[0]["messages"][1]["content"], json!(transcript));
        assert_eq!(requests[0]["messages"].as_array().unwrap().len(), 2);
        // 无工具执行器 → 请求体里没有 `tools` 字段
        assert!(
            requests[0].get("tools").is_none(),
            "草稿请求不该带工具：{:#?}",
            requests[0]
        );

        // 提示逐字钉住：关键句一字不改（英文）
        for needle in [
            "You write one reusable skill for an autonomous coding agent.",
            "at most 60 characters",
            "Reply with the SKILL.md draft only — no code fences, no commentary.",
        ] {
            assert!(
                LEARN_SYSTEM_PROMPT.contains(needle),
                "{LEARN_SYSTEM_PROMPT}"
            );
        }
    }

    /// 模型失败与模型「成功但正文不是草稿」分别报 `Generate` / `Draft`。
    #[tokio::test]
    async fn draft_skill_reports_generation_failure_and_invalid_output() {
        // 内核失败（401）：原因原样带进 `Generate`
        let model = ScriptedModel::start(vec![ScriptStep::Error {
            status: 401,
            body: r#"{"error":"bad key"}"#.into(),
        }]);
        let config = scripted_config(&model);
        let error = draft_skill(
            native_runtime(&config),
            &config,
            "Task: 01 (run)",
            CancellationToken::new(),
        )
        .await
        .expect_err("失败的模型不该给出草稿");
        assert!(matches!(error, LearnError::Generate(_)), "{error:?}");
        let message = error.to_string();
        assert!(
            message.starts_with("learn: generation failed: "),
            "{message}"
        );
        assert!(
            message.len() > "learn: generation failed: ".len(),
            "内核给的原因必须带上：{message}"
        );

        // 模型成功但正文不是草稿：`Draft` 而不是 `Generate`
        let model = ScriptedModel::start(vec![ScriptStep::Text("不是草稿".into())]);
        let config = scripted_config(&model);
        let error = draft_skill(
            native_runtime(&config),
            &config,
            "Task: 01 (run)",
            CancellationToken::new(),
        )
        .await
        .expect_err("非法正文不算草稿");
        assert_eq!(
            error,
            LearnError::Draft("missing or invalid frontmatter".into())
        );
        assert_eq!(
            error.to_string(),
            "learn: invalid draft (missing or invalid frontmatter)"
        );

        // 全空白正文：没有可解析的草稿（与 `/compact` 的正文口径同形）
        let model = ScriptedModel::start(vec![ScriptStep::Text("   ".into())]);
        let config = scripted_config(&model);
        let error = draft_skill(
            native_runtime(&config),
            &config,
            "Task: 01 (run)",
            CancellationToken::new(),
        )
        .await
        .expect_err("全空白不算草稿");
        assert_eq!(error, LearnError::Draft("empty draft".into()));
        assert_eq!(error.to_string(), "learn: invalid draft (empty draft)");
    }
}
