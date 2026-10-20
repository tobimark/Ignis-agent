# IGNIS TUI 视觉与交互升级 · 设计文档

- 日期：2026-10-19
- 范围：`apps/cli` 的 TUI、`packages/runtime` 的模型能力解析、`packages/core` 的轮次上下文统计
- 状态：已批准实现（本文档是实现依据，不覆盖既有 `demo.md` / 历史设计文档）

## 1. 意图

IGNIS 已经具备工具调用、Skills、Memory、会话与原生 Agent Runtime，但 TUI 仍是「能用的调试界面」：
启动只有字标与火焰、输入区只有一个 `❯`、状态栏只有字符预算、工具/Skills/Memory/Runtime 在界面上不可见。
本次升级要把 TUI 变成 IGNIS 的产品门面：第一眼有品牌识别度，第二眼能读懂当前运行环境，
长期使用又不干扰工作。

设计主题：**IGNIS — Your Heraclitus.**
关键词：Futuristic、Minimal、Premium、Ever-Living Fire、Intent、Terminal Native。

## 2. 目标与非目标

### 2.1 目标

1. 保留经典 REPL 的逐字节输出与旧火焰字模；TUI 欢迎面板改用用户提供的 truecolor ANSI
   侧边火焰（`assets/ignis-sidebar-truecolor.ans`）。
2. 新增顶部大型 ASCII `IGNIS` 横幅（真彩/256/基本色/无色四档，宽度自适应）。
3. 新增响应式欢迎面板：真实展示 Model、Provider、Workspace、Session、Runtime、Memory、
   Tools、Skills 与统计摘要；信息不可用时显示 `--` / `Not available`。
4. 输入区升级为「运行状态行 + 上下分隔线 + 动态高度编辑器（空输入 1 行，多行/提示展开）」，
   保留多行、历史、光标、Tab 补全与快捷键。
5. 状态栏展示模型、上下文用量/窗口/进度条/百分比、Agent 状态与耗时；无法确定窗口时显示 `?`/`--`。
6. 工具执行状态升级为 `⚙ name — Running…` / `✓ name — Done · 0.8s` / `✗ name — Failed`，
   Agent 回合结束显示简洁摘要。
7. 自动解析模型上下文窗口：Runtime API → Provider API → 本地 Registry → 用户配置 → Unknown；
   `/model info` 展示来源与有效限制。
8. 终端 Resize、宽字符、中文、Emoji、长路径、列表溢出、滚动位置全部正确处理。
9. 不引入新依赖；不重写 Agent Runtime、Tool Registry、Skill Loader 或 Context 管理。

### 2.2 非目标（本次不做）

- 不引入 ratatui / crossterm / termion / unicode-width 等新依赖。
- 不实现鼠标事件、无界 scrollback、内联图片、终端超链接。
- 不重构 `trim_history` 的字符预算语义；本次只把「模型窗口 / 有效限制 / 当前用量」展示出来，
  为将来的 token 级压缩预留字段，不强行改造上下文管理。
- 不为 Anthropic/Gemini 新增协议实现；它们的已知模型走 Registry，未知模型走用户配置或 Unknown。
- 不改变 `ui.color` / `NO_COLOR` / `TERM=dumb` 的既有颜色降级规则。

### 2.3 反目标（即使技术可用也算失败）

- 用硬编码假数据撑满面板（例如写死工具名、技能名、`200K` 窗口）。
- 每次 Agent 回复都重新打印大型横幅/欢迎面板。
- 为了动画每 100ms 重绘整屏，或在输入时闪烁/重复打印。
- 为了视觉效果改变经典 REPL、任务落库、模型请求或工具结果的字节。
- 在 80×24 下让欢迎面板挤掉输入区与会话内容。

## 3. 现有架构与约束

