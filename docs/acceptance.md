# 验收 harness（手动 · 可选 · 真实模型）

`demo.md` §七给 V0.1 定了三条**内部门槛**；`harness/acceptance.sh` 是这三条的执行器：跑真二进制、
连真实 OpenAI 兼容端点、核对真数据库行与真 JSONL 日志。它**不进 CI**、**不是 `cargo test` 的一部分**，
且在没有真实模型凭据时**拒绝启动**（退出码 2）——「跳过」永远不会被伪装成「通过」：逐项 SKIP 都带原因，
且**门槛级 SKIP**（如本机无 `uv` 导致 Demo B 整条跳过）会把整体结论判成「验收不完整」、退出码非 0。

- 脚本：`harness/acceptance.sh`（bash；`bash -n` 通过）
- 任务清单：`harness/tasks.md`（20 例常见任务，逐条含机器可判定的预期）
- 结果文件：`harness/results-<时间戳>.md`（自动生成，已 gitignore；不含任何密钥）

> **v0.2 的验收记录在下一段「v0.2 验收」**（发布门，2026-10-19）。它是 v0.2 验收的**唯一事实来源**；
> 本文件其余部分仍是 v0.1 三条门槛的执行器说明与历史真机记录（v0.2 沿用同一套门槛，口径未变）。

## v0.2 验收（发布门记录 · 唯一事实来源）

> 口径（与 README 终端矩阵、本计划 R5 同一条）：**逐项写明命令与实际观察；跑不了的记 SKIP + 原因，
> 绝不写成 PASS**；任何一条 SKIP 都让整体结论变成「**验收不完整**」。所有自动化项都是离线、hermetic
> 的（临时 `IGNIS_HOME` + 脚本化假模型），随 `cargo test` 跑；手动项在 tmux（3.6）真 pty 上跑，
> 端点用本机 `python3` stdlib 写的**脚本化 SSE 假端点**（只服务终端矩阵观察，不构成任何门槛证据）。
> 原始证据（tmux pane 文本截图、pane 原始字节流、驱动脚本）留在
> `.superpowers/sdd/2026-10-16-ignis-v0.2-p6-release/matrix-evidence/`（gitignore 目录，随本机留存）。

**结论：验收不完整。** 三条真机门槛（`demos 3` / `tasks 20` / `restarts 10`）**本机没有真实模型凭据、
未执行**；终端矩阵的「Ubuntu 默认终端（gnome-terminal/konsole）」「SSH」两行**本机没有可用的终端环境**；
「浅色主题」的**主观可读性**没有可用的显示环境——三处 SKIP 各有原因（见下）。其余项（§8 九条里的
自动化面、两条新指标、终端矩阵其余四行、全量回归）都已实际执行并逐条记录在案。

### 1. 两条新指标（v0.2 新增）

| 指标（设计 §8 / §9） | 落点 | 本次结果 |
|---|---|---|
| 会话启停 ≥10 次、恢复后 canonical 历史逐条相等 | `apps/cli/tests/sessions.rs::session_restore_keeps_the_canonical_history_across_ten_cycles` | **PASS**（新用例，断言清单见下） |
| 流式 100 次不丢不重（随机分片） | `apps/cli/src/render/markdown.rs::random_split_feeding_matches_whole_feeding_byte_for_byte`（100 轮随机切分 + 逐字符 + 跨 UTF-8）、`packages/runtime/src/think.rs::random_fragmentation_keeps_channel_semantics`（100 轮随机分片） | PASS（随 `cargo test --workspace` 全绿） |

会话用例逐条锁住的行为（同一个临时 `IGNIS_HOME` + 同一份工作区 + **同一个假模型句柄**贯穿十轮）：

1. **第 1 轮不带 `-c`**：banner 的 `Session:` 行是 `new`，请求体（去掉 system）只有本轮输入；
2. **第 2–10 轮都以 `ignis -c` 起**：banner 是 13 字符短 id、十轮同一个（且是库里会话 id 的前缀），
   恢复未命中才会打的 note 一次都没出现；
