# Ignis v0.1 阶段二（P2）：记忆与技能 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**目标：** 让 Ignis 会「记住」与「复用」——跨会话持久记忆（纠正后旧信息失效、project 事实无条件注入）与兼容 SKILL.md 的技能系统（内置三技能 + 用户目录、渐进披露、`load_skill` 执行），并以双进程 e2e 复现 Demo B。

**架构：** `facts` 表（迁移 002）+ FTS5(trigram) 触发器同步；记忆逻辑在 `ignis-core::memory`（supersede、检索、Markdown 镜像、fail-soft）；技能逻辑在 `ignis-core::skills`（内置 `include_str!` 嵌入 + `~/.ignis/skills/` 扫描、frontmatter 解析、渐进披露目录）；`remember`/`load_skill` 两个元工具经现有 `ToolRuntime`（显式 Safe 分类）注册；`TaskRunner` 组装 system prompt 时注入「已知事实」与技能目录，并从事件流收集实际使用的技能名。

**技术栈：** 沿用 P1（**零新依赖**：内置技能用 std `include_str!`；frontmatter 手写解析——include_dir/rust-embed/serde_yaml 均不在离线缓存）。

**设计文档（Spec）：** `docs/superpowers/specs/2026-10-15-ignis-v0.1-design.md` §6.4（元工具）、§6.5（Memory）、§6.6（Skills）、§7 Demo B；冲突以设计文档为准。**前置：** P1 完成（HEAD e3e6ff6，174 测试全绿）。

## 全局约束

继承 P1 全部约束（edition 2024；依赖方向 `core → {runtime, storage}`；提交 Conventional Commits 中文 body + `Co-authored-by: CommandCodeBot <noreply@commandcode.ai>` 尾注；收口三连 `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`）。P2 新增：

- **零新依赖**（本机离线）。内置技能 = `const BUILTIN_SKILLS: &[(&str, &str)]`，用 `include_str!("../../../../skills/<name>/SKILL.md")`（**内容文件必须随代码同任务先行落盘，否则编译失败**）。
- **事实模型（锁死，迁移 002）**：`facts(id INTEGER PK AUTOINCREMENT, scope TEXT[user|project|global], workspace TEXT NULL, key TEXT NOT NULL, text TEXT NOT NULL, status TEXT[active|superseded], superseded_by INTEGER NULL, source_task_id TEXT NULL, created_at INTEGER, updated_at INTEGER)`；`scope=project` 必须带 `workspace`（工作区绝对路径字符串）。FTS：`CREATE VIRTUAL TABLE facts_fts USING fts5(text, content='facts', content_rowid='id', tokenize='trigram')` + ai/ad/au 三触发器同步 + 索引 `(scope, workspace, status)`。
- **supersede 规则（锁死）**：同 `(scope, workspace, key)` 写新值 → 旧 active 行 `status='superseded'、superseded_by=新 id`；检索/注入/镜像只取 `active`。
- **注入规则（锁死 = spec 决策 #21）**：当前工作区的 active `project` 事实**无条件注入**；`user`/`global` 按 FTS 检索（查询 = 任务 prompt；`chars().count() < 3` 时跳过 FTS 仅保留 project 段）取 bm25+新近度前 6 条。段标题英文 `Known facts:`；预算：project 段 ≤2000 字符、检索段 ≤2000 字符（截断附 `… [facts truncated]`）。
- **技能约定（锁死）**：每技能一目录含 `SKILL.md`（frontmatter `name`/`description` + 正文）；来源 `builtin`/`user`；**同名 user 覆盖 builtin**；畸形技能跳过 + warning（`skills list` 以 `! {msg}` 展示）；目录注入系统提示（`- name: description`，user 来源标 `(user)`，单行截 200 字符）；正文经 `load_skill` 加载。
- **元工具分类**：`remember`/`load_skill` 在 `ToolRuntime::classify` 显式 `Safe`（不弹审批、不落审批行；理由：agent 自有状态、可经 CLI 删除/修改）。
- **P1 结转（本阶段做掉）**：`PRAGMA busy_timeout=5000`；迁移在 `user_version > MIGRATIONS.len()` 时报错；热 DAO 用 `prepare_cached`；`~/.ignis` 以 0700 创建（`core::config::ensure_home`）；`http_fetch` 输出再经 `tool_output_limit` 截断。
- 测试用真实临时目录/真实 SQLite；e2e 只走 `CARGO_BIN_EXE_ignis`；不得触碰真实 `~/.ignis`。