- 语言：Rust 2024；CLI 无 TUI 框架，手写 ANSI + `libc` termios/ioctl。
- 帧合成：`apps/cli/src/tui/view.rs::compose` 是纯函数，固定行版式、逐字节金帧测试。
- 状态：`apps/cli/src/tui/app.rs` 不持有 `Session`；会话以 `ViewFacts` 快照进入帧。
- 捕获：Renderer 输出经 `CaptureSink` → `capture::sanitize` → 有界 `Scrollback`。
- 品牌：经典 REPL 走 `brand/mod.rs` + 生成的 `flame.rs` / `brand_block`；TUI 走
  `brand/banner.rs` 与 `brand/sidebar.rs`（嵌入 `assets/ignis-sidebar-truecolor.ans`）。
- 模型配置：`ignis_runtime::ModelConfig`（`base_url` / `model` / `api_key_env` / `protocol`）。
- 上下文：`Limits::context_char_budget` 是**字符**预算；现有 TUI 的 `ctx n chars` 不是 token。
- 真实可用信息：
  - Tools：`ignis_core::tools::available_tool_names()`（6 内置 + 4 元工具）。
  - Skills：`Session::skills().metas()`（内置 + 用户目录）。
  - Memory：`MemoryStore::list(false).len()`。
  - Session：`Session::current_session()`（懒创建，短标签 `new` / 13 字符）。
  - Runtime：`Session::kernel()` + `is_native()`。
  - Model：`Session::model()`（会话级覆盖或配置）。
  - Usage：native 的 `RuntimeEvent::Usage`（每轮），`TaskOutcome::usage` 目前是整轮累计。

## 4. 模型能力解析（ModelCapabilitiesResolver）

### 4.1 数据形状

新增 `ignis_runtime::caps`：

```rust
pub enum CapabilitySource { Runtime, ProviderApi, Catalog, Registry, UserConfig, Unknown }

pub struct ModelCapabilities {
    pub provider: String,              // OpenAI / OpenRouter / Anthropic / Gemini / Ollama / vLLM / OpenAI-compatible
    pub model_id: String,
    pub context_window: Option<u64>,   // 模型原生窗口（Provider API / Registry）
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub runtime_limit: Option<u64>,    // vLLM max_model_len / Ollama context_length
    pub user_limit: Option<u64>,       // config.model.context_window
    pub effective_limit: Option<u64>,  // min(context_window, runtime_limit, user_limit)
    pub source: CapabilitySource,      // context_window（或仅有 user 时的有效值）来源
    pub effective_source: CapabilitySource,
}
```

解析优先级（与需求一致）：

1. **Runtime 实际限制**：vLLM `GET {base}/models` 的 `max_model_len`；Ollama
   `POST {root}/api/show` 的 `*.context_length` / `num_ctx`。
2. **Provider API**：OpenRouter `GET {base}/models` 的 `context_length` 与
   `top_provider.max_completion_tokens`。
3. **通用模型目录**：models.dev 的 `provider/model → limit.context/input/output`。
   先用 `base_url` host 匹配 Provider（`api.minimax.cn` → `minimax`），再按
   `provider/model` / canonical id / 全目录同名模型查找；同名多值冲突时返回 Unknown。
4. **本地 Registry**：OpenAI / Anthropic / Gemini 等常见模型的
   `context_window` 与 `max_output_tokens`（最长前缀匹配；离线兜底）。
5. **用户配置覆盖**：`model.context_window` / `model.max_output_tokens`（`ignis config set` 可写）。
6. **Unknown**：不猜、不默认 200K；UI 显示 `?` / `--`，`/model info` 显示 `Unknown`。

`Context Window ≠ Max Output Tokens`：两者是独立字段；`effective_limit` 是上下文总窗口，
输出上限只展示与预留，不当作窗口。

### 4.2 Provider 探测

按 `base_url` 的 host/path 与 `protocol` 探测：

- `openrouter.ai` → OpenRouter（API 优先）
- `api.anthropic.com` → Anthropic（Catalog/Registry）
- `generativelanguage.googleapis.com` → Gemini（Catalog/Registry）
- `api.openai.com` → OpenAI（Catalog/Registry）
- `:11434` / `ollama` → Ollama（Runtime API）
- `vllm` 或本机自定义端点 → vLLM / 本地 OpenAI-compatible（尝试 Runtime API，失败则 Catalog/Registry/User/Unknown）
- `api.minimax.*` → MiniMax（Catalog；直连 `/models` 不含窗口字段）
- 其余 → OpenAI-compatible（Catalog/Registry/User/Unknown）

