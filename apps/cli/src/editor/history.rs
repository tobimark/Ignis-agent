//! 输入历史：`~/.ignis/history` 的读写与内存列表。
//!
//! 历史就是一份纯文本、一行一条的文件（行尾 `\n`），用户可以直接翻看、删掉不想留的条目；
//! 模块只做文件 I/O 与内存列表，不认识终端——录入、召回与上屏分别是读取层与状态机的事。
//!
//! 三条钉死的口径：
//!
//! - **fail-soft**：历史文件读不动、写不动都只是功能降级——载入得空列表、落盘失败打一行警告，
//!   内存里的条目照旧更新（历史丢了不该拦住用户输入）。
//! - **一行一条**：一次多行提交按行拆成多条（粘贴的代码因此能逐行召回）；空行与纯空白行不入史，
//!   与最新一条相同的输入跳过（相邻去重）。
//! - **上限一千条**：超过就丢最旧；未截断时往文件尾部追加一行，一旦发生截断就整表重写
//!   （把磁盘上多出来的旧条目一并清掉）。
//!
//! 数据目录本身（`~/.ignis`，0700）由会话启动时的 `ensure_home` 负责创建，这里只写文件，
//! 父目录不存在时按落盘失败处理（一行警告）。载入保持只读（读路径不写盘）：超限的文件在下一次
//! 有新条目落盘时被整表重写。落盘不做 `fsync`：历史是便利数据，为每次提交都等一次同步写盘
//! 不值——进程崩溃最多丢最近几条。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 历史保留的条数上限：内存与文件一致（超出的丢最旧）。
pub const HISTORY_LIMIT: usize = 1000;

/// 输入历史：新近在前的条目列表，外加落盘位置。
///
/// 没有路径的实例只活在内存里（[`History::in_memory`]）；其余实例的契约是「内存是真相，
/// 文件是尽力而为的副本」：任何 I/O 失败都不会改变内存里的条目。
pub struct History {
    /// 落盘位置；`None` = 纯内存（状态机单测用）。
    path: Option<PathBuf>,
    /// 条目，下标 0 是最新的一条。
    entries: Vec<String>,
}

impl History {
    /// 从 `path` 载入历史：空行丢掉，多于 [`HISTORY_LIMIT`] 条只留最新的那些。
    ///
    /// 文件缺失 → 空历史；其它读取失败 → 一行警告 + 空历史（fail-soft，从不报错）。
    pub fn load(path: PathBuf) -> Self {
        let entries = match read_entries(&path) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                eprintln!(
                    "warning: failed to read history file {}: {err}",
                    path.display()
                );
                Vec::new()
            }
        };
        Self {
            path: Some(path),
            entries,
        }
    }

    /// 纯内存历史（不落盘）：状态机单测用；生产路径一律走 [`History::load`]。
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: Vec::new(),
        }
    }

    /// 记下一次提交：按行拆开（空行跳过、与最新一条相同的跳过），超过上限丢最旧，
    /// 随后尽力把新条目落盘——未截断时追加，发生截断时整表重写。
    ///
    /// 落盘失败只打一行警告，内存条目照旧更新。
    pub fn push(&mut self, text: &str) {
        let mut added: Vec<&str> = Vec::new();
        let mut truncated = false;
        for line in text.split('\n') {
            // 写入端按 `\n` 分行；行尾的 `\r` 先归一化掉，否则 `lines()` 读回来时会吃掉它，
            // 落盘与内存就对不上了。
            let line = line.strip_suffix('\r').unwrap_or(line);
            let duplicate = self.entries.first().is_some_and(|newest| newest == line);
            if line.trim().is_empty() || duplicate {
                continue;
            }
            self.entries.insert(0, line.to_owned());
            if self.entries.len() > HISTORY_LIMIT {
                self.entries.truncate(HISTORY_LIMIT);
                truncated = true;
            }
            added.push(line);
        }
        if added.is_empty() {
            return;
        }
        if truncated {
            self.rewrite();
        } else {
            for line in added {
                self.write_line(line);
            }
        }
    }

    /// 按下标取条目：0 是最新的一条。
    pub fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }

    /// 条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否有任何条目。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 全部条目，新近在前。
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// 往文件尾部追加一行；失败只打一行警告。
    fn write_line(&self, line: &str) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        if let Err(err) = append_line(path, line) {
            eprintln!(
                "warning: failed to append to history file {}: {err}",
                path.display()
            );
        }
    }

    /// 整表重写（截断到上限后的落盘形态）；失败只打一行警告。
    fn rewrite(&self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        if let Err(err) = rewrite_all(path, &self.entries) {
            eprintln!(
                "warning: failed to rewrite history file {}: {err}",
                path.display()
            );
        }
    }
}

