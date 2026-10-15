# Ignis v0.1 阶段一（P1）：基座与执行循环 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**目标：** 从空仓库到 `ignis run "<任务>"` 能在真实工作区里读文件、跑命令、经审批改文件并交付结果——即 Demo A 端到端可跑通（脚本化假模型 + 夹具）。

**架构：** Cargo workspace 四 crate：`ignis-runtime`（Runtime/ToolExecutor/ApprovalHandler trait、OpenAI 兼容流式客户端、native 执行循环）、`ignis-storage`（rusqlite bundled + 迁移 + 任务/审批记录 + JSONL 任务日志）、`ignis-core`（配置、安全策略、工具实现、TaskRunner、doctor）、`ignis-cli`（bin `ignis`：run/REPL/审批 UI/渲染/辅助命令）。依赖方向 `core → {runtime, storage}`，runtime/storage 为叶子，无循环依赖；runtime 定义 trait，core 实现并随 `RunRequest` 注入。

**技术栈：** Rust（edition 2024, stable）、tokio、reqwest（rustls-tls）、rusqlite（bundled）、clap v4、rustyline、serde/serde_json、async-trait、uuid v7、chrono、thiserror、regex、nix；测试用 axum（`test-support` feature）、assert_cmd、predicates、tempfile。

**设计文档（Spec）：** `docs/superpowers/specs/2026-10-15-ignis-v0.1-design.md`（与 `demo.md` 一并阅读；冲突以设计文档为准）。本计划只覆盖设计文档的第一个里程碑，后续阶段见文末。

## 全局约束

来自设计文档 §3/§4，适用于本计划所有任务：

- 语言/工具链：**Rust**，Cargo workspace，**edition 2024**，`Cargo.lock` 入库；MSRV 以 stable 1.98 起步；开工第一步 `rustup default stable`（工具链已安装但尚未设默认）。
- 发布形态：单二进制 `ignis`（glibc 基线）；Linux / macOS；Node 不是 Ignis 的依赖（仅夹具与外部 CLI 用到）。
- HTTP：reqwest + rustls-tls（避免 OpenSSL 系统依赖）；SQLite：rusqlite `bundled` 特性（内置 FTS5）。
- 安全底线（反目标，违反即失败）：文件与 Shell 限制在工作区内；危险命令、敏感文件读取、外联需审批；**密钥/token 不进入任何日志或错误输出**；非交互无开关时 fail-closed。
- 依赖方向：`core → runtime`、`core → storage`；runtime 定义 `ToolExecutor` / `ApprovalHandler` 等 trait，core 实现（含审批与记录）并注入；runtime 与 storage 之间无依赖。
- 提交：Conventional Commits；一次提交一个可验证单元；提交信息 body 用中文、type/scope 英文（例：`feat(storage): 增加 SQLite 迁移与任务/审批表`）。
- 仓库整洁：`.gitignore` 排除 `target/`、`.commandcode/`、`.agents/`、`skills-lock.json`；`demo.md`、`docs/`（设计文档与本计划）必须入库。
- UI 语言：CLI 界面文案用英文（对齐 demo.md 的界面示意）；代码注释与文档用中文。（本计划作者的决定，可在评审时推翻。）
- 本计划锁定的默认参数：`max_steps=30`、`task_timeout_secs=900`、`token_budget=100_000`（in+out）、`max_retries=3`、`shell_timeout_secs=120`、`tool_output_limit=65_536` bytes、`http_max_bytes=1_048_576`、`context_char_budget=200_000` chars。
- 进程模型约定：一次 `ignis` 进程内 `Policy`（会话级审批规则）与 `ApprovalHandler` 共享；每次任务构造 `ToolRuntime`（带 task_id）。
- 每个任务的收口验证：`cargo fmt --all`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 全绿。
- 环境事实（2026-10-15 已核实）：rustup stable 工具链已装未设默认；node v26.7.0 / npm 11.19.0 可用；`notify-send` 可用（P3 才用）；git 身份已配置；仓库尚无提交（首任务含首次提交）。
- 与设计文档的已知偏差（本计划的决定，可在评审时推翻）：① 跨 crate 端到端测试放在 `apps/cli/tests/` 而非仓库根 `tests/`——因为 `CARGO_BIN_EXE_ignis` 只在 bin 所在 package 的测试里可用，这是让 e2e 稳定拿到被测二进制的最简做法；② 配置模块放 `core::config`（storage 只保留持久化），使 storage 不依赖 runtime。

## 评审重点（Review Focus）

以下五类输入/情形设计文档未逐条规定，但用户会真实遇到。每条都已绑定到具体任务中的测试——执行者必须让这些测试存在且通过：

1. **路径逃逸**：`../` 穿越、绝对路径、指向工作区外的符号链接、不存在的父目录——必须拒绝且无副作用（不创建、不写入）。测试在 Task 5（`resolve_in_workspace` 表格测试）与 Task 6（每个 fs 工具的逃逸用例）。
2. **危险命令变体**：`rm -fr`、`sudo` 前缀、`curl … | sh`、`chmod -R 777`、`dd if=`——必须进危险档；即使 `--yes` 也不放行，只有 `--unsafe` 可以。测试在 Task 5（模式表）与 Task 16（`approval_modes.rs`）。
3. **密钥泄漏**：模型诱导执行 `env` / `echo $KEY`，或工具输出恰好包含 key——shell 子进程环境与 JSONL/SQLite 日志都不得含密钥明文。测试在 Task 7（子进程 env 剔除）、Task 12（JSONL 脱敏）、Task 16（e2e 断言）。
4. **流式协议异常**：SSE 行被 TCP 分片切断、`:` 心跳行、tool_call arguments 多分片且多 index 交叉、429/5xx、流中途断开——不得崩溃；重试只能发生在首块之前。测试在 Task 10。
5. **巨量 / 二进制输出**：2MB 文件、`yes` 刷屏、HTTP 超大体或非 UTF-8 字节——按上限截断、lossy 解码，不 OOM、不 panic。测试在 Task 6（fs_read 截断）、Task 7（shell 截断）、Task 8（http 上限 + 非 UTF-8）。

## 后续阶段（本计划不覆盖）

- **P2 记忆与技能**：memory（facts + supersede + FTS5 trigram + project 无条件注入 + Markdown 镜像）、`remember`/`load_skill` 元工具、memory/skills 子命令、内置三技能嵌入 → Demo B。
- **P3 调度与守护**：jobs/job_runs、interval/cron、受限 profile、daemon（pidfile/flock/foreground）、`notify_user`、alerts 去重与通知通道 → Demo C。
- **P4 兼容内核与交付**：codex/claude 适配器与内核切换、`ignis doctor` 完整版、Docker/systemd、README、验收 harness（20 例）。

---

### Task 1: 环境与工作区脚手架

**Files:**
- Create: `Cargo.toml`、`rust-toolchain.toml`、`.gitignore`、`apps/cli/Cargo.toml|src/main.rs`、`packages/core|runtime|storage/Cargo.toml|src/lib.rs`
- Create: `docs/spikes/2026-10-15-relay-streaming-toolcalls.md`

**Interfaces:**
- Consumes: 无（本仓库尚无代码）。
- Produces: workspace 成员 `ignis-cli`（bin `ignis`）、`ignis-core`、`ignis-runtime`、`ignis-storage`；根 `[workspace.dependencies]`（后续任务 `cargo add` 用 `{ workspace = true }`）。

- [ ] **Step 1: 启用 Rust 工具链**

Run: `rustup default stable && rustc --version && cargo --version`
Expected: 无错误，打印 stable 版本（≥ 1.98）。

- [ ] **Step 2: 写 workspace 骨架**

根 `Cargo.toml`：
```toml
[workspace]
resolver = "3"
members = ["apps/cli", "packages/core", "packages/runtime", "packages/storage"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.98"

[workspace.dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "process", "signal", "time", "sync", "io-util", "fs"] }
tokio-util = "0.7"
futures = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
async-trait = "0.1"
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json", "stream"] }
rusqlite = { version = "0.37", features = ["bundled"] }
uuid = { version = "1", features = ["v7", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
dirs = "6"
regex = "1"
```
版本号由 `cargo add` 解析后回填（上面为示意，特性列表必须保持原样）。四个 crate 的 `Cargo.toml` 用 `[package] name = "ignis-*"` + `[dependencies]` 引用 `{ workspace = true }`；`apps/cli` 加 `[[bin]] name = "ignis"`。`src/lib.rs`/`main.rs` 先放最小内容（lib 空、main 打印一行）。`rust-toolchain.toml` 写 `[toolchain] channel = "stable"`。