不假设 OpenAI-compatible `/models` 一定返回窗口；只在已知会返回的 Provider/本地端点上解析字段。
拿不到字段时由 models.dev 目录兜底，不让用户为每个新模型改 Rust 代码。

### 4.3 缓存与失效

`ModelCapabilityResolver` 持有 `Mutex<HashMap<CacheKey, ModelCapabilities>>`，
key = `(base_url, model, context_window, max_output_tokens)`；另持有一份
`OnceCell<Option<Arc<ModelCatalog>>>`。
- 能力缓存命中直接返回；未命中先加载通用目录，再异步探测后写入。
- 目录来自 `https://models.dev/api.json`，压缩缓存到 `~/.ignis/model_catalog.json`
  （0600），TTL 24h；过期才拉取，失败回退旧缓存/内置 Registry。删除缓存文件可强制刷新。
- 目录查找按 host → Provider、再按 model/canonical id；同名多 Provider 值冲突时不猜。
- 切换模型 / 修改配置后 key 变化，旧条目不会误用；`Session::refresh_model_capabilities`
  在 `/model <name>` 成功后重新解析并替换会话缓存。
- 网络探测 1.2s 超时、目录拉取 3s 超时；失败回退静态解析（Catalog/Registry/User/Unknown），
  不阻塞任务执行。

### 4.4 上下文用量

- 新增 `TaskOutcome::context_usage: Option<Usage>`：`TaskRunner` 记录**最后一个**
  `RuntimeEvent::Usage`（最后一轮请求的 input+output），而 `TaskOutcome::usage` 仍是整轮累计。
- `Session::last_context_usage` 保存它；状态栏优先使用 Provider 数据（exact）。
- 没有 Provider 数据时，用 `PromptStats` 的 `system_chars + history_chars` 做**明确标记的估算**：
  估算值前加 `≈`；绝不把字符数直接写成 token。
- 窗口未知时：`ctx 12.5K/? [----------] --`。
- 窗口已知时：`ctx 12.5K/128K [██████░░░░] 10%`。
- 窗口已知、用量暂时未知时：`ctx -/128K [----------] --`（不把 0 伪装成真实占用）。
- 60% / 85% 阈值用黄/红提示，为后续压缩机制预留接口。

## 5. TUI 视觉与版式

### 5.1 帧版式

正常高度（`rows >= 12`）：

```text
row 0            顶部品牌行：IGNIS v0.2.2 · workspace · kernel · session
row 1..rule       内容区（欢迎面板 / 会话视口）
                   ─────────────────────────────────────────────
                  运行状态行：✦ model │ ctx ... │ [bar] pct │ ● Ready │ 4.2s
                  输入区：❯ 正文（长行按宽度折行；多行时 … 续行；有提示时再加一行）
                   ─────────────────────────────────────────────
最后一行          底部状态行：mode · last usage · hint
```

- 固定 chrome = 5 + 输入行数（顶行 + 2 分隔 + 状态行 + 输入 + 底行）：空输入 6 行，
  内容区 `rows - 6`；多行/提示展开时输入最多 3 行，内容区相应缩小。
- `rows < 12`：去掉两条分隔线，固定 chrome = 3 + 输入行数（空输入 4 行）。
- `rows < 6`：输入区贴底并补空行到 `rows`，绝不吐超过 `rows` 行。
- 输入行数变化时，内容区（消息视口 / 欢迎面板 / 选择器开窗）与 PgUp/PgDn 步长同步重算。
- `compose` 仍是纯函数、逐字节金帧测试；新增 `Rule` / `Meta` 槽位。

### 5.2 品牌横幅

- 大型：ANSI Shadow 字模水平加倍（66 列 × 6 行，`cols >= 140` 且内容高度足够）。
- 中型：ANSI Shadow 原尺寸（33 列 × 6 行，`cols >= 80`）。
- 小型：单行 `IGNIS` 字标（`cols >= 60` 或高度不足）。
- 极简：`IGNIS v0.2.2` 单行。
- 颜色：真彩按列做橙 `#FF6B1A` → 金 `#FFC24B` 渐变；256 色用 `rgb_to_ansi256`；
  基本色用黄；`ColorDepth::None` 零 SGR。每行行尾 reset。
