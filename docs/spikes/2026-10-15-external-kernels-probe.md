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
| 端到端真机跑通 | **已完成**（同日复测，网络可达）：经 OpenAI 兼容端点（Responses API）跑通成功轮 / 工具轮 / 失败轮，真实 `turn.completed` / `turn.failed` / 退出码全部取得；适配器本身也以真 codex 端到端跑出一轮 `TextDelta` + `Done{Success}`（见「实测记录（二）」） |
| 事件 schema 来源 | 四源交叉：① 首轮实测真实吐出的前导事件；② 本机 0.162.1 二进制里的字面量（变体名与字段名）；③ 上游文档（`developers.openai.com/codex/noninteractive` 与社区逐线型 cheatsheet，版本对齐 0.157.x）；④ **复测取得的三条完整真实轨迹**——与 ①②③ 一致，适配器映射逐条对得上 |
| 对适配器的影响 | 映射按四源已确认的形状实现（见下「事件 schema」）；`usage` 的真机形状复测已取得（含 `cache_write_input_tokens`），但 T1 仍不映射 `Usage`，与 `Capabilities{usage:false}` 一致（设计文档 §6.8：外部内核降级为时间/步数上限） |

## 实测记录（一）：出口受限时的前导事件（脱敏：不记路径、不记 thread id、不记配置内容）

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

## 实测记录（二）：复测——OpenAI 兼容端点上的完整轨迹（2026-10-15）

拿到一个可达的 OpenAI 兼容端点后补齐了首轮缺的终局样例。**键只经环境变量（`env_key`）传入，
不落盘、不入命令行、不进日志**；下面记的是端点形状与事件，不记键。

开跑前先确认两件事：

1. 端点探针（两次 `curl`，键经环境变量）：`GET /v1/models` → 200（键有效，清单含 `MiniMax-M2`）；
   `POST /v1/responses {"model":"MiniMax-M2","input":"say ok"}` → 200，`object:"response"`、
   `status:"completed"`。
2. 0.162.1 二进制里的硬约束：`wire_api = "chat"` **已不再支持**（字面量
   `` `wire_api = "chat"` is no longer supported. ``）——自定义 provider 必须说 Responses API
   （`/v1/responses`）。本端点正好支持，故命令里**不带** `wire_api`。

命令（逐字；`-c` 覆盖自定义 provider，`MM_KEY` 只写变量名进 `env_key`）：

```sh
MM_KEY=<键> codex exec --json -C <临时工作区> --sandbox workspace-write --skip-git-repo-check \
  -c model="MiniMax-M2" \
  -c 'model_providers.minimax={name="minimax",base_url="<endpoint>/v1",env_key="MM_KEY",requires_openai_auth=false,supports_websockets=false}' \
  -c model_provider="minimax" \
  "说一句 ok"        # stdin 接 /dev/null
```

`requires_openai_auth=false` 是必须的（否则 CLI 会要求 ChatGPT 登录态）；`supports_websockets=false`
跳过 WebSocket 尝试，直接 HTTPS（首轮实测里见过一次「Falling back from WebSockets to HTTPS」）。

### 轨迹 1：成功轮（退出码 0，共 7 行）

```jsonl
{"type":"thread.started","thread_id":"<uuid>"}
{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Codex is ignoring 1 unrecognized configuration setting. ... user (~/.codex/config.toml): `disable_response_storage` is ignored."}}
{"type":"item.completed","item":{"id":"item_1","type":"error","message":"<同上，重复一次>"}}
{"type":"item.completed","item":{"id":"item_2","type":"error","message":"Model metadata for `MiniMax-M2` not found. Defaulting to fallback metadata; this can degrade performance and cause issues."}}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"ok"}}
{"type":"turn.completed","usage":{"input_tokens":8920,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":52,"reasoning_output_tokens":50}}
```

### 轨迹 2：工具轮（退出码 0，共 11 行；prompt 要求 `ls` 一把并写一个文件）

```jsonl
{"type":"thread.started","thread_id":"<uuid>"}
{"type":"item.completed","item":{"id":"item_2","type":"error","message":"<Model metadata ... 告警，同轨迹 1>"}}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_3","type":"command_execution","command":"/bin/bash -lc 'ls -la'","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_3","type":"command_execution","command":"/bin/bash -lc 'ls -la'","aggregated_output":"total 0\n…\n-rw-rw-r--   1 tobi   tobi       0 … a.txt\ndrwxrwxr-x   2 tobi   tobi      40 … docs\n","exit_code":0,"status":"completed"}}
{"type":"item.started","item":{"id":"item_4","type":"command_execution","command":"/bin/bash -lc 'echo \"ok\" > hello.txt'","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_4","type":"command_execution","command":"/bin/bash -lc 'echo \"ok\" > hello.txt'","aggregated_output":"","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"id":"item_5","type":"agent_message","text":"当前目录有以下文件：…"}}
{"type":"turn.completed","usage":{"input_tokens":27614,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":184,"reasoning_output_tokens":83}}
```

条目字段逐字为 `id` / `type` / `command` / `aggregated_output` / `exit_code` / `status`（与夹具脚本
一致）；`item.started` 时 `exit_code` 为 `null`、`aggregated_output` 为 `""`；`-C <ws>` 下命令的 cwd
就是工作区（`ls -la` 里能看到工作区的真实文件与新建的 `hello.txt`，另有多出的一组只读
（`dr-xr-xr-x`）目录——沙箱注入的环境，不是工作区内容）。

### 轨迹 3：失败轮（退出码 **1**，共 4 行）