- [ ] **Step 3: 写 `.gitignore` 并跑通构建与测试**

`.gitignore` 内容：`/target`、`.commandcode/`、`.agents/`、`skills-lock.json`。
Run: `cargo build && cargo test --workspace`
Expected: 编译通过，0 个测试或空测试通过。

- [ ] **Step 4: 中转「流式 + 工具调用」spike（开工即验证项 #1）**

用本机现有中转 key（从 codex/claude 配置读取，**不得把 key 写入仓库**）执行最小请求：
```bash
curl -sN "$BASE_URL/chat/completions" -H "Authorization: Bearer $RELAY_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"<模型名>","stream":true,"stream_options":{"include_usage":true},
       "tools":[{"type":"function","function":{"name":"get_time","description":"","parameters":{"type":"object","properties":{}}}}],
       "messages":[{"role":"user","content":"现在几点？必须调用 get_time"}]}'
```
观察：是否出现 `data:` 行、`delta.tool_calls` 的 `arguments` 是否分片、最终是否带 `usage`。把结论（支持/不支持、模型名、日期）写入 `docs/spikes/2026-10-15-relay-streaming-toolcalls.md`，**不含 key**。
**若流式工具调用不可用：停下来向作者报告，讨论降级为 `stream:false` 后再继续。** 若 key 在开工时不可得，记录该情况并在 Task 10 前补做。

- [ ] **Step 5: 首次提交**

```bash
git add Cargo.toml Cargo.lock rust-toolchain.toml .gitignore apps packages docs demo.md
git commit -m "chore: 初始化 Cargo workspace 与各 crate 骨架"
```

---

### Task 2: runtime 契约类型与 trait

**Files:**
- Create: `packages/runtime/src/lib.rs`、`packages/runtime/src/test_support/mod.rs`
- Modify: `packages/runtime/Cargo.toml`（加 `test-support` feature）

**Interfaces:**
- Consumes: Task 1 的 workspace。
- Produces（后续所有任务依赖这些名字，不得改名）：
  - `ignis_runtime::{ChatMessage, Role, ToolSpec, ToolCall, ToolResult, Usage}`（`Usage { input_tokens: u64, output_tokens: u64, estimated: bool }`，被 `RuntimeEvent::Usage` 与 `TaskOutcome` 复用）
  - `ignis_runtime::{ApprovalRequest, ApprovalDecision, RiskLevel}`、`trait ApprovalHandler { async fn decide(&self, req: ApprovalRequest) -> ApprovalDecision; }`
  - `ignis_runtime::ToolExecutor { fn specs(&self) -> Vec<ToolSpec>; fn summarize(&self, call: &ToolCall) -> String; async fn execute(&self, call: ToolCall, cancel: CancellationToken) -> ToolResult; }`
  - `ignis_runtime::{RuntimeEvent, RunStatus, Capabilities, RunRequest, Limits, ModelConfig, ModelProtocol}`、`trait Runtime { fn id(&self) -> &'static str; fn capabilities(&self) -> Capabilities; async fn run(&self, req: RunRequest, cancel: CancellationToken) -> BoxStream<'static, RuntimeEvent>; }`
  - `ignis_runtime::test_support::{MockExecutor, MockRuntime, ScriptedApproval, RecordedRequest}`（feature `test-support`）

- [ ] **Step 1: 写失败测试（序列化形状与默认值）**

`packages/runtime/src/lib.rs` 内 `#[cfg(test)] mod tests`：
```rust
#[test]
fn runtime_event_json_shape_is_locked() {
    let ev = RuntimeEvent::TextDelta { text: "hi".into() };
    assert_eq!(serde_json::to_value(&ev).unwrap(), json!({"type":"text_delta","text":"hi"}));
    let ev = RuntimeEvent::ToolExec { name: "fs_read".into(), summary: "Reading a.txt".into(), args: json!({"path":"a.txt"}) };
    assert_eq!(serde_json::to_value(&ev).unwrap(),
        json!({"type":"tool_exec","name":"fs_read","summary":"Reading a.txt","args":{"path":"a.txt"}}));
}

#[test]
fn limits_defaults_are_locked() {
    let l = Limits::default();
    assert_eq!((l.max_steps, l.task_timeout_secs, l.token_budget, l.max_retries), (30, 900, 100_000, 3));
    assert_eq!((l.shell_timeout_secs, l.tool_output_limit, l.http_max_bytes, l.context_char_budget), (120, 65_536, 1_048_576, 200_000));
}

#[test]
fn model_config_roundtrips_with_default_api_key_env() {
    let cfg: ModelConfig = serde_json::from_value(json!({"base_url":"https://x/v1","model":"m"})).unwrap();
    assert_eq!(cfg.api_key_env, "OPENAI_API_KEY");
    assert_eq!(cfg.protocol, ModelProtocol::OpenAi);
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-runtime`
Expected: 编译失败（类型未定义）。

- [ ] **Step 3: 实现类型与 trait**

要点：`RuntimeEvent` 用 `#[serde(tag = "type", rename_all = "snake_case")]`，variant 形状为 `TextDelta{text}`、`ToolExec{name,summary,args}`、`ToolResult{name,ok,output}`、`Usage{input_tokens,output_tokens,estimated}`、`Done{status,summary:Option<String>,error:Option<String>}`（JSONL 日志直接序列化它，形状是跨模块契约）。`RunStatus` 取值 `Success|Failed|Aborted|LimitReached`，serde snake_case（`limit_reached` 与 DB status 一致）。`ApplyRequest`/trait 用 `#[async_trait::async_trait]`。`RunRequest { messages: Vec<ChatMessage>, system_prompt: String, model: ModelConfig, limits: Limits, executor: Arc<dyn ToolExecutor> }`（不派生 Clone/Debug）。`pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;`。`Limits`/`ModelConfig` 均 `#[serde(default)]` 逐字段可缺省。

- [ ] **Step 4: 写 test_support 与对象安全测试**

`test_support` 提供：`MockExecutor`（`results: Mutex<VecDeque<ToolResult>>`，记录收到的 `ToolCall`，specs 返回一个 `echo` 工具）、`MockRuntime`（按序回放事件的 BoxStream，记录 `RecordedRequest{system_prompt, messages}`）、`ScriptedApproval`（队列决策 + 记录请求）。测试：
```rust
#[tokio::test]
async fn mocks_satisfy_object_safe_traits() {
    let exec: Arc<dyn ToolExecutor> = MockExecutor::new(vec![]);
    assert_eq!(exec.specs().len(), 1);
    let rt: Arc<dyn Runtime> = MockRuntime::new(vec![RuntimeEvent::Done { status: RunStatus::Success, summary: None, error: None }]);
    assert_eq!(rt.id(), "mock");
}
```

- [ ] **Step 5: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/runtime
git commit -m "feat(runtime): 定义 Runtime/ToolExecutor/ApprovalHandler 契约与默认上限"
```

---

### Task 3: storage：SQLite 迁移、任务与审批表、FTS5/trigram 能力测试

**Files:**
- Create: `packages/storage/src/{lib.rs,db.rs,migrations.rs,tasks.rs,approvals.rs}`、`packages/storage/tests/fts_trigram.rs`
- Modify: `packages/storage/Cargo.toml`（`rusqlite`、`thiserror`、`serde_json`、`chrono`）

**Interfaces:**
- Consumes: 无（不依赖 runtime/core）。
- Produces:
  - `ignis_storage::Db`：`open(path: &Path) -> Result<Db>`（建父目录、`PRAGMA journal_mode=WAL; foreign_keys=ON`、自动迁移）、`open_in_memory()`、`with_conn<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T>`
  - `ignis_storage::{NewTask, TaskRecord}`、`Db::{insert_task(&self, t: &NewTask) -> Result<()>, finish_task(&self, id: &str, status: &str, summary: Option<&str>, error: Option<&str>, input_tokens: u64, output_tokens: u64, usage_estimated: bool, ended_at: i64) -> Result<()>, get_task(&self, id: &str) -> Result<Option<TaskRecord>>, list_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>>}`
  - `ignis_storage::{NewApproval, ApprovalRecord}`、`Db::{insert_approval, list_approvals}`
  - `ignis_storage::StorageError`
  - 状态字符串（与 `RunStatus` 一致）：`running|success|failed|aborted|limit_reached`；决策：`allow|allow_session|deny`；模式：`interactive|preauthorized|unattended`

- [ ] **Step 1: 写失败测试（迁移幂等 + 往返）**

`db.rs` 测试：`open` 后 `tasks`/`approvals` 存在；连续两次 `Db::open(同一路径)` 不报错（迁移幂等）。`tasks.rs` 测试：插入 `NewTask{id:"01",kind:"run",prompt:"p",kernel:"native",workspace:"/w",started_at:1}` → `get_task` 返回 `status=="running"`；`finish_task("01","success",Some("done"),None,12,34,false,99)` 后 `ended_at==99`、`input_tokens==12`。`approvals.rs` 测试：插入后 `list_approvals("01")` 顺序与字段一致。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-storage`
Expected: 编译失败 / 表不存在。

