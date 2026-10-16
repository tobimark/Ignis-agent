# 验收 harness（手动 · 可选 · 真实模型）

`demo.md` §七给 V0.1 定了三条**内部门槛**；`harness/acceptance.sh` 是这三条的执行器：跑真二进制、
连真实 OpenAI 兼容端点、核对真数据库行与真 JSONL 日志。它**不进 CI**、**不是 `cargo test` 的一部分**，
且在没有真实模型凭据时**拒绝启动**（退出码 2）——「跳过」永远不会被伪装成「通过」。

- 脚本：`harness/acceptance.sh`（bash；`bash -n` 通过）
- 任务清单：`harness/tasks.md`（20 例常见任务，逐条含机器可判定的预期）
- 结果文件：`harness/results-<时间戳>.md`（自动生成，已 gitignore；不含任何密钥）

## 1. 三条门槛

| # | 门槛（demo.md §七） | 命令 | 判定口径 |
|---|-------------------|------|---------|
| 1 | 三个演示场景**各连续成功 3 次** | `harness/acceptance.sh demos 3` | A / B / C 三行门槛各为 `3/3` 才算 PASS；任一次 FAIL → 该场景 FAIL → 退出码 1 |
| 2 | 20 次常见任务端到端成功率 **≥80%** | `harness/acceptance.sh tasks 20` | `PASS / (PASS+FAIL) ≥ 80%`；`harness/tasks.md` 的前 N 条按顺序执行 |
| 3 | 10 次服务重启**不重复执行、不丢记录** | `harness/acceptance.sh restarts 10` | 每次 `stop → start` 后核对：同一时刻无重复触发、无重复 task 引用、无孤儿运行行、历史运行不减少；收尾无守护进程残留 |

三条各自独立跑；一次完整的验收 = 三个命令都跑（都退出 0）。`--dry-run` 只演练流程、不联网、
不写结果文件，用于确认脚本与夹具就位。

### 每个场景具体核对了什么

| 场景 | 证据（全部来自真库/真文件/真日志） |
|------|-----------------------------------|
| Demo A（修好坏项目） | 修复前 `npm test` 必须失败（复现）；`ignis run` 退出 0 且 `tasks.status=success`；修复后 `npm test` 退出 0；`approvals` 含写操作（`fs_edit`/`fs_write`）；`input_tokens+output_tokens > 0`；`logs/<task_id>.jsonl` 存在且含 `task_end` |
| Demo B（跨会话记忆） | 会话 1 `remember` 落库（active 事实含 uv）；**新进程**会话 2 的工具日志出现 `uv sync` / `uv run`（注入命中）；会话 3 纠正后旧 uv 事实 `superseded`、新 pip 事实 active。**uv 未安装 → 整条记 SKIP（原因写明），不记 PASS** |
| Demo C（后台主动工作） | `schedule_create` 有审批留痕、job 落库且 active；`ignis daemon start` 在 CLI 进程退出**之后**；等到一轮 `job_runs` 终态 success，其 `task.kind=schedule`（只可能是守护进程写的）；`job_runs.started_at ≥` 创建任务的 `ended_at`（后台触发的时间证据）；告警按 `source_job_id` 落库且 `ignis alerts` 可见；`ignis schedule history` 可见该运行；调度运行的 token 记账 > 0；调度任务 JSONL 存在（字节数即「时间线字节数」）；`daemon stop` 之后不再运行 |

## 2. demo.md 验收清单十条 → 本仓库的检查位置

| # | demo.md 条目 | 检查位置 |
|---|-------------|---------|
| 1 | 干净 Ubuntu 按文档安装并启动 | README 安装主线 + `ignis doctor`（harness 前置自检，退出 0 才准跑） |
| 2 | 真实完成一个文件/代码任务 | `demos` 的 Demo A（修复前失败 → 修复 → 复跑通过 + 审批 + 日志） |
| 3 | 高风险操作经过正确的权限检查 | Demo A 的 `approvals` 写操作留痕；`tasks` 第 19 条要求路径越界被拒（日志含「路径越出工作区」） |
| 4 | 重启后能读取之前保存的记忆 | `demos` 的 Demo B（新进程会话 2/3）；无 uv 时由 `tasks` 第 10/11 条覆盖（写入 → 新进程 `memory search` 命中） |
| 5 | ≥2 个 Skill 被发现、选择、执行 | `demos` 记录技能目录（内置 3 个可见）；`tasks` 第 12–14 条要求三个技能各自被 `load_skill` 加载（JSONL 参数为证） |
| 6 | CLI 退出后后台计划任务仍触发 | `demos` 的 Demo C 与 `restarts` 模式（守护进程启动晚于 CLI 退出、运行行 kind=schedule） |
| 7 | Telegram 收到真实告警 | **推迟**（IM 非目标，见设计文档 §7）：harness 只核对替代渠道（CLI 收件箱 `ignis alerts` + 落库）；本项不记 PASS |
| 8 | 能查看任务结果、失败原因与执行日志 | 每个场景核对 JSONL 收尾行；Demo C 另核对 `ignis schedule history` 输出；`ignis task show` 的离线断言在 `apps/cli/tests/` |
| 9 | 重启不丢记录、不重复危险操作 | `restarts` 模式（重复触发/重复引用/孤儿行/丢行/残留五项检查） |
| 10 | 任务具备超时、取消与成本上限 | 离线 e2e（`apps/cli/tests/limits.rs`、超时/中止用例）+ harness 的 token 记账断言（native 每次运行 tokens>0）；harness 不主动制造超时/取消 |

