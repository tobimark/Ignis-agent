//! 元工具：`remember`（写入一条长期记忆）与 `load_skill`（读取一个技能的 SKILL.md 全文）。
//!
//! 两者都只写 agent 自有状态（记忆事实、技能目录，用户可经 CLI 改删），因此
//! [`crate::tools::ToolRuntime`] 一律按 `Safe` 分类：不弹审批、不落审批行。参数一律先解析成
//! 中文可读的错误再回填给模型（缺参、非法 scope、空 key/text、未知技能）。

use std::sync::Arc;

use async_trait::async_trait;
use ignis_runtime::ToolSpec;
use serde_json::{Value, json};

use super::util::{required_str, truncate_output};
use super::{Tool, ToolCtx, ToolError};
use crate::memory::{FactScope, MemoryError, MemoryStore};
use crate::skills::SkillCatalog;

/// v0.1 元工具：`remember` 与 `load_skill`；返回顺序即注册顺序，也就是模型看到的工具表顺序。
pub fn meta_tools(memory: Arc<MemoryStore>, skills: Arc<SkillCatalog>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(Remember { memory }),
        Box::new(LoadSkill { skills }),
    ]
}

/// 写入一条长期记忆；同一个 `(scope, workspace, key)` 的新值取代旧值（supersede）。
struct Remember {
    memory: Arc<MemoryStore>,
}

#[async_trait]
impl Tool for Remember {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remember".into(),
            description: "Save one durable fact to long-term memory so later tasks can use it. \
                          Writing the same (scope, key) again replaces the old value."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "enum": ["user", "project", "global"],
                        "description": "Where the fact applies: `user` = preferences that travel across projects, `project` = facts about the current workspace, `global` = knowledge shared by every project"
                    },
                    "key": {
                        "type": "string",
                        "description": "Short stable identifier of the fact, e.g. `pkg_manager`; writing the same key again replaces it"
                    },
                    "text": {
                        "type": "string",
                        "description": "The fact itself, one sentence is usually enough"
                    }
                },
                "required": ["scope", "key", "text"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
        let scope = scope_arg(args)?;
        let key = required_str(args, "key")?;
        let text = required_str(args, "text")?;
        // project 事实绑定当前工作区；user/global 与工作区无关，忽略它
        let workspace = (scope == FactScope::Project).then_some(ctx.workspace);
        // 写入前的 active 行：写完拿 id 一比就知道这次是不是真的取代了旧值
        let previous = self
            .memory
            .recall_by_key(scope, workspace, key)
            .map_err(memory_failure)?;
        let record = self
            .memory
            .remember(scope, workspace, key, text, None)
            .map_err(memory_failure)?;

        // 回显的是真正入库的那份文本（`MemoryStore` 存储前裁掉首尾空白）
        let mut output = format!(
            "已记住（{}/{}）：{}",
            scope.as_str(),
            key.trim(),
            text.trim()
        );
        if previous.is_some_and(|old| old.id != record.id) {
            output.push_str("（旧值已失效）");
        }
        Ok(output)
    }
}

/// 记忆层错误转工具错误：`MemoryError` 的各种校验消息本身就是中文人话，原样回填给模型。
fn memory_failure(error: MemoryError) -> ToolError {
    ToolError::message(error.to_string())
}

/// `scope` 参数：只接受 `user` / `project` / `global` 三个字面量。
///
/// 参数形态的错误（缺参、类型不对、拼错的 scope）在这里给出中文说明；`key`/`text` 的空白校验
/// 交给 [`MemoryStore::remember`]，它的消息同样是中文。
fn scope_arg(args: &Value) -> Result<FactScope, ToolError> {
    let raw = required_str(args, "scope")?;
    match raw {
        "user" => Ok(FactScope::User),
        "project" => Ok(FactScope::Project),
        "global" => Ok(FactScope::Global),
        other => Err(ToolError::message(format!(
            "参数 `scope` 只能是 user、project 或 global，收到「{other}」"
        ))),
    }
}

