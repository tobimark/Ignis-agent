//! 系统提示词：人格、工作区与工具，外加组装期注入的已知事实与技能目录。

use std::path::Path;

use ignis_runtime::ToolSpec;
use ignis_storage::FactRecord;

use crate::skills::{SkillMeta, skills_prompt_section};

/// `Known facts:` 段的标题（spec 决策 #21 锁死的英文措辞）。
const FACTS_HEADER: &str = "Known facts:";
/// 绑定工作区、无条件注入的 project 事实的字符预算。
const PROJECT_FACTS_BUDGET: usize = 2000;
/// 按 prompt 检索命中的 `user`/`global` 事实的字符预算。
const RETRIEVED_FACTS_BUDGET: usize = 2000;
/// 某一类事实放不下时段尾补的标记行；截断以「整条事实」为单位，不切坏半句话。
const TRUNCATED_MARKER: &str = "… [facts truncated]";
/// 走 project 预算的 `facts.scope` 取值。
const PROJECT_SCOPE: &str = "project";
/// 工具使用指引里点名 `remember` 的一句：用户说出的持久偏好或环境事实值得记下来。
///
/// 与工具段同一道门控：只有工具可用的内核（native）才写进去——外部内核没有 `remember` 这个
/// 元工具，写了只会让模型去追一个不存在的工具。
const REMEMBER_GUIDANCE: &str =
    "Call `remember` when the user states a durable preference or environment fact.";
/// 外部内核（`Capabilities.tools == false`）的工具段：顶替工具清单的一行说明（决策 #4）。
const TOOLS_UNAVAILABLE: &str = "Tools: unavailable in this kernel (Ignis tools do not run here).";

/// 组装 system prompt：英文骨架（人格一行 + 工作区路径 + 工具使用要求 + 工具段 + 「用用户的语言
/// 回答」）之后依次追加已知事实段与技能目录段；两段都为空时整个不出现，骨架保持原样。
///
/// 工具段依内核能力二选一（`tools_unavailable` 即 `!Capabilities.tools`）：native 逐个点名工具
/// （完整描述由内核的 `tools` 字段下发），让模型知道手上有什么、在哪儿干活；外部内核自带工具循环、
/// Ignis 的工具对它们不可用，用一行 [`TOOLS_UNAVAILABLE`] 顶替清单。
/// 事实逐条 `- [{scope}/{key}] {text}`（见 [`facts_section`]）；技能只列名称与描述，正文交给
/// `load_skill` 按需读取（渐进披露）。
pub fn build_system_prompt(
    workspace: &Path,
    tools: &[ToolSpec],
    tools_unavailable: bool,
    facts: &[FactRecord],
    skills: &[SkillMeta],
) -> String {
    // 工具段依内核能力二选一：外部内核（`tools: false`）不注入工具，只留一行说明——清单里点名
    // 的工具对它们跑不起来，模型不该照着调用
    let tools_line = if tools_unavailable {
        TOOLS_UNAVAILABLE.to_string()
    } else {
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("Available tools: {names}")
    };
    // 工具指引与工具段同一道门控：外部内核没有这些元工具，指引整句不出现；native 逐字节不变
    let guidance = if tools_unavailable {
        String::new()
    } else {
        format!(" {REMEMBER_GUIDANCE}")
    };
    let mut prompt = format!(
        "You are Ignis, a personal AI agent that does real work with tools.\n\
         Workspace: {}\n\
         Use the tools instead of guessing; prefer absolute paths inside the workspace, and keep \
         replies concise.{guidance}\n\
         {tools_line}\n\
         Answer in the user's language.",
        workspace.display()
    );
    if let Some(section) = facts_section(facts) {
        prompt.push('\n');
        prompt.push_str(&section);
    }
    if !skills.is_empty() {
        prompt.push('\n');
        prompt.push_str(&skills_prompt_section(skills));
    }
    prompt
}