3. **每轮的请求体 vs 库行双向核对**：第 i 轮送进内核的非 system 消息**逐条等于**「前 i−1 轮的
   `session_messages` 行 + 本轮输入」（角色、内容、顺序），每轮 `/exit` 退出后直读库，
   `session_messages` 又**逐条等于**十轮的全量 canonical 序列（含 `seq` 连续性）——没有近似；
4. **第 10 轮结束**：`session_messages` 条数 == `2 × 10`；`tasks` 恰好 10 行且 `session_id` **全部**
   指向同一条会话；模型句柄累计恰好 10 份请求（一个句柄贯穿，多发的请求会让后续轮次答复错位而当场失败）。

本次运行证据：`cargo test -p ignis-cli --test sessions` → `1 passed`（连续 5 次全绿，单次 0.83–0.93s）；
`cargo test --workspace` 里同一目标一次通过。

### 2. 设计 §8 九条 → 证据映射

| # | 设计 §8 条目 | 证据（路径 / 用例名） | 本次结论 |
|---|---|---|---|
| 1 | CLI 渲染：连续流式 100 次（随机分片 + e2e）无重复、丢字、布局错乱；无未处理的推理标记 | 属性测试 `apps/cli/src/render/markdown.rs::random_split_feeding_matches_whole_feeding_byte_for_byte`、`packages/runtime/src/think.rs::random_fragmentation_keeps_channel_semantics`；模式矩阵单测（`apps/cli/src/render/mod.rs`，含 `minimal_mode_hides_tool_lines_and_summarizes_thinking`）；e2e `apps/cli/tests/pty.rs::pty_submits_a_chinese_prompt_and_renders_the_answer`、`apps/cli/tests/render.rs::inline_reasoning_is_split_out_of_the_answer_channel` | PASS（`cargo test` 全绿）+ 矩阵 tmux / 80 列两行的实机观察 |
| 2 | 输入编辑：多行、中文、长文本粘贴、历史导航、命令补全 | `apps/cli/tests/pty.rs`：`pty_ctrl_j_edits_a_multi_line_prompt_and_submits_once`、`pty_long_paste_does_not_submit_until_enter`、`pty_long_input_line_wraps_with_the_continuation_marker`、`pty_pasted_text_with_tabs_renders_expanded_and_submits_verbatim`、`pty_up_arrow_recalls_history_and_restores_the_draft`、`pty_tab_completes_a_unique_prefix_and_lists_candidates`；手动抽查：矩阵 tmux 行（`↑` 召回）与 80 列行（输入行折行） | PASS |
| 3 | 会话恢复：≥10 次关闭/重启（`-c`），canonical 历史逐条完整 | `apps/cli/tests/sessions.rs::session_restore_keeps_the_canonical_history_across_ten_cycles`（本次新增）；旁证 `apps/cli/tests/pty.rs::pty_continue_restores_the_previous_turn_for_the_model` | **PASS**（见上「两条新指标」） |
| 4 | Context 管理：`/context` 区分真实用量与无数据；`/compact` 后关键决策仍在 | `apps/cli/src/commands/slash.rs::context_lines_matrix`、`dispatch_context_reads_the_session_state`、`help_lines_carry_the_context_row`；`apps/cli/tests/pty.rs::pty_compact_summarizes_and_keeps_the_raw_rows` | PASS（离线面）；「用一个真实任务验证复述」需要真实模型 → 该子项不在本机执行 |
| 5 | 长期记忆：保存 / 跨会话读取 / 编辑 / 删除全部通过；`remember` 确认行可见 | `apps/cli/tests/remember.rs::remember_success_prints_the_confirmation_line`、`remember_failure_prints_no_confirmation_line`；跨进程复用 `apps/cli/tests/demo_b.rs::demo_b_remembers_reuses_and_corrects_across_processes`；记忆 CRUD 单测在 `apps/cli/src/commands/memory.rs`；真机 harness Demo B → **SKIP**（无凭据；且本机无 `uv`） | PASS（离线面）；真机面 SKIP |
| 6 | Skills：3 个内置技能可展示、加载、记录实际使用；`reload` 生效 | `apps/cli/tests/pty.rs::pty_skills_reload_picks_up_a_new_skill_file`、`pty_learn_saves_the_draft_after_confirmation`、`pty_learn_discard_and_refusal_leave_no_file`；`skills_used` 落库断言见 `apps/cli/tests/kernel.rs` 与 core 单测；真机 harness「技能目录」记录 → **SKIP**（无凭据） | PASS（离线面）；真机面 SKIP |
| 7 | 工具观察与取消：进度 / 异常 / 取消状态一致；中断绝不报成成功 | 工具行配对与异常展示：`apps/cli/src/render/mod.rs::normal_mode_prints_the_exec_and_result_lines_of_a_paired_tool_call`、`tool_result_pairs_by_call_id_not_first_in_first_out`、`failed_tool_result_prints_the_first_line_of_its_output_on_a_second_line`、`apps/cli/tests/render.rs::tool_completion_line_pairs_with_the_exec_line_and_carries_the_elapsed`；取消：`packages/runtime/src/native/mod.rs::cancel_token_aborts_mid_stream`、`cancel_before_first_request_aborts`、`apps/cli/tests/tui.rs::pty_tui_ctrl_c_cancels_a_running_task_without_leaving`（`● Aborted`）；触限三处一致：`apps/cli/tests/limits.rs::max_steps_reaches_limit_and_stays_visible` | PASS |
| 8 | 终端兼容：Ubuntu 终端、SSH、tmux、80 列、无色通过基础测试 | 本文件「3. 终端矩阵」段（tmux / 80 列 / 无色 = PASS；Ubuntu 默认终端、SSH、浅色主观项 = SKIP + 原因）；自动化：`apps/cli/src/render/width.rs` 折行单测、`apps/cli/src/tui/view.rs::compose_without_color_emits_no_sgr`、`apps/cli/tests/render.rs::no_color_env_overrides_always_color_in_config`、`apps/cli/tests/tui.rs` 两条降级用例、`apps/cli/tests/pty.rs::pty_long_input_line_wraps_with_the_continuation_marker` | **部分 PASS / 部分 SKIP**（3 项 SKIP 各带原因）→ 该项按「验收不完整」记 |
| 9 | 功能回归：v0.1 全部 e2e 通过 | `cargo test --workspace`：29 个测试目标、**926 passed / 0 failed / 2 ignored**（v0.1 套件 `demo_a`、`demo_b`、`demo_c`、`approval_modes`、`limits`、`redaction`、`kernel`、`daemon_lifecycle`、`autostart` 全绿）；`cargo clippy --workspace --all-targets -- -D warnings` 零警告；`cargo fmt --all --check` 干净 | PASS |

