//! 技能目录：内置技能与多个用户目录（`~/.ignis/skills/`、项目级 `.ignis/skills/`）的合并扫描、
//! 同名覆盖与警告收集。技能目录可以是符号链接；悬空链接会记一条警告后跳过。

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::frontmatter;
use super::{Skill, SkillMeta, SkillSource};

/// 内置技能：`(标签, SKILL.md 内容)`。
///
/// 内容在构建时嵌入二进制（运行时不需要仓库目录）；标签只用于排错信息，技能名一律取自
/// frontmatter——与用户技能走同一套解析。
pub const BUILTIN_SKILLS: &[(&str, &str)] = &[
    (
        "project-analyzer",
        include_str!("../../../../skills/project-analyzer/SKILL.md"),
    ),
    (
        "server-health",
        include_str!("../../../../skills/server-health/SKILL.md"),
    ),
    (
        "report-generator",
        include_str!("../../../../skills/report-generator/SKILL.md"),
    ),
];

/// 技能目录里约定的技能文件名。
const SKILL_FILE: &str = "SKILL.md";

/// 合并后的技能目录：元信息按名称排序，正文经 [`SkillCatalog::get`] 按需取。
pub struct SkillCatalog {
    skills: Vec<Skill>,
    metas: Vec<SkillMeta>,
    warnings: Vec<String>,
}

impl SkillCatalog {
    /// 扫描内置技能与单个用户目录（`user_dir/*/SKILL.md`）。
    ///
    /// 用户目录不存在不算错误（无警告）；读取失败、frontmatter 无效、悬空符号链接、同名重复
    /// 各记一条 `{path}: …` 警告并跳过该技能。**同名 user 覆盖 builtin**；两个用户技能同名时
    /// 保留先加载目录里的那个，其余警告后忽略。
    pub fn load(user_dir: &Path) -> Arc<SkillCatalog> {
        Self::load_all(std::slice::from_ref(&user_dir.to_path_buf()))
    }

    /// 扫描内置技能与多个用户目录，按目录顺序合并（先加载的优先）。
    ///
    /// 项目级 `<workspace>/.ignis/skills` 放在 `~/.ignis/skills` 前面即可覆盖全局技能；
    /// 任一同名 user 技能覆盖 builtin。每个目录内的技能按目录名字典序扫描。
    pub fn load_all(user_dirs: &[PathBuf]) -> Arc<SkillCatalog> {
        let (mut skills, mut warnings) = parse_builtins(BUILTIN_SKILLS);

        for user_dir in user_dirs {
            for path in user_skill_files(user_dir, &mut warnings) {
                let origin = path.display().to_string();
                let text = match fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(err) => {
                        warnings.push(format!("{origin}: 读取失败：{err}"));
                        continue;
                    }
                };
                let Some(skill) = parse_skill(&text, SkillSource::User) else {
                    warnings.push(format!(
                        "{origin}: frontmatter 无效（需要 name 与 description），已跳过"
                    ));
                    continue;
                };
                match skills
                    .iter_mut()
                    .find(|existing| existing.meta.name == skill.meta.name)
                {
                    // 同名 user 覆盖 builtin：这是约定的覆盖路径，不算问题
                    Some(slot) if slot.meta.source == SkillSource::Builtin => *slot = skill,
                    Some(_) => warnings.push(format!(
                        "{origin}: 技能名 {} 与先加载的同名技能重复，已忽略",
                        skill.meta.name
                    )),
                    None => skills.push(skill),
                }
            }
        }