/// 渲染 `Known facts:` 段；没有任何事实时返回 `None`（整段省略）。
///
/// project 事实排在前、检索事实（`user`/`global`）在后，两类各按一份字符预算记账（含行尾换行）：
/// 顺序即优先级，单条超预算的只跳过自己（见 [`push_class`]），段尾补一行 [`TRUNCATED_MARKER`]
/// （标记本身不计入预算）。
fn facts_section(facts: &[FactRecord]) -> Option<String> {
    if facts.is_empty() {
        return None;
    }
    let mut lines = vec![FACTS_HEADER.to_owned()];
    let project = facts.iter().filter(|fact| fact.scope == PROJECT_SCOPE);
    let retrieved = facts.iter().filter(|fact| fact.scope != PROJECT_SCOPE);
    let project_truncated = push_class(&mut lines, project, PROJECT_FACTS_BUDGET);
    let retrieved_truncated = push_class(&mut lines, retrieved, RETRIEVED_FACTS_BUDGET);
    if project_truncated || retrieved_truncated {
        lines.push(TRUNCATED_MARKER.to_owned());
    }
    Some(lines.join("\n"))
}

/// 把一类事实按预算追加进 `lines`：一条一行，单条超预算的**只跳过自己**，后面放得下的条目照旧
/// 追加（顺序即优先级，但一条失控的超长事实不该把整类事实挡在提示之外）；有任何一条没放进去就
/// 返回 `true`（段尾补一行截断标记）。
fn push_class<'a>(
    lines: &mut Vec<String>,
    facts: impl Iterator<Item = &'a FactRecord>,
    budget: usize,
) -> bool {
    let mut used = 0;
    let mut truncated = false;
    for fact in facts {
        let line = fact_line(fact);
        let cost = line.chars().count() + 1;
        if used + cost > budget {
            truncated = true;
            continue;
        }
        used += cost;
        lines.push(line);
    }
    truncated
}

