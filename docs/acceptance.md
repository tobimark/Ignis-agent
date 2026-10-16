# 验收 harness（手动 · 可选 · 真实模型）

`demo.md` §七给 V0.1 定了三条**内部门槛**；`harness/acceptance.sh` 是这三条的执行器：跑真二进制、
连真实 OpenAI 兼容端点、核对真数据库行与真 JSONL 日志。它**不进 CI**、**不是 `cargo test` 的一部分**，
且在没有真实模型凭据时**拒绝启动**（退出码 2）——「跳过」永远不会被伪装成「通过」：逐项 SKIP 都带原因，
且**门槛级 SKIP**（如本机无 `uv` 导致 Demo B 整条跳过）会把整体结论判成「验收不完整」、退出码非 0。

- 脚本：`harness/acceptance.sh`（bash；`bash -n` 通过）
- 任务清单：`harness/tasks.md`（20 例常见任务，逐条含机器可判定的预期）
- 结果文件：`harness/results-<时间戳>.md`（自动生成，已 gitignore；不含任何密钥）

## 1. 三条门槛

| # | 门槛（demo.md §七） | 命令 | 判定口径 |
|---|-------------------|------|---------|
| 1 | 三个演示场景**各连续成功 3 次** | `harness/acceptance.sh demos 3` | A / B / C 三行门槛各为 `3/3` 才算 PASS；任一次 FAIL → 该场景 FAIL → 退出码 1；任一场景整条 SKIP（如无 uv 的 B）→ 门槛 SKIP → 结论「验收不完整」→ 退出码 1 |
| 2 | 20 次常见任务端到端成功率 **≥80%** | `harness/acceptance.sh tasks 20` | `PASS / (PASS+FAIL) ≥ 80%`；`harness/tasks.md` 的前 N 条按顺序执行 |
| 3 | 10 次服务重启**不重复执行、不丢记录** | `harness/acceptance.sh restarts 10` | 每次 `stop → start` 后核对：同一时刻无重复触发、无重复 task 引用、无孤儿运行行、历史运行不减少；收尾无守护进程残留 |

三条各自独立跑；一次完整的验收 = 三个命令都跑且都退出 0（即没有任何 FAIL、门槛行也没有 SKIP）。
`--dry-run` 只演练流程、不联网、不写结果文件，用于确认脚本与夹具就位。

> 口径说明：`restarts` 模式**刻意**用 20 秒间隔的调度任务（脚本里的 `PROMPT_RESTART`）——重启周期必须
> 真的产出新的运行行；≥1 分钟的「从未运行」job 即使修复后也要等到 `created_at + 间隔`（60s）才首次
> 到期，与重启周期不匹配。**>30 秒的首次到期路径**由 gateway 修复的判别测试（`never_run` 两条用例）与
> `demos` 的 Demo C（1 分钟档）覆盖，不靠 restarts 模式。

### 每个场景具体核对了什么

| 场景 | 证据（全部来自真库/真文件/真日志） |
|------|-----------------------------------|
| Demo A（修好坏项目） | 修复前 `npm test` 必须失败（复现）；`ignis run` 退出 0 且 `tasks.status=success`；修复后 `npm test` 退出 0；`approvals` 含写操作（`fs_edit`/`fs_write`）；`input_tokens+output_tokens > 0`；`logs/<task_id>.jsonl` 存在且含 `task_end` |
| Demo B（跨会话记忆） | 会话 1 `remember` 落库（active 事实含 uv）；**新进程**会话 2 的工具日志出现 `uv sync` / `uv run`（注入命中）；会话 3 纠正后旧 uv 事实 `superseded`、新 pip 事实 active。**uv 未安装 → 整条记 SKIP（原因写明），不记 PASS**；门槛行随之 SKIP，整体结论为「验收不完整」（退出码 1） |
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
- Demo B 完整跑需要 `uv`（`curl -LsSf https://astral.sh/uv/install.sh | sh`）；未安装则 B 记 SKIP，
  且该门槛 SKIP 会让整体结论变成「验收不完整」（退出码 1）——B 项必须装了 uv 才会被真正测量。
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