- [ ] **Step 3: 实现迁移与 DAO**

迁移用 `PRAGMA user_version` 计数：`MIGRATIONS: &[&str]`，逐条在事务中执行后 `user_version = idx+1`。迁移 001 建表：
```sql
CREATE TABLE tasks (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, prompt TEXT NOT NULL, kernel TEXT NOT NULL,
  workspace TEXT NOT NULL, status TEXT NOT NULL, summary TEXT, error TEXT,
  input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
  usage_estimated INTEGER NOT NULL DEFAULT 0,
  started_at INTEGER NOT NULL, ended_at INTEGER
);
CREATE TABLE approvals (
  id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL, tool TEXT NOT NULL,
  summary TEXT NOT NULL, risk TEXT NOT NULL, decision TEXT NOT NULL, mode TEXT NOT NULL,
  decided_at INTEGER NOT NULL
);
CREATE INDEX idx_approvals_task ON approvals(task_id);
```
`Db` 内部 `Mutex<Connection>`；DAO 全走 `with_conn`。时间戳为 epoch 毫秒（`i64`）。

- [ ] **Step 4: 写 FTS5/trigram spike 测试（开工即验证项 #3）**

`packages/storage/tests/fts_trigram.rs`：
```rust
#[test]
fn bundled_sqlite_supports_fts5_trigram_chinese_substring() {
    let db = ignis_storage::Db::open_in_memory().unwrap();
    db.with_conn(|c| {
        c.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x, tokenize='trigram');
                         INSERT INTO t(x) VALUES ('项目使用 uv 管理 Python 环境');")
    }).unwrap();
    let n: i64 = db.with_conn(|c| c.query_row("SELECT count(*) FROM t WHERE t MATCH 'Python'", [], |r| r.get(0))).unwrap();
    assert_eq!(n, 1);
    let n: i64 = db.with_conn(|c| c.query_row("SELECT count(*) FROM t WHERE x LIKE '%uv 管理%'", [], |r| r.get(0))).unwrap();
    assert_eq!(n, 1);
}
```
（约束已知：trigram 的 MATCH 查询需 ≥3 个 unicode 字符；短查询走索引化 LIKE——P2 检索要利用这一点。若测试失败说明 bundled SQLite 不支持，停下向作者报告。）

- [ ] **Step 5: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/storage
git commit -m "feat(storage): 增加 SQLite 迁移、任务/审批表与 FTS5 trigram 验证"
```

---

### Task 4: core::config：配置结构、加载/保存、环境覆盖、脱敏

**Files:**
- Create: `packages/core/src/{lib.rs,config.rs}`
- Modify: `packages/core/Cargo.toml`（`ignis-runtime`、`ignis-storage`、`serde`、`serde_json`、`thiserror`、`dirs`、`chrono`、`uuid`、`tokio`、`tokio-util`、`async-trait`、`regex`、`nix`）

**Interfaces:**
- Consumes: `ignis_runtime::{Limits, ModelConfig, ModelProtocol}`（Task 2）。
- Produces:
  - `ignis_core::config::{Config, ConfigError, load_config, save_config, ignis_home}`
  - `Config { workspace_root: PathBuf, default_kernel: String /*"native"*/, model: ModelConfig, api_key: Option<String>, limits: Limits }`：实现 `Default`（测试可 `Config { api_key: Some(..), ..Default::default() }` 构造），全部字段 `#[serde(default)]`（配置文件可只写要覆盖的字段），`Debug` 手写实现（`api_key` 恒打印 `"***"`）。
  - 环境覆盖（只存在于生效值，不写回文件）：`IGNIS_HOME`、`IGNIS_WORKSPACE`、`IGNIS_BASE_URL`、`IGNIS_MODEL`、`IGNIS_API_KEY`
  - `load_config(home: &Path) -> Result<Config, ConfigError>`；缺失文件用默认值；`save_config(home, &Config)` 原子写（临时文件 + rename）且权限 0600。

- [ ] **Step 1: 写失败测试**

```rust
#[test] fn missing_file_yields_defaults() { /* home 指向空目录 → workspace_root 为当前目录，kernel 为 "native" */ }
#[test] fn env_overrides_win_over_file() { /* 写文件 model.model="a"；set IGNIS_MODEL=b → 生效 "b" */ }
#[test] fn save_creates_0600_file() { /* 保存后 metadata.permissions().mode() & 0o777 == 0o600 */ }
#[test] fn debug_never_prints_api_key() { /* api_key=Some("sk-test-secret") → !format!("{:?}", cfg).contains("sk-test-secret") */ }
#[test] fn invalid_json_reports_path() { /* 文件写 "{oops" → Err 信息含文件路径 */ }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-core`
Expected: 编译失败（config 未实现）。

- [ ] **Step 3: 实现 config**

要点：`ignis_home()` = `IGNIS_HOME` 或 `~/.ignis`（`dirs::home_dir`）。`load_config` 顺序：默认 → 文件（存在则反序列化）→ 环境变量覆盖。`save_config` 用 `std::fs::OpenOptions::new().mode(0o600).create(true).truncate(true)`（unix）写临时文件后 `rename`，保证权限不被 umask 放宽。`Config` 中 `workspace_root` 默认 `std::env::current_dir()`。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/core
git commit -m "feat(core): 增加配置加载/保存、环境覆盖与密钥脱敏"
```

---

### Task 5: core::security：路径沙箱、敏感文件、危险命令、审批策略、脱敏器

**Files:**
- Create: `packages/core/src/security/{mod.rs,sandbox.rs,sensitive.rs,dangerous.rs,policy.rs,redact.rs}`
- Modify: `packages/core/src/lib.rs`

**Interfaces:**
- Consumes: `ignis_runtime::{RiskLevel, ApprovalRequest, ApprovalDecision}`（Task 2）。
- Produces:
  - `ignis_core::security::resolve_in_workspace(workspace: &Path, user_path: &str) -> Result<PathBuf, SecurityError>`（先做词法规范化——折叠 `.`/`..`，越出工作区即拒绝；再对规范化路径最近的已存在祖先目录 `canonicalize` 并比对 `starts_with` 工作区规范路径，防符号链接逃逸；绝对路径落在工作区外同样拒绝）
  - `ignis_core::security::is_sensitive(rel_path: &Path) -> bool`（`.env`、`.env.*`、`*.pem`、`*.key`、`id_rsa*`、`id_ed25519*`、`.netrc`、`.git-credentials`）
  - `ignis_core::security::is_dangerous_command(cmd: &str) -> bool`（regex 表，见 Step 3）
  - `ignis_core::security::{ApprovalMode, PolicyDecision, Policy}`：`ApprovalMode::{Interactive, Preauthorized{unsafe_ops: bool}, Unattended}`；`Policy::new(mode)`、`Policy::mode(&self) -> ApprovalMode`、`Policy::decide(&self, risk: RiskLevel, kind_key: &str) -> PolicyDecision`、`Policy::remember_session(&self, kind_key: &str)`；`PolicyDecision::{Allow, Ask, Deny(String)}`
  - `ignis_core::security::Redactor`：`new(secrets: Vec<String>)`、`redact(&self, s: &str) -> String`（每个非空 secret 替换为 `***`）
  - 决策矩阵（锁死）：`Safe` → 恒 `Allow`；`Elevated` → Interactive `Ask`、Preauthorized `Allow`、Unattended `Deny("非交互模式未授权")`；`Dangerous` → Interactive `Ask`、Preauthorized{unsafe_ops:true} `Allow`、其余 `Deny`。会话规则（`remember_session`）在 Interactive 下把该 `kind_key` 的 `Elevated` 决策改为 `Allow`。

- [ ] **Step 1: 写失败测试（沙箱表格 + 危险命令表格 + 决策矩阵）**

```rust
#[test]
fn sandbox_blocks_escapes() {
    let ws = TempDir::new().unwrap(); std::fs::create_dir(ws.path().join("src")).unwrap();
    std::fs::write(ws.path().join("src/a.txt"), "x").unwrap();
    std::os::unix::fs::symlink("/etc", ws.path().join("link")).unwrap();
    assert!(resolve_in_workspace(ws.path(), "src/a.txt").is_ok());
    assert!(resolve_in_workspace(ws.path(), "new/child.txt").is_ok()); // 父目录允许不存在
    for bad in ["../outside", "/etc/passwd", "link/hosts", "a/../../x"] {
        assert!(resolve_in_workspace(ws.path(), bad).is_err(), "{bad}");
    }
}