**§8 的「端到端演示（可重复）」**：三次启动的演示脚本在 README（Task 5 落地，需要真实端点）；
本机执行不了真机演示（无凭据）→ **SKIP + 原因**。其中 `--tui` 那一次的等价观察在矩阵 tmux 行：
`ignis --tui` 进备用屏、跑一轮、`/exit` 还原备用屏与 termios。

### 3. 终端矩阵（手动，逐项记录）

本机环境事实（记录时刻）：Ubuntu x86_64、`tmux 3.6`（`/usr/bin/tmux`，pane `TERM=tmux-256color`）、
**无图形终端**（`gnome-terminal` / `konsole` 未安装，只装了需要 X 的 `xfce4-terminal`；`xdpyinfo` 对
`DISPLAY=localhost:10.0` 与 `:0` 都失败）、**无可用的 SSH 目标**（回环探测 255：`Permission denied`）、
无 `uv`、`python3` 可用（假端点与证据核对都用它）。

| 终端 | 结论 | 记录（命令 + 实际观察） |
|---|---|---|
| Ubuntu 终端（默认，gnome-terminal/konsole） | **SKIP + 原因** | 本机没有可用的图形终端：`command -v gnome-terminal konsole` 皆空（只有 `xfce4-terminal`），`xdpyinfo` 对 `DISPLAY=localhost:10.0` / `:0` 都失败。**不记 PASS**。「编辑器重绘/历史/Ctrl+C/退出复原」的等价观察在 tmux 行完成（tmux pane 是真 pty）。 |
| tmux | **PASS** | `tmux -f /dev/null new-session -d -s matrix-a -x 200 -y 50 bash --noprofile --norc`，在其中 `cd <ws> && env IGNIS_HOME=… IGNIS_WORKSPACE=… IGNIS_BASE_URL=http://127.0.0.1:8899/v1 IGNIS_MODEL=fake-matrix-model IGNIS_API_KEY=test-key target/release/ignis`。观察：banner 5 行（`IGNIS v0.2.0` / `● Ready` / `Workspace:` / `Runtime: native · Skills: 3 · Memory: 0 facts` / `Session: new`）；一轮任务流式渲染 + 结束行 `● Done · 0.0s · 0 tools · task 01a14ff2-aa6b`；`↑` 召回上一条输入（屏幕上同一句出现两次）；`/help` 15 条命令；**空闲 Ctrl+C** 干净退出、`echo $?` = `0`（P6-T1 的 SIGINT 提前安装修复在真 pty 上复验）；退出后 `stty -a` 里 `echo,echoe,icanon,isig` 全部复原；pane 原始字节流 3085 B、SGR 6 条（默认 `ui.color auto` + tty = 有色，颜色正对照）。另一局 `-x 80 -y 24`：经典 REPL 一轮 + 空闲 Ctrl+C 退出 + `stty` 复原；`ignis --tui` 进备用屏（原始流 `\x1b[?1049h` ×1 / `\x1b[?1049l` ×1）、首帧状态行 `ctx 0/200000 chars · last - · mode normal · PgUp/PgDn scroll`、跑一轮、`/exit` → `echo $?` = `0`、`stty` 复原。 |
| SSH | **SKIP + 原因** | 探测：`ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 localhost true` → 退出 255（`Permission denied (publickey,password)`）；本机没有可用的「SSH 到目标终端」环境（设计 §3 环境事实）。**不记 PASS**。 |
| 80 列 | **PASS（含 1 条版式注意）** | 路径 a（`stty cols 80`）：pane 200×50 里先 `stty cols 80; stty size` → `50 80` 再起 ignis。观察：超长输入行（94 字符 / 约 158 列）由编辑器按显示宽度折成 **80 / 80 / 34** 三行、续行带 `… ` 标记，提交与落库的文本逐字不变（库里 `session_messages.content` 与输入逐字相等）；列表项由渲染层折行、续行对齐（续行 62 列 ≤ 80）；`/help` 15 行、最宽 92 列；`/exit` 退出码 0；`stty` 复原。路径 b（真实 80×24 pane）：banner 的 `Workspace:` 长行由终端软折行；`/help` 的 3 行在终端边缘软折行。**版式注意**：`/help` 的 `/context`（88 列）、`/compact`（85 列）、`/kernel`（92 列）三行超过 80 列——命令层输出不经渲染层折行，在真实 80 列终端上由终端软折行（续行是 `rn usage` / `ssage` / `odex\|claude)`），**文字完整、无错乱**；`/help` 是 P6 明确冻结的面（「只做收口，不动 `/help`」），故记为版式注意而不是本任务的修复项。 |
| 无色（`NO_COLOR=1`） | **PASS** | 80×24 pane 里 `NO_COLOR=1` 跑一轮 + `/help` + `/exit`：pane 原始字节流里 **SGR 序列 0 条**（CSI 37 条，全是清行/光标类重绘，不是颜色），文字与布局与有色一致。另一例 `ignis config set ui.color never`（`config.json` = `{"ui":{"color":"never"}}`）同样一轮：**SGR 0 条**（CSI 23 条）。正对照：默认那一轮 SGR 6 条（`\x1b[2m` 代码围栏 dim、`\x1b[1m` 标题、`\x1b[0m` 复位）——有无色的差别确实来自应用自己。 |
| 浅色主题（`ui.theme=light`） | **SKIP + 原因（主观项）** | `ignis config set ui.theme light`（`config.json` = `{"ui":{"theme":"light"}}`）在 80×24 pane 里跑一轮 + `/exit`：不崩、答复正常、`stty` 复原；原始流 SGR 集合 `\x1b[0m` / `\x1b[1m` / `\x1b[90m`，与 dark 的 `\x1b[0m` / `\x1b[1m` / `\x1b[2m` **只差 dim 槽**（light 用亮黑 90）——主题切换确实生效（客观部分，只作附注，不构成该项 PASS）。**主观可读性（浅色背景上的对比度）本机没有可用显示环境，无法目视判断 → SKIP**。 |