## 评审重点（Review Focus）

1. **纠正后的旧值绝不再出现**：supersede 后旧文本不得再进入任何检索/注入（含同 key 连续两次纠正、镜像同步、缓存无残留）；测试在 T1/T2/T7。
2. **project 事实按工作区隔离**：A 工作区的 project 事实不得注入 B（同 key 不同 workspace 并存）；测试在 T2/T5。
3. **短查询与中文检索不丢**：`<3` unicode 字符查询走 LIKE 回退（不误命中、不报错）；中文 ≥3 字符子串命中；测试在 T1/T2。
4. **畸形技能不破坏系统**：无 frontmatter、超长描述、同名冲突、描述含换行/控制字符 —— 跳过并警告、绝不 panic，prompt 段结构不被破坏；测试在 T3。
5. **记忆是 fail-soft**：镜像目录不可写、检索出错时任务照常执行（注入缺席而非失败），错误有可见记录；测试在 T2/T5。

## 后续阶段

P3 调度与守护（Demo C）、P4 兼容内核与交付（codex/claude、doctor 完整版、Docker/systemd、README、验收 harness）。

---

### Task 1: storage 加固（P1 结转）+ facts 迁移 002（FTS5 触发器）+ ensure_home

**Files:**
- Modify: `packages/storage/src/{db.rs,migrations.rs,tasks.rs,approvals.rs}`、`packages/core/src/config.rs`、`apps/cli/src/commands/mod.rs`（调用 `ensure_home`）
- Create: `packages/storage/src/facts.rs`
- Test: 内联 + `packages/storage/tests/facts_fts.rs`

**Interfaces:**
- Consumes: P1 的 `Db`/迁移框架/`with_conn`。
- Produces:
  - `NewFact {scope: String, workspace: Option<String>, key: String, text: String, source_task_id: Option<String>}`；`FactRecord {id: i64, scope: String, workspace: Option<String>, key: String, text: String, status: String, superseded_by: Option<i64>, created_at: i64, updated_at: i64}`
  - `Db::{insert_fact(&NewFact) -> Result<i64>, supersede_fact(&self, old_id: i64, by_id: i64) -> Result<()>, active_fact_by_key(&self, scope: &str, workspace: Option<&str>, key: &str) -> Result<Option<FactRecord>>, active_facts_in_workspace(&self, workspace: &str) -> Result<Vec<FactRecord>>, list_facts(&self, include_superseded: bool) -> Result<Vec<FactRecord>>, search_facts(&self, query: &str, limit: usize) -> Result<Vec<FactRecord>>, get_fact(&self, id: i64) -> Result<Option<FactRecord>>, update_fact_text(&self, id: i64, text: &str) -> Result<()>, delete_fact(&self, id: i64) -> Result<()>}`
  - 迁移 002 DDL（facts + facts_fts + 三触发器 + 索引）；`search_facts` 内部：`query.chars().count() >= 3` → `facts_fts MATCH`（bm25 排序）；否则 `LIKE '%…%'` 回退（参数绑定、转义 `%`/`_`）
- [ ] **Step 1: 写失败测试**
```rust
// 迁移：user_version==2；幂等二次 open；
// 触发器：insert → MATCH '项目使用'==1；update_fact_text → 旧文本 MATCH==0；delete → 消失
// supersede_fact：旧行 superseded + superseded_by=新 id；active_fact_by_key 只回 active
// search：2 字符查询走 LIKE（命中子串且不含 `%` 注入）；三字符中文命中
// 加固：PRAGMA busy_timeout==5000；user_version=99 → open Err 含版本号
```
- [ ] **Step 2: 运行确认失败**（Run: `cargo test -p ignis-storage`，Expected: 编译失败/表不存在）
- [ ] **Step 3: 实现**（迁移 002 按 Interfaces 原文；`db.rs` 加 busy_timeout + 版本守卫；热 DAO 换 `prepare_cached`；`core::config::ensure_home(home) -> Result<()>`（create_dir_all + unix 0700）并在 CLI `Session` 起始处调用）
- [ ] **Step 4: 收口并提交**
```bash
git add packages/storage packages/core apps/cli
git commit -m "feat(storage): 加固存储并新增 facts 表与 FTS5 触发器（迁移 002）" -m "Co-authored-by: CommandCodeBot <noreply@commandcode.ai>"
```

