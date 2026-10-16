# Spike：中转「流式 + 工具调用」（开工即验证项 #1）

- 日期：2026-10-15
- base_url：`https://opencode.ai/zen/go/v1`（Claude Code 配置 `ANTHROPIC_BASE_URL=https://opencode.ai/zen/go` 对应的 OpenAI 兼容路径）
- 模型：`gpt-5.6-luna`（Codex 配置中的模型，且出现在中转 `GET /v1/models` 返回的 37 个模型里）
- 请求：brief 中的最小请求（`stream:true` + `stream_options.include_usage:true` + `tools:[get_time]` + 单条 user 消息）
- 结果：**未能取得结论——本轮请求未到达模型，未观测到任何流式响应。** Task 10 开工时重跑，仍是同一结论（见下「Task 10 重跑」与「结论」）。

## 执行记录（不含 key）

1. `GET https://opencode.ai/zen/go/v1/models` → `200`，`{"object":"list"}`，返回 37 个模型，含 `gpt-5.6-luna`；说明 key 有效、端点可达。
2. `POST https://opencode.ai/zen/go/v1/chat/completions`（上面的最小请求）→ `400` `MissingSessionID`：该端点要求 `x-opencode-session` 头做会话路由。
3. 补上 `x-opencode-session: <uuid>` 重发 → `429` `GoUsageLimitError`（`"Go usage limit exceeded"`，`limitName: "weekly"`）：中转 Go 套餐本周额度已用尽，请求在模型调用前被拒。
4. 备用路径 `https://opencode.ai/zen/v1/chat/completions` 同一请求 → `403` `{"type":"server_error","message":"Upstream request failed: Model access is disabled"}`（该路径下 `gpt-5.6-luna` 未开通；`GET /zen/v1/models` 鉴权正常返回 `200`）。
5. Codex 侧默认路径 `https://api.openai.com/v1` 在本机网络下不可用：DNS 解析到非 OpenAI 地址且连接挂起（`curl` exit 28、0 字节输出），无法作为验证路径。

## Task 10 重跑（2026-10-15，native 模型客户端开工前）

按同一 procedure 重跑（key 仍只经环境变量 / 管道传给 `curl`，未落盘、未进日志）：

1. `GET https://opencode.ai/zen/go/v1/models` → `200`：端点可达、key 有效。
2. `POST https://opencode.ai/zen/go/v1/chat/completions`（`stream:true` + `stream_options.include_usage:true` + 单个 `get_time` 工具 + `x-opencode-session: <uuid>`）→ `429` `GoUsageLimitError`（`"Go usage limit exceeded"`，`limitName: "weekly"`），响应体 182 字节、**0 条 `data:` 行**：请求在模型调用前被拒，仍未到达模型。
3. 备用路径 `https://opencode.ai/zen/v1/chat/completions` 同一请求 → `403` `{"error":{"type":"server_error","message":"Upstream request failed: Model access is disabled"}}`、0 条 `data:` 行。
4. 省略 `x-opencode-session` 时 → `400` `MissingSessionID`（与首轮一致，仅用于确认路由行为未变）。

## 待验证项（两轮均未观测）

- 是否出现 `data:` 开头的 SSE 行
- `delta.tool_calls[].function.arguments` 是否分片下发
- 最终 chunk 是否携带 `usage`
- 结论：流式工具调用整体是否可用

## 结论

**仍不可得（2026-10-15）**：本机既有中转 key 可得且端点可达（`GET /v1/models` → 200），但 Go 套餐周额度仍在 429 状态、`/zen` 备用路径 403，两轮 spike 都没有到达模型，**不对流式工具调用是否可用作任何猜测**。按 Task 10 的 dispatch：客户端与 `test_support` 假模型服务的测试完全离线、不依赖本结论，故继续实现；待额度恢复后再补做本 spike，若届时确认与流式工具调用不兼容，再按设计文档 §10 风险 1 讨论降级（`stream:false` 或 Anthropic 协议）。

key 处理：仅当时从 `~/.codex/auth.json` 读入进程变量并放进 `Authorization` 头，未写入仓库、报告、日志或终端输出；响应体已确认不含 key。

## 复验（2026-10-15，作者提供的 MiniMax 端点）——命门通关

作者提供了可用端点与 key（仅经环境变量使用，未落盘、未进仓库/日志；本文件不含任何 key 材料）：

- 端点：`https://api.minimax.cn/v1`（OpenAI 兼容；另有 `https://api.minimax.cn/anthropic`，供 claude 适配器探测用）
- 模型列表（`GET /v1/models` → 200）：`MiniMax-M3.1-Flash-Preview`、`MiniMax-M3`、`MiniMax-M2.7(-highspeed)`、`MiniMax-M2.5(-highspeed)`、`MiniMax-M2.1(-highspeed)`、`MiniMax-M2`
- 流式 + 工具调用（`stream:true` + `stream_options.include_usage:true` + 强制 `tool_choice`，模型 `MiniMax-M2`）**实测通过**：
  1. 出现 `data:` 行（`chat.completion.chunk`），`delta.content` 增量下发（含内联 `<think>…</think>` 思考块）
  2. `delta.tool_calls[]` 在单个 chunk 内给出**完整调用**：`{id:"call_function_…", type:"function", function:{name, arguments}, index:0}`，该 chunk `finish_reason:"tool_calls"`（本轮 `arguments` 未分片；组装器已兼容分片）
  3. 末块为 `choices:[]` + `usage`（`total_tokens/prompt_tokens/completion_tokens` 与 `completion_tokens_details.reasoning_tokens`）
  4. **不发 `data: [DONE]` 哨兵**——流以 EOF 收尾；native 客户端按 EOF 正常结束（与 design §6.2 的解析路径兼容）
- 全栈真机验证（native 内核）：
  - `ignis run "用 fs_list …"` → 流式思考 + `→ Listing .` 执行 + 终结答复；退出 0、`success`、token 记账（in=3636/out=58）
  - **Demo A 真机**：对 `examples/broken-project` 副本执行 `ignis run "分析这个项目的测试失败原因，修复它并重新运行测试" --yes` → 模型读文件、跑 `npm test`、精确诊断为「多退一层 `..`」、`→ Editing src/config.js`、重跑通过、中文总结；退出 0、21.2s、token in=19994/out=915；审批记录恰 3 条 `allow/preauthorized`（shell_run×2 + fs_edit）；修复后 `npm test` 退出 0。

## 最终结论

**流式工具调用可用（2026-10-15 复验）**：风险 #1 关闭。既有中转 `opencode.ai/zen/go` 侧的两轮「不可得」为套餐额度/端点配置问题，非协议不兼容；native 内核的 SSE 解析、工具调用组装、usage 记账与审批链路均已在本机真机验证。降级路径（`stream:false` / Anthropic 协议）无需启用。