- `0`：没有任何 FAIL，且**门槛行没有 SKIP**。逐项明细里允许带原因的 SKIP（`tasks` 模式逐条只记
  PASS/FAIL，SKIP 与门槛 SKIP 只出现在 `demos` 的 Demo B、无 `python3` 的密钥原文扫描这类路径）；
  一旦门槛行出现 SKIP（例如无 uv 时 Demo B 整条跳过），结论是「验收不完整」，退出码 1——某条门槛
  从未被测量，就不能叫通过。
- `1`：有 FAIL（含门槛未达标）**或**门槛行有 SKIP（「验收不完整」）。注意：门槛行按成功率判定，
  而退出码更严——任何一条 FAIL 都会给 1，例如 17/20（85%，门槛 PASS）里那 3 条失败仍会让退出码为 1。
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
| （运行后填写） | 例：`https://your-endpoint/v1` / `your-model` | `demos 3` / `tasks 20` / `restarts 10` | 例：A 3/3、B SKIP（无 uv）→「验收不完整」退出码 1；tasks 17/20；restarts 10/10 | `harness/results-<ts>.md` |
| 2026-10-16 | `https://api.minimax.cn/v1` / `MiniMax-M2` | `demos 1`（脚本首跑，**不是**门槛达标） | Doctor ✓；A 1/1 PASS；B SKIP（无 uv）；C 0/1 FAIL（真阳性，见下） | `harness/results-20261016-154501.md` |
| 2026-10-16 | 同上 | `tasks 2`（前两条，**不是**门槛达标） | 2/2 = 100%（N=2 < 20，未计完整门槛）；退出码 0 | `harness/results-20261016-155357.md` |
| 2026-10-16 | 同上 | `demos 1`（**中间调试运行**，**不是**门槛达标） | Doctor ✓；A 0/1 FAIL（run 退出码 1、修复后 `npm test` 仍失败——模型侧未产出修复）；B SKIP；C 0/1 FAIL（创建 run 本身失败，job 未落库） | `harness/results-20261016-161316.md` |
| 2026-10-16 | 同上 | `demos 1`（**修复后复验**，**不是**门槛达标） | Doctor ✓；A 1/1 PASS；B SKIP（无 uv）；**C 1/1 PASS**（`run=success kind=schedule alert=1`，daemon 停止后无残留）；按当时脚本为退出码 0（现行口径见下注） | `harness/results-20261016-161436.md` |
| 2026-10-16 | 同上 | `demos 1`（**中间调试运行**，**不是**门槛达标） | Doctor ✓；A 1/1 PASS；B SKIP（无 uv）→「验收不完整」退出码 1；C 0/1 FAIL：调度运行 success 但 **`alert=0`**——作业提示（模型自写）漏掉 / 写偏模拟步骤时观测值低于阈值，**「不告警」是正确行为**（模型侧抖动，非产品缺陷；归因见下「Demo C 稳定性说明」） | `harness/results-20261016-163751.md` |
| 2026-10-16 | 同上 | `demos 1`（门槛 SKIP 语义修复后复验，**不是**门槛达标） | Doctor ✓；A 1/1 PASS；B SKIP（无 uv）；C 1/1 PASS；结论「**验收不完整**：Demo B ×1 未运行」——跳过 ≠ 通过，退出码 1 | `harness/results-20261016-164015.md` |
| 2026-10-16 | 同上 | `demos 1`（**中间调试运行**，**不是**门槛达标） | Doctor ✓；A 1/1 PASS；B SKIP（无 uv）→「验收不完整」退出码 1；C 0/1 FAIL：调度运行 success 但 **`alert=0`**——归因：作业提示（模型自写）漏掉模拟步骤，观测值低于 85% 阈值，**「不告警」是正确行为**（模型侧抖动，非产品缺陷） | `harness/results-20261016-184015.md` |
| 2026-10-16 | 同上 | `demos 1`（**中间调试运行**，**不是**门槛达标） | Doctor ✓；A 1/1 PASS；B SKIP（无 uv）→「验收不完整」退出码 1；C 0/1 FAIL：与 184015 同因——**`alert=0`**（作业提示漏掉模拟步骤；「不告警」是正确行为） | `harness/results-20261016-185235.md` |
| 2026-10-16 | 同上 | `demos 1`（**修复后复验**，**不是**门槛达标；`d4acc71` 之后） | Doctor ✓；A 1/1 PASS；B SKIP（无 uv）→「验收不完整」退出码 1；**C 1/1 PASS**：全链路通过——作业提示含 `check-health.sh --simulate 92` → 守护 `shell_run` 读到 92% → `notify_user` → `alerts` 行（`warning / Disk usage high: 92%`）；`alert=1`、daemon 停止后无残留 | `harness/results-20261016-185458.md` |