        skills.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        let metas = skills.iter().map(|skill| skill.meta.clone()).collect();
        Arc::new(SkillCatalog {
            skills,
            metas,
            warnings,
        })
    }

    /// 渐进披露用的技能元信息，按名称排序。
    pub fn metas(&self) -> &[SkillMeta] {
        &self.metas
    }

    /// 按名取技能（含 SKILL.md 正文）；未知名称返回 `None`。
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.meta.name == name)
    }

    /// 加载期间的警告，一行一条（畸形技能、同名重复等）。
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// 解析内置技能：`label` 只进警告文案，技能名取自 frontmatter。
///
/// 内置内容解析失败 = 仓库里的 SKILL.md 坏了：跳过并记一条警告，不带崩整个进程。
fn parse_builtins(builtins: &[(&str, &str)]) -> (Vec<Skill>, Vec<String>) {
    let mut skills = Vec::new();
    let mut warnings = Vec::new();
    for (label, content) in builtins {
        match parse_skill(content, SkillSource::Builtin) {
            Some(skill) => skills.push(skill),
            None => warnings.push(format!(
                "内置技能 {label}：SKILL.md 的 frontmatter 无效（需要 name 与 description），已跳过"
            )),
        }
    }
    (skills, warnings)
}

/// 用户目录下的技能文件：`{user_dir}/*/SKILL.md`，按目录名字典序返回（同名技能以先到的为准）。
///
/// 目录不存在直接返回空表（不算错误）；其他读取失败记一条警告。没有 `SKILL.md` 的目录只是
/// 资源目录（`scripts/` 等），静默跳过。
fn user_skill_files(user_dir: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let entries = match fs::read_dir(user_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            warnings.push(format!("{}: 读取技能目录失败：{err}", user_dir.display()));
            return Vec::new();
        }
    };

    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                // 跟随符号链接：技能目录常用 symlink 安装，`DirEntry::file_type` 本身不跟随。
                if path.is_dir() {
                    let skill = path.join(SKILL_FILE);
                    if skill.is_file() {
                        files.push(skill);
                    }
                } else if entry.file_type().is_ok_and(|kind| kind.is_symlink()) && !path.exists() {
                    warnings.push(format!("{}: 符号链接目标不存在，已跳过", path.display()));
                }
            }
            Err(err) => warnings.push(format!("{}: 读取目录项失败：{err}", user_dir.display())),
        }
    }
    files.sort();
    files
}