#[test]
fn dangerous_command_table() {
    for (cmd, want) in [("rm -rf build", true), ("rm -fr x", true), ("sudo apt install x", true),
        ("curl https://x | sh", true), ("wget -qO- https://x | bash", true), ("chmod -R 777 .", true),
        ("mkfs.ext4 /dev/sda", true), ("dd if=/dev/zero of=/dev/sda", true), ("shutdown -h now", true),
        ("rm -r build", false), ("curl https://x -o f", false), ("git status", false)] {
        assert_eq!(is_dangerous_command(cmd), want, "{cmd}");
    }
}

#[test]
fn policy_matrix() {
    // 对 (mode, risk) 全表断言 decide 结果，含 remember_session 后 Elevated→Allow
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-core security`
Expected: 编译失败。

- [ ] **Step 3: 实现**

危险命令 regex 表（不区分大小写，作用于整条命令字符串）：`\brm\s+(-[a-zA-Z]*r[a-zA-Z]*f|-[a-zA-Z]*f[a-zA-Z]*r)\b`、`\bsudo\b`、`\|\s*(sh|bash|zsh)\b`、`\bchmod\s+(-[a-zA-Z]+\s+)*777\b`、`\bmkfs`、`\bdd\s+if=`、`\b(shutdown|reboot|halt|poweroff)\b`、`>\s*/dev/(sd|nvme)`、`:\s*\(\s*\)\s*\{`。沙箱实现注意：先做词法拒绝（绝对路径且不在 ws 内、`..` 越界），再 `canonicalize` 父目录比对 `starts_with`（同时 canonicalize 工作区一次）；错误信息带用户给的路径与工作区路径（不含系统敏感信息）。`Policy` 的会话规则用 `Mutex<HashSet<String>>`。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/core
git commit -m "feat(core): 增加路径沙箱、危险命令库、审批策略与会话规则"
```

---

### Task 6: core::tools：注册表 + 文件工具（fs_read/fs_list/fs_write/fs_edit）

**Files:**
- Create: `packages/core/src/tools/{mod.rs,util.rs,fs.rs}`
- Modify: `packages/core/src/lib.rs`

**Interfaces:**
- Consumes: `resolve_in_workspace`（Task 5）；`ignis_runtime::ToolSpec`（Task 2）。
- Produces:
  - `ignis_core::tools::{Tool, ToolCtx, ToolError, Registry, default_registry}`
  - `trait Tool { fn spec(&self) -> ToolSpec; async fn run(&self, args: &serde_json::Value, ctx: &ToolCtx<'_>) -> Result<String, ToolError>; }`
  - `ToolCtx<'a> { workspace: &'a Path, cancel: CancellationToken, limits: &'a Limits }`
  - `Registry::specs() -> Vec<ToolSpec>`、`Registry::get(name: &str) -> Option<&dyn Tool>`
  - `util::truncate_output(s: &str, limit: usize) -> String`（按字符边界截断，追加 `\n… [truncated: output exceeded {limit} bytes]`）
  - 工具参数契约：`fs_read{path, offset?:int, limit?:int}`、`fs_list{path?}`（默认 `.`）、`fs_write{path, content}`（自动建父目录）、`fs_edit{path, old_string, new_string, replace_all?:bool=false}`（默认要求唯一匹配，0 或 >1 处报错并说明匹配数）

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test] async fn fs_read_returns_content_and_truncates() {
    // 写 100KB 文件 → 输出以 "[truncated" 结尾且长度 <= tool_output_limit + 尾部标记
}
#[tokio::test] async fn fs_write_creates_parent_dirs() { /* 写 "a/b/c.txt" 成功，内容一致 */ }
#[tokio::test] async fn fs_edit_requires_unique_match() {
    // 文件含两个 "foo"：old_string="foo" → Err（信息含 "2"）；replace_all=true → 两处都变
}
#[tokio::test] async fn fs_tools_reject_workspace_escape() {
    // 对 fs_read/fs_list/fs_write/fs_edit 各试 "../out.txt" → Err，且工作区外文件不存在
}
#[tokio::test] async fn fs_list_marks_dirs_and_sorts() { /* 返回 "src/", "a.txt" 排序且目录带 "/" 后缀 */ }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-core tools`
Expected: 编译失败。

- [ ] **Step 3: 实现**

每个工具先 `resolve_in_workspace`（写操作检查父目录），`fs_read` 输出经 `truncate_output`；`fs_read` 的 `offset/limit` 以行为单位，超限时提示总行数。`fs_edit` 校验 `old_string` 非空；替换后原样写回（保留其余内容）。错误统一转 `ToolError::message(String)`，不 panic。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/core
git commit -m "feat(core): 增加工具注册表与文件工具（读/列/写/编辑）"
```

---

### Task 7: core::tools：shell_run（超时、进程组终止、输出截断、env 剔除）

**Files:**
- Create: `packages/core/src/tools/shell.rs`
- Modify: `packages/core/src/tools/mod.rs`

**Interfaces:**
- Consumes: `ToolCtx`/`Tool`（Task 6）、`Limits`（Task 2）。
- Produces: 注册名 `shell_run`，参数 `{command: string, cwd?: string}`（`cwd` 相对工作区，默认工作区根）；返回 `exit code: {n}\n{stdout+stderr 合并，超 tool_output_limit 截断}`；超时/中止返回 `Err`（信息含 "timeout" / "cancelled"）。

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test] async fn runs_command_in_workspace() { /* "pwd" 输出 == 工作区路径；exit code: 0 */ }
#[tokio::test] async fn kills_process_group_on_timeout() {
    // limits.shell_timeout_secs = 1；命令 "sleep 12345 & sleep 12346"
    // 断言：Err 含 "timeout"；耗时 < 4s；测试结束 `pgrep -f 12345` 无结果（Linux）
}
#[tokio::test] async fn strips_secret_env_vars() {
    // 进程 env 设 FOO_KEY=secret（含 _KEY/_TOKEN/_SECRET/_PASSWORD 后缀者剔除）
    // 命令 'echo "[$FOO_KEY]"' → 输出 "[]"
}
#[tokio::test] async fn truncates_noisy_output() { /* "yes" + 超时或 head → 输出含 "[truncated" */ }
#[tokio::test] async fn rejects_cwd_escape() { /* cwd=".." → Err */ }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-core shell`
Expected: 编译失败。

- [ ] **Step 3: 实现**

用 `tokio::process::Command::new("sh").arg("-c").arg(cmd)`，`.process_group(0)`、`.kill_on_drop(true)`、`.stdout(piped())`、`.stderr(piped())`；环境用 `env_remove` 剔除名匹配 `(?i)(_KEY|_TOKEN|_SECRET|_PASSWORD)$` 的变量。运行用 `tokio::select!`：分支 A 等 `child.wait_with_output()`，分支 B `tokio::time::sleep(shell_timeout)`，分支 C `cancel.cancelled()`；B/C 触发时 `nix::sys::signal::killpg(Pid::from_raw(pid), SIGKILL)` 后再 `wait()` 收尸。合并 stdout+stderr（stdout 在前），`truncate_output` 截断。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/core
git commit -m "feat(core): 增加 shell_run（超时、进程组终止、输出截断、密钥环境剔除）"
```

---

### Task 8: core::tools：http_fetch（大小上限、超时、非 UTF-8）

**Files:**
- Create: `packages/core/src/tools/http.rs`
- Modify: `packages/core/src/tools/mod.rs`、`packages/core/Cargo.toml`（`reqwest { workspace = true }`）

**Interfaces:**
- Consumes: `ToolCtx`（Task 6）、`Limits`（Task 2）。
- Produces: 注册名 `http_fetch`，参数 `{url: string, method?: "GET"|"POST", body?: string}`；返回 `status: {n}\ncontent-type: {t}\n{body 截断至 http_max_bytes}`；请求超时 30s，重定向最多 3 次；响应体按 lossy UTF-8 解码。

- [ ] **Step 1: 写失败测试（本地 std TcpListener 假服务器）**

```rust
#[tokio::test] async fn fetches_and_caps_body() { /* 服 5MB 响应 → 输出含 "[truncated" 且 ≤ 上限+标记 */ }
#[tokio::test] async fn non_utf8_body_is_lossy() { /* 服 \xFF\xFE 字节 → 不 panic，含替换字符 */ }
#[tokio::test] async fn request_timeout_is_reported() { /* 服方 sleep 40s → Err 含 "timeout"（测试用 tokio::time::pause 或缩短常量） */ }
#[tokio::test] async fn follows_redirects_up_to_limit() { /* /a 302 → /b 返回体命中 */ }
```
（假服务器用手写 `std::net::TcpListener` + 线程，不引入测试框架依赖。）

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-core http`
Expected: 编译失败。

