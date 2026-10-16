# Ignis v0.1 阶段四（P4）：兼容内核与交付 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**目标：** 把 Ignis 变成「可交付的完整仓库」——codex / claude 两套兼容内核（浅兼容：外置内核用自家沙箱，Ignis 负责调度、超时、中止、日志、记忆注入与通知）、内核运行时切换与能力门控、`ignis doctor` 完整版、Docker / systemd 部署示例、中文为主的 README、可选的量化验收 harness，以及真机 relay 结论的收口。

**架构：** 两个新 Runtime 实现落 `ignis-runtime`（`codex.rs`/`claude.rs`：spawn 外部 CLI、解析其 JSONL/stream-json、映射为统一 `RuntimeEvent` 流；能力声明 `tools:false, approvals:false, usage:false`）；内核选择与外部内核的 prompt 渲染（canonical 会话 → 单串）在 core/CLI；交付物（README/Dockerfile/systemd/harness）在仓库根与 `deploy/`、`harness/`。

**技术栈：** 沿用 P1–P3（**零新依赖**；外部 CLI 仅探测与 spawn；测试用可执行假 CLI shell 脚本产出固定 JSONL/stream-json）。

**设计文档（Spec）：** `docs/superpowers/specs/2026-10-15-ignis-v0.1-design.md` §6.1（doctor/`/kernel`）、§6.3（Runtime 接口与三个内核、能力边界、用量缺失降级）、§6.9（部署）、§7/§8（验收清单与测试策略）、§9（构建者选择）、§10（风险 4/5）；`demo.md` 为产品验收清单。**前置：** P3 完成（HEAD 718836e，432 测试全绿 + Demo C）。

## 全局约束

继承 P1–P3 全部约束（edition 2024；依赖方向 `core → {runtime, storage}`、`gateway → core`；提交规范与尾注；收口三连 `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`）。P4 新增：

- **零新依赖**（含 Docker/systemd/harness：只用仓库已有工具与 shell）。
- **能力边界（锁死 = 决策 #4）**：三内核的记忆「读」（prompt 注入）一致；记忆「写」（`remember`）与技能全文加载（`load_skill`）完整能力仅 native；外置内核的 system prompt 中工具清单**必须为空并附一句说明**（Ignis 工具对外置内核不可用），技能目录仍以列表注入（可自行读 SKILL.md，尽力而为）；外置内核用量缺失 → 上限退化为墙钟+步数，CLI/README 明说。
- **外置内核沙箱（锁死）**：codex 以 `codex exec --json -C <ws> --sandbox workspace-write --skip-git-repo-check <prompt>` 启动（确切参数与事件 schema 以**开工实测本机已装版本**为准，记录到 `docs/spikes/2026-10-15-external-kernels-probe.md`）；claude 以 `claude -p --output-format stream-json`（cwd=工作区，`--permission-mode` 取安全基线）启动；两者都：stdout 逐行 JSON 解析、未知事件宽容跳过、进程组终止（SIGKILL pgid）、非零退出 → `Done{Failed, error: 人话}`、缺失二进制 → 构造期清晰错误。
- **canonical 会话（锁死 = 决策 #11）**：不依赖外部 CLI 自身的 `--resume`；core 把 system prompt + 历史渲染为**单个 prompt 字符串**交给外置内核（渲染函数 `render_external_prompt(system_prompt, &[ChatMessage]) -> String`，只含 role 标注与文本，工具调用/结果以紧凑文本行呈现）。
- **内核切换（锁死 = 决策 #11）**：`--kernel native|codex|claude`（全局参数，作用于本次任务与后续 REPL 任务）、REPL `/kernel <name>`（显示能力提示行，如 `note: kernel=codex — tools run inside codex's sandbox; remember/load_skill unavailable; usage may be missing`）。
- **doctor（锁死）**：在现有六项上追加 `codex`/`claude` 检查（二进制存在 + `--version` 输出；缺失显示安装提示；选择非 native 内核时对应缺失 = ✗，否则为警告行 `!`）；`native` 恒 ✓。
- **交付物（锁死）**：README（中文为主 + 英文摘要；干净 Ubuntu 安装主线、快速开始、三演示手动脚本、安全模型、限制）；`Dockerfile`（多阶段：rust builder → slim 运行镜像，只带二进制，技能已嵌入；`VOLUME /root/.ignis`）；`deploy/ignis-daemon.service`（`ExecStart=… ignis daemon start --foreground`）；`deploy/README` 一句话说明。
- **验收 harness**：`harness/acceptance.sh`（手动、可选、需真实模型环境变量；三个模式：`demos N`（三演示各连续 N 次，默认 3）/`tasks N`（`harness/tasks.md` 的 20 例，默认全跑，成功率门槛 ≥80%）/`restarts N`（默认 10，调度重启不重复/不丢记录）），输出汇总表并落 `harness/results-<ts>.md`；CI 不依赖网络与模型（harness 不进 `cargo test`）。
- **真机 relay spike 收口**：本任务组第一步重试「流式+工具调用」探测（额度若恢复）；把结论（成功/仍不可得/降级决定）写入既有 spike 文档并同步 README 的「已知限制」。

