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