- [ ] **Step 3: 实现**

共享一个 `reqwest::Client`（`connect_timeout 10s`、`read_timeout 30s`、`redirect::Policy::limited(3)`、`user_agent "ignis/0.1"`）。读体时流式累计并在 `http_max_bytes` 处停止（`bytes_stream()` + 累计长度，超限即丢弃余量）。仅允许 `http`/`https` scheme。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/core
git commit -m "feat(core): 增加 http_fetch（大小上限、超时、重定向限制）"
```

---

### Task 9: core::tools::ToolRuntime：审批执行器（分类 → 策略 → 提示 → 记录 → 执行）

**Files:**
- Create: `packages/core/src/tools/runtime.rs`
- Modify: `packages/core/src/tools/mod.rs`

**Interfaces:**
- Consumes: `Policy`/`Redactor`/`is_sensitive`/`is_dangerous_command`（Task 5）、`Registry`（Task 6）、`Db::{insert_approval, NewApproval}`（Task 3）、`ignis_runtime::{ToolExecutor, ToolCall, ToolResult, ApprovalHandler, ApprovalRequest, RiskLevel}`（Task 2）。
- Produces:
  - `ignis_core::tools::ToolRuntime`，构造 `ToolRuntime::new(registry: Registry, policy: Arc<Policy>, approvals: Arc<dyn ApprovalHandler>, db: Arc<Db>, task_id: String, workspace: PathBuf, limits: Limits, redactor: Redactor)`
  - `impl ToolExecutor for ToolRuntime`：`specs()` 返回注册表全部；`summarize(&call)` 生成行动摘要；`execute(&call, cancel)`
  - 分类规则：`fs_read/fs_list` → `Safe`；`*` 路径命中 `is_sensitive` → `Dangerous`；`fs_write/fs_edit` → `Elevated`；`shell_run` → `Elevated`，命中 `is_dangerous_command` → `Dangerous`；`http_fetch` → `Elevated`
  - 摘要文案（锁死输出格式）：`Reading {path}` / `Listing {path}` / `Writing {path}` / `Editing {path}` / `Running: {command}`（截 80 字符）/ `Fetching {url}` / 未知工具 `Running {name}`
  - 行为：`Deny` → 不执行，写一条 `decision="deny"` 审批记录，返回 `ToolResult{ok:false, output:"denied by policy: {reason}"}`；`Ask` → `handler.decide`，`AllowOnce`/`AllowSession`（后者调 `policy.remember_session`）放行，`Reject` 拒绝；两者都写审批记录（`mode` 取 `policy.mode()` 的字符串）。执行错误 → `ToolResult{ok:false, output: 错误信息}`；输出最终过 `redactor.redact`。

- [ ] **Step 1: 写失败测试（用 `ScriptedApproval`）**

```rust
#[tokio::test] async fn safe_read_does_not_ask() { /* fs_read 成功且 approvals 表为空 */ }
#[tokio::test] async fn elevated_write_records_approval() {
    // ScriptedApproval=[AllowOnce] → 文件存在；approvals 行 (fs_write, allow, interactive)
}
#[tokio::test] async fn reject_prevents_side_effects() { /* ScriptedApproval=[Reject] → 文件不存在，行 decision=deny */ }
#[tokio::test] async fn session_allow_skips_second_prompt() {
    // 第一次 AllowSession → 第二次同类 Elevated 不再调用 handler（seen 长度 1）
}
#[tokio::test] async fn unattended_denies_and_records() { /* Policy=Unattended → 写被拒，mode=unattended */ }
#[tokio::test] async fn unknown_tool_returns_error_result() { /* ok=false，不 panic */ }
#[tokio::test] async fn secret_in_output_is_redacted() { /* redactor 含 "sk-x"；工具输出含它 → ToolResult.output 无明文 */ }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-core tool_runtime`
Expected: 编译失败。

- [ ] **Step 3: 实现**

`kind_key` 用工具名（会话规则粒度：工具级）。审批记录的 `summary` 用 `summarize` 输出，`risk` 用 `RiskLevel` 的 serde 小写名。批准后执行前再检查 `cancel.is_cancelled()`，已取消直接返回 `ok:false "cancelled"`。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/core
git commit -m "feat(core): 增加带审批的 ToolRuntime 执行器与审批记录"
```

---

### Task 10: runtime::openai：流式客户端 + test_support 假模型服务

**Files:**
- Create: `packages/runtime/src/openai/{mod.rs,sse.rs,assembler.rs}`、`packages/runtime/src/test_support/scripted_model.rs`
- Modify: `packages/runtime/src/lib.rs`、`packages/runtime/Cargo.toml`（`test-support` feature 加 `axum`）

**Interfaces:**
- Consumes: `ModelConfig`、`ChatMessage`、`ToolSpec`、`ToolCall`、`Usage`（Task 2）。
- Produces:
  - `ignis_runtime::openai::{OpenAiClient, LlmError, ChatChunk}`
  - `OpenAiClient::new(cfg: &ModelConfig, api_key: String) -> Result<OpenAiClient, LlmError>`；`async fn stream_chat(&self, messages: &[ChatMessage], tools: &[ToolSpec], system: &str) -> Result<BoxStream<'static, Result<ChatChunk, LlmError>>, LlmError>`
  - `ChatChunk { text: Option<String>, tool_call_deltas: Vec<ToolCallDelta>, finish_reason: Option<String>, usage: Option<Usage> }`；`ToolCallDelta { index: usize, id: Option<String>, name: Option<String>, args_delta: Option<String> }`
  - `ignis_runtime::openai::sse::SseDecoder`（`push(&mut self, bytes: &[u8]) -> Result<Vec<String>, LlmError>`）
  - `ignis_runtime::openai::assembler::ToolCallAssembler`（`push_delta(idx, id, name, args)`、`finish() -> Result<Vec<ToolCall>, LlmError>`，按 index 组装、args 拼接后 parse）
  - `ignis_runtime::test_support::{ScriptedModel, ScriptStep, ScriptedModelHandle}`：`ScriptedModel::start(script: Vec<ScriptStep>) -> ScriptedModelHandle`（内部线程跑 axum，绑定 127.0.0.1:0；`start()` 同步阻塞至绑定完成，可在 `#[tokio::test]` 里直接调用）；`ScriptStep::{Text(String), ToolCalls{text, calls: Vec<(String, serde_json::Value)>}, Error{status: u16, body: String}, AbruptClose(String), DropConnection}`（`AbruptClose` = 送出一段文本后断流；`DropConnection` = 未响应直接断开，用于重试测试）；句柄暴露 `base_url()`（`http://127.0.0.1:{port}/v1`）与 `requests() -> Vec<serde_json::Value>`；`Text`/`ToolCalls` 的 args 拆成 3 个 delta 分片发送，usage 在末块给出。
  - 重试规则（钉死）：仅在**未收到响应或收到非 2xx**（连接错误 / 429 / 5xx）时重试，次数用 `OpenAiClient::with_max_retries(n)`（默认 3）、退避用 `with_backoff(base)`（默认 500ms，`base * 2^(n-1)` + 0–20% 抖动）配置；收到 2xx 之后（无论流是否中途断开）**永不重试**，错误作为流内错误上抛。

- [ ] **Step 1: 写失败测试（SSE 解析、分片组装）**