> 注：2026-10-16 早前的两次 `demos 1`（154501 / 161436）是**当时**脚本的结论；其中 161436 那次带 B SKIP
> 却仍打印「验收通过、退出码 0」。本修复（门槛 SKIP ⇒「验收不完整」、退出码 1）落地后，任何带 B SKIP 的
> `demos` 运行都不会再打印「验收通过」——164015 那次已按新口径退出 1。`tasks 2` 无门槛 SKIP，仍为退出码 0。
> 161316 / 163751 两次中间运行的 A/C 失败分别是模型侧未完成与作业提示漏掉模拟步骤的抖动（原因见行内），
> 不是脚本或产品缺陷；两者的结果文件都保留为「中间调试运行」的原始记录。此后 184015 / 185235 的
> `alert=0` 同因（提示漏掉 / 写偏 `--simulate 92` 时观测值低于阈值，「不告警」是正确行为；归因见下
> 「Demo C 稳定性说明」）；185458 全链路 PASS 复验了调度与告警机制本身。

### Demo C 稳定性说明（两段式链路；`demos 3` 连胜取决于模型波动，不等价于调度器复验）

Demo C 是**两段式、作业提示由模型自写**的管线：第一段 `ignis run`（`PROMPT_C`）里，模型**自己写**
要落库的调度作业提示（`jobs.prompt`）；第二段由守护进程按点执行该提示。因此「Demo C 连续 3 次成功」
不只取决于调度器与告警链路，也取决于模型每次都把提示写对（含 `check-health.sh --simulate 92` 与
「超过 85% 就 `notify_user`」）。提示漏掉 / 写偏模拟步骤时，作业运行观测到的值低于 85% 阈值，
**「不告警」是正确行为**——163751 / 184015 / 185235 的 `alert=0` 都是这一因（模型侧抖动，不是 C 链路
回归）；这类失败请先查提示，再怀疑产品。

`--keep` 诊断配方（保留本次临时目录后按两处查；`<home>` = `<临时目录>/demo-c-1/home`）：

```sh
# 1) 作业提示里有没有模拟步骤（92% 观测值的前提）
sqlite3 <home>/ignis.db "SELECT name, prompt FROM jobs"
# 取这次调度运行的 task_id，用于第 2 步（多轮运行时取最新一行）
sqlite3 <home>/ignis.db "SELECT task_id FROM job_runs ORDER BY started_at DESC LIMIT 1"
# 2) 守护那轮运行的 JSONL 里有没有调用 notify_user（提示正确 + 有观测却不落告警，才怀疑链路）
grep -o '"name":"notify_user"' <home>/logs/<task_id>.jsonl
```

判据：提示原文含 `--simulate 92`、且运行成功却不落告警 → 怀疑产品链路；提示本身漏步骤 → 模型抖动，
按「正确行为」归因，不判产品缺陷。**调度与告警机制本身**已由 185458 的真机全链路（`shell_run` 读到
92% → `notify_user` → `alerts` 行 `warning / Disk usage high: 92%`）与 `packages/gateway` 的测试
（调度循环 / 到期锚 / 失败告警与投递渠道）验证，不需要靠 `demos 3` 连胜来证明。Demo C 详情行自本波起
记录 `job=<name> simulate=yes|no`，并在 `alert=0` 失败时附作业提示 / JSONL 的归因提示
（见 `harness/acceptance.sh` 的 `demo_c_once`）。

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