## 评审重点（Review Focus）

1. **外置内核不越权**：codex 的沙箱开关与 claude 的 permission-mode 以实测为准且写进探测记录；适配器不得自行放开（如 `danger-full-access`）；缺失/未知事件不崩溃；测试落在 T1/T2（fake CLI 的恶意/未知行用例）。
2. **外置内核下 Ignis 的「安全承诺」不撒谎**：system prompt 不含 Ignis 工具清单（模型不会尝试调用不存在的工具）、记忆注入仍在、用量缺失时上限退化且有提示；测试落在 T3。
3. **canonical 渲染保真**：多轮历史（含工具调用与结果）渲染为单串后信息不丢失（工具名/关键参数/结果首行保留）；测试落在 T3。
4. **交付物真能跑**：README 的安装步骤在干净环境可复制（至少在本机以 `cargo install --path apps/cli` + `ignis doctor` 冒烟）；Docker 构建冒烟可执行（本机 Docker 可用；步骤写进 README）；systemd 单元路径正确；测试/冒烟落在 T5。
5. **doctor 诚实**：外部 CLI 缺失/版本漂移时给可执行提示而非静默 ✓；测试落在 T4。

## 后续

P4 完成后：全仓库终审（P4 分支终审）→ 收尾（Rulings 汇总交付、工作区清理）。

---

### Task 1: codex 适配器（含真机探测）

**Files:** Create `packages/runtime/src/codex.rs`、`docs/spikes/2026-10-15-external-kernels-probe.md`；Modify `packages/runtime/src/lib.rs`；Test: 内联 + `packages/runtime/tests/fake_cli.rs`（或复用 test_support）+ `scripts/fake-codex`（测试夹具 shell 脚本）

**Interfaces:**
- Consumes: P1 契约（`Runtime`/`RuntimeEvent`/`RunRequest`）。
- Produces: `ignis_runtime::codex::CodexRuntime`：`new(workspace: PathBuf, model: Option<String>) -> Self`；`impl Runtime`，`id() == "codex"`，`capabilities() == Capabilities{tools:false, approvals:false, usage:false}`；`run(req, cancel)`：spawn `codex exec --json -C <ws> --sandbox workspace-write --skip-git-repo-check <rendered prompt>`（渲染经 T3 的 `render_external_prompt`——先以 `req.system_prompt + 渲染消息` 直拼作为 T1 的过渡实现，T3 落地后改用共享函数）、按行解析 JSONL：已知事件映射（逐行对照探测记录）、未知行跳过、`stdout` 结束/非零退出 → `Done{Failed}`、取消 → 杀进程组 + `Done{Aborted}`、超时（`req.limits.task_timeout_secs`）同样处理；缺失 `codex` 二进制 → `new()` 或 `run()` 的清晰中文错误。
- 探测（Step 0）：`codex --version`、`codex exec --json "说一句 ok"`、观察事件 schema；把**实际命令、事件样例（脱敏）与沙箱开关**写入 spike 文档；若 CLI 不可用或额度不足 → 记录并改按「假 CLI 单测 + 文档待验」推进（不得阻塞）。

- [ ] **Step 1: 探测并记录**（含 `codex exec --help` 的沙箱/JSON 开关确认）
- [ ] **Step 2: 写失败测试**（fake-codex 脚本产出探测到的真实 schema 样例：正常文本事件、工具/命令事件（只观察）、未知行、非零退出、挂起被取消；断言映射为 RuntimeEvent 序列与 `Done` 状态；缺失二进制错误）
- [ ] **Step 3: 失败确认 → 实现 → 全绿**
- [ ] **Step 4: 收口并提交**（`feat(runtime): 增加 codex 兼容内核适配器`）

---

### Task 2: claude 适配器（含真机探测）

**Files:** Create `packages/runtime/src/claude.rs`；Modify `lib.rs`、spike 文档（追加 claude 段）；Test: 内联 + fake-claude 脚本

**Interfaces:** 同 T1 形状：`ignis_runtime::claude::ClaudeRuntime`，`id() == "claude"`，`capabilities()` 同上；spawn `claude -p --output-format stream-json`（cwd=工作区；`--permission-mode` 以实测安全基线为准，默认取「只读/默认」档并在文档记录）；解析 stream-json（`system`/`assistant`/`result` 等消息 → TextDelta/ToolExec 观察/Usage 若存在/Done；`is_error`/非零退出 → Failed）；取消/超时同上。

