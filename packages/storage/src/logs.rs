//! JSONL 任务日志：每个任务一个 `{task_id}.jsonl`，每行一个 JSON 对象，只追加。
//!
//! 写入方（TaskRunner）负责在落盘前把整行过一遍脱敏；这里只保证「一行一条、追加、
//! 目录按需创建」的落盘契约，`ignis task show` 按行读取同一个文件。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::db::Result;

/// 任务日志目录上的日志句柄。
pub struct TaskLog {
    dir: PathBuf,
}

impl TaskLog {
    /// 打开日志目录，缺失时创建。
    pub fn new(dir: &Path) -> Result<TaskLog> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    /// 某个任务的日志文件路径：`{dir}/{task_id}.jsonl`。
    pub fn path(&self, task_id: &str) -> PathBuf {
        self.dir.join(format!("{task_id}.jsonl"))
    }

    /// 追加一行 JSON：一行一个对象，末尾补换行；文件不存在则创建。
    pub fn append(&self, task_id: &str, record: &serde_json::Value) -> Result<()> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(task_id))?;
        file.write_all(line.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;

    /// 单个测试独占的临时目录，析构时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ignis-storage-logs-{}-{}",
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

    #[test]
    fn new_creates_dir_and_path_follows_task_id() {
        let base = TempDir::new();
        let dir = base.path().join("not-created");

        let log = TaskLog::new(&dir).unwrap();

        assert!(dir.is_dir());
        assert_eq!(log.path("01"), dir.join("01.jsonl"));
    }

    #[test]
    fn append_writes_one_json_object_per_line_in_append_mode() {
        let base = TempDir::new();
        let log = TaskLog::new(&base.path().join("logs")).unwrap();

        log.append("01", &json!({"type": "task_start", "task_id": "01"}))
            .unwrap();
        log.append("01", &json!({"type": "done", "status": "success"}))
            .unwrap();
        log.append("02", &json!({"type": "task_start", "task_id": "02"}))
            .unwrap();

        let raw = fs::read_to_string(log.path("01")).unwrap();
        assert_eq!(
            raw,
            "{\"task_id\":\"01\",\"type\":\"task_start\"}\n{\"status\":\"success\",\"type\":\"done\"}\n"
        );
        // 任务之间互不干扰
        assert_eq!(
            fs::read_to_string(log.path("02")).unwrap(),
            "{\"task_id\":\"02\",\"type\":\"task_start\"}\n"
        );
    }
}