- 横幅下方：`Your Heraclitus.` 与 `v0.2.2 · Ever-Living Fire.`（按高度取舍）。
- 所有行按 `display_width` 居中；任何行不得超过终端宽度。

### 5.3 欢迎面板

- 圆角边框 `╭╮╰╯`，标题 `IGNIS v0.2.2 · Ever-Living Fire. · Native Runtime`。
- `cols >= 100`：双栏（左 35% / 右 65%），左栏火焰 + Model/Provider/Workspace/Session/Runtime/Memory，
  右栏 Tools/Skills/统计。
- `80 <= cols < 100`：紧凑双栏（火焰缩略图 + 右栏）。
- `60 <= cols < 80`：单栏。
- `cols < 60` 或高度不足：极简 3–4 行（字标、模型、上下文/状态、统计）。
- 火焰：TUI 使用 `assets/ignis-sidebar-truecolor.ans`（11×16 truecolor ANSI），按可用高度
  采样 4–11 行；高度足够时完整 11 行，80×24 等紧凑布局 4 行。True 保留 truecolor，
  Ansi256 转色号，Basic16 / None 退化为无 SGR 剪影。经典 REPL 的 `flame::FULL/COMPACT`
  仍保留，不受影响。
- 欢迎语：`Fire, With Intent.`；提示行：`✦ Tip: Type /help to explore commands.`。
- Tools：按实际工具名分类（filesystem/shell/network/agent/other），每类最多展示 4 个，
  其余 `+N more`；不展示不存在的工具。
- Skills：按真实来源 `builtin` / `user` 分组，每类最多 4 个，其余 `+N more`。
  扫描内置 + 项目级 `<workspace>/.ignis/skills` + 全局 `~/.ignis/skills`；目录符号链接跟随，
  悬空链接以 `! path: 符号链接目标不存在` 告警。
- 技能调用：发现后的名称可直接写成 `/<name> <task>`；任务文本要求模型先 `load_skill`
  读全文再执行（命令表里的静态命令优先）。
- 统计：`{tools} tools · {skills} skills · {memory} memories · /help for commands`；
  窄栏（右栏 < 60 列）把尾缀降为 `/help`，保证 80 列下不被截断。
- 所有值来自 `ViewFacts`；不可用显示 `--` / `Not available`。
- 欢迎面板是**实时视图**，不写入 scrollback：首次提交/命令输出后自然消失，
  Resize 时按新尺寸重新合成；不会在每轮回复后重复出现。

### 5.4 输入区与状态栏

- 输入区是**动态高度**编辑器：空输入只占一行 `❯`；多行输入显示 1–2 行正文（续行 `… `）；
  Tab/斜杠提示非空时再加一行；不是固定三行。保留多行、历史、Ctrl+A/E/U/W、Tab 补全、Ctrl+C/D。
- 超过终端可用列数的单行按显示宽度折成物理行（续行 `… `），窗口最多两行正文；
  折行、制表位展开与光标列复用 `editor::draw` 的走行算法，缓冲文本一个字节不改。
- 分隔线形成上下边界；输入首行 `❯ ` 品牌色加粗。
- 输入 `/` 时，提示行实时展示 `slash::complete` 的候选（不新增/虚构命令）。
- 运行状态行（输入区上方）：
  - `✦ {model} │ ctx {used}/{window} [{bar}] {pct}% │ {glyph} {state} │ {elapsed}`
  - 宽度分档：>=110 全量；80–109 去耗时；60–79 去模型；<60 只留状态 + 上下文。
  - 上下文窗口未知：`ctx 12.5K/? [----------] --`。
  - 估算用量：`ctx ≈12.5K/128K`。
- Agent 状态：`● Ready` / `◈ Thinking` / `⚙ Running Tool` / `◉ Streaming` / `✓ Completed` / `✗ Error`，
  对应绿/琥珀/青/青/绿/红。Thinking 字形每秒轮换一次；无色/`NO_COLOR` 时静止。
