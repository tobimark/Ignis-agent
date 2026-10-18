# Ignis

**Ignis 是一个用 Rust 从零实现执行循环的本地个人 AI Agent**：单二进制 CLI，可切换执行内核（`native` / `codex` / `claude`），具备长期记忆、技能与后台定时任务，能把真实任务做完（读写文件、跑 Shell、抓网页），而不是只聊天。

**v0.2（版本号 `0.2.0`，主题 *Interactive & Persistent*）** 在 v0.1 的「能跑、能记、能调度」之上补齐终端交互与持久化：流式渲染 + 推理/正文分道、多行行编辑器、15 个本地斜杠命令、可恢复的持久会话、上下文可视化与压缩，以及全屏 TUI Beta（`ignis --tui`）。

- **Rust**（edition 2024，MSRV 1.98；`rust-toolchain.toml` 固定 `stable`）
- **单二进制** `ignis`（glibc 基线，无运行时依赖；内置技能编译进二进制）
- **Linux / macOS**（内核发现目前只在 Unix 实测，见[限制](#限制)）
- **设计理念**：*Ignis should remember, act, and improve.*（记忆、执行，并在用户可控的前提下从经验中改进）

---

## v0.2 新增

- **渲染**：模型输出的 Markdown 子集（围栏代码块、`#`–`####` 标题、`- `/`* ` 无序列表、`> ` 引用、`---` 分隔线、行内 `` `code` `` 与 `**粗体**`；表格 / 链接 / 图片 / 斜体原样通过）；推理（`think`）与正文**分道渲染**——normal 以 `· ` 起行、debug 加 `[reasoning] ` 前缀、minimal 只给收尾摘要行；输出模式 `minimal` / `normal` / `debug`（`/details` 或 `ui.output_mode` 切换）；主题 dark / light + 无色（`ui.theme`、`ui.color`、`NO_COLOR`）。
- **输入**：多行编辑（`Alt+Enter` / `Ctrl+J` 换行）、历史（`~/.ignis/history`）、Tab 补全（命令名与 `/kernel` 取值）、bracketed paste（整段粘贴不被当成逐行提交）、按显示宽度折行与 CJK 全角宽度（`\t` 只按 8 列制表位展开到屏幕）；raw mode 不可用（`TERM` 缺失 / `dumb`、stdout 不是终端）时回退按行输入，并打一行 `note: line editing unavailable (<原因>); using plain line input`。
- **命令**：15 个本地斜杠命令（分组、顺序与文案见[命令参考](#命令参考)，以 `/help` 为准）；除 `/compact`、`/learn` 各要一次模型往返外，其余 13 条全在本地完成、不消耗模型；未知命令只在本地报错（`Unknown command: /<名字> — /help lists commands`，绝不送给模型）。
- **会话**：`-c` / `--continue` 恢复本工作区最近一次会话；REPL 里 `/new` 开新会话、`/sessions` 列最近会话、`/resume <id>`（≥8 字符唯一前缀）切换；canonical 历史（用户提示词 + assistant 最终答复）按轮落库、`tasks.session_id` 记下每轮任务归属。
- **上下文**：`/context` 只读展示历史条数与字符、字符预算、最近一轮用量与组装统计（facts / skills / system chars）；`/compact` 把较早轮次摘要化（保留最近两轮，原始消息不删）。
- **记忆·技能**：`/memory <list|search|add|edit|delete>`（与 `ignis memory …` 同一份口径）；`remember` 元工具成功后固定打一行确认 `  ✓ memory: <key> (<scope>) <文本 ≤60 字符>`；`/skills [reload]` 列技能目录或重扫；实验性 `/learn [task-id]` 从一条已完成任务生成 `SKILL.md` 草稿，**确认后**才落盘（不自动部署、不自动执行）。
- **TUI Beta**：`ignis --tui` 全屏界面（备用屏 + 帧合成，复用同一套 markdown / 宽度 / 调色板与命令分发）；不满足条件时只打一行 note 并降级到经典 REPL。

## 设计理念：remember, act, improve

- **Remember（记得住）**：事实跨会话持久化（SQLite + FTS5 trigram + 可读 Markdown 镜像）；任务前把相关记忆注入系统提示，纠正同一 `key` 的旧值会被取代、不再注入。
- **Act（做得到）**：文件、Shell、HTTP 三类真实工具 + `remember` / `load_skill` / `notify_user` / `schedule_create` 四个元工具；工具调用逐条经审批策略并写审计。
- **Improve（会改进）**：成功经验与常见错误沉淀为记忆，下次任务复用；技能不自动演进——`/learn` 只把一条已完成任务草拟成技能（未经测试、需用户确认才落盘）。

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

想要完全静态的二进制可以自行加 musl target 构建；本仓库不提供 musl 构建。

### Docker（可选）

镜像只带二进制（内置技能已嵌入），数据目录挂 `~/.ignis`：

```sh
docker build -t ignis .
docker run --rm -it -v ~/.ignis:/root/.ignis ignis doctor
docker run --rm -it -v ~/.ignis:/root/.ignis -v "$PWD:/workspace" -w /workspace \
  ignis --workspace . run "用 fs_list 列一下当前目录" --yes
```

构建阶段需要能访问 crates.io（构建镜像 pin 在 `rust:1-slim-bookworm`，`cargo build` 带 `--locked`）。`docker build` **未在本仓库的交付环境执行**（registry 不可达），Docker 部分是文档级交付，镜像定义以 [`Dockerfile`](Dockerfile) 为准。systemd 部署示例见 [`deploy/README.md`](deploy/README.md)。

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
ignis -c                              # 恢复本工作区最近一次会话
ignis --tui                           # 全屏 TUI（Beta）；要恢复会话就 ignis --tui -c
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

`ignis doctor` 按生效内核分级：选 `native` 缺密钥、或 `base_url` 端点不可达 → `✗`；选外置内核时这两条都只给 `!`（用不上 Ignis 的密钥，也不访问那个端点），而选中却没装对应 CLI 才是 `✗`。配置里的内核名写错是另一条路径：`✗ kernel` 单列一行（其余检查按 `native` 兜底，缺钥匙的 `api_key` 随之降级为 `!`，退出码已由 `✗ kernel` 顶上）。

两点补充：① `IGNIS_MODEL` / 配置里的 `model.model` 只作用于 `native`（OpenAI 兼容端点的模型名）；两个外置内核一律用各自 CLI 的默认模型，Ignis **不转发**模型名（见 `apps/cli/src/commands/kernel.rs`）。② Docker 镜像只带 `ignis` 二进制，**不含 codex / claude**——要在容器里用外置内核需自行安装并在容器内登录。

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

全局参数（子命令与 REPL 通用）：`--kernel native|codex|claude`、`--workspace <DIR>`、`--yes`、`--unsafe`。

REPL 专属开关（与任何子命令同用是用法错误，退出码 1）：

| 开关 | 作用 |
|---|---|
| `-c` / `--continue` | 恢复**本工作区**最近一次会话并装载 canonical 历史；没有旧会话时打 `note: no previous session in this workspace; starting a new one` 后照常起步 |
| `--tui` | 用全屏 TUI（Beta）进入 REPL；不满足条件时打一行 `note: TUI unavailable (<原因>); falling back to the classic REPL` 并降级到经典 REPL |

`-c` 只是恢复会话，不删数据；`--tui` **不**默认恢复会话——要恢复就 `ignis --tui -c`。

### 斜杠命令（15 个，`/help` 为准）

分组、顺序、参数提示与一行描述都取自命令表 `COMMANDS`（`apps/cli/src/commands/slash.rs`），与 `/help` 的输出同源；命令名逐字匹配（`/Help` 是未知命令），未知命令打到 stderr、**不送模型**。

| 分组 | 命令 | `/help` 里的描述（逐字） |
|---|---|---|
| SESSION | `/new` | Start a new session |
| SESSION | `/sessions` | List recent sessions in this workspace |
| SESSION | `/resume <id>` | Resume a session from this workspace |
| AGENT | `/model [name]` | Show or set the model of this session |
| AGENT | `/status` | Show kernel, model, workspace, alerts and last usage |
| AGENT | `/tasks [N]` | List the most recent tasks (default 10) |
| AGENT | `/skills [reload]` | List the skill catalog, or reload it |
| INTELLIGENCE | `/memory <list\|search\|add\|edit\|delete>` | Read or edit long-term memory |
| INTELLIGENCE | `/context` | Show the context budget and the last turn usage |
| INTELLIGENCE | `/compact` | Summarize older turns into a summary message |
| INTELLIGENCE | `/learn [task-id]` | Draft a skill from a completed task |
| INTERFACE | `/details [minimal\|normal\|debug]` | Show or switch the output mode |
| INTERFACE | `/exit` | Leave the REPL |
| INTERFACE | `/help` | Show this list |
| INTERFACE | `/kernel [name]` | Show or switch the execution kernel (native\|codex\|claude) |

`/compact` 与 `/learn` 各要一次**无工具的 native 请求**（`config.model` + Ignis 凭据）：外置内核或没有 Ignis 凭据时报 `error: compact requires native kernel credentials (current kernel does not provide them)` / `error: learn requires native kernel credentials (current kernel does not provide them)`，且不改状态。其余 13 条命令全在本地完成（`/context` 只读、不消耗模型）。

## 配置

`ignis config show` 打印生效配置（环境变量覆盖已应用；`api_key` 恒为 `***`）。`ignis config set <key> <value>` 以**文件原文**为底稿只改写被点名的那一项写回 `config.json`（0600；不会把默认值物化进文件，也不会持久化环境派生值），白名单之外一律报「未知配置项」。可写的点路径共 **17** 项：

| 点路径 | 取值 |
|---|---|
| `workspace_root` | 文本（路径） |
| `default_kernel` | 文本，`native` / `codex` / `claude`（写错名字是启动期硬错，不静默回落） |
| `api_key` | 文本（密钥；回显恒为 `***`） |
| `model.base_url` | 文本（URL） |
| `model.model` | 文本（模型名，只作用于 `native`） |
| `model.api_key_env` | 文本（环境变量名，默认 `OPENAI_API_KEY`） |
| `limits.max_steps` | 0-4294967295 的整数 |
| `limits.task_timeout_secs` | 非负整数 |
| `limits.token_budget` | 非负整数 |
| `limits.max_retries` | 0-4294967295 的整数 |
| `limits.shell_timeout_secs` | 非负整数 |
| `limits.tool_output_limit` | 非负整数 |
| `limits.http_max_bytes` | 非负整数 |
| `limits.context_char_budget` | 非负整数（`/context` 的 budget 位读它） |
| `ui.output_mode` | `minimal` / `normal` / `debug` |
| `ui.color` | `auto` / `always` / `never` |
| `ui.theme` | `dark` / `light` |

`ui.*` 三段：`output_mode` 是渲染档位（与 `/details [minimal|normal|debug]` 同一套取值，会话内切换即时生效）；`color` 是颜色策略；`theme` 是调色板（dark / light 只差 dim 槽）。颜色优先级：**`NO_COLOR` 非空恒无色**（含 `ui.color always`；空串按 NO_COLOR 的约定视为未设置）→ 否则看 `ui.color`：`auto` 跟 stdout 是否 TTY、`never` 恒无色、`always` 恒有色。

## 终端兼容

v0.2 的目标是「Ubuntu 默认终端、`tmux`、SSH、80 列、无色、浅色主题下都不崩、不折行错乱」。下面这张矩阵是手动检查表（每行都跑同一组检查项）：

| 终端 | 检查项 | 结论 | 记录 |
|---|---|---|---|
| Ubuntu 终端（默认） | 启动 banner、提交一次任务、编辑器重绘/历史/Ctrl+C、`/help` 在 80 列不折行错乱、TUI 可用或降级、退出后 `stty` 复原 | **SKIP + 原因**：本机没有可用的图形终端（`gnome-terminal` / `konsole` 未安装，X 显示不可用） | `docs/acceptance.md` 的终端矩阵（Task 6 填真机结果）；本机自动化覆盖见下 |
| tmux | 启动 banner、提交一次任务、编辑器重绘/历史/Ctrl+C、`/help` 在 80 列不折行错乱、TUI 可用或降级、退出后 `stty` 复原 | **PASS**：经典 REPL 与 `--tui` 各一轮；空闲 Ctrl+C 以 0 退出、退出后 `stty` 复原 | 同上；`apps/cli/tests/pty.rs` 的 pty 用例是接近 tmux 的自动化面 |
| SSH | 启动 banner、提交一次任务、编辑器重绘/历史/Ctrl+C、`/help` 在 80 列不折行错乱、TUI 可用或降级、退出后 `stty` 复原 | **SKIP + 原因**：本机没有可用的 SSH 目标终端（回环探测退出 255） | 同上；本机没有可用的目标终端时该项记 SKIP + 原因（**不记 PASS**） |
| 80 列 | 启动 banner、提交一次任务、编辑器重绘/历史/Ctrl+C、`/help` 在 80 列不折行错乱、TUI 可用或降级、退出后 `stty` 复原 | **PASS（含 1 条版式注意）**：输入行按显示宽度折行（`… ` 续行标记）、答复折行与 `stty` 复原正常；`/help` 3 行 >80 列由终端软折行（文字完整，见 `docs/acceptance.md`） | 同上；折行/制表位的本地自动化在 `apps/cli/src/render/width.rs` 与 `tests/render.rs` |
| 无色（`NO_COLOR=1`） | 启动 banner、提交一次任务、编辑器重绘/历史/Ctrl+C、`/help` 在 80 列不折行错乱、TUI 可用或降级、退出后 `stty` 复原 | **PASS**：`NO_COLOR=1` 与 `ignis config set ui.color never` 各一轮，原始流**零 SGR**（彩色正对照 6 条） | 同上；无色帧的本地自动化在 `apps/cli/src/tui/view.rs` 与 `tests/render.rs` |
| 浅色主题（`ui.theme=light`） | 启动 banner、提交一次任务、编辑器重绘/历史/Ctrl+C、`/help` 在 80 列不折行错乱、TUI 可用或降级、退出后 `stty` 复原 | **SKIP + 原因（主观项）**：本机没有可用显示环境，浅色背景的对比度无法目视判断；`ui.theme light` 下的客观观察（不崩、调色板只差 dim 槽）已记录在 `docs/acceptance.md`，不构成该项 PASS | 同上；调色板的本地自动化在 `apps/cli/src/render/mod.rs` 的 palette 用例 |

> **结论列的口径**：这一列只放 Task 6 真机跑完后**实际写进** [`docs/acceptance.md`](docs/acceptance.md) 的结果（逐项 PASS，或在跑不了时记 SKIP 并写明原因）。2026-10-19 发布门已按该口径填入：本机三门 SKIP（默认终端、SSH、浅色主题的主观项）都带原因，**SKIP 不记 PASS**；逐项命令与实际观察见 `docs/acceptance.md` 的「v0.2 验收」段。矩阵行与那段里的终端矩阵同源，README 只写结论列。

## 三个演示

> 三启动演示在仓库根目录执行；Demo A/B/C（v0.1 三场景）在临时目录执行 `examples/` 夹具。真实演示都需要一个可用的 OpenAI 兼容端点（`IGNIS_BASE_URL` / `IGNIS_MODEL` / `IGNIS_API_KEY`，见[快速开始](#快速开始)）。

### v0.2 三启动演示（可重复，主演示）

同一条记忆、同一个会话跨三次启动被复用：第一次记下项目约定，第二次用 `-c` 恢复同一会话并在任务里复用记忆，第三次在 TUI 里恢复同一会话。

```sh
# 第一次：起 REPL（工作区 = 当前仓库根）
ignis --workspace .
# 在提示符下输入：
#   这个项目使用 uv 管理 Python 环境，请记住
# 期望：工具日志里出现 remember 的确认行   ✓ memory: pkg_manager (project) …
# 退出：
#   /exit

# 第二次：恢复本工作区最近一次会话
ignis -c
# 输入 /memory —— 应看到 pkg_manager（scope=project，值就是上一轮记下的话）
# 输入：
#   帮我检查这个项目的依赖并运行测试
# 期望：工具日志里出现 uv sync / uv run（复用记忆，模型选择了 uv 工作流）
# 退出：
#   /exit

# 第三次：在 TUI 里恢复同一会话
ignis --tui -c
# 顶部状态行：IGNIS v0.2.0 · <工作区> · kernel native · session <13 字符短 id>
# 输入 /status —— session: 行与上面同值（与第二次同一个会话 id）
# 退出：
#   /exit
```

- 第一次的确认行形状固定为 `  ✓ memory: <key> (<scope>) <文本 ≤60 字符>`（key 与文本由模型按你的话定，示例里是 `pkg_manager`）；`/memory` 的 `KEY` 列能看到同一事实（`SCOPE` 列是 `project`）。记忆是 `scope=project`，因此只在本工作区注入。
- 第二次的 `uv sync` / `uv run` 是**模型的选择**：机器上没装 uv 时按本仓库既有口径只断言「模型选择了 uv 工作流」（工具日志里出现这两个命令），不断言依赖真的装上。
- 第三次的短 id 取会话 id 前 13 字符（与 `/sessions`、`/status` 同一口径，`/resume` 与 `ignis task show` 接受 ≥8 字符的唯一前缀）。TUI 不打印经典 banner，会话身份看顶部状态行与 `/status` 的 `session:` 行。
- 任务与会话的绑定落在 `tasks.session_id`（每轮任务都写）：`ignis task list` 显示最近任务（第 5 列 `PROMPT` 即上面第二/第三次那条），**表里不含会话列**；要核对绑定就直接查库，例如
  `sqlite3 ~/.ignis/ignis.db "select id, session_id from tasks order by started_at desc limit 5"`（或再起一次 `ignis -c`，看到的是同一个会话）。

### v0.1 三场景（回归）

以下 Demo A/B/C 是 v0.1 交付的三场景，命令与期望**原样保留**——它们仍是本仓库的回归面（自动化用例在 `apps/cli/tests/demo_a.rs` / `demo_b.rs` / `demo_c.rs`）。`examples/` 是夹具，复制到临时目录以免污染仓库。

#### Demo A · AI 真正完成工作

```sh
demo_a=$(mktemp -d)
cp -r examples/broken-project "$demo_a/project"
cd "$demo_a/project"

npm test          # 预期失败：ENOENT（配置路径多退了一层 ..）
ignis --workspace . run "分析这个项目的测试失败原因，修复它并重新运行测试" --yes
npm test          # 预期通过
```

Ignis 会读 `src/config.js`、跑 `npm test`、定位错误、经审批层修改文件（这里用 `--yes` 预授权，审批记录照样写入）、重跑并汇报；修复后的 `npm test` 退出 0。夹具的测试用 `npm` 跑 `node --test`（Node ≥ 18）。

#### Demo B · AI 记住过去的经验

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

#### Demo C · AI 在后台主动工作

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
- **Demo C（2026-10-16 真机复验）**：调度全链路通过（作业提示含 `--simulate 92` → 守护 `shell_run` 读到 92% → `notify_user` → `alerts` 落库）；该场景的 `demos 3` 连胜受模型波动影响——作业提示由模型自写，漏掉模拟步骤时「不告警」是正确行为，稳定性说明与 `--keep` 诊断配方见 [`docs/acceptance.md`](docs/acceptance.md) §5。

## 架构

```
                  ┌───────────────────────────────────────────────┐
  用户 ──────▶    │ apps/cli（bin: ignis）                        │
                  │  REPL · TUI(Beta) · 子命令 · 审批 UI · daemon │
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
- **CLI 分层**（`apps/cli`）：`render/`（流式渲染：markdown 子集、宽度与折行、调色板）、`editor/`（raw mode + 行编辑器）、`commands/`（子命令与斜杠命令）、`tui/`（Beta 全屏界面：备用屏、捕获、帧合成、输入路由），用同一个 `Session` 装配内核与存储。
- **依赖方向**：`runtime` 定义 `Runtime` / `ToolExecutor` 等 trait（含内核能力声明），`core` 实现工具执行与审批并注入；native 执行循环在 `runtime` 侧——所以是 `core → runtime`，无循环依赖。`runtime` 的 `RuntimeEvent` 序列化形状就是任务 JSONL 的格式，属跨模块契约。
- **数据流**：CLI 或调度器发起 Task → core 组装上下文（记忆、技能目录、工作区）→ 内核执行（native 的工具调用经 security 层审批）→ 事件流回写 JSONL 与 UI → 结束写用量、记忆、会话行与告警。

## 安全模型

- **工作区沙箱**：文件与 Shell 工具解析后的路径必须落在工作区内（工作区外的读写直接拒绝）；Shell 的工作目录固定为工作区。
- **审批三档**（风险等级 × 运行模式）：
  - 交互终端（stdin 是 TTY，且无开关）：`Safe` 直接执行；写文件 / Shell / 外联请求 `Ask`，终端内联提示 `[a]pprove / [s]ession-allow / [r]eject / [d]etails`；危险操作同样询问。
  - `--yes`：预授权常规风险操作（`Elevated` 直接放行）；危险操作仍拒绝，需要显式 `--unsafe`。
  - `--unsafe`：预授权全部操作（含危险命令库）。
  - 非交互且无开关（管道、脚本、无人值守）：**fail-closed**——写 / Shell / 外联一律拒绝并记录原因，模型收到拒绝结果后可继续。
- **危险命令库**：`rm -rf`、`sudo`、`curl | sh`、`chmod 777`、`mkfs`、`dd if=`、关机类命令、写块设备、fork bomb 等强制走危险档。
- **敏感文件**：`.env` / `.env.*`、`*.pem`、`*.key`、`id_rsa*`、`id_ed25519*`、`.netrc`、`.git-credentials` 的读写升级为危险档（含软链别名解析后的判定）。
- **审计**：所有非 `Safe` 调用（放行或拒绝）都会写 `approvals` 记录；记录写不进去就**不执行**（fail-closed）。`ignis task show <id>` 可查审批记录与事件流。
- **脱敏**：密钥统一经脱敏器，不进入任务日志、错误输出与终端；`config show` 与 `Debug` 恒显示 `***`；配置文件写盘为 0600。
- **后台任务**：创建时即确定执行边界（工具白名单 + 风险上限 + 时间/token 上限 + 工作区），运行期严格执行；失败有记录并告警，不无限重试，重叠执行跳过，停机错过的时点不补跑。
- **外置内核**：工具在 codex / claude 自己的沙箱里执行（`workspace-write` / `manual` 档，适配器不自行放宽）；Ignis 不经手审批，也没有审批行——这些内核的能力边界见[限制](#限制)。

## 限制

1. **调度 / 守护进程只跑 `native`**（决策 #18）：`packages/gateway` 的守护进程固定装配 `NativeRuntime`；`jobs.kernel` 列存在但恒为 `native`（`schedule_create` 没有内核参数），`--kernel codex|claude` 不影响守护进程。即使配置 `default_kernel` 是外置内核，守护进程仍需要 Ignis 自己的 OpenAI 兼容凭据。
2. **外置内核下 Ignis 工具 / 审批 / 审计不适用**：工具在 codex / claude 自己的沙箱里执行；Ignis 不注入工具、不经手审批、没有 `approvals` 行；`remember` / `load_skill` / `notify_user` / `schedule_create` 不可用（能力门控会把系统提示里的工具清单收成一句说明）。
3. **缺失已配置的外置 CLI 时 `run` 直接失败，无回退**：`ignis run --kernel codex` 在会话装配期就报「找不到 codex CLI…」，不会悄悄回落 `native`；`ignis doctor` 对选中的缺失内核给 `✗`。
4. **`base_url` 探测按生效内核分级**：选 `native` 时端点不可达（或地址不成形）是 `✗`；选 `codex` / `claude` 时降级为 `!`（Ignis 不访问这个端点）。注意守护进程固定跑 `native`（见限制 #1）——外部内核下的 `!` 只说明当前交互内核不受影响，调度任务仍会在这个端点上失败。
5. **Windows 上内核发现未做 PATHEXT 解析**：`find_program` 只做「PATH 目录拼接 + 存在性 + Unix 执行位」判定，本机只在 Unix 实测。
6. **`--version` 探测有界、但不解析输出语义**：探测把 `stdin` 接空设备并限时 5s（超时杀子进程，按「CLI 不可用」如实报，不再拖住 `ignis doctor`）；代价是两处假读数——启动噪声打到 `stderr` 可能被当成版本（假 `✓`），5s 内不吐输出的 CLI 被判不可用。
7. **外置内核用量与上限退化**：codex / claude 不出 `Usage` 事件（`Capabilities{usage:false}`），token 记为 0；因此 token / 成本上限对它们不生效，只剩墙钟超时（`limits.task_timeout_secs`）与取消（REPL Ctrl-C / SIGINT）。
8. **调度重扫窗口 30s**：新建 / 暂停 / 恢复的调度任务最迟 30s 生效（守护进程每轮最多睡 30s 重读 active job；无到期任务时也一样）。
9. **`/kernel` 不共享外置会话**：每轮任务都把 Ignis 的 canonical 历史渲染进 prompt 交给一个新的 CLI 进程，不依赖也不复用 codex / claude 自己的 `--resume` 会话（切换内核不会带过来外置 CLI 的会话状态）。
10. **relay spike 结论（已关闭）**：原生中转（`opencode.ai/zen/go`）两轮「不可得」为套餐额度 / 端点配置问题，**不是协议不兼容**；随后以一个可用的 OpenAI 兼容端点（`https://api.minimax.cn/v1`，模型 `MiniMax-M2`）复验，流式工具调用、`usage` 记账与审批链路全部通过（该端点不发 `[DONE]`，流以 EOF 收尾，native 客户端按 EOF 正常结束）。降级路径（`stream:false` / Anthropic 协议）无需启用。详见 [`docs/spikes/2026-10-15-relay-streaming-toolcalls.md`](docs/spikes/2026-10-15-relay-streaming-toolcalls.md)。
11. **调度策略**：失败即记录 + 告警、不无限重试；上一次运行未结束时再次到期只跳过并记日志；停机期间错过的时点不补跑（防重启后重复执行危险操作）；过期作业（间隔长于重扫窗口）按「首次观察 + 间隔」触发一次、不补跑。多作业并存时，短间隔作业若始终被更早到期的作业先唤醒，可能被反复推后（P3 既有行为；单作业场景含 Demo C 不受影响，见 [`docs/acceptance.md`](docs/acceptance.md) §5）。

## 已知偏差

- **TUI 是 Beta（`ignis --tui`）**：刻意不做鼠标（没有鼠标事件解码与命中测试，终端自身的选区 / 滚动照旧可用）；滚动区有界（只留最近 2000 行，超出的丢最旧）；任务运行中不接受输入（忙碌期按键一律丢弃，输入区首行是 `working…`、提示行是 `Ctrl+C cancels`）；Esc 的消歧窗口是一个 100ms tick（`Alt+Enter` 被切在两个 tick 之间时可能被当成 Enter）；`\t` 只按 8 列制表位展开到屏幕（提交 / 落盘 / 送模型的文本一个字节不动）；**SIGTERM / SIGTSTP / SIGHUP 没有处理器**——被 `kill`、`Ctrl+Z` 挂起或终端断开时进程直接走掉，备用屏与 raw mode 由终端自己收尾（回到 shell 可能看起来坏掉），敲 `stty sane` 或 `reset` 复原；正常退出（`/exit`、空缓冲 `Ctrl+D`、空闲 `Ctrl+C`、stdin 关闭）与 panic 都由终端守卫的 `Drop` 还原。
- **`--tui` 不默认恢复会话**：只有 `-c` / `--continue` 才装载本工作区最近一次会话（要恢复就 `ignis --tui -c`）；TUI 不满足条件（stdin / stdout 不是终端、`TERM` 是哑终端、尺寸小于 20×8）时只多打一行 `note: TUI unavailable (<原因>); falling back to the classic REPL`，其余一切照旧。
- **经典 REPL 在「读一行」期间不响应 Ctrl-C**：审批提示与 `/learn` 的保存确认（`save this skill draft? [s]ave [d]iscard> `）在读一行期间不响应 Ctrl-C——进程级 SIGINT 只被接住、不取消那次同步读；模型往返本身（任务轮次、`/compact`、`/learn` 的草稿生成）可以被 Ctrl-C 取消。TUI 里 Ctrl-C 会取消本轮（任务 / 压缩 / 学习），不转发给工具、也不改审批决定。
- **`/compact` 与 `/learn` 需要 native 凭据**：两者各发一次无工具的 native 请求，外置内核或没有 Ignis 凭据时明确报错（不静默降级、不改状态）；外置内核的用量 / 上限退化（token 记 0、只剩墙钟超时与取消）与「不经 Ignis 审批、无 `approvals` 行」见[限制](#限制) #2、#7。
- **脱敏是字面替换**：已知密钥按字面替换为 `***`（先 JSON 转义形式、后原文），不做正则或相似度匹配——密钥的截断 / 重组 / 编码变体不在覆盖内；**修复前已落库的明文行不回写**（没有 scrub / migration，新代码不再产生新的明文行）。
- **banner 形状与 `demo.md` 概念图不同**（`demo.md` 原文标注「CLI 概念展示，并非已经实现的 Ignis 界面」）：实际启动屏为 `IGNIS v0.2.0` / `● Ready` / `Workspace: <dir>` / `Runtime: <kernel> · Skills: <n> · Memory: <n> facts` / `Session: <new|13 字符短 id>`（版本号逐字取包版本 `env!("CARGO_PKG_VERSION")`，当前 `0.2.0`），有未读告警时追加一行 `⚠ N unread alerts — run \`ignis alerts\``。
- **设计文档措辞与 `demo.md` 不完全一致**：`docs/superpowers/specs/2026-10-15-ignis-v0.1-design.md` 与 `docs/superpowers/specs/2026-10-16-ignis-v0.2-design.md` 按对话中的新决定固化了工程取舍（Rust 取代 TS/Node、Telegram 推迟为 CLI 收件箱 / 桌面通知、内核数量、v0.2 的交付口径等），冲突时以设计文档为准。
- **设计文档的 DDL 措辞与实现的库表不同**：v0.1 设计文档 §6.7「任务模型」只给字段级草案（§9 把「数据库 schema 字段级细化」留给构建者），与实现有三处出入——实现多了 `jobs.workspace` / `jobs.last_run_at` / `job_runs.task_id`，用量是 `job_runs.input_tokens` + `output_tokens` 两列（没有单列 `usage`），`alerts` 表在设计文档里只有文字描述；v0.2 设计 §6.6 的会话 DDL 与实现的 004 一致。一律以 [`packages/storage/src/migrations.rs`](packages/storage/src/migrations.rs) 的 001–004 DDL 为准。
- **`demo.md` 按作者要求保持原文未修改**；上述偏差只记录在这里，不回改历史文档。
- **e2e 测试位于 `apps/cli/tests/`**：`CARGO_BIN_EXE_ignis`（真构建二进制路径）只对拥有 bin 的包可用，`packages/*` 里的 crate 没有 bin，因此这类用例不能下沉到 `packages/`。

## 开发

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # 离线：不依赖网络与真实模型
```

- 单元测试随各 crate；外置内核适配器的假 CLI 集成测试在 `packages/runtime/tests/`。
- 端到端测试在 `apps/cli/tests/`（`demo_a.rs` / `demo_b.rs` / `demo_c.rs` / `approval_modes.rs` / `kernel.rs` / `daemon_lifecycle.rs` / `autostart.rs` / `limits.rs` / `redaction.rs` / `remember.rs` / `render.rs` / `tui.rs` / `pty.rs`）：用真构建的 `ignis` 二进制 + `ignis-runtime` 的脚本化假模型（`test-support`），断言落在真文件、真数据库行与真进程上。
- 真实模型的量化验收（三场景各连续 3 次、20 例常见任务 ≥80%、10 次重启）是**手动、可选**执行（控制额度消耗），不进 CI：三条门槛见 [`harness/acceptance.sh`](harness/acceptance.sh)（`demos 3` / `tasks 20` / `restarts 10`），门槛定义、运行方法与最近结果记录见 [`docs/acceptance.md`](docs/acceptance.md)。
- 提交信息用 Conventional Commits（英文 type/scope，中文正文）。
- Docker / systemd 部署见上文与 [`deploy/README.md`](deploy/README.md)。

## English summary

Ignis v0.2 is a self-hosted, single-binary personal AI agent written in Rust. It implements its own agent loop (the `native` kernel, talking to any OpenAI-compatible endpoint with streaming tool calls, markdown rendering and a separate reasoning channel) and can also delegate a task to an installed `codex` or `claude` CLI (`--kernel`). It keeps long-term memory, discovers `SKILL.md` skills, and runs scheduled jobs from a background daemon — designed around one idea: *Ignis should remember, act, and improve*. On top of that, v0.2 makes the terminal interactive and persistent: a multiline editor with history, tab completion and bracketed paste; sessions that survive restarts (`-c`, `/sessions`, `/resume`, `/context`, `/compact`); fifteen local slash commands listed by `/help`; `ui.output_mode` / `ui.color` / `ui.theme` configuration; a full-screen TUI Beta (`--tui`); and SIGINT-safe terminal restoration. Tools run inside the workspace and every non-safe call is approval-gated and audited; external kernels run tools in their own sandbox and do not participate in Ignis approvals or usage accounting. Build it with `cargo build --release`, verify with `ignis doctor`, and start with `ignis run "<task>" --yes`.