- [ ] **Step 1: 探测并记录**（`claude --version`、`claude -p --output-format stream-json "说一句 ok"`、`--permission-mode` 可用值）
- [ ] **Step 2-4: 同 T1 的 TDD 循环**（fake-claude 脚本按实测 schema；提交 `feat(runtime): 增加 claude 兼容内核适配器`）

---

### Task 3: 内核切换、能力门控与外部内核 prompt 渲染

**Files:** Create `packages/runtime/src/render.rs`（或 core::task::external）；Modify `apps/cli/src/{cli.rs,commands/mod.rs,repl.rs}`、`packages/core/src/task/prompts.rs`（能力感知的工具段）；Test: 内联 + `apps/cli/tests/kernel.rs`

**Interfaces:**
- Produces:
  - `render_external_prompt(system_prompt: &str, messages: &[ChatMessage]) -> String`（钉死：`{system}\n\n---\n` 后按序渲染 user/assistant/tool：`User: …`/`Assistant: …`/`Tool {name} → {result 首行+截断}`；assistant 的 tool_calls 渲染为 `Assistant: [calls: {name}({args 紧凑 JSON})]`；空历史允许）
  - `build_system_prompt` 依 `Capabilities.tools == false` 输出：`Tools: unavailable in this kernel (Ignis tools do not run here).` 替代工具清单；技能段与事实段照旧
  - CLI：`--kernel` 值域扩至 `native|codex|claude`；`mk_runtime(kernel, cfg, workspace)` 构造对应实现（codex/claude 缺二进制 → `error:` 人话）；选定非 native 时打印一行能力提示；REPL `/kernel [name]`（无参显示当前）切换后续任务并打印提示；`ignis run` 与 REPL 共用
- [ ] **Step 1: 写失败测试**（渲染保真：多轮 + tool_calls + 截断；能力门控：external → 无工具清单 + 说明行、native 不变；mk_runtime 选择与错误；提示文案）
- [ ] **Step 2: 失败 → 实现**；**Step 3: 冒烟**（`--kernel codex` 在本机跑一条只读任务，记录输出；若 CLI/额度不可用则记录跳过理由）；**Step 4: 收口并提交**（`feat(core,cli): 增加内核切换、能力门控与外部内核渲染`）

---

### Task 4: doctor 完整版（外部 CLI 探测）

**Files:** Modify `packages/core/src/doctor.rs`、`apps/cli/src/commands/doctor.rs`；Test: 内联

**Interfaces:** 追加检查项：`codex`（`codex --version` 可执行 → ✓ + 版本短串；不可执行 → 选择 native 时 `!` 警告、选择 codex 时 ✗）、`claude` 同形；`CheckResult` 增加 `level: Ok|Warn|Fail`（或等效字段）以承载「缺失为警告」；doctor 退出码规则改为「任一 Fail → 1；仅 Warn → 0 并逐行显示」；README 限制节引用这些行为。

- [ ] **Step 1-4: TDD + 冒烟（本机 `ignis doctor` 全输出现状）→ 提交 `feat(core,cli): doctor 增加外部内核探测与警告级`**

---

### Task 5: README + Docker + systemd

**Files:** Create `README.md`、`Dockerfile`、`.dockerignore`、`deploy/ignis-daemon.service`、`deploy/README.md`；Modify 无（文档任务）

**Interfaces:**
- README 结构（钉死）：一句话 + 徽标式信息（Rust/单二进制/Linux·macOS）→ 设计理念（remember, act, improve）→ 安装（干净 Ubuntu：构建依赖 + rustup + clone + `cargo build --release` 或 `cargo install --path apps/cli` + `ignis doctor`）→ 快速开始（config、key、`ignis run`、REPL）→ 命令参考（P1–P3 全部子命令）→ 三个演示的手动步骤（Demo A/B/C，注明 uv/`notify-send` 依赖）→ 架构（ASCII 图：core/runtime/storage/gateway + 依赖方向）→ 安全模型（工作区沙箱、审批三档、审计、脱敏、非交互 fail-closed、外置内核沙箱说明）→ 限制与已知问题（外置内核用量缺失/上限退化、调度 30s 重扫、`/kernel` 不共享外置会话、relay spike 结论）→ 开发（测试、e2e、harness、Conventional Commits）→ 英文摘要段。
- Dockerfile：`FROM rust:1-slim AS build`（或本机可用 tag）→ `cargo build --release -p ignis-cli` → `FROM debian:bookworm-slim` + 只拷贝二进制 → `VOLUME /root/.ignis` → `ENTRYPOINT ["ignis"]`；`.dockerignore` 排除 `target/`、`.git`、`.superpowers`；冒烟步骤写 README（`docker build -t ignis . && docker run --rm -it ignis doctor`，需挂载配置）。
- systemd：`deploy/ignis-daemon.service`（`ExecStart=%h/.local/bin/ignis daemon start --foreground`、`Restart=on-failure`、`Environment=IGNIS_HOME=%h/.ignis`），`deploy/README.md` 三步说明。
- [ ] **Step 1: 写文档三件套**；**Step 2: 冒烟**（`cargo install --path apps/cli --root /tmp/ignis-install` 后以该二进制跑 `doctor`/`run` 一次；`docker build` 若可用则冒烟 `docker run --rm ignis:dev doctor`——耗时允许则做，否则记录未在本机执行）；**Step 3: 提交**（`docs: 增加 README、Docker 与 systemd 部署示例`）