### 4. harness 记录（三条真机门槛）

| 命令 | 本次结果 |
|---|---|
| `harness/acceptance.sh demos 3` | **退出码 2**：前置检查未通过——`IGNIS_BASE_URL` / `IGNIS_MODEL` / `IGNIS_API_KEY` 都未设置（本机没有真实模型凭据）。**没有写结果文件**（脚本不给「没跑过」的产物）。三条门槛 → **SKIP + 原因** |
| `harness/acceptance.sh tasks 20` / `restarts 10` | 同上未执行（`preflight` 在缺凭据时先拒；`tasks` / `restarts` 与 `demos` 共用同一前置检查） |
| `harness/acceptance.sh --dry-run demos 3` | **退出码 0**、`DRY=9`（A×3、B×1、C×3、技能目录、doctor）；证明头注释更新后脚本结构与判定子句原样在位（脚本行为未改） |

> 另：本机无 `uv`，真实 Demo B 在 v0.1 起就记 SKIP——本次口径未变（门槛级 SKIP ⇒「验收不完整」，退出码 1）。
> 三条门槛本机结论一律是 **SKIP（原因：无真实模型凭据）**，不得写成 PASS。

### 5. 发布门（本次收口的命令与结果）

```sh
cargo fmt --all -- --check                 # 干净
cargo clippy --workspace --all-targets -- -D warnings   # 零警告
cargo test --workspace                     # 29 个测试目标：926 passed / 0 failed / 2 ignored
cargo build --release --locked             # 成功
target/release/ignis --version             # 逐字：ignis 0.2.0
```


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
6s 内 0 次运行）。修复即把首次观察锚**泛化**到**间隔 > 重扫窗口**的过期 interval 作业：锚点
（`last_run_at`，从未运行过时取 `created_at`）`+ 间隔 ≤ 现在` 时，首次观察快照 `首次观察 + 间隔`
并复用同一锚——一个守护进程生命周期内触发一次、不补跑；锚点 + 间隔 仍晚于现在（新 job、刚跑过的
job）维持原拍子**不进快照**；cron 一律维持无状态口径；`due_at()`（CLI `schedule list` 用）不变；
无 schema 变更，重启重新快照（仍是刻意的「不补跑」取舍）。判别测试（`packages/gateway/src/loop.rs`）：
① `last_run_at = now − 2·间隔`、重扫 1s、间隔 3s：修复前 6s 内 0 次运行（RED）、修复后 ~3s 触发；
② 过期作业只触发一次、随后由 `last_run_at + 间隔` 续拍（重扫窗口不补跑）；③ 刚跑过的作业
（`last_run_at + 间隔` 仍晚于现在）按原拍子触发且不进快照；既有两条休眠作业用例保持 GREEN。
（**口径收紧（Wave E）**：这里的「过期作业」被限定为**间隔 > 重扫窗口**的作业；间隔 ≤ 重扫窗口
维持原有无状态口径（唤醒点即到期点，本来就不会滑失），见下面的 Wave E 补丁。）