```rust
#[test]
fn sse_decoder_handles_split_boundaries_and_comments() {
    let mut d = SseDecoder::new();
    assert!(d.push(b"data: {\"a\":").unwrap().is_empty());                    // 行未收全
    assert_eq!(d.push(b"1}\n\n: keep-alive\n\ndata: [DONE]\n\n").unwrap(),
               vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]);          // 心跳行忽略
}

#[test]
fn tool_call_assembler_merges_interleaved_fragments() {
    let mut a = ToolCallAssembler::new();
    a.push_delta(0, Some("call_1"), Some("fs_read"), Some("{\"pa"));
    a.push_delta(1, Some("call_2"), Some("fs_list"), Some("{}"));
    a.push_delta(0, None, None, Some("th\":\"a.txt\"}"));
    assert_eq!(a.finish().unwrap(), vec![
        ToolCall { id: "call_1".into(), name: "fs_read".into(), args: json!({"path":"a.txt"}) },
        ToolCall { id: "call_2".into(), name: "fs_list".into(), args: json!({}) },
    ]);
}

#[tokio::test]
async fn streams_text_and_usage_from_scripted_model() { /* ScriptStep::Text → 多个 ChatChunk；末块 usage 有值 */ }
#[tokio::test]
async fn connection_drop_retries_then_reports_readably() {
    // 脚本 [DropConnection, Error{status:401}] + max_retries=1 → Err 信息含 "401"，且 requests().len()==2
}
#[tokio::test]
async fn no_retry_after_stream_started() { /* 单步 [AbruptClose("partial")] → 错误只出现一次，requests().len()==1 */ }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-runtime openai --features test-support`
Expected: 编译失败。

- [ ] **Step 3: 实现**

请求体：`{"model", "stream": true, "stream_options": {"include_usage": true}, "messages": [{"role":"system",...}, ...], "tools": [{"type":"function","function":{"name","description","parameters"}}]}`；消息映射：`Role::Tool` 带 `tool_call_id`，`assistant` 带 `tool_calls`。解析只取 `choices[0].delta`（content / tool_calls[]）与顶层 `usage`；`finish_reason` 透传。错误人话映射：401→`模型鉴权失败 (HTTP 401)：检查 apiKey/OPENAI_API_KEY`；429→速率限制；5xx→服务端错误（附状态码）；解析失败→`模型响应格式异常`。上限：单次流错误若发生在首块之后不再重试，直接 `Err`。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/runtime
git commit -m "feat(runtime): 增加 OpenAI 兼容流式客户端与脚本化假模型服务"
```

---

### Task 11: runtime::native：执行循环（上限、中止、事件序列）

**Files:**
- Create: `packages/runtime/src/native/mod.rs`
- Modify: `packages/runtime/src/lib.rs`

**Interfaces:**
- Consumes: `OpenAiClient`（Task 10）、契约类型（Task 2）、`test_support`（Task 2/10）。
- Produces:
  - `ignis_runtime::native::NativeRuntime`：`new(client: OpenAiClient) -> Self`；`impl Runtime`，`id() == "native"`，`capabilities() == Capabilities{tools:true, approvals:true, usage:true}`
  - 事件序列契约：请求开始后逐 `TextDelta` → 若有工具调用逐 `ToolExec`/`ToolResult`（按 index 顺序串行）→ 继续下一轮 → 最后恰好一个 `Done`。步数用尽 → `Done{LimitReached, error: Some("max_steps")}`（先发一条 `Usage` 如有）；墙钟超时 → `Done{LimitReached, error: Some("task_timeout")}`；取消 → `Done{Aborted}`；模型/协议错误 → `Done{Failed, error: Some(人话)}`。token 超限（in+out > budget）→ `Done{LimitReached, error: Some("token_budget")}`。
  - 上下文裁剪 `trim_history(messages, char_budget) -> Vec<ChatMessage>`：按“turn（user 起到下一个 user 前）”从最旧整段丢弃，不拆散 tool 结果。

- [ ] **Step 1: 写失败测试（假模型 + MockExecutor）**

```rust
#[tokio::test]
async fn text_only_run_finishes_with_summary() {
    // 脚本 [Text("hello")] → 事件序列 TextDelta* + Done{Success, summary=="hello"}
}
#[tokio::test]
async fn tool_call_is_executed_and_result_fed_back() {
    // 脚本 [ToolCalls{fs_list}, Text("done")]，MockExecutor 返回 "a.txt"
    // → 事件含 ToolExec(summary="Listing .")、ToolResult(ok)
    // → 断言假模型收到的第 2 个请求含 role=tool 且 content=="a.txt"
}
#[tokio::test]
async fn step_limit_stops_loop() { /* max_steps=1，脚本两次 ToolCalls → LimitReached，executor 只被调用 1 次 */ }
#[tokio::test]
async fn cancel_token_aborts_mid_stream() { /* 收到首个 TextDelta 后 cancel → Done{Aborted} */ }
#[tokio::test]
async fn trim_history_drops_oldest_turns_intact() { /* 3 轮对话 + 小预算 → 保留最新 2 轮，tool 消息不孤立 */ }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-runtime native --features test-support`
Expected: 编译失败。

- [ ] **Step 3: 实现**

循环骨架：`loop { steps += 1; 检查步数/token/墙钟; 请求 = 组装(system + trim_history(messages), executor.specs()); 流式消费 → 累积文本与 assembly; 若有 tool_calls: 对每个依次 emit ToolExec{summarize} → executor.execute(call, cancel.clone()) → 追加 assistant(tool_calls)+tool(result) 消息, emit ToolResult; 继续 } else { emit Done{Success, summary:text}; break } }`。整轮用 `tokio::select!` 与 `cancel.cancelled()`、`sleep_until(deadline)` 竞争。事件输出：`tokio::sync::mpsc::channel(64)` + `tokio::spawn` 跑循环，返回 `Box::pin(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|ev| (ev, rx)) }))`（不引入额外 crate）。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/runtime
git commit -m "feat(runtime): 实现 native 执行循环（工具回填、上限、中止、重试）"
```

---

### Task 12: core::task::TaskRunner：上下文组装、JSONL 日志、记账、脱敏

**Files:**
- Create: `packages/core/src/task/{mod.rs,prompts.rs}`、`packages/storage/src/logs.rs`
- Modify: `packages/core/src/lib.rs`、`packages/storage/src/lib.rs`

**Interfaces:**
- Consumes: `Db`（Task 3）、`Config`（Task 4）、`Redactor`/`Policy`（Task 5）、`default_registry`（Task 6）、`ToolRuntime`（Task 9）、`ignis_runtime::{Runtime, RuntimeEvent, RunStatus, RunRequest, ChatMessage, Limits, ToolSpec, Usage}`（Task 2）。
- Produces:
  - `ignis_storage::TaskLog`：`new(dir: &Path)`、`path(&self, task_id: &str) -> PathBuf`、`append(&self, task_id: &str, record: &serde_json::Value) -> Result<()>`（每行一个 JSON；文件追加）
  - `ignis_core::task::{TaskRunner, TaskRequest, TaskOutcome, TaskKind}`：`TaskRunner::new(db: Arc<Db>, config: Arc<Config>, runtime: Arc<dyn Runtime>, policy: Arc<Policy>, approvals: Arc<dyn ApprovalHandler>, log_dir: PathBuf)`
  - `async fn run(&self, req: TaskRequest, sink: UnboundedSender<RuntimeEvent>, cancel: CancellationToken) -> Result<TaskOutcome, CoreError>`
  - `TaskRequest { prompt: String, history: Vec<ChatMessage>, kind: TaskKind /*Run|Repl*/, workspace: PathBuf }`；`TaskOutcome { id: String, status: RunStatus, final_text: Option<String>, usage: Usage, messages: Vec<ChatMessage> }`
  - `prompts::build_system_prompt(workspace: &Path, tools: &[ToolSpec]) -> String`（英文、简洁：人格一行 + 工作区路径 + 工具使用要求 + “用用户的语言回答”）
  - 行为：任务开始写 `tasks` 行（status=running）+ JSONL 首行 `{"type":"task_start",...}`；每个 `RuntimeEvent` 序列化为 JSONL 行（`redactor.redact` 后写入）并转发给 sink；`Usage` 累加；结束时 `finish_task` + JSONL `task_end` 收尾行（内核自己的 `Done` 行保持不变；任何中途 Err 须先 best-effort 把行收成 failed 再上抛）。

- [ ] **Step 1: 写失败测试（MockRuntime + 临时目录）**

