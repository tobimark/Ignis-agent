# Ignis

**Ignis 是一个用 Rust 从零实现执行循环的本地个人 AI Agent**：单二进制 CLI，可切换执行内核（`native` / `codex` / `claude`），具备长期记忆、技能与后台定时任务，能把真实任务做完（读写文件、跑 Shell、抓网页），而不是只聊天。

- **Rust**（edition 2024，MSRV 1.98；`rust-toolchain.toml` 固定 `stable`）
- **单二进制** `ignis`（glibc 基线，无运行时依赖；内置技能编译进二进制）
- **Linux / macOS**（内核发现目前只在 Unix 实测，见[限制](#限制)）
- **设计理念**：*Ignis should remember, act, and improve.*（记忆、执行，并在用户可控的前提下从经验中改进）

---

## 设计理念：remember, act, improve

- **Remember（记得住）**：事实跨会话持久化（SQLite + FTS5 trigram + 可读 Markdown 镜像）；任务前把相关记忆注入系统提示，纠正同一 `key` 的旧值会被取代、不再注入。
- **Act（做得到）**：文件、Shell、HTTP 三类真实工具 + `remember` / `load_skill` / `notify_user` / `schedule_create` 四个元工具；工具调用逐条经审批策略并写审计。
- **Improve（会改进）**：成功经验与常见错误沉淀为记忆，下次任务复用；v0.1 只做「保存并复用」，自动生成技能留给后续版本。

## 安装（干净的 Ubuntu）

```sh
# 1. 构建依赖（bundled SQLite 需要 C 编译器）与 rustup
sudo apt-get update
sudo apt-get install -y build-essential curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. 克隆并构建（二选一）
git clone <repo-url> ignis-agent && cd ignis-agent
cargo build --release        # 产出 ./target/release/ignis
# 或者装进 ~/.cargo/bin：
cargo install --path apps/cli

# 3. 自检：缺什么会逐行告诉你（`✓` 通过 / `!` 警告 / `✗` 需处理，任一 ✗ 退出码 1）
ignis doctor
```

想要完全静态的二进制可以自行加 musl target 构建；v0.1 仓库不提供 musl 构建。

### Docker（可选）

镜像只带二进制（内置技能已嵌入），数据目录挂 `~/.ignis`：

```sh
docker build -t ignis .
docker run --rm -it -v ~/.ignis:/root/.ignis ignis doctor
docker run --rm -it -v ~/.ignis:/root/.ignis -v "$PWD:/workspace" -w /workspace \
  ignis --workspace . run "用 fs_list 列一下当前目录" --yes
```

构建阶段需要能访问 crates.io。systemd 部署示例见 [`deploy/README.md`](deploy/README.md)。

## 快速开始

Ignis 的状态都在数据目录（默认 `~/.ignis`，可用 `IGNIS_HOME` 改）：`config.json`（0600）、`ignis.db`、`logs/`、`memory/`、`skills/`。配置文件缺失时全部走默认值（`ignis config show` 看生效值），`ignis doctor` 会按需创建数据目录与数据库。

```sh
# 1. 指向任意 OpenAI 兼容端点（环境变量只影响本次生效值，不落盘）
export IGNIS_BASE_URL=https://your-endpoint/v1
export IGNIS_MODEL=your-model
export IGNIS_API_KEY=sk-...

# 也可以用配置文件（写进 ~/.ignis/config.json，权限 0600）：
ignis config set model.base_url https://your-endpoint/v1
ignis config set model.model your-model
ignis config set workspace_root /path/to/your/project

# 2. 自检：home / config / api_key / base_url / workspace / db + codex、claude 探测
ignis doctor

# 3. 一次性任务；无 TTY 时写/Shell/外联操作默认 fail-closed，须用 --yes 预授权
ignis --workspace /path/to/your/project run "用 fs_list 列一下当前目录" --yes

# 4. 交互式 REPL（需要 TTY；不带子命令即进入）
ignis --workspace /path/to/your/project
```

环境变量覆盖（只影响生效值，不写回文件）：`IGNIS_HOME`（数据目录）、`IGNIS_WORKSPACE`（工作区）、`IGNIS_BASE_URL`、`IGNIS_MODEL`、`IGNIS_API_KEY`。密钥来源：`IGNIS_API_KEY`（覆盖一切）→ `config.json` 的 `api_key` → 配置 `model.api_key_env` 指定的环境变量（默认 `OPENAI_API_KEY`）。只有 `native` 内核需要 Ignis 的密钥；两个外置内核用各自 CLI 的凭据（详见下表）。

## 执行内核与适配器矩阵

`--kernel native|codex|claude` 选择执行内核；REPL 里用 `/kernel [name]` 切换，只影响后续任务。解析顺序：`--kernel` > `config.default_kernel` > `native`；配置里写错名字是硬错（不静默回落）。

| | `native`（默认） | `codex` | `claude` |
|---|---|---|---|
| 执行循环 | Ignis 自带（OpenAI 兼容端点，`stream:true` 流式 + 工具调用） | spawn 已安装的 `codex exec --json` | spawn 已安装的 `claude -p --verbose --output-format stream-json` |
| 工具在哪执行 | **工作区内，由 Ignis 自己执行**（路径沙箱 + 审批策略） | codex 自己的沙箱（适配器给 `--sandbox workspace-write`） | claude 自己的权限档（适配器给 `--permission-mode manual`） |
| Ignis 审批 / 审计行 | 按风险三档（Safe / Elevated / Dangerous）判定 + `approvals` 审计 | 不适用（非交互 CLI，无审批通道） | 不适用 |
| 用量记账 | 精确（来自 API usage） | 不映射（记为 0） | 不映射（记为 0） |
| Ignis 元工具（`remember` / `load_skill` / `notify_user` / `schedule_create`） | 全部可用 | 不可用（能力门控：工具清单收成一句说明） | 不可用 |
| 凭据 | `IGNIS_API_KEY`（或 `model.api_key_env`） | 用户自己的 codex 登录 / 配置 | 用户自己的 claude 凭据 |
| 验证状态 | 真机：流式 + 工具调用、Demo A 全栈 | 真机三态：success / tool / failed | 真机一轮含工具调用；超时收束 |

`ignis doctor` 按生效内核分级：选 `native` 缺密钥 → `✗`；选外置内核缺 Ignis 密钥只给 `!` 警告（用不上），而选中却没装对应 CLI 才是 `✗`。

## 架构

```
                  ┌───────────────────────────────────────────────┐
  用户 ──────▶    │ apps/cli（bin: ignis）                        │
                  │  REPL · 子命令 · 审批 UI · daemon 模式入口     │
                  └───────────────┬───────────────────────────────┘
                                  │ Session / Task
                  ┌───────────────▼───────────────────────────────┐
  daemon 模式 ──▶ │ packages/core（ignis-core）                    │
                  │  task · memory · skills · tools                │
                  │  scheduler · security（审批 / 策略 / 脱敏）     │
                  └──────┬───────────────────────┬────────────────┘
                         │ Runtime trait          │ Storage
                  ┌──────▼────────────┐   ┌───────▼──────────┐
                  │ packages/runtime  │   │ packages/storage │
                  │ native / codex    │   │ rusqlite(bundled)│
                  │ / claude          │   │ ~/.ignis/        │
                  └───────────────────┘   └──────────────────┘
                  ┌───────────────────┐
                  │ packages/gateway  │  守护进程逻辑：调度循环、通知投递
                  │  Notifier（IM 预留）│  （由 `ignis daemon` 模式驱动）
                  └───────────────────┘
```

- **核心模块**（`packages/core`）：`task`（任务执行、事件流与会话历史）、`memory`、`skills`、`scheduler`、`security`（审批 / 策略 / 脱敏）、`tools`；另有 `alerts`、`config`、`doctor`。
- **依赖方向**：`runtime` 定义 `Runtime` / `ToolExecutor` 等 trait（含内核能力声明），`core` 实现工具执行与审批并注入；native 执行循环在 `runtime` 侧——所以是 `core → runtime`，无循环依赖。`runtime` 的 `RuntimeEvent` 序列化形状就是任务 JSONL 的格式，属跨模块契约。
- **数据流**：CLI 或调度器发起 Task → core 组装上下文（记忆、技能目录、工作区）→ 内核执行（native 的工具调用经 security 层审批）→ 事件流回写 JSONL 与 UI → 结束写用量、记忆与告警。

## 安全模型

- **工作区沙箱**：文件与 Shell 工具解析后的路径必须落在工作区内（工作区外的读写直接拒绝）；Shell 的工作目录固定为工作区。
- **审批三档**（风险等级 × 运行模式）：
  - 交互终端（stdin 是 TTY，且无开关）：`Safe` 直接执行；写文件 / Shell / 外联请求 `Ask`，终端内联提示 `[a]pprove / [s]ession-allow / [r]eject / [d]etails`；危险操作同样询问。
  - `--yes`：预授权常规风险操作（`Elevated` 直接放行）；危险操作仍拒绝，需要显式 `--unsafe`。
  - `--unsafe`：预授权全部操作（含危险命令库）。
  - 非交互且无开关（管道、脚本、无人值守）：**fail-closed**——写 / Shell / 外联一律拒绝并记录原因，模型收到拒绝结果后可继续。
- **危险命令库**：`rm -rf`、`sudo`、`curl | sh`、`chmod 777`、`mkfs`、`dd if=`、关机类命令、写块设备、fork bomb 等强制走危险档。
- **敏感文件**：`.env` / `.env.*`、`*.pem`、`*.key`、`id_rsa*`、`.netrc`、`.git-credentials` 的读写升级为危险档（含软链别名解析后的判定）。
- **审计**：所有非 `Safe` 调用（放行或拒绝）都会写 `approvals` 记录；记录写不进去就**不执行**（fail-closed）。`ignis task show <id>` 可查审批记录与事件流。
- **脱敏**：密钥统一经脱敏器，不进入任务日志、错误输出与终端；`config show` 与 `Debug` 恒显示 `***`；配置文件写盘为 0600。
- **后台任务**：创建时即确定执行边界（工具白名单 + 风险上限 + 时间/token 上限 + 工作区），运行期严格执行；失败有记录并告警，不无限重试，重叠执行跳过，停机错过的时点不补跑。
- **外置内核**：工具在 codex / claude 自己的沙箱里执行（`workspace-write` / `manual` 档，适配器不自行放宽）；Ignis 不经手审批，也没有审批行——这些内核的能力边界见[限制](#限制)。

## 命令参考

```text
ignis                                  # 交互式 REPL（需要 TTY；不带子命令时进入）
ignis run "<task>"                     # 一次性任务；退出码：success 0 / failed、limit_reached 1 / aborted 130
ignis task list [--limit N]            # 最近的任务（默认 20 条）
ignis task show <id>                   # 任务详情：头部、审批记录与事件流
ignis config show                      # 打印生效配置（环境变量覆盖已应用，api_key 恒为 ***）
ignis config set <key> <value>         # 写回配置文件的标量项（见下）
ignis memory list [--all]              # 事实列表（默认只看 active，新近在前）
ignis memory search <query>            # 按关键词检索事实
ignis memory add --scope <user|project|global> --key <k> <text>   # 同 key 再写一次即取代旧值
ignis memory edit <id> <text>          # 改写一条事实
ignis memory delete <id>               # 删除一条事实
ignis skills list                      # 列出技能（内置 + ~/.ignis/skills，按名称排序）
ignis doctor                           # 环境自检（含 codex / claude 探测）
ignis daemon start [--foreground] | stop | status   # 调度守护进程（start 默认后台自守护）
ignis schedule list | pause <id> | resume <id> | remove <id> | history [<id>] [--limit N]
ignis alerts [--all] [--read]          # 告警收件箱（默认只看未读；--read 展示后标记已读）
```

全局参数：`--kernel native|codex|claude`、`--workspace <DIR>`、`--yes`、`--unsafe`（上述子命令通用）。

`config set` 可写的点路径：`workspace_root`、`default_kernel`、`api_key`、`model.base_url`、`model.model`、`model.api_key_env`、`limits.max_steps`、`limits.task_timeout_secs`、`limits.token_budget`、`limits.max_retries`、`limits.shell_timeout_secs`、`limits.tool_output_limit`、`limits.http_max_bytes`、`limits.context_char_budget`。

## 三个演示

> 以下命令在仓库根目录执行；`examples/` 是夹具，复制到临时目录以免污染仓库。真实演示需要一个可用的 OpenAI 兼容端点（`IGNIS_BASE_URL` / `IGNIS_MODEL` / `IGNIS_API_KEY`，见[快速开始](#快速开始)）。

### Demo A · AI 真正完成工作

```sh
demo_a=$(mktemp -d)
cp -r examples/broken-project "$demo_a/project"
cd "$demo_a/project"

npm test          # 预期失败：ENOENT（配置路径多退了一层 ..）
ignis --workspace . run "分析这个项目的测试失败原因，修复它并重新运行测试" --yes
npm test          # 预期通过
```

Ignis 会读 `src/config.js`、跑 `npm test`、定位错误、经审批层修改文件（这里用 `--yes` 预授权，审批记录照样写入）、重跑并汇报；修复后的 `npm test` 退出 0。夹具的测试用 `npm` 跑 `node --test`（Node ≥ 18）。

### Demo B · AI 记住过去的经验

```sh
demo_b=$(mktemp -d)
cp -r examples/uv-project "$demo_b/project"
cd "$demo_b/project"

# 会话 1：告诉它这个项目用 uv，并写进项目记忆（scope=project）
ignis --workspace . run "这个项目使用 uv 管理 Python 环境，请记住" --yes
ignis memory list            # 应看到 pkg_manager 事实

# 会话 2：run 是一次性进程，天然等价于「退出后再进来」——复用上一会话的记忆
ignis --workspace . run "帮我检查这个项目的依赖并运行测试" --yes

# 纠正：同 key 再写一次，旧值被取代、不再注入
ignis --workspace . run "不对，这个项目改用 pip 了" --yes
ignis memory list            # 旧值 superseded
```

真实跑 `uv sync` / `uv run pytest` 需要安装 uv（`curl -LsSf https://astral.sh/uv/install.sh | sh`）。本机未装 uv：夹具的自动化用例不断言真实依赖安装，只断言「模型选择了 uv 工作流」；真实演示前请先安装 uv。

### Demo C · AI 在后台主动工作

```sh
# 夹具：examples/test-server/check-health.sh --simulate 92 → {"disk_pct":92,...}
# 1) 创建调度任务（schedule_create 是 Elevated 风险：需要 --yes 或交互确认）
ignis --workspace "$PWD" run "创建一个调度任务：每隔 1 分钟运行 examples/test-server/check-health.sh --simulate 92，磁盘超过 85% 就用 notify_user 告警" --yes
ignis schedule list

# 2) 退出 CLI；守护进程继续工作
ignis daemon start
ignis daemon status

# 3) 至少一个周期之后
ignis alerts                 # 告警收件箱（未读）
ignis schedule history       # 运行历史（含失败原因）
ignis daemon stop            # 幂等：没在跑也算成功
```

调度任务在创建时确定受限 profile（工具白名单 + 风险上限 + 限额）与工作区；守护进程随 CLI 退出继续运行，权限不因后台而放宽。通知渠道：CLI 未读收件箱 + 桌面通知（`notify-send`，不可用时降级为 `~/.ignis/logs/alerts.log`）+ 日志文件。

### 本机真机实测（2026-10-15，只记端点形状，不含任何 key）

- **native 内核**打 OpenAI 兼容端点（`https://api.minimax.cn/v1`，模型 `MiniMax-M2`）：
  - 流式 + 工具调用通过：`data:` 增量、完整 `tool_calls` 块、末块 `usage`；该端点不发 `[DONE]`，客户端按 EOF 收尾。
  - Demo A 真机：21.2s 完成，3 条审批记录（`allow` / `preauthorized`：`shell_run` ×2 + `fs_edit`），修复后 `npm test` 退出 0。
- **codex 0.162.1**：真机三态（success / tool / failed）全部映射零改动。
- **claude 2.1.274**：真机一轮含工具调用（7.4s）；端点不可达时按 `task_timeout` 收束、零残留进程。
- 密钥只经环境变量传入，未落盘、未入仓库与日志。

## 限制

1. **调度 / 守护进程只跑 `native`**（决策 #18）：`packages/gateway` 的守护进程固定装配 `NativeRuntime`；`jobs.kernel` 列存在但恒为 `native`（`schedule_create` 没有内核参数），`--kernel codex|claude` 不影响守护进程。即使配置 `default_kernel` 是外置内核，守护进程仍需要 Ignis 自己的 OpenAI 兼容凭据。
2. **外置内核下 Ignis 工具 / 审批 / 审计不适用**：工具在 codex / claude 自己的沙箱里执行；Ignis 不注入工具、不经手审批、没有 `approvals` 行；`remember` / `load_skill` / `notify_user` / `schedule_create` 不可用（能力门控会把系统提示里的工具清单收成一句说明）。
3. **缺失已配置的外置 CLI 时 `run` 直接失败，无回退**：`ignis run --kernel codex` 在会话装配期就报「找不到 codex CLI…」，不会悄悄回落 `native`；`ignis doctor` 对选中的缺失内核给 `✗`。
4. **`base_url` 探测对所有内核都是硬检查**：选 `codex` / `claude` 时若配置里的 OpenAI 兼容端点不可达，`base_url` 仍报 `✗`（退出码 1），即使这次运行根本不用它。
5. **Windows 上内核发现未做 PATHEXT 解析**：`find_program` 只做「PATH 目录拼接 + 存在性 + Unix 执行位」判定，本机只在 Unix 实测。
6. **`--version` 探测没有超时**：卡住的 `codex --version` / `claude --version` 会把 `ignis doctor` 拖住（要堵需要 spawn + 有界等待）。
7. **外置内核用量与上限退化**：codex / claude 不出 `Usage` 事件（`Capabilities{usage:false}`），token 记为 0；因此 token / 成本上限对它们不生效，只剩墙钟超时（`limits.task_timeout_secs`）与取消（REPL Ctrl-C / SIGINT）。
8. **调度重扫窗口 30s**：新建 / 暂停 / 恢复的调度任务最迟 30s 生效（守护进程每轮最多睡 30s 重读 active job；无到期任务时也一样）。
9. **`/kernel` 不共享外置会话**：每轮任务都把 Ignis 的 canonical 历史渲染进 prompt 交给一个新的 CLI 进程，不依赖也不复用 codex / claude 自己的 `--resume` 会话（切换内核不会带过来外置 CLI 的会话状态）。
10. **relay spike 结论**：原生中转（`opencode.ai`）当时额度 / 路由不可得，两轮未达模型；随后以一个可用的 OpenAI 兼容端点复验，确认流式工具调用可用（`docs/spikes/2026-10-15-relay-streaming-toolcalls.md`）。native 内核依赖这一结论，降级路径（`stream:false` / Anthropic 协议）无需启用。
11. **调度策略**：失败即记录 + 告警、不无限重试；上一次运行未结束时再次到期只跳过并记日志；停机期间错过的时点不补跑（防重启后重复执行危险操作）。

## 已知偏差

- **banner 形状与 `demo.md` 概念图不同**（`demo.md` 原文标注「CLI 概念展示，并非已经实现的 Ignis 界面」）：实际启动屏为 `IGNIS v0.1` / `● Ready` / `Workspace: <dir>` / `Runtime: <kernel> · Skills: <n> · Memory: <n facts|off>`，有未读告警时追加一行 `⚠ N unread alerts — run \`ignis alerts\``。
- **设计文档措辞与 `demo.md` 不完全一致**：`docs/superpowers/specs/2026-10-15-ignis-v0.1-design.md` 按对话中的新决定固化了工程取舍（Rust 取代 TS/Node、Telegram 推迟为 CLI 收件箱 / 桌面通知、内核数量等），冲突时以设计文档为准。
- **`demo.md` 按作者要求保持原文未修改**；上述偏差只记录在这里，不回改历史文档。
- **e2e 测试位于 `apps/cli/tests/`**：`CARGO_BIN_EXE_ignis`（真构建二进制路径）只对拥有 bin 的包可用，`packages/*` 里的 crate 没有 bin，因此这类用例不能下沉到 `packages/`。

## 开发

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # 离线：不依赖网络与真实模型
```

- 单元测试随各 crate；外置内核适配器的假 CLI 集成测试在 `packages/runtime/tests/`。
- 端到端测试在 `apps/cli/tests/`（`demo_a.rs` / `demo_b.rs` / `demo_c.rs` / `approval_modes.rs` / `kernel.rs` / `daemon_lifecycle.rs` / `autostart.rs` / `limits.rs` / `redaction.rs`）：用真构建的 `ignis` 二进制 + `ignis-runtime` 的脚本化假模型（`test-support`），断言落在真文件、真数据库行与真进程上。
- 真实模型的量化验收（三场景各连续 3 次、20 例常见任务 ≥80%、10 次重启）是**手动、可选**执行（控制额度消耗），不进 CI；脚本随后续任务交付。
- 提交信息用 Conventional Commits（英文 type/scope，中文正文）。
- Docker / systemd 部署见上文与 [`deploy/README.md`](deploy/README.md)。

## English summary

Ignis v0.1 is a self-hosted, single-binary personal AI agent written in Rust. It implements its own agent loop (the `native` kernel, talking to any OpenAI-compatible endpoint with streaming tool calls) and can also delegate a task to an installed `codex` or `claude` CLI (`--kernel`). It keeps long-term memory, discovers `SKILL.md` skills, and runs scheduled jobs from a background daemon — designed around one idea: *Ignis should remember, act, and improve*. Tools run inside the workspace and every non-safe call is approval-gated and audited; external kernels run tools in their own sandbox and do not participate in Ignis approvals or usage accounting. Build it with `cargo build --release`, verify with `ignis doctor`, and start with `ignis run "<task>" --yes`.