- 底部状态行：`mode {mode} · last in=… out=… · {hint}`，保持简洁。

### 5.5 消息与工具渲染

- 用户消息：`❯ 内容`（续行 `… `），品牌色标记；任务轮次后留一个空行。
- Assistant：复用现有 Markdown 渲染（标题/列表/代码块/行内代码/路径/强调）。
- Thinking：只在状态行显示，不无条件展示内部推理；Debug 模式仍按既有渲染器输出。
- 工具行：新增 Renderer flavor `Tui`（经典路径默认 `Classic`，逐字节不变）：
  - 开始：`⚙ {tool} — {summary}`（dim）
  - 成功：`✓ {tool} — Done · 0.8s`
  - 失败：`✗ {tool} — Failed · 0.8s`
  - 长输出仍由 Debug 首行预览 + 有界 scrollback 承担。
- 回合摘要：TUI flavor 用 `✓ Done · 3.2s · 4 tools · task {short}`；
  Failed/Aborted/Limit reached 用对应字形；经典路径继续 `● Done …`。
- `--tui -c` 命中时把 canonical 历史重放进滚动区：`resumed session …` 提示 + 用户 `❯`
  回显 + assistant Markdown（复用实时渲染器）；欢迎面板自然让位，不会看起来像新会话。
  `-c` 仍只作用于当前 workspace。
- 状态推断：TUI 捕获面只根据渲染器输出的行首更新 Agent 状态（`⚙` → Running Tool，
  正文 → Streaming，任务终态 → Completed/Error），不解析模型内部推理。

## 6. Slash 命令

- `/model info`：展示
  `Model Information / Provider / Model / Context Window / Max Input / Max Output /
   Runtime Limit / User Limit / Effective Limit / Source`。
  未知值显示 `--`；来源显示 `Runtime API` / `Provider API` / `Model Registry` / `User config` / `Unknown`。
- `/model` 的帮助与补全更新为 `[name|info]`。
- `/model <name>` 成功后重新解析能力并刷新 TUI；经典 REPL 在 `/model` / `/model info` 后同样刷新。
- 其余命令、帮助分组、补全与既有文案保持不变。

## 7. 响应式与性能

- 宽度/高度分档只影响合成结果，状态不丢失；SIGWINCH 后立即重算。
- 宽字符/中文/Emoji：沿用 `render::width` 的显示宽度与 `truncate_to_width`；不切半格。
- 长路径：中间截断（保留开头与结尾）；长模型名：截断尾部。
- 欢迎面板高度封顶，Tools/Skills 折叠；不允许无限增长。
- 空闲期只在输入/Resize/捕获变化时重绘；忙碌期流式输出按捕获变化重绘，
  耗时/Thinking 动画最多 1Hz；无整屏持续动画。
- 无色档零 SGR；`TERM=dumb` / 非 tty 仍走经典 REPL 降级。
- 不破坏终端选区、复制粘贴、Ctrl+C/D 行为；不新增 panic 路径。

## 8. 兼容性

- 经典 REPL 的输出、品牌块、工具行、结束行逐字节不变：新行为全部由 `RenderFlavor::Tui` /
  TUI 专属合成函数门控。
- 既有测试断言只允许因「TUI 版式升级」按新契约更新；不得删除或弱化经典路径的断言。
- 配置新增字段使用 `serde(default)` + `skip_serializing_if`，旧 `config.json` 无需迁移。
- `context_char_budget` 继续用于 `trim_history`；新 token 字段只用于展示与预警。

## 9. 验收与测试

### 9.1 单元测试

- `caps`：Provider 探测、Registry 最长前缀、OpenRouter/vLLM/Ollama JSON 解析、
  用户覆盖、Runtime/Provider/Catalog/Registry 冲突取 min、Unknown、缓存 key 与失效。
- `catalog`：models.dev JSON 解析、host → Provider、provider/model 精确匹配、canonical id、
  同名多 Provider 冲突不猜、缓存 roundtrip；测试全部用 fixture，不访问网络。
