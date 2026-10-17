//! Demo B 端到端：真二进制 + 脚本化假模型，跨进程验证「记住 → 复用 → 纠正 → 纠正生效」，
//! 外加技能发现与执行（验收 #5：两个技能都要被真的发现与执行）。
//!
//! 六个段落各自起一次 `ignis run` 进程，共享同一个 `IGNIS_HOME` 与工作区副本——记忆要跨进程
//! （重启）才算记住。段与段之间的断言全部落在真库、真 JSONL 与假模型实际收到的请求上。
//! 夹具是 `examples/uv-project`；uv 与 pytest **不必真装**，这里只断言「选了 uv 工作流」。

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{TempDir, copy_dir, ignis_cmd, latest_task, output_text, read_facts, tool_call};
use ignis_runtime::test_support::{ScriptStep, ScriptedModel, ScriptedModelHandle};
use serde_json::{Value, json};

/// 夹具项目 `examples/uv-project` 的绝对路径。
fn uv_fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/uv-project")
}

/// 六个段落的 prompt：逐字照抄 brief（第六段是验收 #5 要求的第二个技能）。
const PROMPT_REMEMBER: &str = "这个项目使用 uv 管理 Python 环境，请记住";
const PROMPT_REUSE: &str = "帮我检查这个项目的依赖并运行测试";
const PROMPT_CORRECT: &str = "不对，这个项目改用 pip 了";
const PROMPT_PLAIN: &str = "随便跑个任务";
const PROMPT_SKILLS: &str = "检查一下服务器健康状况";
const PROMPT_ANALYZE: &str = "分析一下这个项目";

/// 事实的位置：两次写入同一个 key 就构成「纠正」。
const FACT_KEY: &str = "pkg_manager";
/// 这个 key 的两版文本：先 uv，后 pip。
const UV_TEXT: &str = "使用 uv 管理 Python 环境";
const PIP_TEXT: &str = "使用 pip 管理 Python 环境";

/// 内置技能的目录名（`skills/<name>/SKILL.md`）：验收 #5 要求这两个都被真的发现与执行。
const SERVER_HEALTH_SKILL: &str = "server-health";
const PROJECT_ANALYZER_SKILL: &str = "project-analyzer";
/// `server-health` 正文里的一句（`skills/server-health/SKILL.md`）。
///
/// 只可能经 `load_skill` 的工具结果进入请求：系统提示里的技能目录只有名称与描述（渐进披露），
/// 目录与系统提示都不含这句正文。
const SKILL_BODY_MARKER: &str = "任何一步失败都记录原因后继续";
/// `project-analyzer` 正文里的一句（`skills/project-analyzer/SKILL.md`），口径同上：
/// 目录注入只带名称与描述（`description` 是英文那句），正文只可能经 `load_skill` 到达模型。
const ANALYZER_BODY_MARKER: &str = "先拿证据再下结论";

/// 跑一次 `ignis run <prompt> --yes`（独立进程），返回输出；退出码与内容由调用方断言。
fn run_ignis(home: &Path, project: &Path, model: &ScriptedModelHandle, prompt: &str) -> Output {
    ignis_cmd(home, project, &model.base_url())
        .args(["run", prompt, "--yes"])
        .output()
        .expect("ignis 二进制应可执行")
}

/// 一次请求里的 system 消息正文（`messages[0]`，`role` 恒为 `system`）。
///
/// 事实与技能目录的注入断言都从这里读：它就是模型真正看到的那份文本。
fn system_message(request: &Value) -> &str {
    let messages = request["messages"]
        .as_array()
        .expect("请求体里的 messages 应为数组");
    let first = messages.first().expect("请求至少带一条 system 消息");
    assert_eq!(first["role"], "system", "{request:#?}");
    first["content"].as_str().expect("system 消息应为文本")
}

/// stdout 里的结束行：渲染的文本增量不补换行，结束行可能紧跟最终答复在同一行，所以按
/// `contains` 找行；调用方再钉它的行尾（技能标注就挂在行尾）。
fn finished_line(stdout: &str) -> Option<&str> {
    stdout.lines().find(|line| line.contains("● Done"))
}

/// 任务日志（`IGNIS_HOME/logs/<task_id>.jsonl`）的原文；断言失败时贴进消息里便于定位。
fn task_log(home: &Path, task_id: &str) -> String {
    fs::read_to_string(home.join("logs").join(format!("{task_id}.jsonl"))).expect("任务日志应存在")
}

/// 日志里的 `tool_exec` 序列：`(name, args)`，按发生顺序——钉住模型实际调用了什么。
fn tool_exec_sequence(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("日志行应是 JSON"))
        .filter(|line| line["type"] == "tool_exec")
        .map(|line| {
            (
                line["name"].as_str().unwrap_or_default().to_owned(),
                line["args"].to_string(),
            )
        })
        .collect()
}