/// 读取一个技能的 SKILL.md 正文（渐进披露的第二步；目录已在系统提示里列出）。
struct LoadSkill {
    skills: Arc<SkillCatalog>,
}

#[async_trait]
impl Tool for LoadSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "load_skill".into(),
            description:
                "Read the full instructions of one skill listed in the `Available skills` \
                          section of the system prompt, then follow them with the regular tools."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill name exactly as listed in the system prompt"
                    }
                },
                "required": ["name"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError> {
        let name = required_str(args, "name")?;
        match self.skills.get(name) {
            // 正文按工具输出上限截断：技能指令很长时也不撑爆上下文
            Some(skill) => Ok(truncate_output(&skill.body, ctx.limits.tool_output_limit)),
            None => Err(ToolError::message(format!(
                "未找到技能「{name}」；可用技能：{}",
                available_names(&self.skills)
            ))),
        }
    }
}

/// 未命中时列出的可用技能名，顺序与系统提示里的目录一致（按名称排序）。
fn available_names(skills: &SkillCatalog) -> String {
    skills
        .metas()
        .iter()
        .map(|meta| meta.name.as_str())
        .collect::<Vec<&str>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_runtime::test_support::ScriptedApproval;
    use ignis_runtime::{ApprovalHandler, Limits, ToolCall, ToolExecutor, ToolResult};
    use ignis_storage::{ApprovalRecord, Db, FactRecord};
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::meta_tools;
    use crate::memory::MemoryStore;
    use crate::security::{ApprovalMode, Policy, Redactor};
    use crate::skills::SkillCatalog;
    use crate::tools::{ToolRuntime, default_registry};

    /// 本次任务的 id：审批记录按它归属。
    const TASK_ID: &str = "01J9Z6Q3T7R8S9V0W1X2Y3Z4AB";
    /// 临时技能正文里的标记行：命中 `load_skill` 时必须原样出现。
    const MARKER: &str = "META-SKILL-MARKER-7";

    /// 单个测试独占的临时目录，析构时清理（本 crate 不引入 tempfile 依赖）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-tools-meta-{}-{}",
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

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 一份临时 SKILL.md：frontmatter + 正文。
    fn skill_md(name: &str, body: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: Temporary skill for the meta tool tests.\n---\n{body}"
        )
    }

    /// 元工具的测试环境：真实的内存库 + 临时工作区/镜像目录/技能目录。
    ///
    /// 注册表 = 内置工具 + `meta_tools`（生产装配在 Task 5 的 `TaskRunner`，这里手工组装）；
    /// 审批方是空决策队列的 `ScriptedApproval`（耗尽即拒绝）——元工具一旦被判成非 `Safe`，
    /// 调用就会被拒并留下审批行，相关断言立刻失败。
    struct Env {
        dir: TempDir,
        db: Arc<Db>,
        approval: Arc<ScriptedApproval>,
        runtime: ToolRuntime,
    }

    impl Env {
        fn new() -> Env {
            Self::with_limits(Limits::default())
        }

        fn with_limits(limits: Limits) -> Env {
            let dir = TempDir::new();
            let skills_dir = dir.path().join("skills");
            for (name, body) in [
                ("my-skill", format!("# my-skill\n{MARKER}\n")),
                ("big", format!("# big\n{}\n", "技".repeat(100))),
            ] {
                let path = skills_dir.join(name);
                fs::create_dir_all(&path).unwrap();
                fs::write(path.join("SKILL.md"), skill_md(name, &body)).unwrap();
            }

            let db = Arc::new(Db::open_in_memory().unwrap());
            let memory = MemoryStore::new(Arc::clone(&db), dir.path().join("memory"));
            let skills = SkillCatalog::load(&skills_dir);
            let mut registry = default_registry();
            for tool in meta_tools(memory, skills) {
                registry.register(tool);
            }
            let approval = ScriptedApproval::new(Vec::new());
            let runtime = ToolRuntime::new(
                registry,
                Arc::new(Policy::new(ApprovalMode::Interactive)),
                Arc::clone(&approval) as Arc<dyn ApprovalHandler>,
                Arc::clone(&db),
                TASK_ID.to_string(),
                dir.path().to_path_buf(),
                limits,
                Redactor::new(Vec::new()),
            );
            Env {
                dir,
                db,
                approval,
                runtime,
            }
        }

        async fn call(&self, name: &str, args: Value) -> ToolResult {
            let call = ToolCall {
                id: format!("call-{name}"),
                name: name.to_string(),
                args,
            };
            self.runtime.execute(call, CancellationToken::new()).await
        }

        /// 审批记录（按决定先后）。
        fn approvals(&self) -> Vec<ApprovalRecord> {
            self.db.list_approvals(TASK_ID).unwrap()
        }

        /// 某个位置的 `active` 事实行。
        fn active_fact(&self, scope: &str, workspace: Option<&str>, key: &str) -> FactRecord {
            self.db
                .active_fact_by_key(scope, workspace, key)
                .unwrap()
                .unwrap_or_else(|| panic!("facts 里应有 {scope}/{key} 的 active 行"))
        }

        fn mirror_dir(&self) -> PathBuf {
            self.dir.path().join("memory")
        }

        fn mirror(&self, file: &str) -> String {
            fs::read_to_string(self.mirror_dir().join(file)).unwrap()
        }
    }

    #[tokio::test]
    async fn remember_is_safe_persists_the_fact_and_writes_the_mirror() {
        let env = Env::new();

        let result = env
            .call(
                "remember",
                json!({"scope": "user", "key": "pkg_manager", "text": "使用 uv 管理 Python 依赖"}),
            )
            .await;

        assert!(result.ok, "{:?}", result.output);
        assert_eq!(
            result.output.as_deref(),
            Some("已记住（user/pkg_manager）：使用 uv 管理 Python 依赖")
        );
        // Safe：既不弹审批，也不留审批行
        assert!(env.approval.seen().is_empty());
        assert!(env.approvals().is_empty());

        // 事实入库：user 范围不带工作区，镜像跟着重写
        let fact = env.active_fact("user", None, "pkg_manager");
        assert_eq!(fact.text, "使用 uv 管理 Python 依赖");
        assert_eq!(fact.status, "active");
        assert_eq!(fact.workspace, None);
        assert!(env.mirror("user.md").contains("使用 uv 管理 Python 依赖"));
    }

    #[tokio::test]
    async fn second_remember_for_the_same_key_supersedes_the_old_row() {
        let env = Env::new();

        let first = env
            .call(
                "remember",
                json!({"scope": "user", "key": "pkg_manager", "text": "使用 pip"}),
            )
            .await;
        assert!(first.ok, "{:?}", first.output);
        assert!(
            !first.output.unwrap().contains("（旧值已失效）"),
            "首次写入不应该报 supersede"
        );

        let second = env
            .call(
                "remember",
                json!({"scope": "user", "key": "pkg_manager", "text": "改用 uv"}),
            )
            .await;
        assert!(second.ok, "{:?}", second.output);
        assert_eq!(
            second.output.as_deref(),
            Some("已记住（user/pkg_manager）：改用 uv（旧值已失效）")
        );

        let rows = env.db.list_facts(true).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        let old = rows.iter().find(|row| row.text == "使用 pip").unwrap();
        let new = rows.iter().find(|row| row.text == "改用 uv").unwrap();
        assert_eq!(
            (old.status.as_str(), old.superseded_by),
            ("superseded", Some(new.id))
        );
        assert_eq!(new.status, "active");
        // 读路径只剩新值
        assert_eq!(env.active_fact("user", None, "pkg_manager").id, new.id);
    }

    #[tokio::test]
    async fn project_scope_lands_on_the_current_workspace() {
        let env = Env::new();

        let result = env
            .call(
                "remember",
                json!({"scope": "project", "key": "pkg_manager", "text": "使用 uv"}),
            )
            .await;
        assert!(result.ok, "{:?}", result.output);

        // 落库的 workspace 就是 `ToolCtx.workspace`，别的路径查不到
        let workspace = env.dir.path().to_string_lossy().into_owned();
        let fact = env.active_fact("project", Some(&workspace), "pkg_manager");
        assert_eq!(fact.workspace.as_deref(), Some(workspace.as_str()));
        assert_eq!(fact.text, "使用 uv");
        assert!(
            env.db
                .active_fact_by_key("project", Some("/nowhere/else"), "pkg_manager")
                .unwrap()
                .is_none()
        );

        // 镜像按工作区命名（不在本文具职责内，只验证确实写出了一个文件）
        let files: Vec<String> = fs::read_dir(env.mirror_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(env.mirror(&files[0]).contains("使用 uv"));
    }

    #[tokio::test]
    async fn remember_echoes_the_trimmed_key_and_text() {
        let env = Env::new();

        let result = env
            .call(
                "remember",
                json!({"scope": "global", "key": "  editor  ", "text": "  偏好 Neovim  "}),
            )
            .await;

        assert!(result.ok, "{:?}", result.output);
        // 回显的就是真正落库的那份文本（`MemoryStore` 入库前裁掉首尾空白）
        assert_eq!(
            result.output.as_deref(),
            Some("已记住（global/editor）：偏好 Neovim")
        );
        assert_eq!(
            env.active_fact("global", None, "editor").text,
            "偏好 Neovim"
        );
    }

    #[tokio::test]
    async fn remember_reports_bad_arguments_in_chinese() {
        let env = Env::new();

        for (args, expected) in [
            (json!({"key": "k", "text": "值"}), "缺少必需的参数 `scope`"),
            (
                json!({"scope": 3, "key": "k", "text": "值"}),
                "必须是字符串",
            ),
            (
                json!({"scope": "Project", "key": "k", "text": "值"}),
                "只能是 user、project 或 global",
            ),
            (
                json!({"scope": "user", "text": "值"}),
                "缺少必需的参数 `key`",
            ),
            (
                json!({"scope": "user", "key": "k"}),
                "缺少必需的参数 `text`",
            ),
            (
                json!({"scope": "user", "key": "  ", "text": "值"}),
                "记忆的 key 不能为空",
            ),
            (
                json!({"scope": "user", "key": "k", "text": " \n "}),
                "记忆的 text 不能为空",
            ),
        ] {
            let result = env.call("remember", args.clone()).await;
            assert!(!result.ok, "{args}");
            let output = result.output.unwrap();
            assert!(output.contains(expected), "{args}: {output}");
        }

        // 参数不对就不落库，也不留审批行
        assert!(env.db.list_facts(true).unwrap().is_empty());
        assert!(env.approvals().is_empty());
        assert!(env.approval.seen().is_empty());
    }

    #[tokio::test]
    async fn load_skill_returns_the_body_and_lists_names_on_a_miss() {
        let env = Env::new();

        let hit = env.call("load_skill", json!({"name": "my-skill"})).await;
        assert!(hit.ok, "{:?}", hit.output);
        let output = hit.output.unwrap();
        assert!(output.contains(MARKER), "{output}");
        // frontmatter 不进正文（渐进披露只取 SKILL.md 正文）
        assert!(!output.contains("description:"), "{output}");
        assert!(env.approval.seen().is_empty() && env.approvals().is_empty());

        let miss = env.call("load_skill", json!({"name": "nope"})).await;
        assert!(!miss.ok);
        assert_eq!(
            miss.output.as_deref(),
            Some(
                "未找到技能「nope」；可用技能：big, my-skill, project-analyzer, \
                 report-generator, server-health"
            )
        );
    }

    #[tokio::test]
    async fn load_skill_truncates_the_body_to_the_output_limit() {
        let env = Env::with_limits(Limits {
            tool_output_limit: 32,
            ..Limits::default()
        });

        let result = env.call("load_skill", json!({"name": "big"})).await;

        assert!(result.ok, "{:?}", result.output);
        let output = result.output.unwrap();
        assert!(
            output.ends_with("… [truncated: output exceeded 32 bytes]"),
            "{output}"
        );
    }
}
