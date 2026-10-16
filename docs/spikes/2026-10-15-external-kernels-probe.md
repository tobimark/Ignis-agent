# Spike：外置内核（codex / claude）真机事件 schema 与沙箱开关

- 日期：2026-10-15（本机）
- 动机：设计文档 §10 风险 #4——外置内核的命令行参数与事件 schema 随其版本漂移，适配器开工前必须实测确认
- 范围：T1（codex）在此落笔；T2（claude）追加自己的小节，不改本节结论

## 结论摘要（先读这段）

| 项 | 结论 |
|---|---|
| CLI 版本 | `codex-cli 0.162.1`（npm `@openai/codex`，经 `~/.local/bin/codex` 软链到包内 `bin/codex.js`） |
| `--json` | 存在（`codex exec --json`：stdout 逐行 JSONL） |
| `-C` / `--cd` | 存在（`Tell the agent to use the specified directory as its working root`） |
| `-s` / `--sandbox` | 存在，取值 `read-only` \| `workspace-write` \| `danger-full-access`；**T1 采用 `workspace-write`**（任务书钉死，见下「沙箱开关」） |
| `--skip-git-repo-check` | 存在（git 仓库外可跑） |
| `-m` / `--model` | 存在（`Model the agent should use`）；`CodexRuntime` 只在 `model` 为 `Some` 时追加 `-m <值>` |
| 端到端真机跑通 | **未完成**：本机到模型端点的网络出口不通（详见「实测记录」），CLI 停在重连退避里，240s 内未见 `turn.completed` |
| 事件 schema 来源 | 三源交叉：① 本次实测真实吐出的前导事件；② 本机 0.162.1 二进制里的字面量（变体名与字段名）；③ 上游文档（`developers.openai.com/codex/noninteractive` 与社区逐线型 cheatsheet，版本对齐 0.157.x） |
| 对适配器的影响 | 映射按「三源」里已确认的形状实现（见下「事件 schema」）；`usage` 未在真机流里观测到成功样例，T1 不映射 `Usage`，与 `Capabilities{usage:false}` 一致 |

## 实测记录（脱敏：不记路径、不记 thread id、不记配置内容）

命令与结果：

1. `codex --version` → `codex-cli 0.162.1`（退出码 0）。
2. `codex exec --help` → 退出码 0，逐字确认了上表六个开关与三个沙箱取值。
3. 真机跑：
   `codex exec --json -C <临时目录> --sandbox workspace-write --skip-git-repo-check "说一句 ok"`（stdin 接 `/dev/null`，240s 上限）
   → 退出码 124（被外层 `timeout` 终止）：stdout 9 行 JSONL 全是前导/错误事件，**没有** `turn.completed`；
   stderr 两行 `failed to refresh available models: Connection failed: error sending request`。
4. 网络对照：`curl -m 15 https://api.openai.com/v1/models` → `Connection timed out`（curl exit 28）。
   说明本机当前**出口受限**，与 CLI 登录态无关：认证没问题（请求已进入模型调用阶段，报错是传输层超时而非 401/403）。

真实吐出的 9 行（trimmed；配置警告里的用户路径已换成 `~`）：

```jsonl
{"type":"thread.started","thread_id":"<uuid>"}
{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Codex is ignoring 1 unrecognized configuration setting. ... user (~/.codex/config.toml): `disable_response_storage` is ignored."}}
{"type":"item.completed","item":{"id":"item_1","type":"error","message":"<同上，重复一次>"}}
{"type":"turn.started"}
{"type":"error","message":"Reconnecting... 2/5 (request timed out)"}
{"type":"error","message":"Reconnecting... 3/5 (request timed out)"}
{"type":"error","message":"Reconnecting... 4/5 (request timed out)"}
{"type":"error","message":"Reconnecting... 5/5 (request timed out)"}
{"type":"item.completed","item":{"id":"item_2","type":"error","message":"Falling back from WebSockets to HTTPS transport. request timed out"}}
```

可以直接读出来的结论：

- 事件是**逐行 JSON 对象**，顶层有 `type`；`item.*` 行把条目放在 `item` 里，条目自带 `id` 与 `type`。
- `error` 行是**过程性**的（重连退避），不是终局；`item.completed` 里的 `item.type == "error"` 是告警条目（本例是配置警告），同样不是终局。
- 真正判成功/失败要看 `turn.completed` / `turn.failed`（本次两者都没等到，进程被外层杀掉）——所以「流结束/被杀但没见到成功终局」必须能被适配器判成失败或中止，这条路径是**实测暴露出来的**，不是假想。

