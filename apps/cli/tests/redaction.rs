//! C1 回归：密钥来自 `api_key_env`（默认 `OPENAI_API_KEY`）时，也必须进脱敏器。
//!
//! 旧实现解析生效密钥时只把它交给了 HTTP 客户端：`TaskRunner` 与 `ToolRuntime` 的
//! `Redactor` 都从 `config.api_key` 取密钥，环境变量路径解析出的那一份没有回填。于是
//! 「文件里存着密钥、模型让工具读它」这条最普通的路径会把密钥明文写进 JSONL，也明文
//! 回填给模型。这里用「工作区文件内容 = 密钥」复现整条链路，并断言两处都没有明文。

mod common;

use std::fs;

use common::{TempDir, ignis_cmd_with_api_key_env, latest_task, output_text, tool_call};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel};
use serde_json::json;

/// 只从 `OPENAI_API_KEY` 拿到的密钥；断言「任何落盘/回填的文本里都不许出现它」。
const KEY: &str = "sk-e2e-api-key-env-7f3c";

#[test]
fn api_key_env_key_never_reaches_the_jsonl_or_the_model() {
    let model = ScriptedModel::start(vec![
        tool_call("fs_read", json!({"path": "notes.txt"})),
        ScriptStep::Text("读完了。".into()),
    ]);

    let dir = TempDir::new("redaction");
    let home = dir.join("home");
    let project = dir.join("project");
    fs::create_dir_all(&project).expect("工作区应可创建");
    fs::write(project.join("notes.txt"), format!("OPENAI_API_KEY={KEY}\n")).expect("夹具应可写入");

    let output = ignis_cmd_with_api_key_env(&home, &project, &model.base_url(), KEY)
        .args(["run", "读一下 notes.txt"])
        .output()
        .expect("ignis 二进制应可执行");
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");

    // JSONL：工具结果里的密钥必须已被抹掉
    let db = home.join("ignis.db");
    let (task_id, status, _) = latest_task(&db);
    assert_eq!(status, "success", "{task_id}\n{text}");
    let raw = fs::read_to_string(home.join("logs").join(format!("{task_id}.jsonl")))
        .expect("任务日志应存在");
    assert!(!raw.contains(KEY), "JSONL 里出现了明文密钥：{raw}");
    assert!(raw.contains("***"), "JSONL 里应留下脱敏痕迹：{raw}");

    // 假模型收到的请求体：工具结果回填给模型时同样必须是脱敏后的文本
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    let recorded = serde_json::to_string(&requests).expect("请求体应可序列化");
    assert!(
        !recorded.contains(KEY),
        "请求体里出现了明文密钥：{recorded}"
    );
    assert!(
        recorded.contains("***"),
        "请求体里应有脱敏后的文本：{recorded}"
    );
}
