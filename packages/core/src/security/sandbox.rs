//! 路径沙箱：把用户给的路径约束在工作区内，防住词法穿越与符号链接逃逸。

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// 路径被拒绝的原因；错误信息只带用户路径与工作区路径，不泄露工作区外的文件内容。
#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    /// 词法折叠或符号链接解析后落在工作区之外。
    #[error("路径越出工作区：{user_path}（工作区：{}）", .workspace.display())]
    OutsideWorkspace {
        user_path: String,
        workspace: PathBuf,
    },

    /// 工作区本身不存在或无法规范化，此时任何路径都不可信。
    #[error("工作区不可用：{}：{source}", .workspace.display())]
    WorkspaceUnavailable {
        workspace: PathBuf,
        #[source]
        source: io::Error,
    },

    /// 路径无法规范化（悬空符号链接、链接成环、权限不足等）：fail-closed。
    #[error("路径无法解析：{user_path}（工作区：{}）：{source}", .workspace.display())]
    Unresolvable {
        user_path: String,
        workspace: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// 把 `user_path` 解析成工作区内的绝对路径；越界或无法验证时返回 [`SecurityError`]。
/// 工作区本身必须已存在，否则返回 [`SecurityError::WorkspaceUnavailable`]。
///
/// 两段式校验：先做词法规范化（折叠 `.`/`..`，绝对路径须落在工作区拼写内；中间目录不
/// 允许不存在也无妨），再对「最深的已存在祖先」做 `canonicalize` 并比对工作区的规范路径
/// ——符号链接的父目录与链接目标本身都被覆盖。已存在的目标返回其规范路径，不存在的目标
/// 返回「规范祖先 + 剩余后缀」，调用方可以直接据此建目录或写文件。
pub fn resolve_in_workspace(workspace: &Path, user_path: &str) -> Result<PathBuf, SecurityError> {
    let root = workspace
        .canonicalize()
        .map_err(|source| SecurityError::WorkspaceUnavailable {
            workspace: workspace.to_path_buf(),
            source,
        })?;

    let normalized = lexical_normalize(workspace, &root, user_path)?;

    // 只对已存在的部分做 canonicalize：不存在的尾段拼回规范祖先，保证返回路径可用
    let (anchor, suffix) =
        split_existing(&normalized).map_err(|source| unresolvable(workspace, user_path, source))?;
    let anchor = anchor
        .canonicalize()
        .map_err(|source| unresolvable(workspace, user_path, source))?;
    let resolved = suffix
        .iter()
        .rev()
        .fold(anchor, |path, name| path.join(name));
    if !resolved.starts_with(&root) {
        return Err(outside(workspace, user_path));
    }
    Ok(resolved)
}

fn outside(workspace: &Path, user_path: &str) -> SecurityError {
    SecurityError::OutsideWorkspace {
        user_path: user_path.to_string(),
        workspace: workspace.to_path_buf(),
    }
}

fn unresolvable(workspace: &Path, user_path: &str, source: io::Error) -> SecurityError {
    SecurityError::Unresolvable {
        user_path: user_path.to_string(),
        workspace: workspace.to_path_buf(),
        source,
    }
}

/// 词法规范化：折叠 `.` 与 `..`；`..` 越过工作区根、或绝对路径落在工作区（原始拼写与
/// 规范拼写都算）之外时立即拒绝——这一步不要求中间目录存在。
fn lexical_normalize(
    workspace: &Path,
    root: &Path,
    user_path: &str,
) -> Result<PathBuf, SecurityError> {
    let raw = Path::new(user_path);
    if raw.is_absolute() {
        // 按组件折叠后再判归属：`/ws/../etc` 这类写法折叠后落在工作区外，必须拒绝
        let folded = fold_absolute(raw);
        if folded.starts_with(workspace) || folded.starts_with(root) {
            return Ok(folded);
        }
        return Err(outside(workspace, user_path));
    }

    let segments = fold_relative(raw).ok_or_else(|| outside(workspace, user_path))?;
    let mut path = workspace.to_path_buf();
    for segment in segments {
        path.push(segment);
    }
    Ok(path)
}

/// 折叠相对路径的组件；`..` 越过工作区根（栈已空）时返回 `None`。
fn fold_relative(raw: &Path) -> Option<Vec<&OsStr>> {
    let mut segments: Vec<&OsStr> = Vec::new();
    for component in raw.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                segments.pop()?;
            }
            Component::Normal(segment) => segments.push(segment),
            // 相对路径不会给出根/前缀；万一给出（含 `..` 越顶后的平台差异）一律拒绝
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(segments)
}

/// 折叠绝对路径的组件；`..` 越过根时按 POSIX 语义停在根（后续归属判定仍会拒绝）。
fn fold_absolute(raw: &Path) -> PathBuf {
    let mut folded = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Prefix(prefix) => folded.push(prefix.as_os_str()),
            Component::RootDir => folded.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                folded.pop();
            }
            Component::Normal(segment) => folded.push(segment),
        }
    }
    folded
}