- `TaskOutcome::context_usage`：最后一个 Usage 与累计 Usage 分离。
- Banner：三档宽度、66/33 列、无色零 SGR、行宽不超限、渐变。
- Welcome：80×24 / 100×30 / 140×40 / 180×50 / 60×24 / 40×12 的行数/行宽/真实数据/折叠。
- Frame：规则行、状态行、元信息行、光标落位、任意尺寸不 panic、无色帧零 SGR。
- Agent 状态：Ready → Thinking → Running Tool → Streaming → Completed/Error；1Hz 刷新。
- Slash：`/model info` 文案、来源、未知窗口、用户覆盖、有效限制。

### 9.2 pty / e2e

- 启动欢迎面板正常显示，`--no-banner` 不显示大型横幅。
- 空输入只占一行；多行输入/提示展开时内容区同步收缩，不产生空白输入行。
- 80 列下 100 字符输入折成 `❯ ` / `… ` 两行，光标在续行末尾，不截断。
- 发送消息、流式回复、工具行、结束摘要正常。
- `--tui -c` 在同一 workspace 重放上一轮用户/assistant 历史；跨 workspace 不会误恢复。
- Resize 不产生乱码/布局错乱；80×24 输入区可用。
- 既有 pty 用例全部回归。

### 9.3 提交门禁

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 10. 实现里程碑

1. `ignis-runtime::caps` + `ModelConfig` 新字段 + `/model info`（TDD：先红后绿）。
2. `TaskOutcome::context_usage` + Session 访问器 + 上下文估算。
3. `RenderFlavor` + TUI 工具/结束行渲染（经典路径字节不变）。
4. `brand::banner` + `tui::welcome` + 响应式欢迎面板。
5. `view` 新版式（Rule/Meta/状态行/输入区）+ `app` Agent 状态/耗时/欢迎开关。
6. pty 回归、README/配置文档、全量门禁。

## 11. 已知限制

- Token 估算没有引入 tokenizer 依赖：无 Provider Usage 时用字符比例估算，并明确标 `≈`；
  首轮请求前的上下文占用可能与本机 tokenizer 有偏差。
- Provider API 不返回窗口时依赖 models.dev 通用目录；目录由社区维护，未收录的新模型或同名
  多 Provider 冲突会显示 Unknown，请用 `model.context_window` 显式配置。
- 目录首次拉取约 1.3MB（压缩缓存 0600），24h 刷新一次；离线时回退旧缓存/内置 Registry。
- vLLM/Ollama 的探测依赖各自端点形状；端点不可达时回退 Registry/User/Unknown。
- 不实现 token 级上下文裁剪；`trim_history` 仍按字符预算工作，token 阈值只用于展示预警。
- 欢迎面板是启动视图，不进入 scrollback；`--no-banner` 会关闭整块欢迎视图。


## 13. 0.2.2 交互增量

- 命令面板：`Ctrl+P` 搜索静态命令与当前技能目录；选中项填入输入区（有参数的命令带尾随空格），
  回车执行。空输入状态栏提示 `Type a task · / commands · Ctrl+P palette`；`/` 补全同时列出技能。
- Agent 状态语言：`Ready / Thinking / Executing / Waiting / Scheduled / Failed / Completed`；
  审批/确认期 Waiting，工具与 Skill 执行期 Executing，调度任务成功收尾 Scheduled。滚动区用
  `◈ Thinking…` / `◉ Response` 段标区分思考与正文。
- 交互模式：`/mode [normal|plan|auto]`。Normal 逐次审批；Plan 由只读工具 profile
  （`fs_read`/`fs_list`/`load_skill`）+ 计划提示约束；Auto 用 Preauthorized 策略
  （Safe/Elevated 放行、Dangerous 拒绝），仅 native。模式显示在状态行与 meta 行。
- `/usage`：按需展示有效窗口/来源、最近一次 Provider 输入 token、最后一轮 in/out、历史与字符
  预算；Provider 不提供的缓存/累计/压缩记录不伪造。
- 测试：panel 过滤/填充、状态 Waiting/Scheduled、模式策略矩阵、用法面板来源字段、真实 pty
  Ctrl+P 与项目技能调用。