/// 读文件里的条目：空行丢掉，磁盘顺序（最旧在前）翻成新近在前，只留最新的 [`HISTORY_LIMIT`] 条。
fn read_entries(path: &Path) -> io::Result<Vec<String>> {
    let text = fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(HISTORY_LIMIT)
        .map(str::to_owned)
        .collect())
}

/// 以 0600 追加一行（文件不存在则新建）。
fn append_line(path: &Path, line: &str) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    tighten(&file)?;
    writeln!(file, "{line}")
}

/// 以 0600 整表重写（清空后逐条写，磁盘顺序 = 最旧在前）。
fn rewrite_all(path: &Path, entries: &[String]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    tighten(&file)?;
    for entry in entries.iter().rev() {
        writeln!(file, "{entry}")?;
    }
    Ok(())
}

/// 把文件权限收紧到 0600：`mode` 只管新建，umask 又只收窄不放宽，所以打开后显式再落一次。
#[cfg(unix)]
fn tighten(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

/// 非 unix 平台没有 POSIX 权限位：权限交给系统默认。
#[cfg(not(unix))]
fn tighten(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{HISTORY_LIMIT, History};

    /// 单个测试独占的临时目录，析构时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-cli-history-{}-{label}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// 历史文件路径：`<临时目录>/history`。
        fn history_path(&self) -> PathBuf {
            self.0.join("history")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 文件权限位（只取权限部分）。
    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// 把文件权限改成给定的位（验证落盘时会不会收紧）。
    #[cfg(unix)]
    fn set_mode(path: &Path, bits: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
    }

    /// `entries()` 的 `&str` 视图，省掉断言里的 `.to_owned()` 噪音。
    fn lines(history: &History) -> Vec<&str> {
        history.entries().iter().map(String::as_str).collect()
    }

    #[test]
    fn temp_history_round_trips_and_is_0600() {
        let dir = TempDir::new("round-trip");
        let path = dir.history_path();

        let mut history = History::load(path.clone());
        assert_eq!(history.len(), 0, "文件还不存在时应是空历史");
        history.push("第一行");
        history.push("第二行");

        // 内存：新近在前
        assert_eq!(history.len(), 2);
        assert_eq!(history.get(0), Some("第二行"));
        assert_eq!(history.get(1), Some("第一行"));
        assert_eq!(history.get(2), None);
        assert_eq!(lines(&history), ["第二行", "第一行"]);

        // 磁盘：一行一条、行尾 `\n`、最旧在前
        assert_eq!(fs::read_to_string(&path).unwrap(), "第一行\n第二行\n");

        // 重新载入：与内存里的顺序一致
        let reloaded = History::load(path.clone());
        assert_eq!(lines(&reloaded), ["第二行", "第一行"]);

        #[cfg(unix)]
        {
            assert_eq!(mode(&path), 0o600, "新建的历史文件应是 0600");
            // 宽权限的旧文件再落盘时收紧
            set_mode(&path, 0o644);
            history.push("第三行");
            assert_eq!(mode(&path), 0o600, "再次落盘应收紧到 0600");
        }
    }

    #[test]
    fn push_dedupes_adjacent_and_skips_blanks() {
        let mut history = History::in_memory();
        for line in ["a", "a", "  ", "b"] {
            history.push(line);
        }
        assert_eq!(lines(&history), ["b", "a"]);

        // 纯空白与「与最新一条相同」都跳过
        history.push("\t");
        history.push("b");
        assert_eq!(lines(&history), ["b", "a"]);

        // 去重只看最新一条：隔开的重复照记（历史是流水账）
        history.push("a");
        assert_eq!(lines(&history), ["a", "b", "a"]);
    }

    #[test]
    fn multiline_push_becomes_one_entry_per_line() {
        let dir = TempDir::new("multiline");
        let path = dir.history_path();
        let mut history = History::load(path.clone());

        history.push("第一行\n\n第二行\n");
        assert_eq!(lines(&history), ["第二行", "第一行"]);
        assert_eq!(fs::read_to_string(&path).unwrap(), "第一行\n第二行\n");
    }

    #[test]
    fn over_cap_truncates_to_the_newest_thousand() {
        let dir = TempDir::new("over-cap");
        let path = dir.history_path();
        // 预置 1005 行：只有最新的 1000 行该留下
        let preset: String = (1..=1005).map(|i| format!("line{i}\n")).collect();
        fs::write(&path, preset).unwrap();

        let mut history = History::load(path.clone());
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(lines(&history)[0], "line1005");
        assert_eq!(lines(&history)[HISTORY_LIMIT - 1], "line6");

        // 宽权限的旧文件在整表重写时收紧
        #[cfg(unix)]
        set_mode(&path, 0o644);

        // 再压一条：内存仍是一千条，文件被整表重写成与内存一致
        history.push("line1006");
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(lines(&history)[0], "line1006");
        assert_eq!(lines(&history)[HISTORY_LIMIT - 1], "line7");

        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600, "整表重写应收紧到 0600");

        let on_disk = fs::read_to_string(&path).unwrap();
        let mut from_file: Vec<&str> = on_disk.lines().collect();
        assert_eq!(from_file.len(), HISTORY_LIMIT, "文件也应被截断到上限");
        from_file.reverse(); // 磁盘最旧在前，内存新近在前
        assert_eq!(from_file, lines(&history), "文件内容应与内存条目一致");
        assert_eq!(from_file[0], "line1006");
        assert_eq!(from_file[HISTORY_LIMIT - 1], "line7");
    }

    #[test]
    fn load_is_read_only_and_an_over_cap_file_self_heals_on_the_next_push() {
        let dir = TempDir::new("read-only-load");
        let path = dir.history_path();
        let preset: String = (1..=1005).map(|i| format!("line{i}\n")).collect();
        fs::write(&path, &preset).unwrap();

        // 载入只读：内存截断到上限，文件原样不动（超限文件留到下一次有新条目时再收拾）
        let mut history = History::load(path.clone());
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(fs::read_to_string(&path).unwrap(), preset);

        // 相邻去重的重复输入什么都不写，文件还是原样
        history.push("line1005");
        assert_eq!(lines(&history)[0], "line1005");
        assert_eq!(fs::read_to_string(&path).unwrap(), preset);

        // 下一条新条目落盘时整表重写，文件自愈到与内存一致
        history.push("line1006");
        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.lines().count(), HISTORY_LIMIT);
        assert_eq!(on_disk.lines().last(), Some("line1006"));
    }

    #[test]
    fn unwritable_path_is_fail_soft() {
        let dir = TempDir::new("unwritable");
        // 父路径是普通文件：这个「目录」打不开，落盘必然失败
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let path = blocker.join("history");

        let mut history = History::load(path.clone());
        assert_eq!(history.len(), 0);
        history.push("仍然记着");

        // 内存条目照旧更新，落盘失败只打一行警告
        assert_eq!(lines(&history), ["仍然记着"]);
        assert!(!path.exists(), "落盘失败不该留下文件");
        assert!(blocker.is_file(), "阻塞路径不该被改动");
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = TempDir::new("missing");
        let path = dir.history_path();

        let history = History::load(path.clone());
        assert_eq!(history.len(), 0);
        assert!(history.entries().is_empty());
        assert_eq!(history.get(0), None);
        assert!(!path.exists(), "载入不该创建文件");
    }
}