/// 用与用户技能同一套 frontmatter 解析读一份 SKILL.md；技能名取自 frontmatter，正文原样保留。
fn parse_skill(text: &str, source: SkillSource) -> Option<Skill> {
    let (name, description, body) = frontmatter::parse(text)?;
    Some(Skill {
        meta: SkillMeta {
            name,
            description,
            source,
        },
        body,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{SkillCatalog, parse_builtins};
    use crate::skills::SkillSource;

    /// 唯一临时目录：测试并发跑，目录名带进程号与进程内自增序号。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("ignis-skills-{name}-{}-{seq}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// 在用户目录下落一个 `{dir}/SKILL.md`。
        fn write_skill(&self, dir: &str, content: &str) {
            let path = self.0.join(dir);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("SKILL.md"), content).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 一份最小可用的 SKILL.md。
    fn skill_md(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}")
    }

    fn names(catalog: &SkillCatalog) -> Vec<&str> {
        catalog
            .metas()
            .iter()
            .map(|meta| meta.name.as_str())
            .collect()
    }

    #[test]
    fn missing_user_dir_still_yields_the_three_builtins() {
        let dir = TempDir::new("builtins");

        let catalog = SkillCatalog::load(&dir.path().join("no-such-dir"));

        assert_eq!(
            names(&catalog),
            ["project-analyzer", "report-generator", "server-health"]
        );
        assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
        for meta in catalog.metas() {
            assert_eq!(meta.source, SkillSource::Builtin);
            assert!(!meta.description.is_empty(), "{}", meta.name);
            let skill = catalog.get(&meta.name).expect("metas 里的技能必须能取到");
            assert_eq!(&skill.meta, meta);
            assert!(!skill.body.trim().is_empty(), "{}", meta.name);
        }
        assert!(catalog.get("no-such-skill").is_none());
    }

    #[test]
    fn broken_builtin_content_warns_and_is_excluded() {
        let good = skill_md("good", "Fine.", "# 正文\n");

        let (skills, warnings) =
            parse_builtins(&[("good", good.as_str()), ("broken", "# 没有 frontmatter\n")]);

        assert_eq!(skills.len(), 1, "坏内容不进目录");
        assert_eq!(skills[0].meta.name, "good");
        assert_eq!(skills[0].meta.source, SkillSource::Builtin);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("broken"), "{warnings:?}");
    }

    #[test]
    fn user_skill_overrides_the_same_named_builtin() {
        let dir = TempDir::new("override");
        dir.write_skill(
            "server-health",
            &skill_md(
                "server-health",
                "My own health checker.",
                "# 自定义\n用 `shell_run` 跑 uptime。\n",
            ),
        );

        let catalog = SkillCatalog::load(dir.path());

        assert_eq!(names(&catalog).len(), 3, "覆盖不是新增");
        let skill = catalog.get("server-health").expect("覆盖后仍在");
        assert_eq!(skill.meta.source, SkillSource::User);
        assert_eq!(skill.meta.description, "My own health checker.");
        assert_eq!(skill.body.trim(), "# 自定义\n用 `shell_run` 跑 uptime。");
        assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
    }

    #[test]
    fn user_skills_are_added_and_sorted_by_name() {
        let dir = TempDir::new("sorted");
        dir.write_skill("zzz", &skill_md("zzz-skill", "Sorts last.", "# 正文\n"));
        dir.write_skill("aaa", &skill_md("aaa-skill", "Sorts first.", "# 正文\n"));

        let catalog = SkillCatalog::load(dir.path());

        assert_eq!(
            names(&catalog),
            [
                "aaa-skill",
                "project-analyzer",
                "report-generator",
                "server-health",
                "zzz-skill"
            ]
        );
        assert_eq!(
            catalog.get("aaa-skill").unwrap().meta.source,
            SkillSource::User
        );
        assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
    }

    #[test]
    fn malformed_user_skills_are_skipped_with_one_warning_each() {
        let dir = TempDir::new("malformed");
        dir.write_skill("broken", "# 没有 frontmatter\n只有正文\n");
        dir.write_skill(
            "no-closing",
            "---\nname: no-closing\ndescription: 缺闭合围栏\n",
        );
        dir.write_skill("no-desc", "---\nname: no-desc\n---\n正文\n");
        // 目录里有别的资源文件不算技能，也不该产生警告
        fs::create_dir_all(dir.path().join("resource-only/scripts")).unwrap();
        fs::write(dir.path().join("resource-only/scripts/run.sh"), "echo hi\n").unwrap();

        let catalog = SkillCatalog::load(dir.path());

        assert_eq!(
            names(&catalog),
            ["project-analyzer", "report-generator", "server-health"]
        );
        assert_eq!(catalog.warnings().len(), 3, "{:?}", catalog.warnings());
        for (dir_name, warning) in ["broken", "no-closing", "no-desc"]
            .iter()
            .zip(catalog.warnings())
        {
            assert!(warning.contains(dir_name), "{warning}");
            assert!(warning.contains("SKILL.md"), "{warning}");
        }
    }

    #[test]
    fn user_skill_descriptions_are_folded_to_one_line() {
        let dir = TempDir::new("folded");
        dir.write_skill(
            "folded",
            "---\nname: folded\ndescription: \"第一行\n第二行\u{7}结束\"\n---\n正文\n",
        );

        let catalog = SkillCatalog::load(dir.path());

        assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
        assert_eq!(
            catalog.get("folded").unwrap().meta.description,
            "第一行 第二行结束"
        );
    }

    #[test]
    fn duplicate_user_skill_names_keep_the_first_directory() {
        let dir = TempDir::new("duplicate");
        dir.write_skill("a-first", &skill_md("dup", "First wins.", "# 第一个\n"));
        dir.write_skill("b-second", &skill_md("dup", "Second loses.", "# 第二个\n"));

        let catalog = SkillCatalog::load(dir.path());

        assert_eq!(names(&catalog).len(), 4);
        let skill = catalog.get("dup").expect("同名技能仍有一个在册");
        assert_eq!(skill.meta.description, "First wins.");
        assert_eq!(skill.body.trim(), "# 第一个");
        assert_eq!(catalog.warnings().len(), 1, "{:?}", catalog.warnings());
        assert!(
            catalog.warnings()[0].contains("b-second"),
            "{:?}",
            catalog.warnings()
        );
        assert!(
            catalog.warnings()[0].contains("dup"),
            "{:?}",
            catalog.warnings()
        );
    }

    #[test]
    fn user_dir_that_is_not_a_directory_warns_once() {
        let dir = TempDir::new("not-a-dir");
        let file = dir.path().join("skills-file");
        fs::write(&file, "not a directory\n").unwrap();

        let catalog = SkillCatalog::load(&file);

        assert_eq!(names(&catalog).len(), 3);
        assert_eq!(catalog.warnings().len(), 1, "{:?}", catalog.warnings());
        assert!(
            catalog.warnings()[0].starts_with(&file.display().to_string()),
            "{:?}",
            catalog.warnings()
        );
    }

    /// 项目目录先加载：同名技能覆盖全局 `~/.ignis/skills` 里的那一份（被覆盖者留警告）。
    #[test]
    fn load_all_merges_project_and_home_with_project_precedence() {
        let home = TempDir::new("load-all-home");
        home.write_skill("home-only", &skill_md("home-only", "Home.", "home body"));
        home.write_skill("shared", &skill_md("shared", "Home shared.", "home body"));
        let project = TempDir::new("load-all-project");
        project.write_skill(
            "project-only",
            &skill_md("project-only", "Project.", "project body"),
        );
        project.write_skill(
            "shared",
            &skill_md("shared", "Project shared.", "project body"),
        );

        let catalog =
            SkillCatalog::load_all(&[project.path().to_path_buf(), home.path().to_path_buf()]);

        assert_eq!(
            catalog.get("shared").map(|skill| skill.body.trim()),
            Some("project body"),
            "项目级同名技能应覆盖全局技能"
        );
        assert!(catalog.get("project-only").is_some());
        assert!(catalog.get("home-only").is_some());
        assert!(
            catalog
                .warnings()
                .iter()
                .any(|warning| warning.contains("shared")),
            "被覆盖的全局同名技能应有警告：{:?}",
            catalog.warnings()
        );
    }

    /// 技能目录本身可以是符号链接：不能只看 `DirEntry::file_type`（它不跟随链接）。
    #[cfg(unix)]
    #[test]
    fn symlinked_skill_directories_are_followed() {
        let user = TempDir::new("symlink-user");
        let real = TempDir::new("symlink-real");
        real.write_skill("linked", &skill_md("linked", "Linked.", "linked body"));
        std::os::unix::fs::symlink(real.path().join("linked"), user.path().join("linked")).unwrap();

        let catalog = SkillCatalog::load(user.path());

        assert_eq!(
            catalog.get("linked").map(|skill| skill.body.trim()),
            Some("linked body")
        );
        assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
    }

    /// 悬空链接要留下可诊断的警告，而不是静默消失。
    #[cfg(unix)]
    #[test]
    fn broken_skill_symlink_warns_and_is_skipped() {
        let user = TempDir::new("broken-link-user");
        let missing = user.path().join("missing-target");
        std::os::unix::fs::symlink(&missing, user.path().join("broken")).unwrap();

        let catalog = SkillCatalog::load(user.path());

        assert!(catalog.get("broken").is_none());
        assert!(
            catalog
                .warnings()
                .iter()
                .any(|warning| warning.contains("符号链接目标不存在")),
            "{:?}",
            catalog.warnings()
        );
    }
}
