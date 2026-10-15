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

/// 组装 system prompt：英文骨架（人格一行 + 工作区路径 + 工具使用要求 + 「用用户的语言回答」）
/// 之后依次追加已知事实段与技能目录段；两段都为空时整个不出现，骨架保持原样。
///
/// 工具逐个点名（完整描述由内核的 `tools` 字段下发），让模型知道手上有什么、在哪儿干活；
/// 事实逐条 `- [{scope}/{key}] {text}`（见 [`facts_section`]）；技能只列名称与描述，正文交给
/// `load_skill` 按需读取（渐进披露）。
pub fn build_system_prompt(
    workspace: &Path,
    tools: &[ToolSpec],
    facts: &[FactRecord],
    skills: &[SkillMeta],
) -> String {
    let names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut prompt = format!(
        "You are Ignis, a personal AI agent that does real work with tools.\n\
         Workspace: {}\n\
         Use the tools instead of guessing; prefer absolute paths inside the workspace, and keep \
         replies concise.\n\
         Available tools: {names}\n\
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
/// 顺序即优先级，某一类一旦放不下下一条就停在那里，段尾补一行 [`TRUNCATED_MARKER`]
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

/// 把一类事实按预算追加进 `lines`：一条一行，放不下的整条跳过（顺序即优先级，从此截断），
/// 有任何一条没放进去就返回 `true`。
fn push_class<'a>(
    lines: &mut Vec<String>,
    facts: impl Iterator<Item = &'a FactRecord>,
    budget: usize,
) -> bool {
    let mut used = 0;
    for fact in facts {
        let line = fact_line(fact);
        let cost = line.chars().count() + 1;
        if used + cost > budget {
            return true;
        }
        used += cost;
        lines.push(line);
    }
    false
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

        let prompt = build_system_prompt(Path::new("/tmp/ignis-ws"), &tools, &[], &[]);

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

        let prompt = build_system_prompt(Path::new("/w/a"), &[], &facts, &[]);

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

        let prompt = build_system_prompt(Path::new("/w/a"), &[], &facts, &[]);

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

        let prompt = build_system_prompt(Path::new("/w/a"), &[], &[], &metas);

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
}