**补丁（Wave E，`fix(gateway): 过期锚仅作用于间隔长于重扫窗口的作业`）**：Wave C 的泛化把快照
也施加到**间隔 ≤ 重扫窗口**的作业上，打乱了原有的调谐并回归了 `apps/cli/tests/demo_c.rs` 的
暂停/恢复契约——`demo_c_daemon_keeps_scheduling_after_the_cli_exits` 8/8 确定性失败（卡在第 5 步
`暂停后迟到的那一轮` 的 8s 等待；单变量 A/B：只把 `packages/gateway/src/loop.rs` 回退到 `15e9e49`
即 2/2 通过，Wave C 的 `d4acc71` 引入）。机制：**间隔 ≤ 重扫窗口时唤醒点
`min(now + every, now + rescan)` 就是到期点 `now + every`**（Demo C 是 2s ≤ 30s），循环在到期那一刻
醒来触发，「永不到期」的滑失在这档下不可能发生；而快照值过一拍即成过去值，`plan()` 的在飞分支
（`due_at <= now && running`）据此把该 job 从 `due` 里摘掉——**暂停前已经在计划里、暂停后才到点的
「迟到一轮」不再落地**（守护日志：滑动口径下两轮相隔 2.002s，快照口径下不再有第三轮）。
**修复后的口径**：间隔 > 重扫窗口的 interval 作业，其锚点 + 间隔 已过时按首次观察 + 间隔 起算
（不补跑，一次）；间隔 ≤ 重扫窗口维持原有无状态口径（唤醒点即到期点，本来就不会滑失）——不钉锚、
不打那行告警。`due_at()`（CLI `schedule list` 用）、cron、锚点取 `last_run_at ?? created_at` 与
告警/失败语义全部不变。判别测试（`packages/gateway/src/loop.rs`，判别不依赖时序）：
① 新增（RED）间隔 2s ≤ 重扫 5s、`last_run_at = now − 2·间隔`、同一秒内两次扫描：修复前首次扫描
即进快照并打出「按首次观察 + interval 起算」告警、一拍后的在飞扫描把该 job 从 `due` 里摘空
（0 条，正是 demo_c 丢的那一轮）；修复后不进快照、到期点 = 唤醒点 = `现在 + 间隔`、在飞扫描后
下一拍仍留在计划里（1 条）；② 循环级用例：同样的过期作业（重扫 5s）首轮后照常按拍续跑（两轮相隔
~一个间隔，不被重扫窗口拖长）；③ Wave A/C 的**间隔 > 重扫窗口**用例（3s > 1s 的休眠/过期作业、
快照复用、原拍子、绝不补跑）全部保持 GREEN；④ **端到端契约**：`cargo test -p ignis-cli --test demo_c`
修复前 12.23s 失败（第 5 步超时）、修复后六步全过（~42s，暂停/恢复/重启时序一条没削）——改动调度
语义必须跑这条 e2e，本回归正是「复审没跑 e2e」漏掉的。