## 3. 运行方法

前置条件：

- 已构建二进制：`cargo build --release`（或 `IGNIS_BIN=/path/to/ignis`）。
- 本机有 `npm`（Demo A 与任务 #7）、`sqlite3` 或 `python3`（直读 `ignis.db` 核对证据）。
- Demo B 完整跑需要 `uv`（`curl -LsSf https://astral.sh/uv/install.sh | sh`）；未安装则 B 记 SKIP。
- 任意 OpenAI 兼容端点（本仓库在 `https://api.minimax.cn/v1` + `MiniMax-M2` 上真机验证过 native 内核）。

```sh
export IGNIS_BASE_URL=https://your-endpoint/v1   # 任意 OpenAI 兼容端点
export IGNIS_MODEL=your-model
export IGNIS_API_KEY=sk-...                      # 只经环境变量传入；不会写入仓库/结果文件/日志

# 三条门槛（可分开跑；也可先 --dry-run 演练）
./harness/acceptance.sh demos 3
./harness/acceptance.sh tasks 20
./harness/acceptance.sh restarts 10
```

可调环境变量：`IGNIS_BIN`、`IGNIS_ACCEPT_RUN_TIMEOUT`（单次 run 上限秒数，默认 420）、
`IGNIS_ACCEPT_C_WAIT`（Demo C 等一轮调度运行，默认 180）、`IGNIS_ACCEPT_RESTART_SETTLE`（每次
重启后的观察秒数，默认 6）、`IGNIS_ACCEPT_RESTART_BASELINE_WAIT`（restarts 基线等待，默认 180）、
`IGNIS_ACCEPT_ALLOW_CI=1`（在 CI 环境里显式放行——默认 CI 下直接拒绝）。

退出码：

- `0`：没有任何 FAIL（**允许**带原因的 SKIP；门槛行也全 PASS）。
- `1`：有 FAIL（含门槛未达标）。注意：门槛行按成功率判定，而退出码更严——任何一条 FAIL 都会给 1，
  例如 17/20（85%，门槛 PASS）里那 3 条失败仍会让退出码为 1。
- `2`：前置检查未通过（缺环境变量/缺工具/`doctor` 未通过/CI 环境）——**不是**「通过」。

`--dry-run`：不检查凭据、不联网、不写结果文件，只打印每个场景将执行的命令与判定子句；
结束时明确提示「这不是通过」。`--keep`：保留临时目录用于排查。

## 4. 隔离与安全

- **不碰真实数据**：每次运行自建 `mktemp -d` 临时目录，`IGNIS_HOME` 与工作区都在里面；
  夹具（`examples/`）复制到临时目录后再跑，仓库本身不被写入。
- **退出即清场**（`trap EXIT INT TERM`）：先 `daemon stop`，仍活着就按 pidfile 强制收尸，
  再递归删掉临时目录——测试运行后 `pgrep ignis` 无残留、`/tmp/ignis-acceptance.*` 不残留。
- **密钥卫生**：密钥只从环境变量读；结果文件与终端输出不含密钥（端点 URL 里的 `user:pass@`
  也会被打码）；harness 还会用 python3 扫描本次所有日志与工作区，确认没有混入 key 原文。
- **不进 CI**：检测到 `CI` 环境变量时直接拒绝（除非显式 `IGNIS_ACCEPT_ALLOW_CI=1`）；
  `cargo test` 与任何自动化流程都不依赖本脚本。

## 5. 最近结果记录区

