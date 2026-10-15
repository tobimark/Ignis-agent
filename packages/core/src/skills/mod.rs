//! 技能（skills）：兼容 SKILL.md 约定的技能目录与渐进披露。
//!
//! 技能来源两类：仓库内置的三个（构建时嵌入二进制，见 [`BUILTIN_SKILLS`]）与用户目录
//! `~/.ignis/skills/`（目录由调用方给出，本模块不读环境变量）。两者合并扫描：**同名 user 覆盖
//! builtin**，畸形技能跳过并记一条警告，绝不 panic。
//!
//! 渐进披露分两步：任务启动时只把名称 + 描述注入系统提示（[`skills_prompt_section`]），模型需要
//! 时再经工具读取 SKILL.md 全文（[`SkillCatalog::get`] 的 `body`）。

mod catalog;
pub mod frontmatter;

pub use catalog::{BUILTIN_SKILLS, SkillCatalog};

/// 系统提示里单行描述的最大字符数（按 unicode 字符截断，不切坏中文）。
const DESCRIPTION_LIMIT: usize = 200;

/// 渐进披露段的固定表头（提示模型用 `load_skill` 取全文）。
const PROMPT_HEADER: &str = "Available skills (call load_skill to read one):";

/// 技能来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// 仓库内置（`include_str!` 嵌入）。
    Builtin,
    /// 用户目录里的技能。
    User,
}

/// 技能元信息：渐进披露只需要这些，正文见 [`Skill::body`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
}

/// 一个技能：元信息 + SKILL.md 正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub meta: SkillMeta,
    pub body: String,
}

/// 渐进披露的系统提示段：固定表头，之后每个技能一行 `- {name}: {description}`（user 来源追加
/// ` (user)`）；描述按字符截到 [`DESCRIPTION_LIMIT`]，段尾不留空行。
pub fn skills_prompt_section(metas: &[SkillMeta]) -> String {
    let mut lines = vec![PROMPT_HEADER.to_owned()];
    for meta in metas {
        let mut line = format!(
            "- {}: {}",
            meta.name,
            truncate(&meta.description, DESCRIPTION_LIMIT)
        );
        if meta.source == SkillSource::User {
            line.push_str(" (user)");
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// 按 unicode 字符截断（不按字节，中文不会被切坏）。
fn truncate(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        BUILTIN_SKILLS, SkillCatalog, SkillMeta, SkillSource, frontmatter, skills_prompt_section,
    };

    fn meta(name: &str, description: &str, source: SkillSource) -> SkillMeta {
        SkillMeta {
            name: name.to_owned(),
            description: description.to_owned(),
            source,
        }
    }

    #[test]
    fn prompt_section_lists_one_line_per_skill() {
        let metas = [
            meta("alpha", "First skill.", SkillSource::Builtin),
            meta("beta", "Second skill.", SkillSource::User),
        ];

        assert_eq!(
            skills_prompt_section(&metas),
            "Available skills (call load_skill to read one):\n\
             - alpha: First skill.\n\
             - beta: Second skill. (user)"
        );
    }

    #[test]
    fn prompt_section_truncates_long_descriptions_by_chars() {
        let metas = [meta("long", &"技".repeat(300), SkillSource::User)];

        let section = skills_prompt_section(&metas);

        assert_eq!(section.lines().count(), 2, "{section}");
        let line = section.lines().nth(1).unwrap();
        let description = line
            .strip_prefix("- long: ")
            .and_then(|rest| rest.strip_suffix(" (user)"))
            .unwrap();
        assert_eq!(description.chars().count(), 200);
        assert_eq!(description, "技".repeat(200));
    }

    #[test]
    fn prompt_section_of_no_skills_is_just_the_header() {
        let section = skills_prompt_section(&[]);

        assert_eq!(section, "Available skills (call load_skill to read one):");
        assert!(!section.ends_with('\n'));
    }

    #[test]
    fn prompt_section_of_the_builtins_has_one_line_per_skill() {
        let catalog = SkillCatalog::load(Path::new("/nonexistent/ignis-skills-test"));
        let section = skills_prompt_section(catalog.metas());

        assert_eq!(section.lines().count(), catalog.metas().len() + 1);
        assert_eq!(
            section.lines().next().unwrap(),
            "Available skills (call load_skill to read one):"
        );
        for (line, meta) in section.lines().skip(1).zip(catalog.metas()) {
            let prefix = format!("- {}: ", meta.name);
            assert!(line.starts_with(prefix.as_str()), "{line}");
        }
        assert!(!section.ends_with('\n'));
    }

    #[test]
    fn builtin_bodies_cover_the_documented_steps() {
        let catalog = SkillCatalog::load(Path::new("/nonexistent/ignis-skills-test"));
        let body = |name: &str| catalog.get(name).expect("内置技能必须在册").body.clone();

        let analyzer = body("project-analyzer");
        for needle in [
            "fs_list",
            "package.json",
            "pyproject.toml",
            "Cargo.toml",
            "shell_run",
            "fs_read",
            "fs_edit",
        ] {
            assert!(analyzer.contains(needle), "project-analyzer 缺少 {needle}");
        }

        let health = body("server-health");
        for needle in ["df -h", "free -m", "docker ps", "85%"] {
            assert!(health.contains(needle), "server-health 缺少 {needle}");
        }

        let report = body("report-generator");
        for needle in ["## 概述", "## 证据", "## 结论", "fs_write"] {
            assert!(report.contains(needle), "report-generator 缺少 {needle}");
        }
    }

    #[test]
    fn builtin_content_parses_with_the_shared_frontmatter_parser() {
        for (label, content) in BUILTIN_SKILLS {
            let parsed = frontmatter::parse(content);
            assert!(parsed.is_some(), "{label}: 内置 SKILL.md 解析失败");
            let (name, description, body) = parsed.unwrap();
            assert_eq!(name, *label);
            assert!(!description.contains('\n'), "{label}: 描述必须是单行");
            assert!(!body.trim().is_empty(), "{label}: 正文不能为空");
        }
    }
}
