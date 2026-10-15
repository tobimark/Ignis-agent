//! Markdown 镜像：把事实渲染成 `{mirror_dir}/*.md`，供人直接阅读。
//!
//! 镜像是纯派生物：内容永远由 `facts` 表决定，`superseded` 行绝不出现。写失败由调用方记一行
//! 警告（fail-soft），所以这里只管「长什么样、怎么写」，「什么时候写」在 store。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use ignis_storage::FactRecord;

use super::FactScope;

/// 只有 active 行会出现在镜像里。
const STATUS_ACTIVE: &str = "active";

/// 事实行的固定格式：`- [scope/key] text（更新：RFC3339）`。
fn fact_line(fact: &FactRecord) -> String {
    format!(
        "- [{}/{}] {}（更新：{}）",
        fact.scope,
        fact.key,
        fact.text,
        rfc3339(fact.updated_at)
    )
}

/// epoch 毫秒 → RFC3339（UTC、秒精度）；超出 chrono 表示范围时退回毫秒整数，不 panic。
fn rfc3339(millis: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(millis)
        .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| millis.to_string())
}

/// 一个作用域对应的镜像文件：`user.md` / `global.md` / `project-{工作区}.md`。
///
/// 工作区绝对路径里的 `/` 换成 `_` 才能当文件名（`/home/dev/app` →
/// `project-_home_dev_app.md`）；`project` 没有工作区时无名可起，返回 `None`（调用方跳过）。
pub(crate) fn mirror_path(
    mirror_dir: &Path,
    scope: FactScope,
    workspace: Option<&str>,
) -> Option<PathBuf> {
    match scope {
        FactScope::User => Some(mirror_dir.join("user.md")),
        FactScope::Global => Some(mirror_dir.join("global.md")),
        FactScope::Project => {
            workspace.map(|ws| mirror_dir.join(format!("project-{}.md", ws.replace('/', "_"))))
        }
    }
}

/// 渲染一个镜像文件的全文：`project` 首行是标题，随后每条 active 事实一行（顺序由调用方给，
/// 新近在前）。
///
/// 空作用域渲染成空串（`project` 只剩标题行）——文件照写不误，好让被删掉的事实从镜像里消失。
pub(crate) fn render(scope: FactScope, workspace: Option<&str>, facts: &[FactRecord]) -> String {
    let mut out = String::new();
    if scope == FactScope::Project {
        // 标题用工作区原样路径；文件名里的 `_` 只是为文件系统的妥协
        out.push_str(&format!("# 项目记忆：{}\n", workspace.unwrap_or_default()));
    }
    for fact in facts.iter().filter(|fact| fact.status == STATUS_ACTIVE) {
        out.push_str(&fact_line(fact));
        out.push('\n');
    }
    out
}

/// 覆盖写入镜像文件；父目录不存在则先建。
pub(crate) fn write(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_private(path, content.as_bytes())
}

/// 以 0600 写入：`mode` 只在新建时生效，已存在的旧文件再收紧一次（umask 只收窄不放宽）。
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(data)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ignis_storage::FactRecord;

    use super::{mirror_path, render};
    use crate::memory::FactScope;

    /// `1_700_000_000_000` ms = `2023-11-14T22:13:20Z`，用来钉住 RFC3339 期望值。
    fn fact(id: i64, scope: &str, key: &str, text: &str, status: &str) -> FactRecord {
        FactRecord {
            id,
            scope: scope.to_owned(),
            workspace: None,
            key: key.to_owned(),
            text: text.to_owned(),
            status: status.to_owned(),
            superseded_by: None,
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn mirror_path_uses_scope_names_and_sanitizes_workspace_paths() {
        let dir = Path::new("/home/u/.ignis/memory");

        assert_eq!(
            mirror_path(dir, FactScope::User, None),
            Some(dir.join("user.md"))
        );
        assert_eq!(
            mirror_path(dir, FactScope::Global, None),
            Some(dir.join("global.md"))
        );
        // 工作区绝对路径里的 `/` 换成 `_` 才能当文件名
        assert_eq!(
            mirror_path(dir, FactScope::Project, Some("/home/dev/app")),
            Some(dir.join("project-_home_dev_app.md"))
        );
        // project 没有工作区时无名可起
        assert_eq!(mirror_path(dir, FactScope::Project, None), None);
        // user/global 的文件名固定，偶发绑定值不影响选择
        assert_eq!(
            mirror_path(dir, FactScope::User, Some("/w")),
            Some(dir.join("user.md"))
        );
    }

    #[test]
    fn render_skips_superseded_rows_and_prefixes_the_project_title() {
        let facts = vec![
            fact(1, "user", "editor", "偏好 Neovim", "active"),
            fact(2, "user", "old", "被取代的旧偏好", "superseded"),
        ];

        assert_eq!(
            render(FactScope::User, None, &facts),
            "- [user/editor] 偏好 Neovim（更新：2023-11-14T22:13:20Z）\n"
        );
        // 空作用域渲染成空串：文件照写，旧内容随之消失
        assert_eq!(render(FactScope::User, None, &[]), "");

        let project = vec![fact(3, "project", "pkg_manager", "使用 uv", "active")];
        assert_eq!(
            render(FactScope::Project, Some("/w/a"), &project),
            "# 项目记忆：/w/a\n- [project/pkg_manager] 使用 uv（更新：2023-11-14T22:13:20Z）\n"
        );
        // 没有事实时也留标题行
        assert_eq!(
            render(FactScope::Project, Some("/w/a"), &[]),
            "# 项目记忆：/w/a\n"
        );
    }
}