## 事件 schema（适配器按这份映射）

上游文档口径（0.157.x 实测 + 官方 noninteractive 文档），叠加本机 0.162.1 二进制里的字面量确认（`thread.started` / `turn.started` / `turn.completed` / `turn.failed` / `item.started` / `item.updated` / `item.completed` / `error`，条目类型 `agent_message` / `agent_reasoning` / `command_execution` / `file_change` / `mcp_tool_call` / `token_count`，字段 `aggregated_output` / `exit_code` / `cached_input_tokens` / `reasoning_output_tokens`）：

```jsonl
{"type":"thread.started","thread_id":"<uuid>"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"**Confirming readiness for response**"}}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"OK."}}
{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"bash -lc ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"bash -lc ls","aggregated_output":"docs\nsrc\n","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"id":"item_3","type":"file_change","changes":[{"path":"docs/x.md","kind":"add"}],"status":"completed"}}
{"type":"turn.completed","usage":{"input_tokens":24763,"cached_input_tokens":24448,"cache_write_input_tokens":0,"output_tokens":122,"reasoning_output_tokens":64}}
{"type":"turn.failed","error":{"message":"model response stream ended unexpectedly"}}
{"type":"error","message":"stream error: broken pipe"}
```

字段要点（上游文档明确写着、且与二进制字面量一致的）：

- `agent_message` 只出现在 `item.completed`，正文在 `item.text`；可选的 `item.phase`（`commentary` 过程话术 / `final_answer` 最终答复）在旧版本里没有。
- `command_execution` 出现在 `item.started` 与 `item.completed`：`item.command`、`item.aggregated_output`（截断到 64 KiB）、`item.exit_code`（完成前为 `null`）、`item.status`（`in_progress` / `completed` / `failed` / `declined`）。
- `turn.completed.usage` 是**整条 thread 的累计值**，不是本轮增量；`cached_input_tokens` 是 `input_tokens` 的子集，`reasoning_output_tokens` 是 `output_tokens` 的子集。
- `type:"error"` 里 `Reconnecting... X/Y` 是重连提示（非致命），其余 `error` 视为流级错误。
- 上游明确写着「未知字段/未知条目类型要容忍」：条目类型不认识时应当丢这一条，而不是让整条流失败。

## 沙箱开关

- 取值（`--help` 逐字）：`read-only`、`workspace-write`、`danger-full-access`。
- T1 采用 `--sandbox workspace-write`：任务书与派单都逐字钉了这条命令；同时 `exec` 是非交互模式，没有审批通道，`workspace-write` 是「能干活、又不出工作区」的档位。`danger-full-access` 只在外部已沙箱化时才该用，适配器不自行放宽。
- 附带确认：`--skip-git-repo-check` 必带（工作区可能不是 git 仓库，不带会被拒）；`-C <ws>` 与进程 `cwd` 同时设成工作区。

## 未完成的验证（等网络恢复再补）

1. 一次成功的 `turn.completed` 真机样例（尤其 `usage` 的嵌套形状与「累计值」语义）——**本机出口受限，未取得**。
2. `turn.failed` 的真实退出码（非零？还是 0？）——未取得；适配器对两者都按失败处理，不依赖该细节。
3. 取消/超时打断时 CLI 自己的收尾行为（本次是被外层 `timeout` SIGTERM 掉的，不是适配器的进程组 SIGKILL 路径）——未取得；适配器按「杀整组 + 自己收尸」实现，与 `shell_run` 同款。

因此 T1 的默认序列仍是 `packages/runtime/tests/fixtures/fake-codex.sh` 假 CLI：它逐字回放上面「事件 schema」里的样例（含未知行/非 JSON 行、非零退出、挂起、`turn.failed`），真机路径待网络恢复后按同一文档复测。

## 复测 procedure（网络可用时）

```sh
codex --version
codex exec --json -C "$(mktemp -d)" --sandbox workspace-write --skip-git-repo-check "说一句 ok"
# 期望：thread.started → turn.started → item.completed(agent_message) → turn.completed(usage)，退出码 0
```

若 `turn.completed` 的形状与本节不符，改 `packages/runtime/src/codex.rs` 的映射与本文件，再改夹具脚本——三处一起动。