```rust
#[tokio::test]
async fn persists_task_row_and_events_in_order() { /* 断言 tasks.status=="success"、JSONL 行序与事件一致、TaskOutcome.final_text */ }
#[tokio::test]
async fn failed_run_records_error() { /* Done{Failed, error:"boom"} → status=="failed"、row.error 含 "boom"、退出状态 Failed */ }
#[tokio::test]
async fn usage_sums_across_events() { /* 两条 Usage(10,5)+(1,2) → outcome.usage==(11,7) */ }
#[tokio::test]
async fn jsonl_is_redacted() {
    // Config.api_key=Some("sk-test-secret")；MockRuntime 的 ToolResult.output 含该串
    // → 文件 !contains("sk-test-secret") 且 contains("***")
}
#[test]
fn system_prompt_mentions_workspace_and_tools() { /* 含工作区路径字符串与工具名 */ }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-core task`
Expected: 编译失败。

- [ ] **Step 3: 实现**

`run` 内：生成 `task_id`（`uuid::Uuid::now_v7()`，展示用短前缀不必要）；构造 `ToolRuntime`（task_id/workspace/limits/redactor 来自 self）；组装 `RunRequest`（`messages = history + [user(prompt)]`，system 用 `build_system_prompt`）；`runtime.run(req, cancel)` 拿流后 `while let Some(ev) = stream.next().await` → 转 JSONL（`{"ts": RFC3339, ...event}`，整行过 `redact`）→ `let _ = sink.send(ev.clone())` → 记账；`Done` 时收尾。`TaskOutcome.messages = history + [user(prompt)] + [assistant(final_text)]`（P1 约定：REPL 跨轮上下文只带最终答复，不带工具调用细节；canonical 会话记录留给 P4 的统一内核切换）。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add packages/core packages/storage
git commit -m "feat(core): 增加 TaskRunner（上下文组装、JSONL 任务日志、记账、脱敏）"
```

---

### Task 13: CLI 模块：流式渲染器 + 审批 UI（可单测）

**Files:**
- Create: `apps/cli/src/{render.rs,approval.rs}`
- Modify: `apps/cli/Cargo.toml`（`clap`、`rustyline`、`anyhow`、`tokio`、`tokio-util`、`futures`、`serde_json`、`tracing`、`tracing-subscriber`、三个内部 crate；dev: `assert_cmd`、`predicates`、`tempfile`、`rusqlite`、`ignis-runtime` feature `test-support`）、`apps/cli/src/main.rs`

**Interfaces:**
- Consumes: `ignis_runtime::{RuntimeEvent, ApprovalRequest, ApprovalDecision, RiskLevel}`（Task 2）、`Policy`（Task 5）、`TaskRunner::run` 的 sink（Task 12）。
- Produces:
  - `ignis_cli::render::Renderer { out: Box<dyn Write + Send>, color: bool }`：`on_event(&mut self, ev: &RuntimeEvent)`；文本增量原样输出；`ToolExec` → `→ {summary}`（dim）；`ToolResult{ok:false}` → `  ✗ {首行/200 字符}`；`Done` 不渲染（结束行由命令层打印）；`fn format_task_end(status: RunStatus, elapsed: Duration, usage: &Usage) -> String`（如 `● Finished: success · 1.2s · tokens in=1234 out=567`，`estimated` 时 token 前加 `~`）
  - `ignis_cli::approval::{InteractiveApproval, resolve_mode, format_prompt, parse_choice, decide_from_lines}`
  - `resolve_mode(yes: bool, unsafe_ops: bool, stdin_is_tty: bool) -> ApprovalMode`：`--unsafe` → `Preauthorized{unsafe_ops:true}`；`--yes` → `Preauthorized{unsafe_ops:false}`；tty → `Interactive`；否则 `Unattended`
  - 提示词格式（锁死）：`Approval required\n  Action: {summary}\n  Risk: {risk}\n[a]pprove  [s]ession-allow  [r]eject  [d]etails\nchoice> `
  - `InteractiveApproval`：实现 `ApprovalHandler`；`tokio::task::spawn_blocking` 里读 stdin 循环：`a`→AllowOnce、`s`→AllowSession、`r`→Reject、`d`→打印 args 的美化 JSON 后再问一轮、其它→重问。另提供 `PolicyOnlyApproval`（恒 Reject；仅当策略已经 Allow/Deny、不会真正被问到时使用）。

- [ ] **Step 1: 写失败测试**

```rust
#[test] fn parse_choice_maps_keys() { /* "a"→Approve, "S"→Session, "r"→Reject, "d"→Details, "x"→None */ }
#[test] fn decide_from_lines_handles_details_then_approve() { /* 输入 ["d","a"]：输出含 "{\n  \"path\"" 且返回 AllowOnce */ }
#[test] fn renderer_prints_tool_lines_and_text() { /* 事件序列 → 输出含 "→ Reading a.txt" 与文本增量 */ }
#[test] fn task_end_line_marks_estimated_tokens() { /* estimated=true → 含 "~in=" */ }
#[test] fn resolve_mode_precedence() { /* (y,u,tty) 全表 */ }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-cli`
Expected: 编译失败。

- [ ] **Step 3: 实现**

ANSI 手写（不引 TUI）：`\x1b[2m` dim、`\x1b[31m` red、重置 `\x1b[0m`；非 tty（`std::io::stdout().is_terminal()`）时 `color=false`。`Renderer` 的 `out` 可注入（测试用 `Vec<u8>`）。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add apps/cli
git commit -m "feat(cli): 增加流式渲染器与交互式审批 UI"
```

---

### Task 14: CLI 命令：`ignis run` 与 REPL

**Files:**
- Create: `apps/cli/src/{cli.rs,commands/run.rs,repl.rs}`、`apps/cli/src/commands/mod.rs`
- Modify: `apps/cli/src/main.rs`

**Interfaces:**
- Consumes: Task 13 的 `Renderer`/审批；Task 12 的 `TaskRunner`；Task 4 的 `load_config`；Task 3 的 `Db`。
- Produces:
  - clap 结构：全局 `--kernel <native>`（P1 仅 native，其它值报错提示 P4 才有）、`--workspace <dir>`、`--yes`、`--unsafe`；子命令 `run <task>`、`task show|list`、`config show|set`、`doctor`；无子命令 → REPL。
  - 退出码：`0` 成功；`1` 任务失败或触限；`130` 用户中止（Ctrl-C）。错误一律 `error: {人话}` 到 stderr，不打印堆栈（`RUST_BACKTRACE` 除外）。
  - `run` 流程：加载配置 → `resolve_mode` → 建 `Db`/`Policy`/`NativeRuntime`（`OpenAiClient::new`，key 取 env 覆盖或配置；再 `with_max_retries(cfg.limits.max_retries)`）→ 起 `tokio::signal::ctrl_c` 任务触发 `CancellationToken` → `TaskRunner::run` + 渲染任务消费 sink → 打印结束行 → 按状态设退出码。
  - REPL 流程：banner（`IGNIS v0.1` / `● Ready` / `Workspace: {path}` / `Runtime: native`）、prompt `❯ `；空行跳过；`/exit`、`/help`；每行输入作为一次 `TaskKind::Repl` 任务，任务结束后把 `outcome.messages` 存回 REPL 历史；Ctrl-C：任务运行中 → `cancel.cancel()`；空闲 → 退出；EOF（Ctrl-D）→ 退出。stdin 非 tty 时拒绝启动 REPL：`error: 交互模式需要 TTY；请用 ignis run`，退出码 1。

- [ ] **Step 1: 写失败单测（退出码映射与 banner）**

`exit_code_for(RunStatus)` 全表（Success→0、Failed→1、LimitReached→1、Aborted→130）；`banner_lines(workspace, kernel)` 输出 4 行且含 "IGNIS v0.1"、"● Ready"。

- [ ] **Step 2: 实现 run 与 REPL，跑单测**

Run: `cargo test -p ignis-cli`
Expected: PASS。

- [ ] **Step 3: 手动冒烟**

Run: `cargo run -p ignis-cli -- --help`（列出 run/task/config/doctor）
Run: `cargo run -p ignis-cli -- run "hi"`（无 key 环境 → `error:` 人话提示，退出码 1）
Run: `cargo run -p ignis-cli`（REPL 出现 banner 与 `❯ `，输入 `/exit` 正常退出）
Expected: 以上全部符合预期，无 panic、无堆栈。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add apps/cli
git commit -m "feat(cli): 增加 run 子命令、REPL 与 Ctrl-C 中止"
```

---

### Task 15: CLI 辅助命令：`task show|list`、`config show|set`、`doctor`

**Files:**
- Create: `apps/cli/src/commands/{task.rs,config.rs,doctor.rs}`、`packages/core/src/doctor.rs`
- Modify: `apps/cli/src/commands/mod.rs`、`packages/core/src/lib.rs`

