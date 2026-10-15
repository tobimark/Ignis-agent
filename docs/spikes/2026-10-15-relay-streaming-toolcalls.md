# Spike：中转「流式 + 工具调用」（开工即验证项 #1）

- 日期：2026-10-15
- base_url：`https://opencode.ai/zen/go/v1`（Claude Code 配置 `ANTHROPIC_BASE_URL=https://opencode.ai/zen/go` 对应的 OpenAI 兼容路径）
- 模型：`gpt-5.6-luna`（Codex 配置中的模型，且出现在中转 `GET /v1/models` 返回的 37 个模型里）
- 请求：brief 中的最小请求（`stream:true` + `stream_options.include_usage:true` + `tools:[get_time]` + 单条 user 消息）
- 结果：**未能取得结论——本轮请求未到达模型，未观测到任何流式响应。**

## 执行记录（不含 key）

1. `GET https://opencode.ai/zen/go/v1/models` → `200`，`{"object":"list"}`，返回 37 个模型，含 `gpt-5.6-luna`；说明 key 有效、端点可达。
2. `POST https://opencode.ai/zen/go/v1/chat/completions`（上面的最小请求）→ `400` `MissingSessionID`：该端点要求 `x-opencode-session` 头做会话路由。
3. 补上 `x-opencode-session: <uuid>` 重发 → `429` `GoUsageLimitError`（`"Go usage limit exceeded"`，`limitName: "weekly"`）：中转 Go 套餐本周额度已用尽，请求在模型调用前被拒。
4. 备用路径 `https://opencode.ai/zen/v1/chat/completions` 同一请求 → `403` `{"type":"server_error","message":"Upstream request failed: Model access is disabled"}`（该路径下 `gpt-5.6-luna` 未开通；`GET /zen/v1/models` 鉴权正常返回 `200`）。
5. Codex 侧默认路径 `https://api.openai.com/v1` 在本机网络下不可用：DNS 解析到非 OpenAI 地址且连接挂起（`curl` exit 28、0 字节输出），无法作为验证路径。

## 待验证项（本轮未观测，Task 10 前补做）

- 是否出现 `data:` 开头的 SSE 行
- `delta.tool_calls[].function.arguments` 是否分片下发
- 最终 chunk 是否携带 `usage`
- 结论：流式工具调用整体是否可用

## 结论

key 可得（取自本机既有 CLI 配置，只经环境变量传入命令行，未写入仓库、日志或终端输出）；但中转 Go 套餐周额度耗尽，**本轮无法验证「流式 + 工具调用」，且不对结果作任何猜测**。待额度恢复后在 Task 10（native 模型客户端）之前补做本 spike；若届时仍不可用，按设计文档 §10 风险 1 降级为 `stream:false` 或改走 Anthropic 协议实现。