**补丁（Wave A，`fix(gateway): 补上休眠作业的首次到期锚并收紧收尾证据`）**：上面的「锚在创建
时刻」对**休眠**作业仍不完整——`created_at + 间隔` 在守护进程启动前就已过去时（守护进程停机期间
创建，或停机 ≥ 一个间隔），每轮重扫都把到期现算成 `now + 间隔` 并跳过：job 会**静默地永不触发**
（真机复现：昨日创建的 `10m` job，今早启动守护进程，138s 内 0 次运行、`last_run_at` 仍为 NULL）。
修复：调度循环在首次观察到这类 job 时把锚钉进**进程内快照**（= 首次观察 + 间隔），此后每轮复用
同一锚——一个守护进程生命周期内只在锚点触发一次、不补跑；首轮在飞期间该 job 不参与唤醒（不空转、
不补出第二行），运行结束后由 `last_run_at + 间隔` 续拍；重启后锚重新快照（不落库是刻意的「不补跑」
取舍）。公开的 `due_at()`（CLI `schedule list` 用）保持无状态不变。判别用例（`packages/gateway/src/loop.rs`）：
`created_at = now − 2·间隔`、重扫 1s、间隔 3s 的休眠 job 修复前 6s 内 0 次运行（RED）、修复后
~3s 触发（GREEN）；另有「首轮后 cadence = `last_run_at + 间隔`、重扫窗口不补跑」与快照复用用例。
（上面这条快照当时只覆盖「从未运行」的休眠 job；同一类缺陷的**跑过**作业变体由 Wave C 泛化修复，见下。）
`demos` 的 Demo C 详情行现在记录 `expr=<kind/expr>`，并在间隔 ≤ 30s 重扫窗口时打印 note 说明
「>30s 首次到期」路径未被覆盖（见 `harness/acceptance.sh`）。

**补丁（Wave C，`fix(gateway): interval 作业过期后按首次观察重新锚定（泛化停机场景）`）**：Wave A
的快照只治了「从未运行」的一半，**同一滑动到期缺陷对跑过的作业完全一样**——`last_run_at + 间隔`
已经过去时（守护进程停机 ≥ 一个间隔：重启、升级、崩溃、笔记本休眠），每轮重扫都把 `next_due` 现算成
`now + 间隔` 并跳过，job 会**静默地永不再触发**（重扫周期 ≤ 30s ≤ 间隔时必然发生；修前判别测试
6s 内 0 次运行）。修复即把首次观察锚**泛化**到一切过期的 interval 作业：锚点（`last_run_at`，
从未运行过时取 `created_at`）`+ 间隔 ≤ 现在` 时，首次观察快照 `首次观察 + 间隔` 并复用同一锚——
一个守护进程生命周期内触发一次、不补跑；锚点 + 间隔 仍晚于现在（新 job、刚跑过的 job）维持原拍子
**不进快照**；cron 一律维持无状态口径；`due_at()`（CLI `schedule list` 用）不变；无 schema 变更，
重启重新快照（仍是刻意的「不补跑」取舍）。判别测试（`packages/gateway/src/loop.rs`）：
① `last_run_at = now − 2·间隔`、重扫 1s、间隔 3s：修复前 6s 内 0 次运行（RED）、修复后 ~3s 触发；
② 过期作业只触发一次、随后由 `last_run_at + 间隔` 续拍（重扫窗口不补跑）；③ 刚跑过的作业
（`last_run_at + 间隔` 仍晚于现在）按原拍子触发且不进快照；既有两条休眠作业用例保持 GREEN。

脚本自测记录（**不是验收结果**，仅验证脚本逻辑。假端点把 Demo C 的 `schedule_create` 脚本化为
**2 秒档**间隔，与 `apps/cli/tests/demo_c.rs` 的 `2s` 同档；**注意**：短间隔（≤30s 重扫窗口）的假端点
覆盖不到「>30s 的从未运行 job 首次到期」这条路径——真机首跑正是卡在这里，因此那次「C 3/3」只证明
脚本管线可走通，**不构成 1 分钟路径的验收证据**；该路径由 gateway 修复的判别测试与真机 Demo C 覆盖）：
2026-10-16 用本地假端点（脚本化 SSE）跑过 `demos 3`（三场景 3/3）、`tasks 5`（5/5）与 `tasks 20`
（7/20，门槛 FAIL、退出码 1）、`restarts 1/2`（重复/孤儿/丢失全 0），并验证了缺凭据与 CI 的拒绝路径、
`--dry-run`、`--keep`、以及 `sqlite3`/`python3` 两种证据读取后端。