---

### Task 6: 验收 harness + 真机 relay spike 收口

**Files:** Create `harness/acceptance.sh`（+x）、`harness/tasks.md`、`docs/acceptance.md`；Modify 既有 spike 文档（relay 结论）

**Interfaces:**
- `harness/acceptance.sh [demos|tasks|restarts] [N]`：前置检查（真实模型 env：`IGNIS_BASE_URL`/`IGNIS_API_KEY` 等；缺则清晰退出）；`demos N`：对 `examples/` 三个场景各做 N 次连续真实运行（用真实模型，复用 README 演示步骤；每次核对关键断言：A 测试转绿、B 注入命中、C 告警落库），逐次记录；`tasks N`：跑 `harness/tasks.md` 的前 N 条（默认 20），统计成功率与门槛（≥80%）；`restarts N`：启停守护 N 次，检查 `job_runs`/`tasks` 无重复、无丢失；结束打印汇总表并落 `harness/results-<ts>.md`。
- `harness/tasks.md`：20 条常见任务（读写/搜索/测试/构建/http 抓取/记忆/技能/调度示例），每条含预期结果。
- `docs/acceptance.md`：门槛定义（三场景各连续 3 次、20 例 ≥80%、10 次重启）、运行方法与最近结果记录区。
- 真机 relay：执行一次重试；把「可用（含事件样例）/仍不可得/降级决定」写入 spike 文档，并在 README 限制节同步一句。
- [ ] **Step 1: 写 harness 与文档**；**Step 2: 语法冒烟**（`bash -n`；无真实 key 时的提前退出路径实测）；**Step 3: relay 重试并记录**；**Step 4: 提交**（`feat(harness): 增加量化验收脚本与文档，收口 relay 结论`）

---

### Task 7: P4 收口清理（低风险项打包）

**Files:** Modify `apps/cli/src/{render.rs,commands/task.rs}`（状态映射统一用 `RunStatus::as_str`）、`demo.md`（banner 形状与 README 对齐的注释/或保持）、`apps/cli/tests/demo_c.rs`（夹具契约小测：`check-health.sh --simulate 92` 输出 JSON 断言 + `--simulate abc` 退出 2）、`apps/cli/src/commands/daemon.rs`（`daemon run` 校验 `foreground`）；Test: 相应内联/集成

**Interfaces:** 纯清理：① CLI 两处 `RunStatus` 四串映射改用 `ignis_runtime::RunStatus::as_str()`；② `demo_c.rs` 增一条夹具契约测试（`sh check-health.sh` 与 `--simulate 92` 输出可解析、`--simulate abc` 退出 2）；③ `daemon run` 无 `--foreground` 时报错（隐藏入口必须显式携带）；④ `docs/` 与 README 的已知漂移（P2 记的 demo.md banner、P1 记的设计文档 DDL 措辞）在 README「已知偏差」节各记一行，不改历史文档。
- [ ] **Step 1-3: 小步修改 + TDD + 收口**；**Step 4: 提交**（`chore: P4 收口清理（映射统一、夹具契约、入口校验与文档偏差记录）`）

---

## 覆盖与自评

- 映射：§6.3 → T1/T2/T3；§6.1（doctor、`/kernel`）→ T3/T4；§6.9 → T5；§7（验收清单：#1 安装、#7 已推迟为 CLI/桌面通知替代、量化门槛）→ T5/T6；§8（假 CLI 验证映射、CI 不依赖模型）→ T1/T2/T6；§10 风险 4（事件漂移：探测记录 + 宽容解析）→ T1/T2，风险 5（uv）→ T5 README；决策 #4/#11 → T3。
- 排序约束（预检）：T3 的 `render_external_prompt` 被 T1/T2 的过渡实现引用——T1/T2 先直拼、T3 落地后统一（已在 T1 Produces 注明）；T4 的 doctor 探测复用 T1/T2 的探测结论；T6 的 harness 依赖 T5 的 README 步骤文本。
- 计划内**不做**：真正套壳级集成（MCP 工具层，接口已留）；Telegram；musl 静态构建（README 一句可选说明即可）；跨机调度。