#[test]
fn demo_b_remembers_reuses_and_corrects_across_processes() {
    let dir = TempDir::new("demo-b");
    let home = dir.join("home");
    let project = dir.join("project");
    copy_dir(&uv_fixture_dir(), &project);
    let db = home.join("ignis.db");
    // 事实里存的 workspace 与进程收到的路径逐字一致（原样入库）；镜像文件名由它派生
    let workspace = project.display().to_string();

    // ---- 1. 记住：模型调用 remember 把「这个项目用 uv」写进 project 记忆 ----
    let model = ScriptedModel::start(vec![
        tool_call(
            "remember",
            json!({"scope": "project", "key": FACT_KEY, "text": UV_TEXT}),
        ),
        ScriptStep::Text("已记住：这个项目用 uv 管理 Python 环境。".into()),
    ]);
    let output = run_ignis(&home, &project, &model, PROMPT_REMEMBER);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert_eq!(model.requests().len(), 2, "remember 一条事实应两次请求");

    // 库里恰好一行 active，文本就是模型写的那句
    let facts = read_facts(&db, FACT_KEY);
    assert_eq!(facts.len(), 1, "该 key 应恰好一行：{facts:?}");
    assert_eq!(
        (facts[0].1.as_str(), facts[0].2, facts[0].3.as_str()),
        ("active", None, UV_TEXT)
    );
    // `read_facts` 的口径不带 scope/workspace 两列：单独直读一次，确认这条是绑定在工作区
    // 副本上的 project 事实（不是无主的 user/global 事实）
    let (stored_scope, stored_workspace): (String, Option<String>) =
        rusqlite::Connection::open(&db)
            .expect("库文件应可直读")
            .query_row(
                "SELECT scope, workspace FROM facts WHERE id = ?1",
                [facts[0].0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("本次运行应写出一条事实行");
    assert_eq!(
        (stored_scope.as_str(), stored_workspace.as_deref()),
        ("project", Some(workspace.as_str()))
    );

    // 人类可读的镜像：文件名由工作区派生（`/` → `_`），标题与正文都落上
    let mirror = home
        .join("memory")
        .join(format!("project-{}.md", workspace.replace('/', "_")));
    let content = fs::read_to_string(&mirror).expect("记忆镜像应写入 IGNIS_HOME/memory");
    assert!(
        content.starts_with(&format!("# 项目记忆：{workspace}\n")),
        "{content}"
    );
    assert!(content.contains(UV_TEXT), "{content}");

    // ---- 2. 复用：新进程的 prompt 与 uv 无关，事实照样无条件进系统提示 ----
    let model = ScriptedModel::start(vec![
        tool_call("shell_run", json!({"command": "uv sync"})),
        ScriptStep::Text("已按项目约定用 uv 检查依赖并运行测试。".into()),
    ]);
    let output = run_ignis(&home, &project, &model, PROMPT_REUSE);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    // 无条件注入：prompt 里没有 uv / Python / 环境 这些词，project 事实仍排在已知事实段
    assert!(
        system_message(&requests[0]).contains(UV_TEXT),
        "{:#?}",
        requests[0]["messages"]
    );

    // 工作流选择落在任务日志里（uv 没装时 `uv sync` 真执行会失败，断言只看选了什么）
    let (task_id, status, error) = latest_task(&db);
    assert_eq!((status.as_str(), error.as_str()), ("success", ""), "{text}");
    let raw = fs::read_to_string(home.join("logs").join(format!("{task_id}.jsonl")))
        .expect("任务日志应存在");
    assert!(raw.contains("uv sync"), "{raw}");

    // ---- 3. 纠正：同一个 key 写新值，旧行标 superseded 并指向新行 ----
    let model = ScriptedModel::start(vec![
        tool_call(
            "remember",
            json!({"scope": "project", "key": FACT_KEY, "text": PIP_TEXT}),
        ),
        ScriptStep::Text("已更新：这个项目改用 pip。".into()),
    ]);
    let output = run_ignis(&home, &project, &model, PROMPT_CORRECT);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");

    let facts = read_facts(&db, FACT_KEY);
    assert_eq!(facts.len(), 2, "两次写入应留两行（一旧一新）：{facts:?}");
    let (old, new) = (&facts[0], &facts[1]);
    assert_eq!(
        (old.1.as_str(), old.2, old.3.as_str()),
        ("superseded", Some(new.0), UV_TEXT)
    );
    assert_eq!(
        (new.1.as_str(), new.2, new.3.as_str()),
        ("active", None, PIP_TEXT)
    );

    // ---- 4. 纠正生效：新进程里只注入新文本，旧文本不再出现 ----
    let model = ScriptedModel::start(vec![ScriptStep::Text("好的。".into())]);
    let output = run_ignis(&home, &project, &model, PROMPT_PLAIN);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    let requests = model.requests();
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let system = system_message(&requests[0]);
    assert!(system.contains(PIP_TEXT), "{system}");
    assert!(
        !system.contains(UV_TEXT),
        "被取代的旧文本不得再注入：{system}"
    );

    // ---- 5. 技能发现与执行（验收 #5）：目录注入 → load_skill 读全文 → 按技能执行 ----
    let model = ScriptedModel::start(vec![
        tool_call("load_skill", json!({"name": SERVER_HEALTH_SKILL})),
        tool_call("shell_run", json!({"command": "df -h"})),
        ScriptStep::Text("磁盘检查完成：各挂载点使用率正常。".into()),
    ]);
    let output = run_ignis(&home, &project, &model, PROMPT_SKILLS);
    let text = output_text(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{text}");
    let requests = model.requests();
    assert_eq!(requests.len(), 3, "{requests:#?}");

    // 发现：第一份系统提示列出技能目录（内置三个，这里钉两个名字）
    let discovery = system_message(&requests[0]);
    for name in [PROJECT_ANALYZER_SKILL, SERVER_HEALTH_SKILL] {
        assert!(discovery.contains(&format!("- {name}: ")), "{discovery}");
    }
    // 执行：正文只经 `load_skill` 的工具结果进入请求——系统提示里没有它（渐进披露），
    // 第二次请求的 tool 消息里有，说明模型真的读到了 SKILL.md
    assert!(!discovery.contains(SKILL_BODY_MARKER), "{discovery}");
    let messages = requests[1]["messages"]
        .as_array()
        .expect("请求体里的 messages 应为数组");
    assert!(
        messages.iter().any(|message| {
            message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(SKILL_BODY_MARKER))
        }),
        "第二次请求应带回 server-health 的 SKILL.md 正文：{messages:#?}"
    );

    // 端行按首次加载顺序标注用过的技能：`skills_used` 恰好是 [server-health]（行尾没有第二项），
    // 任务与 JSONL 都要对得上。
    let end_line = finished_line(&stdout).unwrap_or_else(|| panic!("结束行缺失：{text}"));
    assert!(
        end_line.ends_with(&format!("· skills: {SERVER_HEALTH_SKILL}")),
        "{end_line}"
    );
    let (task_id, status, error) = latest_task(&db);
    assert_eq!((status.as_str(), error.as_str()), ("success", ""), "{text}");
    let raw = task_log(&home, &task_id);
    assert_eq!(
        tool_exec_sequence(&raw),
        [
            ("load_skill", json!({"name": SERVER_HEALTH_SKILL})),
            ("shell_run", json!({"command": "df -h"})),
        ]
        .map(|(name, args)| (name.to_owned(), args.to_string())),
        "{raw}"
    );

    // ---- 6. 第二个技能（验收 #5）：project-analyzer 也要被真的发现、选择和执行 ----
    // 段 5 只跑通了 `server-health`，`project-analyzer` 此前只出现在目录注入的断言里；
    // 验收清单要求「至少 2 个 Skill 能被正确发现、选择和执行」，所以再起一个进程跑完整路径：
    // 目录发现 → `load_skill` 读全文 → 按技能的第一步跑 `shell_run` → 端行标注。
    let model = ScriptedModel::start(vec![
        tool_call("load_skill", json!({"name": PROJECT_ANALYZER_SKILL})),
        tool_call("shell_run", json!({"command": "ls -la"})),
        ScriptStep::Text("项目分析完成：顶层结构如上。".into()),
    ]);
    let output = run_ignis(&home, &project, &model, PROMPT_ANALYZE);
    let text = output_text(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{text}");
    let requests = model.requests();
    assert_eq!(requests.len(), 3, "{requests:#?}");

    // 发现：目录（名称 + 描述）在第一份系统提示里，正文不在——与段 5 同一套渐进披露
    // （段 4 留下的 pip 事实也在这份系统提示里：同一工作区、同一份记忆）
    let discovery = system_message(&requests[0]);
    assert!(
        discovery.contains(&format!("- {PROJECT_ANALYZER_SKILL}: ")),
        "{discovery}"
    );
    assert!(!discovery.contains(ANALYZER_BODY_MARKER), "{discovery}");
    // 执行：第二次请求的 tool 消息带回 SKILL.md 正文，说明模型真的读到了
    let messages = requests[1]["messages"]
        .as_array()
        .expect("请求体里的 messages 应为数组");
    assert!(
        messages.iter().any(|message| {
            message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(ANALYZER_BODY_MARKER))
        }),
        "第二次请求应带回 project-analyzer 的 SKILL.md 正文：{messages:#?}"
    );

    // 端行只标注这一次真正加载的技能（行尾没有第二项）
    let end_line = finished_line(&stdout).unwrap_or_else(|| panic!("结束行缺失：{text}"));
    assert!(
        end_line.ends_with(&format!("· skills: {PROJECT_ANALYZER_SKILL}")),
        "{end_line}"
    );
    // JSONL 钉住真落盘的调用序列：先读技能，再按技能的第一步动手
    let (task_id, status, error) = latest_task(&db);
    assert_eq!((status.as_str(), error.as_str()), ("success", ""), "{text}");
    let raw = task_log(&home, &task_id);
    assert_eq!(
        tool_exec_sequence(&raw),
        [
            ("load_skill", json!({"name": PROJECT_ANALYZER_SKILL})),
            ("shell_run", json!({"command": "ls -la"})),
        ]
        .map(|(name, args)| (name.to_owned(), args.to_string())),
        "{raw}"
    );
}