```jsonl
{"type":"thread.started","thread_id":"<uuid>"}
{"type":"turn.started"}
{"type":"error","message":"invalid params, code: 2013, msg: invalid params, unknown model 'no-such-model-xyz' (2013)"}
{"type":"turn.failed","error":{"message":"invalid params, code: 2013, msg: invalid params, unknown model 'no-such-model-xyz' (2013)"}}
```

（故意用一个不存在的模型名制造失败：`turn.failed.error.message` 就是服务端原因。**真实退出码是
非零的 1**——适配器对 0/1 都按 `Failed` 收，不依赖这个细节。注意 `turn.failed` 前一行是
`type:"error"` 的**非终局**流级错误，两者必须分开处置。）

### 轨迹 4：端点不可达（对照）

把 `base_url` 换成 `http://127.0.0.1:9/v1`：CLI 不进终局，只反复吐
`{"type":"error","message":"Reconnecting... waiting for network (Connection failed: error sending request)"}`，
90s 内没有任何终局行，只能被外层杀掉。这正是适配器需要 `task_timeout` + 杀整组的原因（区分「进程被杀」
与「CLI 自己收束」）。

### 真机轨迹 ↔ 适配器映射对照

| 真机行 | 适配器处置 | 对得上？ |
|---|---|---|
| `thread.started` / `turn.started` | 未知顶层类型 → 静默跳过 | ✓ |
| `item.completed` + `item.type == "error"`（配置/元数据告警、流级错误条目） | 不映射的条目 → 跳过 | ✓ |
| `type:"error"`（重连、`invalid params`） | 非终局 → 跳过（不当终局） | ✓ |
| `item.completed` + `agent_message` | `TextDelta`（`item.text`） | ✓ |
| `item.started` / `item.completed` + `command_execution` | `ToolExec`（name = 条目类型名、summary = `item.command`、args = 原始条目） | ✓ |
| `turn.completed`（带 `usage`） | `Done{Success, summary = 最后一段助手文本}`；`usage` 不出事件（能力声明 `usage:false`） | ✓ |
| `turn.failed`（`error.message`） | `Done{Failed, 「codex 报告失败：<原因>」}` | ✓ |

**三条真实轨迹里没有出现适配器会漏掉的类型，映射零改动**；夹具脚本的三条主形态（文本轮、命令轮、
`turn.failed`）与真机逐字一致。复测另给夹具加了一条 `REAL_FAIL`（非终局 `error` 行 + 真原因 + 退出码 1，
见 `packages/runtime/tests/fixtures/fake-codex.sh`）。

### 适配器端到端真机跑

同一端点上直接跑适配器（一次性 scratch 测试，跑完即删、不入库）：临时 `CODEX_HOME` 放
provider 配置、键只从环境读，`CodexRuntime::new(<临时工作区>, Some("MiniMax-M2"))` 的事件流是

```
TextDelta { text: "ok！有什么我可以帮你的吗？" }
Done { status: Success, summary: Some("ok！有什么我可以帮你的吗？"), error: None }
```

4.15s 跑完、零残留进程——`-C` / `--sandbox workspace-write` / `--skip-git-repo-check` / null stdin
这套真实命令行在真 codex 上可用。复跑方式：`CODEX_HOME=<含 config.toml 的临时目录> MM_KEY=<键>
cargo test -p ignis-runtime --test <该 scratch> -- --nocapture`，其中 `config.toml` 只放
`model` / `model_provider` 与 `[model_providers.minimax]` 的 `name` / `base_url` / `env_key` /
`requires_openai_auth=false` / `supports_websockets=false`（**不放键**）。

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

## 仍未取得的验证

1. ~~成功的 `turn.completed` 真机样例（`usage` 嵌套形状）~~ → **已取得**（轨迹 1/2，含 `cache_write_input_tokens`；本轮无法跨轮验证「累计值」语义，但形状与上游口径一致）。
2. ~~`turn.failed` 的真实退出码~~ → **已取得**：非零的 **1**（适配器对 0/1 都按 `Failed` 收，不依赖该细节）。
3. 取消/超时打断时 CLI 自己的收尾行为——**仍未取得**（真机各轮都是自然结束或被外层 `timeout` 杀掉，没走适配器的进程组 SIGKILL 路径）；但轨迹 4 说明「网络不可达时 CLI 无限重连、永不出终局」，所以「杀整组 + 自己收尸 + `task_timeout`」是必需的兜底。
4. `usage` 真机形状已确认，但适配器仍不出 `Usage` 事件（`Capabilities{usage:false}`）；真要记账需同时改能力声明，留给后续任务。

因此 T1 的默认序列仍是 `packages/runtime/tests/fixtures/fake-codex.sh` 假 CLI：它逐字回放「事件 schema」
（含未知行/非 JSON 行、非零退出、挂起、`turn.failed`、`REAL_FAIL`），真机只作对照与复测。

## 复测 procedure（网络可达时）

```sh
codex --version          # 0.162.1
# 键只经环境变量（下面的 MM_KEY）；provider 用 -c 覆盖，不带 wire_api（0.162.1 只支持 Responses API）
MM_KEY=<键> codex exec --json -C "$(mktemp -d)" --sandbox workspace-write --skip-git-repo-check \
  -c model="MiniMax-M2" \
  -c 'model_providers.minimax={name="minimax",base_url="<endpoint>/v1",env_key="MM_KEY",requires_openai_auth=false,supports_websockets=false}' \
  -c model_provider="minimax" \
  "说一句 ok"
# 期望：thread.started → item.completed(error 告警) → turn.started → item.completed(agent_message) → turn.completed(usage)，退出码 0
```

若 `turn.completed` / `turn.failed` 的形状与本节不符，改 `packages/runtime/src/codex.rs` 的映射、本文件、
再改夹具脚本——三处一起动（并给新形状补一条用例）。