/// 找出「最深的已存在祖先」与其后的剩余组件，用于只 canonicalize 存在的那部分。
fn split_existing(normalized: &Path) -> io::Result<(&Path, Vec<&OsStr>)> {
    let mut current = normalized;
    let mut suffix: Vec<&OsStr> = Vec::new();
    loop {
        if lexists(current)? {
            return Ok((current, suffix));
        }
        let (Some(name), Some(parent)) = (current.file_name(), current.parent()) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "路径没有可用的祖先组件",
            ));
        };
        suffix.push(name);
        current = parent;
    }
}

/// 路径本身是否存在：不跟随符号链接，悬空链接也算存在（必须交给 canonicalize 判定）。
fn lexists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// 单个测试独占的临时目录，析构时清理（本 crate 不引入 tempfile 依赖）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-core-sandbox-{}-{}",
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

    #[test]
    fn sandbox_blocks_escapes() {
        let ws = TempDir::new();
        std::fs::create_dir(ws.path().join("src")).unwrap();
        std::fs::write(ws.path().join("src/a.txt"), "x").unwrap();
        std::os::unix::fs::symlink("/etc", ws.path().join("link")).unwrap();
        assert!(resolve_in_workspace(ws.path(), "src/a.txt").is_ok());
        assert!(resolve_in_workspace(ws.path(), "new/child.txt").is_ok()); // 父目录允许不存在
        for bad in ["../outside", "/etc/passwd", "link/hosts", "a/../../x"] {
            assert!(resolve_in_workspace(ws.path(), bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sandbox_returns_canonical_paths() {
        let ws = TempDir::new();
        std::fs::create_dir(ws.path().join("src")).unwrap();
        std::fs::write(ws.path().join("src/a.txt"), "x").unwrap();
        let root = ws.path().canonicalize().unwrap();

        // 已存在：返回规范路径
        assert_eq!(
            resolve_in_workspace(ws.path(), "src/a.txt").unwrap(),
            root.join("src/a.txt")
        );
        // 不存在：规范祖先 + 剩余后缀，调用方无需再拼
        assert_eq!(
            resolve_in_workspace(ws.path(), "new/child.txt").unwrap(),
            root.join("new/child.txt")
        );
        // `.`/`..` 在词法阶段折叠
        assert_eq!(
            resolve_in_workspace(ws.path(), "./src/../src/a.txt").unwrap(),
            root.join("src/a.txt")
        );
        // 工作区自身
        assert_eq!(resolve_in_workspace(ws.path(), ".").unwrap(), root);
    }

    #[test]
    fn sandbox_rejects_paths_outside_even_with_shared_prefix() {
        let ws = TempDir::new();
        let root = ws.path().display().to_string();

        // 仅字符串前缀相同（组件不同）的路径不算工作区内
        let lookalike = format!("{root}-sibling/a.txt");
        assert!(
            resolve_in_workspace(ws.path(), &lookalike).is_err(),
            "{lookalike}"
        );
        // 语法上在工作区内，折叠 `..` 后越界的绝对路径
        let escaping = format!("{root}/../outside.txt");
        assert!(
            resolve_in_workspace(ws.path(), &escaping).is_err(),
            "{escaping}"
        );
        // 真实存在但在工作区外的绝对路径
        let outside = std::env::temp_dir().join("ignis-outside.txt");
        let outside = outside.to_str().unwrap();
        assert!(
            resolve_in_workspace(ws.path(), outside).is_err(),
            "{outside}"
        );
    }

    #[test]
    fn sandbox_rejects_symlinked_targets() {
        let ws = TempDir::new();
        // 文件级符号链接指向工作区外
        std::os::unix::fs::symlink("/etc/hosts", ws.path().join("hosts.txt")).unwrap();
        assert!(resolve_in_workspace(ws.path(), "hosts.txt").is_err());
        // 经指向工作区外的符号链接新建子文件
        std::os::unix::fs::symlink("/etc", ws.path().join("etc-link")).unwrap();
        assert!(resolve_in_workspace(ws.path(), "etc-link/ignis-nonexistent").is_err());
        // 悬空符号链接无法规范化 → fail-closed
        std::os::unix::fs::symlink("/ignis-nonexistent-target", ws.path().join("dangling"))
            .unwrap();
        assert!(resolve_in_workspace(ws.path(), "dangling").is_err());
        assert!(resolve_in_workspace(ws.path(), "dangling/child").is_err());
    }

    #[test]
    fn sandbox_reports_unusable_workspace_and_offending_paths() {
        let ws = TempDir::new();
        // 工作区不存在时任何路径都不可信
        let missing = ws.path().join("not-created");
        assert!(resolve_in_workspace(&missing, "a.txt").is_err());

        // 错误信息带用户路径与工作区路径
        let err = resolve_in_workspace(ws.path(), "../outside").unwrap_err();
        let msg = err.to_string();
        let workspace = ws.path().display().to_string();
        assert!(msg.contains("../outside"), "err: {msg}");
        assert!(msg.contains(&workspace), "err: {msg}");
    }
}