每次真实验收后把 `harness/results-<时间戳>.md` 的**门槛表**抄成一行；结果文件本身不入库（已
gitignore），需要长期留存时请贴到发布说明或附件里。

| 日期 | 端点 / 模型（形状） | 命令 | 门槛结果 | 结果文件 |
|------|-------------------|------|---------|---------|
| （运行后填写） | 例：`https://your-endpoint/v1` / `your-model` | `demos 3` / `tasks 20` / `restarts 10` | 例：A 3/3、B SKIP（无 uv）、C 3/3；tasks 17/20；restarts 10/10 | `harness/results-<ts>.md` |
| 2026-10-16 | `https://api.minimax.cn/v1` / `MiniMax-M2` | `demos 1`（脚本首跑，**不是**门槛达标） | Doctor ✓；A 1/1 PASS；B SKIP（无 uv）；C 0/1 FAIL（真阳性，见下） | `harness/results-20261016-154501.md` |
| 2026-10-16 | 同上 | `tasks 2`（前两条，**不是**门槛达标） | 2/2 = 100%（N=2 < 20，未计完整门槛）；退出码 0 | `harness/results-20261016-155357.md` |
| 2026-10-16 | 同上 | `demos 1`（**修复后复验**） | Doctor ✓；A 1/1 PASS；B SKIP（无 uv）；**C 1/1 PASS**（`run=success kind=schedule alert=1`，daemon 停止后无残留）；退出码 0 | `harness/results-20261016-161436.md` |

### 2026-10-16 真机首跑发现：Demo C 触发不了（真阳性，已修复）

`demos 1` 里 Demo C 的失败不是脚本问题：**从未运行过（`last_run_at` 为 NULL）且间隔 > 重扫窗口
（`RESCAN_SECS = 30s`）的 interval job 永远不会触发**——`SchedulerLoop::plan()` 每轮用当前 `now`
重新调用 `next_due(spec, now, None)`（得到 `now + interval`），每 30s 一次的重扫把到期时刻一次次
向后推。Demo A 真机通过（修前测试复现失败 → 修复后 `npm test` 退出 0、3 条审批、token 记账）；
Demo C 的「创建 + 审批留痕 + job 落库」也都真实通过，卡在「后台触发」这一步。

独立复现（不耗模型额度：死端点 + 手工插 job 行）：

| job | 间隔 | `last_run_at` | 观察窗口 | 结果 |
|-----|------|---------------|---------|------|
| A | `1m` | NULL | 95s | **0 次运行（永不触发）** |
| B（对照） | `1m` | 插入时 = now | 95s | 60s 触发，共 1 次 |
| C | `20s` | NULL | 45s | 20s 触发，共 1 次 |

`apps/cli/tests/demo_c.rs` 用 2 秒间隔，正好覆盖不到这一档；README 与 demo C 的真机步骤是 1 分钟
示例，恰好命中。修复方向即「把『从未运行』的首次到期锚在创建时刻」；修复落地后 `demos 3` 才可能
三条同时达标。

**修复（本提交，`fix(gateway): 修复从未运行的 interval 作业永远不会到期`）**：到期计算抽成私有
`due_at()` 并改为锚点制——interval 从未运行过（`last_run_at` NULL）时锚在 `created_at`：新 job
的首次到期 = `created_at + 间隔`，重扫不再把到期时刻一次次推后；锚点已过期（停机跨度）仍不补跑
（`now + every`）；首次运行后由 `last_run_at` 保节奏。两条判别测试：① `due_at` 在 created+1/+31/+59
三次重扫都报 `created+60`（间隔 60s）；② 循环级用例（重扫 inject 为 1s、间隔 3s、从未运行）
修复前 8s 内一次不触发（RED）、修复后 ~3s 触发（GREEN）。模块不变量 #1 的措辞同步收紧。真机
复验：`demos 1` 退出码 0，**Demo C 1/1 PASS**（`run=success`、`kind=schedule`、`alert=1`、daemon
停止后无残留，`harness/results-20261016-161436.md`）；`cargo test --workspace` 505 passed /
0 failed / 1 ignored。

脚本自测记录（**不是验收结果**，仅验证脚本逻辑）：2026-10-16 用本地假端点（脚本化 SSE）跑过
`demos 3`（三场景 3/3）、`tasks 5`（5/5）与 `tasks 20`（7/20，门槛 FAIL、退出码 1）、
`restarts 1/2`（重复/孤儿/丢失全 0），并验证了缺凭据与 CI 的拒绝路径、`--dry-run`、`--keep`、
以及 `sqlite3`/`python3` 两种证据读取后端。