/// 一条事实在系统提示里的一行：`- [{scope}/{key}] {text}`。
fn fact_line(fact: &FactRecord) -> String {
    format!("- [{}/{}] {}", fact.scope, fact.key, fact.text)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ignis_storage::FactRecord;

    use super::*;
    use crate::skills::{SkillMeta, SkillSource};

    /// 一条事实行：只填断言关心的字段（渲染不看 id/时间戳）。
    fn fact(id: i64, scope: &str, key: &str, text: &str) -> FactRecord {
        FactRecord {
            id,
            scope: scope.to_owned(),
            workspace: None,
            key: key.to_owned(),
            text: text.to_owned(),
            status: "active".to_owned(),
            superseded_by: None,
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
        }
    }

    fn meta(name: &str, description: &str, source: SkillSource) -> SkillMeta {
        SkillMeta {
            name: name.to_owned(),
            description: description.to_owned(),
            source,
        }
    }

    #[test]
    fn system_prompt_mentions_workspace_and_tools() {
        let tools = crate::tools::default_registry().specs();

        let prompt = build_system_prompt(Path::new("/tmp/ignis-ws"), &tools, false, &[], &[]);

        assert!(prompt.contains("/tmp/ignis-ws"), "{prompt}");
        for name in [
            "fs_read",
            "fs_list",
            "fs_write",
            "fs_edit",
            "shell_run",
            "http_fetch",
        ] {
            assert!(prompt.contains(name), "{prompt}");
        }
        // 人格一行 + 「用用户的语言回答」
        assert!(prompt.contains("Ignis"), "{prompt}");
        assert!(prompt.to_lowercase().contains("language"), "{prompt}");
        // 没有事实、没有技能：两段都不出现，骨架保持原样
        assert!(!prompt.contains(FACTS_HEADER), "{prompt}");
        assert!(!prompt.contains("Available skills"), "{prompt}");
    }

    #[test]
    fn facts_section_lists_project_facts_before_retrieved_ones() {
        // 故意把检索事实放在输入最前面：段内顺序仍然是 project 在前、检索在后
        let facts = [
            fact(1, "user", "editor", "偏好 Neovim"),
            fact(2, "project", "pkg_manager", "使用 uv"),
        ];

        let prompt = build_system_prompt(Path::new("/w/a"), &[], false, &facts, &[]);

        assert!(
            prompt.contains(
                "\nKnown facts:\n- [project/pkg_manager] 使用 uv\n- [user/editor] 偏好 Neovim"
            ),
            "{prompt}"
        );
        assert!(!prompt.contains(TRUNCATED_MARKER), "{prompt}");
        // 事实段与技能段各自独立：没有技能就不留技能段
        assert!(!prompt.contains("Available skills"), "{prompt}");
    }

    #[test]
    fn facts_section_stops_at_each_budget_and_marks_the_cut() {
        // 每行 100 字符（84 个 a 的正文 + 固定前缀），两类各 25 条 → 都超过 2000 字符的预算
        let text = "a".repeat(84);
        let mut facts: Vec<FactRecord> = (0..25)
            .map(|i| fact(i, "project", &format!("k{i:02}"), &text))
            .collect();
        facts.extend((0..25).map(|i| fact(100 + i, "user", &format!("u{i:02}"), &text)));

        let prompt = build_system_prompt(Path::new("/w/a"), &[], false, &facts, &[]);

        let section = prompt
            .split_once(&format!("\n{FACTS_HEADER}\n"))
            .expect("事实段")
            .1;
        // 一次运行只留一条截断标记，且排在两类事实之后
        assert_eq!(section.matches(TRUNCATED_MARKER).count(), 1, "{section}");
        assert!(section.trim_end().ends_with(TRUNCATED_MARKER), "{section}");

        let project: Vec<&str> = section
            .lines()
            .take_while(|line| line.starts_with("- [project/"))
            .collect();
        let retrieved: Vec<&str> = section
            .lines()
            .filter(|line| line.starts_with("- [user/"))
            .collect();
        let used =
            |lines: &[&str]| -> usize { lines.iter().map(|line| line.chars().count() + 1).sum() };
        assert!(!project.is_empty() && !retrieved.is_empty(), "{section}");
        // 预算按字符记账（含行尾换行），放得下的整条原样，放不下的整条不出现
        let project_used = used(&project);
        let retrieved_used = used(&retrieved);
        assert!(project_used <= PROJECT_FACTS_BUDGET, "{project_used}");
        assert!(retrieved_used <= RETRIEVED_FACTS_BUDGET, "{retrieved_used}");
        // 预算要用满：再塞一条就超了，说明不是提前收手
        let cost = |line: &str| line.chars().count() + 1;
        assert!(project_used + cost(project[0]) > PROJECT_FACTS_BUDGET);
        assert!(retrieved_used + cost(retrieved[0]) > RETRIEVED_FACTS_BUDGET);
        // 被截断的尾部整条不出现
        assert!(
            !section.contains("k19") && !section.contains("k24"),
            "{section}"
        );
        assert!(
            !section.contains("u20") && !section.contains("u24"),
            "{section}"
        );
    }

    #[test]
    fn skills_section_lists_one_line_per_skill() {
        let metas = [
            meta("alpha", "First skill.", SkillSource::Builtin),
            meta("beta", "Second skill.", SkillSource::User),
        ];

        let prompt = build_system_prompt(Path::new("/w/a"), &[], false, &[], &metas);

        assert!(
            prompt.contains(
                "\nAvailable skills (call load_skill to read one):\n\
                 - alpha: First skill.\n\
                 - beta: Second skill. (user)"
            ),
            "{prompt}"
        );
        // 没有事实就不留事实段
        assert!(!prompt.contains(FACTS_HEADER), "{prompt}");
    }

    /// 超预算的是**单条**事实，不是「从这条起全丢」：把它跳过，后面放得下的小事实照旧注入
    /// （否则一条失控的超长事实能把整类事实挡在提示之外）。
    #[test]
    fn an_oversized_fact_does_not_hide_the_later_ones() {
        let monster = fact(1, "user", "giant", &"a".repeat(RETRIEVED_FACTS_BUDGET * 2));
        let normal = fact(2, "user", "editor", "偏好 Neovim");
        let project = fact(3, "project", "pkg_manager", "使用 uv");
        let facts = [monster, normal, project];

        let prompt = build_system_prompt(Path::new("/w/a"), &[], false, &facts, &[]);

        assert!(
            prompt.contains("- [project/pkg_manager] 使用 uv"),
            "{prompt}"
        );
        assert!(prompt.contains("- [user/editor] 偏好 Neovim"), "{prompt}");
        // 被跳过的那条整条不出现（key 与正文都不在），段尾只补一条截断标记
        assert!(!prompt.contains("giant"), "{prompt}");
        assert!(!prompt.contains(&"a".repeat(64)), "{prompt}");
        assert_eq!(prompt.matches(TRUNCATED_MARKER).count(), 1, "{prompt}");
        assert!(prompt.trim_end().ends_with(TRUNCATED_MARKER), "{prompt}");
    }

    /// 工具使用指引里点名 `remember`：用户说出持久的偏好或环境事实时就该写进记忆。
    #[test]
    fn tool_guidance_tells_the_model_to_remember_durable_facts() {
        let prompt = build_system_prompt(Path::new("/w/a"), &[], false, &[], &[]);

        assert!(prompt.contains(REMEMBER_GUIDANCE), "{prompt}");
        // 钉住措辞本身（改文案要一起改测试，别把指引悄悄删掉）
        assert!(
            prompt.contains("`remember`") && prompt.contains("durable preference"),
            "{prompt}"
        );
    }

    /// 两种内核的骨架逐字节钉住：native 与今天完全一致（工具指引 + 清单都在），外部内核同一骨架
    /// 但**整句 `remember` 指引与清单一起被顶掉**——它那边没有元工具，写了只会让模型去追不存在的工具。
    #[test]
    fn native_and_external_skeletons_are_pinned_byte_for_byte() {
        let tools = [ToolSpec {
            name: "fs_read".into(),
            description: "Read a file.".into(),
            parameters: serde_json::Value::Null,
        }];

        let native = build_system_prompt(Path::new("/w/a"), &tools, false, &[], &[]);
        assert_eq!(
            native,
            "You are Ignis, a personal AI agent that does real work with tools.\n\
Workspace: /w/a\n\
Use the tools instead of guessing; prefer absolute paths inside the workspace, and keep replies \
concise. Call `remember` when the user states a durable preference or environment fact.\n\
Available tools: fs_read\n\
Answer in the user's language."
        );

        let external = build_system_prompt(Path::new("/w/a"), &tools, true, &[], &[]);
        assert_eq!(
            external,
            format!(
                "You are Ignis, a personal AI agent that does real work with tools.\n\
Workspace: /w/a\n\
Use the tools instead of guessing; prefer absolute paths inside the workspace, and keep replies \
concise.\n\
{TOOLS_UNAVAILABLE}\n\
Answer in the user's language."
            ),
            "外部内核既不出现工具指引（它没有元工具），也不出现工具清单"
        );
        assert!(!external.contains("`remember`"), "{external}");
    }

    /// 能力门控（决策 #4）：外部内核（`Capabilities.tools == false`）的工具段换成一行说明，
    /// 工具清单整体不出现；骨架的其余部分、事实段与技能段照旧。
    #[test]
    fn external_kernels_get_the_unavailable_tools_line_instead_of_the_tool_list() {
        let tools = crate::tools::default_registry().specs();
        let facts = [fact(1, "project", "pkg_manager", "使用 uv")];
        let metas = [meta("alpha", "First skill.", SkillSource::Builtin)];

        let prompt = build_system_prompt(Path::new("/w/a"), &tools, true, &facts, &metas);

        // 说明行逐字，清单段整体不出现
        assert!(prompt.contains(TOOLS_UNAVAILABLE), "{prompt}");
        assert!(!prompt.contains("Available tools:"), "{prompt}");
        assert!(
            prompt.contains(&format!(
                "\n{TOOLS_UNAVAILABLE}\nAnswer in the user's language."
            )),
            "{prompt}"
        );
        // 工具名一个都不在：模型不该尝试调用不存在的工具（技能段标题里的 `load_skill` 不算，
        // 它是技能目录的一部分）
        for name in ["fs_read", "shell_run", "notify_user", "schedule_create"] {
            assert!(!prompt.contains(name), "{prompt}");
        }
        // 工具指引与清单同一道门控：外部内核没有元工具，`remember` 指引整句不出现
        assert!(!prompt.contains(REMEMBER_GUIDANCE), "{prompt}");
        assert!(!prompt.contains("`remember`"), "{prompt}");
        // 骨架其余部分、事实段与技能段照旧
        assert!(prompt.contains("You are Ignis"), "{prompt}");
        assert!(
            prompt.contains("\nKnown facts:\n- [project/pkg_manager] 使用 uv"),
            "{prompt}"
        );
        assert!(
            prompt.contains("\nAvailable skills (call load_skill to read one):\n"),
            "{prompt}"
        );
        assert!(
            prompt.ends_with("\n- alpha: First skill."),
            "技能段照旧（目录一行一条，收尾无多余空行）：{prompt}"
        );

        // native（`false`）不变：清单与指引都在，说明行不在
        let native = build_system_prompt(Path::new("/w/a"), &tools, false, &facts, &metas);
        assert!(native.contains("Available tools: fs_read"), "{native}");
        assert!(native.contains(REMEMBER_GUIDANCE), "{native}");
        assert!(!native.contains(TOOLS_UNAVAILABLE), "{native}");
    }
}