**Interfaces:**
- Consumes: `Db::{get_task, list_tasks, list_approvals}`（Task 3）、`TaskLog`（Task 12）、`load_config/save_config/ignis_home`（Task 4）。
- Produces:
  - `ignis task show <id>`：任务头（id/status/kind/kernel/workspace/起止/耗时/tokens/error/summary）+ 审批记录表 + 事件流（JSONL 逐行格式化：能解析为 `RuntimeEvent` 的行渲染成 `HH:MM:SS type …`，解析不了的行——如 `task_start`——只展示 `type` 字段）；未找到 → `error: task not found: {id}`，退出码 1。
  - `ignis task list [--limit N=20]`：表格（id 前缀 8 位、status、kind、started、prompt 首 40 字符）。
  - `ignis config show`：打印生效配置（api_key 恒为 `***`）；`ignis config set <dotted.key> <value>`：仅支持标量点路径（如 `model.model`、`limits.max_steps`、`default_kernel`），非法路径报错退出 1；保存后回显新值。
  - `ignis_core::doctor::{CheckResult, run_checks}`：`CheckResult { name: &'static str, ok: bool, detail: String }`；检查项：`home`（`~/.ignis` 存在且可写）、`config`（可解析）、`api_key`（env 或文件有值）、`base_url`（TCP 连接主机:端口，5s 超时）、`workspace`（目录存在）、`db`（打开 + 迁移成功）。
  - `ignis doctor`：逐行 `✓/✗ {name}: {detail}`；任一 ✗ → 退出码 1。

- [ ] **Step 1: 写失败测试**

```rust
#[test] fn doctor_reports_unreachable_base_url() { /* base_url=127.0.0.1:1 → base_url 检查 ok=false，detail 含地址 */ }
#[test] fn doctor_all_checks_pass_with_temp_home() { /* 起本地 TcpListener 当 base_url → 全 ✓ */ }
#[test] fn config_show_redacts_key() { /* 生效配置含 key → 输出不含明文 */ }
#[test] fn task_show_output_contains_status_and_events() { /* 用临时 Db + TaskLog 造数据，断言输出行 */ }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p ignis-core doctor && cargo test -p ignis-cli`
Expected: 编译失败。

- [ ] **Step 3: 实现**

`config set` 用白名单匹配点路径（serde_json 中间表示读写，避免反射）；保存走 `save_config`（保持 0600）。`task show` 的事件格式化复用 `ignis_runtime::RuntimeEvent` 的反序列化（JSONL 行含 `ts`，先取 `ts` 再解析其余字段）。

- [ ] **Step 4: 收口验证并提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿。
```bash
git add apps/cli packages/core
git commit -m "feat(cli): 增加 task show/list、config show/set 与 doctor"
```

---

### Task 16: 夹具与端到端验收（Demo A + 审批模式 + 上限）

**Files:**
- Create: `examples/broken-project/{package.json,src/config.js,config/app.json,test/config.test.js}`
- Create: `apps/cli/tests/{demo_a.rs,approval_modes.rs,limits.rs,common/mod.rs}`

**Interfaces:**
- Consumes: 全部前序任务；`ignis_runtime::test_support::ScriptedModel`（Task 10）。
- Produces: 夹具项目与三个 e2e 测试；`common` 提供 `copy_dir`、`read_approvals(db_path, task_id)`（rusqlite 直读）、`run_npm_test(dir) -> ExitStatus`、`ignis_cmd(home, workspace, base_url) -> assert_cmd::Command`（统一注入 `IGNIS_HOME/IGNIS_WORKSPACE/IGNIS_BASE_URL/IGNIS_MODEL=test-model/IGNIS_API_KEY=test-key`）。

- [ ] **Step 1: 写夹具（零依赖 Node 项目，bug = 配置路径假设错误）**

`package.json`：`{"name":"broken-project","private":true,"scripts":{"test":"node --test"}}`
`src/config.js`（**错误版本**，`'..','..'` 比正确路径多退一层）：
```js
const fs = require('node:fs');
const path = require('node:path');
function loadConfig() {
  const configPath = path.join(__dirname, '..', '..', 'config', 'app.json');
  return JSON.parse(fs.readFileSync(configPath, 'utf8'));
}
module.exports = { loadConfig };
```
`config/app.json`：`{"appName":"broken-project","port":8080}`
`test/config.test.js`：`node:test` + `assert`，断言 `loadConfig().appName === "broken-project"` 与 `port === 8080`。
手工确认：在 `examples/broken-project` 下 `npm test` 失败（ENOENT），把 `'..','..'` 改成 `'..'` 后通过。

- [ ] **Step 2: 写 `demo_a.rs`（Demo A 端到端）**

脚本（假模型 5 步）：① `ToolCalls(fs_read{path:"src/config.js"})` ② `ToolCalls(shell_run{command:"npm test"})` ③ `ToolCalls(fs_edit{path:"src/config.js", old_string:"'..', '..'", new_string:"'..'"})` ④ `ToolCalls(shell_run{command:"npm test"})` ⑤ `Text("已修复配置路径，测试通过。")`。
断言：二进制退出码 0；stdout 含 `→ Reading src/config.js` 与 `→ Running: npm test`；复制出的临时工作区里 `npm test` 退出码 0；`requests().len()==5` 且第 3 个请求（索引 2）的消息里含 `role=="tool"` 且内容含 `ENOENT` 与 `exit code: 1`（reporter 无关的失败证据；Node ≥21 默认 spec reporter 下不再出现 `not ok`）（工具结果确实回填）；`approvals` 中该任务恰有 3 条 `allow/preauthorized`（fs_edit + 两次 shell_run，fs_read 为 Safe 不记录）；JSONL 存在且 `!contains("test-key")`。

- [ ] **Step 3: 写 `approval_modes.rs`（fail-closed 与危险命令阶梯）**

三个子测试，各用独立临时工作区：
- `no_flags_denies_everything`：无 `--yes/--unsafe`（且 stdin 非 tty）→ 脚本 `[fs_write 后 Text]`；断言：文件**不存在**、`approvals.decision=="deny"` 且 `mode=="unattended"`、退出码 0（任务本身完成）。
- `yes_allows_write_but_not_dangerous`：`--yes`，工作区预建 `guarded/` 目录 → 脚本 `[fs_write{path:"ok.txt"...}, shell_run{command:"rm -rf guarded"}, Text]`；断言：`ok.txt` 存在、`guarded/` 仍存在、`rm -rf` 对应记录 `deny`。
- `unsafe_executes_dangerous_in_sandbox`：`--unsafe`，工作区预建 `guarded/` → 脚本 `[shell_run{command:"rm -rf guarded"}, Text]`；断言：`guarded/` 消失、对应记录 `allow`。

- [ ] **Step 4: 写 `limits.rs`（上限与失败可见）**

在 `IGNIS_HOME/config.json` 写 `{"limits":{"max_steps":1}}`；脚本为恒返回 `ToolCalls(fs_list)`（多步脚本）；断言：退出码 1；`tasks.status=="limit_reached"`、`error` 含 `max_steps`；JSONL 末行 `done` 状态为 `limit_reached`。

- [ ] **Step 5: 全量验证并提交**

Run: `cargo build -p ignis-cli && cargo test -p ignis-cli --test demo_a --test approval_modes --test limits && cargo test --workspace`
Expected: 全绿（e2e 依赖本机 `npm`；node v26 / npm 11 已核实可用）。
```bash
git add examples apps/cli
git commit -m "test(e2e): 增加 broken-project 夹具与 Demo A、审批模式、上限端到端测试"
```

---

## 覆盖与自评

- 设计文档章节 → 任务映射：§3（环境/技术栈）→Task 1；§4#7/#15/#16/#20（CLI、非交互审批、依赖方向、成本单位）→Task 5/9/13/14/15；§6.1（CLI 命令面，P1 子集）→Task 14/15；§6.2（native 循环、上限、中止、重试）→Task 10/11；§6.3（Runtime trait，native 实现）→Task 2/11；§6.4（工具与安全）→Task 5-9；§6.8（存储/配置/日志/记账，P1 子集）→Task 3/4/12/15；§7 Demo A→Task 16；§8（单元/集成/e2e，P1 子集）→各任务测试；§10 风险 1/3（spike）→Task 1/3。
- 明确推迟到 P2-P4 的：memory/skills、scheduler/daemon/notify、codex/claude/`/kernel`、`ignis memory|skills|schedule|alerts`、Docker/systemd/README/验收 harness。
- 任务间的名字与签名已在各任务 Interfaces/Produces 中固定；执行中如需改动，先改本计划再改代码。
