//! `ignis skills list`：打印合并后的技能目录（内置三技能 + `~/.ignis/skills/`）。

use std::process::ExitCode;

use anyhow::Result;
use ignis_core::config::ignis_home;
use ignis_core::skills::{SkillCatalog, SkillSource};

use super::task::render_table;
use crate::cli::SkillsCommand;

/// `ignis skills`：v0.1 只有 `list`。
pub fn run(command: &SkillsCommand) -> Result<ExitCode> {
    match command {
        SkillsCommand::List => {
            // 技能目录不存在不算错误（`SkillCatalog::load` 只回内置三技能）
            let catalog = SkillCatalog::load(&ignis_home().join("skills"));
            print!("{}", render(&catalog));
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// 技能表：每行 `{name}  {source}  {description}`；加载警告按 `! {msg}` 跟在表后。
fn render(catalog: &SkillCatalog) -> String {
    let mut rows = vec![vec![
        "NAME".to_string(),
        "SOURCE".to_string(),
        "DESCRIPTION".to_string(),
    ]];
    for meta in catalog.metas() {
        rows.push(vec![
            meta.name.clone(),
            source_name(meta.source).to_string(),
            meta.description.clone(),
        ]);
    }

    let mut out = render_table(&rows);
    for warning in catalog.warnings() {
        out.push_str(&format!("! {warning}\n"));
    }
    out
}

/// 技能来源的字面量：`builtin` / `user`。
fn source_name(source: SkillSource) -> &'static str {
    match source {
        SkillSource::Builtin => "builtin",
        SkillSource::User => "user",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ignis_core::skills::SkillCatalog;

    use super::*;

    /// 单个测试独占的临时技能目录，析构时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-cli-skills-{}-{label}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
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

    /// 一行拆成单元格：表格用两个空格分隔，列宽不足处补空格。
    fn cells(line: &str) -> Vec<&str> {
        line.split("  ")
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect()
    }

    #[test]
    fn rows_carry_name_source_and_description() {
        let dir = TempDir::new("rows");
        dir.write_skill(
            "zzz",
            "---\nname: zzz-skill\ndescription: Sorts last.\n---\n# 正文\n",
        );

        let catalog = SkillCatalog::load(dir.path());
        let out = render(&catalog);

        // 表头 + 每个技能一行；内置三技能 + 用户技能，行的顺序即目录顺序（按名称排序）
        assert_eq!(out.lines().count(), catalog.metas().len() + 1, "{out}");
        assert_eq!(
            cells(out.lines().next().unwrap()),
            ["NAME", "SOURCE", "DESCRIPTION"],
            "{out}"
        );
        let row = |name: &str| {
            out.lines()
                .find(|line| line.starts_with(name))
                .unwrap_or_else(|| panic!("{name} 行缺失：{out}"))
        };
        // 来源分别打成 builtin / user
        assert_eq!(cells(row("project-analyzer"))[1], "builtin");
        assert_eq!(
            cells(row("zzz-skill")),
            ["zzz-skill", "user", "Sorts last."],
            "{out}"
        );
        assert_eq!(cells(row("server-health"))[0], "server-health");
        // 用户技能排在内置之后（ID 列就是目录顺序）
        let names: Vec<String> = out.lines().map(|line| cells(line)[0].to_string()).collect();
        let expected: Vec<String> = ["NAME"]
            .into_iter()
            .map(str::to_string)
            .chain(catalog.metas().iter().map(|meta| meta.name.clone()))
            .collect();
        assert_eq!(names, expected, "{out}");
    }

    #[test]
    fn warnings_follow_the_table_each_prefixed_with_a_bang() {
        let dir = TempDir::new("warnings");
        dir.write_skill("broken", "# 没有 frontmatter\n只有正文\n");

        let catalog = SkillCatalog::load(dir.path());
        assert_eq!(catalog.warnings().len(), 1, "{:?}", catalog.warnings());

        let out = render(&catalog);

        let lines: Vec<&str> = out.lines().collect();
        let table_rows = catalog.metas().len() + 1;
        assert_eq!(lines.len(), table_rows + catalog.warnings().len(), "{out}");
        assert!(
            !lines[..table_rows]
                .iter()
                .any(|line| line.starts_with("! ")),
            "警告只能在表后：{out}"
        );
        for (line, warning) in lines[table_rows..].iter().zip(catalog.warnings()) {
            assert_eq!(*line, format!("! {warning}"));
        }
    }
}