---

### Task 2: core::memory（Fact 语义、supersede、检索、Markdown 镜像、fail-soft）

**Files:**
- Create: `packages/core/src/memory/{mod.rs,store.rs,mirror.rs}`
- Modify: `packages/core/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 DAO；`Config`（P1）。
- Produces:
  - `MemoryStore::new(db: Arc<Db>, mirror_dir: PathBuf) -> Arc<MemoryStore>`
  - `FactScope::{User, Project, Global}`（serde lowercase）；`impl FactScope { fn as_str(&self) -> &'static str }`
  - `remember(&self, scope: FactScope, workspace: Option<&Path>, key: &str, text: &str, source_task_id: Option<&str>) -> Result<FactRecord, MemoryError>`（内部：查 active 同 key → `supersede_fact` → `insert_fact`；空 key/text → Err；成功/失败均 **best-effort** 重建受影响镜像）
  - `prompt_facts(&self, workspace: &Path, prompt: &str) -> Result<Vec<FactRecord>, MemoryError>`（注入规则锁死：project 无条件 + FTS 前 6，去重按 id）
  - `search(&self, query: &str, limit: usize) -> Result<Vec<FactRecord>, MemoryError>`；`list(&self, include_superseded: bool)`；`edit(&self, id: i64, text: &str)`；`delete(&self, id: i64)`；`rebuild_mirror(&self) -> Result<(), MemoryError>`
  - 镜像：`{mirror_dir}/{user|global}.md`、`{mirror_dir}/project-{sanitized}.md`（`/`→`_`；首行 `# 项目记忆：{workspace}`；每条 `- [{key}] {text}（{updated_at}）`；0600；仅 active）
  - `MemoryError`（thiserror；含 `not found`）
- [ ] **Step 1: 写失败测试**（supersede 链两次纠正后 search/prompt_facts 只回最新；workspace A/B 隔离；中文 MATCH 与 2 字符 LIKE；镜像含 active、无 superseded、目录只读时 `remember` 仍 `Ok` 且文本仍已入库）；**Step 2: 失败**；**Step 3: 实现**；**Step 4: 收口并提交**（`feat(core): 增加记忆存储（supersede、检索、注入与 Markdown 镜像）`）

---

### Task 3: core::skills（内置三技能内容 + 目录扫描 + frontmatter + 渐进披露）

**Files:**
- Create: `skills/project-analyzer/SKILL.md`、`skills/server-health/SKILL.md`、`skills/report-generator/SKILL.md`（**先落盘**）
- Create: `packages/core/src/skills/{mod.rs,catalog.rs,frontmatter.rs}`
- Modify: `packages/core/src/lib.rs`

**Interfaces:**
- Produces:
  - `const BUILTIN_SKILLS: &[(&str, &str)]`（三个 `include_str!`）
  - `SkillMeta {name: String, description: String, source: SkillSource}`；`SkillSource::{Builtin, User}`；`Skill {meta: SkillMeta, body: String}`
  - `SkillCatalog::load(user_dir: &Path) -> Arc<SkillCatalog>`（用户目录缺失不报错）；`metas(&self) -> &[SkillMeta]`（同名 user 覆盖 builtin；按名称排序）；`get(&self, name: &str) -> Option<&Skill>`；`warnings(&self) -> &[String]`
  - `frontmatter::parse(text: &str) -> Option<(String, String, String)>`：`---` 围栏 + `key: value`；值去首尾引号；无围栏/缺 name → None；描述内换行折叠为空格、控制字符剔除
  - `skills_prompt_section(metas: &[SkillMeta]) -> String`（`Available skills:` 段；每行 `- {name}: {desc}`（user 加 ` (user)`；desc 截 200 字符））
  - SKILL.md 内容（中文正文，步骤只用现有工具；`name`/`description` 英文）：project-analyzer（结构→跑测试→定位失败→修复建议）、server-health（df -h / free -m / docker ps，缺失工具记明并继续）、report-generator（固定小节的中文 Markdown 报告格式）
- [ ] **Step 1: 写失败测试**（frontmatter 正常/无围栏/缺 name/引号/多行描述折叠；catalog：3 个内置 + 临时 user 目录覆盖同名 + 畸形文件 warning 不计入；`skills_prompt_section` 行格式与截断；三个内置正文非空）；**Step 2: 失败**；**Step 3: 实现**；**Step 4: 收口并提交**（`feat(core,skills): 增加技能目录与内置三技能（嵌入、扫描、渐进披露）`）

---

### Task 4: 元工具 remember + load_skill（registry 公开注册 + classify Safe）

**Files:**
- Create: `packages/core/src/tools/meta.rs`
- Modify: `packages/core/src/tools/{mod.rs,runtime.rs}`

**Interfaces:**
- Consumes: Task 2 `MemoryStore`、Task 3 `SkillCatalog`；P1 `Tool`/`ToolCtx`/`Registry`/`ToolRuntime::classify`。
- Produces:
  - `Registry::register(&mut self, tool: Box<dyn Tool>)` 公开（P1 私有 → 公开）
  - `pub fn meta_tools(memory: Arc<MemoryStore>, skills: Arc<SkillCatalog>) -> Vec<Box<dyn Tool>>`（remember + load_skill）
  - `remember`：args `{scope: "user"|"project"|"global", key: string, text: string}`（project 用 `ToolCtx.workspace`；user/global 忽略 workspace）；返回 `已记住（{scope}/{key}）：{text}`，发生 supersede 附 `（旧值已失效）`；参数缺失/非法 scope → `ok:false` 中文
  - `load_skill`：args `{name: string}`；命中 → SKILL.md 全文（经 `tool_output_limit`）；未命中 → `ok:false` 列出可用名称
  - `ToolRuntime::classify`：`remember`/`load_skill` 显式 `Safe`（先于敏感路径与回退臂）
- [ ] **Step 1: 写失败测试**（经 `ToolRuntime::execute`：remember Safe→无审批行、同 key 二次→旧行 superseded、project 落当前 workspace、非法 scope→ok:false；load_skill 命中正文/未命中列名；classify 表新增两臂）；**Step 2: 失败**；**Step 3: 实现**；**Step 4: 收口并提交**（`feat(core): 增加 remember/load_skill 元工具（Safe 分类与公开注册）`）

---

### Task 5: TaskRunner 注入与组合（Known facts + Skills 段、registry 组装、skills_used、fail-soft）

**Files:**
- Modify: `packages/core/src/task/{mod.rs,prompts.rs}`、`apps/cli/src/commands/mod.rs`（构造点）、`apps/cli/tests/*`（构造点）

**Interfaces:**
- Consumes: Task 2/3/4 产物。
- Produces:
  - `TaskRunner::new(db, config, runtime, policy, approvals, log_dir, memory: Arc<MemoryStore>, skills: Arc<SkillCatalog>)`（签名扩展，调用点与测试同步更新）
  - `TaskRunner::run`：组装期调用 `memory.prompt_facts(&workspace, &prompt)` —— **任何 Err 只记 `eprintln!` 警告并继续**（空事实集）；registry = `default_registry()` + `meta_tools(memory, skills)` 注册
  - `prompts::build_system_prompt(workspace, tools, facts: &[FactRecord], skills: &[SkillMeta]) -> String`：英文骨架 + `Known facts:` 段（project 在前、检索在后，预算 2000+2000 截断）+ `Available skills:` 段
  - `TaskOutcome` 增加 `skills_used: Vec<String>`：事件流中按序去重收集 `ToolExec{name == "load_skill"}` 的 `args.name`
- [ ] **Step 1: 写失败测试**（system prompt 含注入事实与技能行；A/B 工作区隔离经 TaskRunner 再验；`prompt_facts` 出错时任务仍 Success 且系统提示无 facts 段；skills_used 去重有序；无 Done 时 skills_used 仍收集已见项）；**Step 2: 失败**；**Step 3: 实现**；**Step 4: 收口并提交**（`feat(core): 任务上下文注入已知事实与技能目录，收集已用技能`）

---

### Task 6: CLI：memory/skills 子命令 + banner 计数 + 端行 skills 后缀

**Files:**
- Create: `apps/cli/src/commands/{memory.rs,skills.rs}`
- Modify: `apps/cli/src/{cli.rs,render.rs,repl.rs}`

**Interfaces:**
- Consumes: Task 2/3/5。
- Produces:
  - `ignis memory list [--all]`（active 默认；列 `id scope key text(60) updated_at`）；`search <query>`；`add --scope <user|project|global> --key <k> <text>`（project → 当前工作区）；`edit <id> <text>`；`delete <id>`；未找到 → `error: memory not found: {id}` 退出 1
  - `ignis skills list`（`{name}  {source}  {description}`；warnings `! {msg}`）
  - REPL banner 第 4 行：`Runtime: native · Skills: {n} · Memory: {active_facts} facts`（无事实 `Memory: off`）
  - `format_task_end` 追加 ` · skills: {a,b}`（`skills_used` 非空时；沿用 Task 16 修订后的签名与断言风格）
- [ ] **Step 1: 写失败测试**（memory 行/技能行/banner/端行后缀格式化单测；`memory add` 走 store 后 list 可见）；**Step 2: 失败**；**Step 3: 实现**；**Step 4: 手动冒烟**（`memory add/list/search/edit/delete` 与 `skills list` 贴输出）；**Step 5: 收口并提交**（`feat(cli): 增加 memory/skills 子命令、banner 计数与端行技能后缀`）

---

### Task 7: Demo B 夹具与双进程 e2e

**Files:**
- Create: `examples/uv-project/{pyproject.toml,app/main.py,tests/test_app.py}`
- Create: `apps/cli/tests/demo_b.rs`（复用 `common/mod.rs`）

**Interfaces:**
- Consumes: 全部前序；`ScriptedModel`；`common::ignis_cmd`/`TempDir`/`latest_task`/`read_approvals`（P1）。
- 夹具：极简 uv 风格项目（pyproject `[project]` + 一个可被收集的测试；**不要求 uv/pytest 存在**）。
- e2e 四段（各自独立进程，共享同一 `IGNIS_HOME` 与工作区副本）：
  1. **记住**：`ignis run "这个项目使用 uv 管理 Python 环境，请记住" --yes`；脚本 `remember{scope:"project",key:"pkg_manager",text:"使用 uv 管理 Python 环境"}` → `Text`；断言 facts active、workspace 匹配、镜像文件含文本。
  2. **复用**：新进程 `ignis run "帮我检查这个项目的依赖并运行测试" --yes`；脚本 `shell_run{"command":"uv sync"}`（真实执行可失败，断言只看工作流选择）→ `Text`；断言该请求 `system_prompt` 含 uv 文本（**与关键词无关的无条件注入**）、JSONL 含 `uv sync`、任务 success。
  3. **纠正**：新进程 `ignis run "不对，这个项目改用 pip 了" --yes`；脚本 `remember`（同 key 新文本）→ `Text`；断言旧 fact `superseded` 且 `superseded_by` 指向新行。
  4. **纠正生效**：新进程任意任务 → `system_prompt` 含 pip 文本、**不含** uv 文本。
  5. **技能发现与执行（验收 #5）**：新进程 `ignis run "检查一下服务器健康状况" --yes`；脚本 `load_skill{name:"server-health"}` → `shell_run{"command":"df -h"}` → `Text`；断言第 2 个请求的 messages 含 server-health 的 SKILL 正文（技能已被模型读到）、`skills_used == ["server-health"]`、端行含 `skills: server-health`。
- [ ] **Step 1: 写夹具并手工确认**（文件齐全、pyproject 可解析）；**Step 2: 写 `demo_b.rs`**；**Step 3: 运行**（Run: `cargo test -p ignis-cli --test demo_b`，Expected: PASS）；**Step 4: 收口并提交**（`test(e2e): 增加 uv-project 夹具与 Demo B 双进程记忆测试`）

---

## 覆盖与自评

- 映射：§6.5 → T1/T2/T5/T6/T7；§6.6 → T3/T4/T5/T6/T7；§6.4（remember/load_skill）→ T4；§7 Demo B → T7；决策 #21 → T2/T5/T7；P1 结转（busy_timeout/版本守卫/prepare_cached/0700/http_fetch 截断）→ T1。
- 排序约束（预检发现并已修正）：技能内容文件必须先于目录代码（`include_str!`）；`load_skill` 必须先于 TaskRunner 的注册表组合（否则引用不存在）；`list_facts` 与公开 `Registry::register` 已在 T1/T4 补齐。
- 计划内**不做**：向量检索、自动摘要、技能自动生成、`/memory` REPL 命令（非目标）。