（**已知残留（P3 既有行为，不在 Wave E 范围）**：上面「间隔 ≤ 重扫窗口就不会滑失」以该作业的到期点
是本轮最早唤醒为前提。若存在另一个到期更早的作业反复先唤醒循环，本作业重算出的 `now + 间隔` 会被
「在飞/未到期即跳过」路径反复推后——真机探针：A=2s、B=3s、重扫 5s、两者都已过期时，12s 内 A 跑 5 次、
B 跑 0 次（暂停 A 后 B 立刻恢复触发）。单作业场景（含 Demo C 与 harness 的 `restarts` 模式）不受影响；
多作业并存的这种饥饿不在 v0.1 修复范围。）

脚本自测记录（**不是验收结果**，仅验证脚本逻辑。假端点把 Demo C 的 `schedule_create` 脚本化为
**2 秒档**间隔，与 `apps/cli/tests/demo_c.rs` 的 `2s` 同档；**注意**：短间隔（≤30s 重扫窗口）的假端点
覆盖不到「>30s 的从未运行 job 首次到期」这条路径——真机首跑正是卡在这里，因此那次「C 3/3」只证明
脚本管线可走通，**不构成 1 分钟路径的验收证据**；该路径由 gateway 修复的判别测试与真机 Demo C 覆盖）：
2026-10-16 用本地假端点（脚本化 SSE）跑过 `demos 3`（三场景 3/3）、`tasks 5`（5/5）与 `tasks 20`
（7/20，门槛 FAIL、退出码 1）、`restarts 1/2`（重复/孤儿/丢失全 0），并验证了缺凭据与 CI 的拒绝路径、
`--dry-run`、`--keep`、以及 `sqlite3`/`python3` 两种证据读取后端。
