//! 帧合成器与有界滚动区：TUI 的「画什么」全在这里——纯函数，不碰终端、不碰 IO。
//!
//! 分工：[`Scrollback`] 是捕获文本的落点（按行摄入、上限 [`SCROLLBACK_LINES`]、记着滚动位置）；
//! 一帧的全部输入是 [`TuiView`]（它不认识会话与终端）；[`compose`] 把它拼成整帧字节；顶部/底部
//! 状态行的文案由 [`top_line`]/[`status_line`] 从 [`ViewFacts`] 快照里拼出，宽度截断统一交给
//! [`compose`]（文案函数只逐字拼，用例才好逐字断言）。帧里的每一条内容行在截断之前都过一遍净化
//! （幂等，放行面看 `color`）：上色帧走 [`capture::sanitize`]，内容里的 ESC 只可能是被放行的 SGR
//! （放行面是 SGR 形状，不是枚举出来的几条）；无色帧走 [`capture::sanitize_plain`]，连 SGR 也不放行。
//! 帧协议自己的序列在一旁照常。
//!
//! 帧协议（逐字钉住，金帧用例锁死）：
//!
//! ```text
//! "\x1b[?25l"                      画帧期间藏光标
//! "\x1b[H"                         回左上角
//! rows 行，每行 = 内容 + ("\x1b[0m" 当 color) + "\x1b[K" + （不是最后一行时 "\r\n"）
//! "\x1b[{row};{col}H"              光标绝对定位（1 基；cursor 为 None 时落在最后一行行尾）
//! "\x1b[?25h"                      画完显光标（cursor 为 None 时不写这一条）
//! ```
//!
//! 版式（`rows` 行自顶向下）：`0` = 顶部状态行；`1 .. first_input` = 消息视口（`rows-5` 行，不足
//! 在下方补空行、超出只留最新的一段）；`first_input .. first_input+3` = 输入区三行
//! （`first_input = max(1, rows-4)`）；最后一行 = 底部状态行。`rows < 5` 时视口整块让位，按
//! `[top] ++ input(3) ++ [status]` 取前 `rows` 行——终端比预期小是常态（用户随手拉窗口）：合成器
//! 绝不吐超过 `rows` 行，光标也一定落在 `1..=rows`/`1..=cols` 里。
//!
//! 颜色（`color`）：`false` 时帧里一条 SGR 都没有（内容里的 SGR 在净化时就丢掉了；定位与清行不是
//! 颜色，照常写）；`true` 时 chrome 走 [`crate::render::palette`]（按 `view.depth` 取品牌档）——顶部/
//! 底部状态行与输入区提示行 dim、顶行的字标（`IGNIS v…`）与输入区首行标记（`❯ `）`bold + brand`、
//! 每行结尾补一条重置：被截断的行不会把颜色漏到下一行。

use std::collections::VecDeque;
use std::time::Duration;

use ignis_core::config::{OutputMode, Theme};
use ignis_core::skills::SkillMeta;
use ignis_core::task::PromptStats;
use ignis_runtime::Usage;
use ignis_runtime::caps::ModelCapabilities;
use ignis_storage::SessionSummary;

use crate::commands::{InteractionMode, slash};
use crate::render::width::{char_width, display_width, truncate_to_width};
use crate::render::{ColorDepth, Palette, palette};

use super::Size;
use super::capture;
use super::term::{CURSOR_HOME, HIDE_CURSOR, SHOW_CURSOR};

/// 滚动区上限（设计 §2.2：不做无界 scrollback）。
pub const SCROLLBACK_LINES: usize = 2000;

/// 输入区首行的标记（与 `editor::draw` 的前缀同字面量）。
const INPUT_MARK: &str = "❯ ";

/// 带分隔线的完整版式的最小行数：低于它去掉两条分隔线，但不牺牲输入区。
pub(crate) const RULE_MIN_ROWS: usize = 12;

/// 每行结尾的清行序列（清到行尾：截断留下的旧内容不留残影）。
const CLEAR_TO_EOL: &str = "\x1b[K";

/// 行分隔：帧里每行都从列 0 起（`\r\n`，光标不粘在行中）。
const LINE_BREAK: &str = "\r\n";

/// SGR 重置：`color=true` 时每行结尾补一条。
const RESET: &str = "\x1b[0m";

/// 有界滚动区 + 滚动位置。
///
/// 捕获文本按行摄入：**没有换行的尾巴留在 `pending` 里**，下一次摄入才拼成一行（渲染器的流式
/// 输出天然跨 tick 断在行中）。`offset` 是「离底部多少行」：0 = 贴底，越大越往回翻。
#[derive(Debug, Default)]
pub struct Scrollback {
    /// 已落地的行（不含 `pending` 里的尾巴）；超过上限丢最旧。
    lines: VecDeque<String>,
    /// 还没有换行的尾巴。
    pending: String,
    /// 离底部的行数（0 = 贴底）。
    offset: usize,
}

impl Scrollback {
    /// 空滚动区。
    pub fn new() -> Self {
        Self::default()
    }

    /// 摄入一段已净化的文本：按 `\n` 切行入区（超过上限丢最旧），末段留在 `pending`。
    ///
    /// 末段与已有 `pending` 拼成同一行：半行跨 tick 断在行中是渲染器的常态，这里必须接得上。
    pub fn push_text(&mut self, text: &str) {
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                // 末段：后面还没有换行，留在 pending 里等下一次摄入（或收尾）
                self.pending.push_str(part);
                return;
            }
            let mut line = std::mem::take(&mut self.pending);
            line.push_str(part);
            self.push_line(line);
        }
    }

    /// 收尾（任务结束）：把 `pending` 作为最后一行落地（非空时）。
    pub fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let line = std::mem::take(&mut self.pending);
        self.push_line(line);
    }

    /// 视口的 `count` 行（自顶向下）；`offset` 是「离底部多少行」，0 = 贴在底部。
    ///
    /// 行数不够就只给这么多行（补空行是 [`compose`] 的事：面板多大只有它知道）。
    pub fn viewport(&self, count: usize) -> Vec<String> {
        let total = self.lines.len();
        let end = total.saturating_sub(self.offset); // 视口下沿（不含）
        let start = end.saturating_sub(count);
        self.lines
            .iter()
            .skip(start)
            .take(end - start)
            .cloned()
            .collect()
    }

    /// 往回翻 `lines` 行；上限是「最旧一行贴顶」（`len - 1`）。
    pub fn scroll_up(&mut self, lines: usize) {
        let max = self.lines.len().saturating_sub(1);
        self.offset = self.offset.saturating_add(lines).min(max);
    }

    /// 往前翻 `lines` 行；到 0 即贴底。
    pub fn scroll_down(&mut self, lines: usize) {
        self.offset = self.offset.saturating_sub(lines);
    }

    /// 贴底（End）。
    pub fn scroll_to_bottom(&mut self) {
        self.offset = 0;
    }

    /// 已落地的行数（不含 `pending` 里的尾巴）。
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// 滚离底部的行数（0 = 贴底）。
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// 还没有任何落地的行。
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// 一行落地：`\t` 按 8 列制表位展开（见 [`expand_tabs`]；仅显示层），超过上限丢最旧
    /// （滚动区不做无界增长）。
    fn push_line(&mut self, line: String) {
        self.lines.push_back(expand_tabs(&line));
        while self.lines.len() > SCROLLBACK_LINES {
            self.lines.pop_front();
        }
    }
}

/// 每 8 列一个制表位（POSIX 的默认制表位）。
const TAB_STOP: usize = 8;

/// 把一行里的 `\t` 展开成到下一个制表位的空格（**仅显示层**：提交文本与渲染字节都不变）。
///
/// 列号按显示宽度累计（`你` 占 2 列），所以宽字符之后的制表位与终端上看到的一致；已经落在制表位
/// 上时补满一整档（与终端一致：`\t` 至少前进 1 列）。宽度口径（[`char_width`]）把控制字符算 0 列，
/// 不展开的话含 Tab 的行在截断与列对齐上都会算错（T3 裁定）。
fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut column = 0usize;
    for ch in line.chars() {
        if ch == '\t' {
            let spaces = TAB_STOP - (column % TAB_STOP);
            out.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            out.push(ch);
            column += char_width(ch);
        }
    }
    out
}

/// 当前 Agent 状态（状态行与动画用）。
///
/// 这是 IGNIS 的运行时状态语言，不是普通聊天框的「正在输入」：`Waiting` 表示等人审批/外部结果，
/// `Scheduled` 表示本轮把任务交给了调度系统，`Executing` 覆盖工具与 Skill 加载。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// 空闲。
    Ready,
    /// 模型正在生成或规划（含正文流式输出）。
    Thinking,
    /// 工具或 Skill 执行中。
    Executing,
    /// 等待用户审批或外部结果。
    Waiting,
    /// 任务已排入调度系统。
    Scheduled,
    /// 最近一轮成功结束。
    Completed,
    /// 最近一轮失败/取消/触限。
    Failed,
}

impl AgentState {
    /// 状态词（CLI chrome）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Thinking => "Thinking",
            Self::Executing => "Executing",
            Self::Waiting => "Waiting",
            Self::Scheduled => "Scheduled",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
        }
    }

    /// 状态字形。Thinking 按 `frame` 在四个字形间轮换（1Hz）；其余状态固定。
    pub fn glyph(self, frame: u8) -> &'static str {
        match self {
            Self::Ready => "●",
            Self::Thinking => ["◈", "◇", "◆", "◇"][usize::from(frame) % 4],
            Self::Executing => "⚙",
            Self::Waiting => "◇",
            Self::Scheduled => "◷",
            Self::Completed => "✓",
            Self::Failed => "✗",
        }
    }
}

/// 上下文用量与窗口：tokens 是实际/估算值，window 未知时为 `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    /// 当前占用（tokens）；`None` = 还没有可用的用量数据（窗口可能仍然已知）。
    pub tokens: Option<u64>,
    /// 是否是估算值（UI 会加 `≈`）。
    pub estimated: bool,
    /// 有效窗口；`None` = 无法确定。
    pub window: Option<u64>,
    /// 使用率（0–100）；用量或窗口未知时为 `None`。
    pub percent: Option<u8>,
}

impl ContextUsage {
    /// 按窗口算使用率；`window == 0` 与未知窗口都按 `None` 处理。
    pub fn new(tokens: u64, estimated: bool, window: Option<u64>) -> Self {
        let percent = window.filter(|window| *window > 0).map(|window| {
            let percent = (tokens.saturating_mul(100) + window / 2) / window;
            percent.min(100) as u8
        });
        Self {
            tokens: Some(tokens),
            estimated,
            window,
            percent,
        }
    }

    /// 有用量窗口但还没有用量：`ctx -/128K [----------] --`，绝不把 0 当成真实占用。
    pub fn unknown(window: Option<u64>) -> Self {
        Self {
            tokens: None,
            estimated: false,
            window,
            percent: None,
        }
    }

    /// 用量展示：`12.5K` / `128K` / `1.0M`；没有数据时 `-`。
    pub fn used_word(&self) -> String {
        self.tokens.map_or_else(|| "-".to_string(), format_tokens)
    }

    /// 窗口展示：`128K` / `?`。
    pub fn window_word(&self) -> String {
        self.window.map_or_else(|| "?".to_string(), format_tokens)
    }

    /// 百分比展示：`10%` / `--`。
    pub fn percent_word(&self) -> String {
        self.percent
            .map_or_else(|| "--".to_string(), |percent| format!("{percent}%"))
    }

    /// 10 格进度条：`[██████░░░░]`；窗口未知时 `[----------]`。
    pub fn bar(&self, width: usize) -> String {
        let width = width.max(1);
        let Some(percent) = self.percent else {
            return format!("[{}]", "-".repeat(width));
        };
        let filled = (usize::from(percent) * width + 50) / 100;
        let filled = filled.min(width);
        format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
    }

    /// 运行状态行里的完整片段：`ctx ≈12.5K/128K [████░░░░░░] 10%`。
    pub fn segment(&self, bar_width: usize) -> String {
        format!(
            "ctx {}{}/{} {} {}",
            if self.estimated { "≈" } else { "" },
            self.used_word(),
            self.window_word(),
            self.bar(bar_width),
            self.percent_word()
        )
    }
}

/// 没有 Provider Usage 时的保守 token 估算：约 0.4 token/字符（英文约 4 字符/token，
/// 中文约 1.5 字符/token，取中间偏保守的值）。UI 必须加 `≈`，不能把它当成精确值。
pub fn estimate_tokens_from_chars(chars: usize) -> u64 {
    (chars as u64)
        .saturating_mul(2)
        .saturating_add(4)
        .saturating_div(5)
}

/// token 数的短写：<1000 原样；<1000K 一位小数（>=100K 取整）；>=1M 一位小数。
pub fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        if tokens < 100_000 {
            let tenths = (tokens + 50) / 100;
            if tenths.is_multiple_of(10) {
                format!("{}K", tenths / 10)
            } else {
                format!("{}.{}K", tenths / 10, tenths % 10)
            }
        } else {
            format!("{}K", (tokens + 500) / 1_000)
        }
    } else {
        let tenths = (tokens + 50_000) / 100_000;
        if tenths.is_multiple_of(10) {
            format!("{}M", tenths / 10)
        } else {
            format!("{}.{}M", tenths / 10, tenths % 10)
        }
    }
}

/// 组装一帧要用的会话事实快照（忙碌期不能借用会话，所以一切都先取成这里的值）。
#[derive(Debug, Clone, PartialEq)]
pub struct ViewFacts {
    /// 版本号（`env!("CARGO_PKG_VERSION")`）。
    pub version: &'static str,
    /// 工作区。
    pub workspace: String,
    /// 内核名。
    pub kernel: String,
    /// 会话标签（`slash::session_label`：`new` 或 13 字符短 id）。
    pub session: String,
    /// 输出模式。
    pub mode: OutputMode,
    /// 交互模式（Normal / Plan / Auto）。
    pub interaction_mode: InteractionMode,
    /// 配色主题。
    pub theme: Theme,
    /// 是否上色（`resolve_color(ui.color, true, NO_COLOR)`）。
    pub color: bool,
    /// 色阶（`color_depth(color, COLORTERM, TERM)`）：chrome 的品牌档按它取。
    pub depth: ColorDepth,
    /// 字符预算（`session.context_budget()`）。
    pub budget: usize,
    /// 有效历史条数（`session.history().len()`）。
    pub history_messages: usize,
    /// 最近一次组装量到的统计（`session.last_prompt_stats()`）。
    pub stats: Option<PromptStats>,
    /// 最近一轮用量（`session.last_usage()`）。
    pub usage: Option<Usage>,
    /// 当前内核是不是 native（决定 usage 展示成数字还是 `n/a`）。
    pub native: bool,
    /// 当前生效模型名。
    pub model: String,
    /// Provider 展示名。
    pub provider: String,
    /// 模型能力快照（窗口/输出上限/来源）。
    pub capabilities: ModelCapabilities,
    /// 当前上下文用量（Provider Usage 或明确标记的估算）。
    pub context: Option<ContextUsage>,
    /// 真实注册的工具名（内置 + 元工具）。
    pub tools: Vec<String>,
    /// 当前技能目录元信息。
    pub skills: Vec<SkillMeta>,
    /// active 记忆条数；读库失败为 `None`。
    pub memory: Option<usize>,
}

/// 输入区正文 + 可选提示行 + 光标（`None` = 藏光标，例如选择器）。
///
/// 高度不是固定的：正文 0–2 行（编辑器 1–2、忙碌/审批 1、选择器 0），提示行非空时再加 1 行。
/// 空输入因此只占一行；多行输入与 Tab/斜杠提示会自动长高。
///
/// `cursor` 是**输入正文内**的 0 基（行、列）：列含 `❯ `/`… ` 前缀的显示宽度；[`compose`] 把它
/// 映射成帧里的绝对坐标（1 基）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBlock {
    /// 正文行（首行 `❯ `、续行 `… `；选择器可以为空）。
    pub lines: Vec<String>,
    /// 提示行（空串 = 不占行）。
    pub hint: String,
    /// 光标（正文内行、列，0 基）。
    pub cursor: Option<(usize, usize)>,
}

impl InputBlock {
    /// 实际占用的行数：正文 + 非空提示行；两者都空时保底一行。
    pub fn height(&self) -> usize {
        (self.lines.len() + usize::from(!self.hint.is_empty())).max(1)
    }
}

/// 一帧的全部内容（合成器不认识会话、终端与 IO）。
#[derive(Debug, Clone, PartialEq)]
pub struct TuiView {
    /// 面板尺寸。
    pub size: Size,
    /// 是否上色（chrome 的上色与每行结尾的重置都看它）。
    pub color: bool,
    /// 色阶（合成时 `palette(view.theme, view.depth)` 用它取品牌档）。
    pub depth: ColorDepth,
    /// 配色主题。
    pub theme: Theme,
    /// 顶部状态行（未截断；合成时按宽度截断）。
    pub top: String,
    /// 运行状态行（模型/上下文/Agent 状态/耗时；输入区上方）。
    pub meta: String,
    /// 消息视口 / 选择器列表 / 欢迎面板（自顶向下）。
    pub viewport: Vec<String>,
    /// 输入区正文/提示行 + 光标。
    pub input: InputBlock,
    /// 底部状态行（未截断；合成时按宽度截断）。
    pub status: String,
}

/// 顶部状态行（逐字）：`IGNIS v{version} · {workspace} · kernel {kernel} · session {session}`。
///
/// 不截断：宽度是 [`compose`] 的事，这里只拼文案。
pub fn top_line(facts: &ViewFacts) -> String {
    format!(
        "IGNIS v{} · {} · kernel {} · session {}",
        facts.version, facts.workspace, facts.kernel, facts.session
    )
}

/// 底部状态行（逐字）：`mode {mode} · last {usage} · {hint}`。
///
/// 上下文/模型/状态已经移到输入区上方的 [`meta_line`]；底行只留模式、最近用量与操作提示。
/// usage 三态：native 有值 `in={i} out={o}`、native 没值 `-`、外置内核 `n/a`。
pub fn status_line(facts: &ViewFacts, hint: &str) -> String {
    format!(
        "mode {} · output {} · last {} · {hint}",
        facts.interaction_mode.label(),
        mode_word(facts.mode),
        usage_word(facts)
    )
}

/// 运行状态行（输入区上方）：`✦ model │ ctx ... │ ● Ready │ 4.2s`。
///
/// 宽度分档优先保住「状态 + 上下文」：>=110 全量；80–109 去耗时；60–79 去模型；
/// <60 只留状态与上下文（无进度条）。所有片段都按显示宽度截断，合成器仍会兜底。
pub fn meta_line(
    facts: &ViewFacts,
    state: AgentState,
    frame: u8,
    elapsed: Option<Duration>,
    cols: usize,
) -> String {
    let model = if facts.native {
        truncate_to_width(&facts.model, 24).to_string()
    } else {
        "(kernel default)".to_string()
    };
    let ctx = facts
        .context
        .map(|context| context.segment(10))
        .unwrap_or_else(|| "ctx -/? [----------] --".to_string());
    let ctx_short = facts
        .context
        .map(|context| {
            format!(
                "ctx {}{}/{}",
                if context.estimated { "≈" } else { "" },
                context.used_word(),
                context.window_word()
            )
        })
        .unwrap_or_else(|| "ctx -/?".to_string());
    let state = format!("{} {}", state.glyph(frame), state.label());
    let interaction = match facts.interaction_mode {
        InteractionMode::Normal => None,
        InteractionMode::Plan => Some("Plan"),
        InteractionMode::Auto => Some("Auto"),
    };
    let elapsed = elapsed.map(format_elapsed_word);
    let mut parts: Vec<String> = if cols >= 80 {
        vec![format!("✦ {model}"), ctx]
    } else if cols >= 60 {
        vec![ctx]
    } else {
        vec![state.clone(), ctx_short]
    };
    if cols >= 60 {
        if let Some(interaction) = interaction {
            parts.push(interaction.to_string());
        }
        parts.push(state);
    }
    if cols >= 110
        && let Some(elapsed) = elapsed
    {
        parts.push(elapsed);
    }
    parts.join(" │ ")
}

/// 耗时展示：`4.2s` / `1m 02s`。
fn format_elapsed_word(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        let total = elapsed.as_secs();
        format!("{}m {:02}s", total / 60, total % 60)
    }
}

/// usage 的展示词：外置内核给 `n/a`（估算与否都不展示数字），native 没值给 `-`。
fn usage_word(facts: &ViewFacts) -> String {
    if !facts.native {
        return "n/a".to_string();
    }
    match facts.usage {
        Some(usage) => format!("in={} out={}", usage.input_tokens, usage.output_tokens),
        None => "-".to_string(),
    }
}

/// 输出模式的三档字面量（与 `ui.output_mode` 的 serde 名一致）。
fn mode_word(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Minimal => "minimal",
        OutputMode::Normal => "normal",
        OutputMode::Debug => "debug",
    }
}

/// 命令面板的一项：可搜索的标签 + 选中后填进输入区的文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    pub label: String,
    pub insert: String,
}

/// 按查询过滤命令面板（大小写不敏感，匹配标签或要填入的命令）。
pub fn filtered_palette<'a>(items: &'a [PaletteItem], query: &str) -> Vec<&'a PaletteItem> {
    let needle = query.trim().to_lowercase();
    items
        .iter()
        .filter(|item| {
            needle.is_empty()
                || item.label.to_lowercase().contains(&needle)
                || item.insert.to_lowercase().contains(&needle)
        })
        .collect()
}

/// 命令面板列表：标题 + 过滤后的项，选中行前缀 `"> "`，空结果给一句说明。
pub fn palette_lines(
    items: &[PaletteItem],
    query: &str,
    cursor: usize,
    max_rows: usize,
) -> Vec<String> {
    let filtered = filtered_palette(items, query);
    let mut lines = vec![format!("Command Palette · {} actions", filtered.len())];
    if filtered.is_empty() {
        lines.push("(no matching commands)".to_string());
    } else {
        for (index, item) in filtered.iter().enumerate() {
            let prefix = if index == cursor { "> " } else { "  " };
            lines.push(format!("{prefix}{}", item.label));
        }
    }
    lines.truncate(max_rows);
    lines
}

/// 选择器列表：复用 [`slash::sessions_lines`] 的表格语义（同一表头、同一短 id、当前会话带 `*`），
/// 选中行前缀 `"> "`、其余行（含表头）前缀 `"  "`；空表回 `["(no sessions)"]`。
///
/// 前缀不破坏列对齐：`sessions_lines` 的每一行都从第 0 列起排版，整体右移两列即可。`cursor` 是
/// 列表里**会话行**的 0 基下标（表头不参与选择），越界（含空表）就是没有选中行。
pub fn selector_lines(
    rows: &[SessionSummary],
    current: Option<&str>,
    cursor: usize,
) -> Vec<String> {
    let table = slash::sessions_lines(rows, current);
    if rows.is_empty() {
        // 空表就是 `(no sessions)` 那一句：没有可选项，不挂选择前缀
        return table;
    }
    table
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            // 第 `cursor+1` 行是选中行（第 0 行是表头）
            let selected = index > 0 && index - 1 == cursor;
            let prefix = if selected { "> " } else { "  " };
            format!("{prefix}{line}")
        })
        .collect()
}

/// 合成整帧字节（纯函数，逐字节可断言）；每一行都过 [`truncate_to_width`]。
///
/// 内容行在**截断之前**统一过一遍净化（幂等）：帧的内容（viewport/top/input/status）各有各的来源
/// （模型输出、命令输出、会话名），清屏/改标题之类的序列绝不能借帧抵达终端。放行面跟着 `view.color`
/// 走：上色帧用 [`capture::sanitize`]（chrome 自己上的色都是正当 SGR，形状命中放行面，原样穿过），
/// 无色帧用 [`capture::sanitize_plain`]（一条 SGR 都不放行——无色帧不补行尾重置，漏进来的属性会跨
/// `\r\n` 一路刷到后面的行）。
pub fn compose(view: &TuiView) -> String {
    let palette = palette(view.theme, view.depth);
    let rows = sanitized(layout(view), view.color);
    let mut frame = String::from(HIDE_CURSOR);
    frame.push_str(CURSOR_HOME);
    for (index, (slot, text)) in rows.iter().enumerate() {
        let line = paint(*slot, text, &palette, view.color);
        frame.push_str(truncate_to_width(&line, view.size.cols));
        if view.color {
            frame.push_str(RESET);
        }
        frame.push_str(CLEAR_TO_EOL);
        if index + 1 < rows.len() {
            frame.push_str(LINE_BREAK);
        }
    }
    let (row, col) = cursor(view, &rows);
    frame.push_str(&format!("\x1b[{row};{col}H"));
    if view.input.cursor.is_some() {
        frame.push_str(SHOW_CURSOR);
    }
    frame
}

/// 版式里的每一条内容行过一遍净化（幂等：普通文本与放行的序列原样保留）。
///
/// `colored` 决定放行面：上色帧放行任意 SGR（[`capture::sanitize`]），无色帧一条 SGR 也不留
/// （[`capture::sanitize_plain`]）——无色帧不补行尾重置，任何属性都会跨行漏下去。
///
/// 在截断之前做：截断之后的行已经是显示层的一部分，内容边界不再可辨。光标列也从净化后的行算
/// （[`cursor`]），内容里的转义因此既进不了帧、也不会把光标位置算歪。
fn sanitized(rows: Vec<(Slot, String)>, colored: bool) -> Vec<(Slot, String)> {
    rows.into_iter()
        .map(|(slot, text)| {
            let text = if colored {
                capture::sanitize(text.as_bytes())
            } else {
                capture::sanitize_plain(text.as_bytes())
            };
            (slot, text)
        })
        .collect()
}

/// 一行在版式里的位置：决定这一行套哪一档 chrome（行内容本身是普通文本，颜色由 [`paint`] 加）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// 顶部状态行（字标 `bold + brand`，其余 dim）。
    Top,
    /// 分隔线（dim）。
    Rule,
    /// 运行状态行（模型/上下文/Agent 状态/耗时）：分段上色。
    Meta,
    /// 消息视口 / 选择器列表 / 欢迎面板：正文自带渲染器的颜色，chrome 不再上色。
    Viewport,
    /// 输入区正文：`0` 首行（标记 `bold + brand`）、`1` 续行。
    Input(usize),
    /// 输入区提示行（dim；Tab/斜杠建议/忙碌说明）。
    Hint,
    /// 底部状态行（dim）。
    Status,
}

/// 版式档位：完整（带分隔线）/ 紧凑 / 极小（只有顶行 + 输入 + 底行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutKind {
    Full,
    Compact,
    Degraded,
}

fn layout_kind(rows: usize) -> LayoutKind {
    if rows >= RULE_MIN_ROWS {
        LayoutKind::Full
    } else if rows >= 6 {
        LayoutKind::Compact
    } else {
        LayoutKind::Degraded
    }
}

/// 一帧的固定行数（不含内容区）：应用层算 PgUp/PgDn 步长与选择器开窗都用它。
///
/// `input_height` 是 [`InputBlock::height`] 的实际值——输入区随多行/提示展开或收起，
/// 内容区因此会相应增减。
pub(crate) fn fixed_rows(rows: usize, input_height: usize) -> usize {
    match layout_kind(rows) {
        LayoutKind::Full => 1 + 2 + 1 + input_height + 1,
        LayoutKind::Compact => 1 + 1 + input_height + 1,
        LayoutKind::Degraded => 0,
    }
}

/// 内容区（消息视口 / 欢迎面板 / 选择器列表）的可视行数。
pub(crate) fn content_rows(size: Size, input_height: usize) -> usize {
    size.rows
        .saturating_sub(fixed_rows(size.rows, input_height))
}

/// 一条分隔线：正好 `cols` 列（合成器仍会兜底截断）。
fn rule_text(cols: usize) -> String {
    "─".repeat(cols)
}

/// 视口：取最新的一段（自顶向下），不足的行在下方补空行。
fn push_viewport(lines: &mut Vec<(Slot, String)>, view: &TuiView, count: usize) {
    let start = view.viewport.len().saturating_sub(count);
    lines.extend(
        view.viewport[start..]
            .iter()
            .cloned()
            .map(|line| (Slot::Viewport, line)),
    );
    for _ in view.viewport.len().min(count)..count {
        lines.push((Slot::Viewport, String::new()));
    }
}

/// 版式：一帧的 `rows` 行各归谁（内容未截断、未上色）。
fn layout(view: &TuiView) -> Vec<(Slot, String)> {
    let rows = view.size.rows;
    let input_rows = input_slots(&view.input);
    let input_height = input_rows.len();
    let fixed = fixed_rows(rows, input_height);
    let mut lines: Vec<(Slot, String)> = Vec::with_capacity(rows);
    lines.push((Slot::Top, view.top.clone()));
    match layout_kind(rows) {
        LayoutKind::Full => {
            push_viewport(&mut lines, view, rows.saturating_sub(fixed));
            lines.push((Slot::Rule, rule_text(view.size.cols)));
            lines.push((Slot::Meta, view.meta.clone()));
            lines.extend(input_rows);
            lines.push((Slot::Rule, rule_text(view.size.cols)));
            lines.push((Slot::Status, view.status.clone()));
        }
        LayoutKind::Compact => {
            push_viewport(&mut lines, view, rows.saturating_sub(fixed));
            lines.push((Slot::Meta, view.meta.clone()));
            lines.extend(input_rows);
            lines.push((Slot::Status, view.status.clone()));
        }
        // 极小尺寸：视口整块让位，输入区贴底（`[top] ++ padding ++ input ++ [status]`）。
        // 补空行保证帧仍恰好 `rows` 行；行数不足时照旧截断。
        LayoutKind::Degraded => {
            let padding = rows.saturating_sub(1 + input_height + 1);
            lines.extend(std::iter::repeat_with(|| (Slot::Viewport, String::new())).take(padding));
            lines.extend(input_rows);
            lines.push((Slot::Status, view.status.clone()));
        }
    }
    lines.truncate(rows);
    lines
}

/// 输入区实际行（正文在前、提示行在后；顺序即版式顺序）。
fn input_slots(input: &InputBlock) -> Vec<(Slot, String)> {
    let mut slots: Vec<(Slot, String)> = input
        .lines
        .iter()
        .enumerate()
        .map(|(row, line)| (Slot::Input(row), line.clone()))
        .collect();
    if !input.hint.is_empty() {
        slots.push((Slot::Hint, input.hint.clone()));
    }
    if slots.is_empty() {
        slots.push((Slot::Input(0), String::new()));
    }
    slots
}

/// 一行普通文本 → 帧里的一行：`color=true` 时按槽位上 chrome。
///
/// 两处品牌落点：顶行的**字标**（首个 ` · ` 之前、以 `IGNIS v` 开头的那一段）与输入区首行的标记
/// `❯ `——都是 `bold + brand`；顶行其余部分与提示行照旧 dim。
///
/// 行尾的重置由 [`compose`] 统一补：输入区首行在标记之后就补一条（标记之后的正文保持默认样式），
/// 被截断的行也靠行尾那条重置兜住。
fn paint(slot: Slot, text: &str, palette: &Palette, colored: bool) -> String {
    if !colored {
        return text.to_string();
    }
    match slot {
        // 顶行：头部（`IGNIS v…`）bold + 品牌色，其余 dim。头部按首个 ` · ` 切出来；切口不匹配
        // （没有分隔符 / 不以字标开头）时整行 dim——与旧口径一致。
        Slot::Top => match text.split_once(" · ") {
            Some((head, rest)) if head.starts_with("IGNIS v") => {
                let (bold, brand, dim) = (palette.bold, palette.brand, palette.dim);
                format!("{bold}{brand}{head}{RESET}{dim} · {rest}")
            }
            _ => {
                let dim = palette.dim;
                format!("{dim}{text}")
            }
        },
        Slot::Rule => {
            let dim = palette.dim;
            format!("{dim}{text}")
        }
        Slot::Meta => paint_meta(text, palette),
        Slot::Input(0) => match text.strip_prefix(INPUT_MARK) {
            Some(rest) => {
                let (bold, brand) = (palette.bold, palette.brand);
                format!("{bold}{brand}{INPUT_MARK}{RESET}{rest}")
            }
            // 忙碌期的首行不是输入标记（`working…`）：与提示行同档的说明性文案（dim）
            None => {
                let dim = palette.dim;
                format!("{dim}{text}")
            }
        },
        Slot::Hint | Slot::Status => {
            let dim = palette.dim;
            format!("{dim}{text}")
        }
        // 视口是渲染器的输出（颜色由它自己带）；输入区续行保持默认样式
        Slot::Viewport | Slot::Input(_) => text.to_string(),
    }
}

/// 运行状态行分段上色：品牌菱形、上下文阈值色、Agent 状态色，分隔符保持 dim。
fn paint_meta(text: &str, palette: &Palette) -> String {
    let dim = palette.dim;
    let mut out = String::new();
    for (index, segment) in text.split(" │ ").enumerate() {
        if index > 0 {
            out.push_str(RESET);
            out.push_str(&format!("{dim} │ "));
        }
        let color = if segment.starts_with('✦') {
            format!("{}{}", palette.bold, palette.brand)
        } else if segment.starts_with('●') || segment.starts_with('✓') {
            palette.ok.to_string()
        } else if segment.starts_with('◈') || segment.starts_with('◇') || segment.starts_with('◆')
        {
            palette.warn.to_string()
        } else if segment.starts_with('⚙') || segment.starts_with('◷') {
            palette.accent.to_string()
        } else if segment.starts_with('✗') {
            palette.error.to_string()
        } else if segment.starts_with("ctx ") {
            context_color(segment, palette).to_string()
        } else {
            dim.to_string()
        };
        out.push_str(&color);
        out.push_str(segment);
        out.push_str(RESET);
    }
    out
}

/// 上下文片段按使用率取色：>=85% 红、>=60% 黄、其余 dim。
fn context_color(segment: &str, palette: &Palette) -> &'static str {
    let Some(percent) = segment
        .find('%')
        .and_then(|index| segment[..index].split_whitespace().last())
        .and_then(|word| word.trim_end_matches('%').parse::<u8>().ok())
    else {
        return palette.dim;
    };
    if percent >= 85 {
        palette.error
    } else if percent >= 60 {
        palette.warn
    } else {
        palette.dim
    }
}

/// 光标落位（1 基）：输入区给的 `(行, 列)` 是正文内 0 基（列含 `❯ ` 前缀的显示宽度），`None`
/// 落在最后一行的行尾。行列都夹在 `1..=rows`/`1..=cols` 里——极小窗口下输入区可能整块被裁掉，
/// 坐标不能跟着越界。
fn cursor(view: &TuiView, rows: &[(Slot, String)]) -> (usize, usize) {
    let max_row = view.size.rows.max(1);
    let max_col = view.size.cols.max(1);
    match view.input.cursor {
        Some((row, col)) => {
            // 从实际版式里找输入正文首行：输入区高度会随内容变化，不能再按固定行数倒推。
            let base = rows
                .iter()
                .position(|(slot, _)| matches!(slot, Slot::Input(0)))
                .unwrap_or(0);
            (
                (base + row + 1).clamp(1, max_row),
                (col + 1).clamp(1, max_col),
            )
        }
        None => {
            let end = rows.last().map_or(0, |(_, text)| {
                display_width(truncate_to_width(text, view.size.cols))
            });
            (max_row, (end + 1).clamp(1, max_col))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ignis_core::config::{OutputMode, Theme};
    use ignis_core::task::PromptStats;
    use ignis_runtime::Usage;
    use ignis_storage::{SessionRecord, SessionSummary};

    use crate::commands::InteractionMode;
    use crate::commands::slash;
    use crate::render::ColorDepth;
    use crate::render::width::{display_width, truncate_to_width};
    use crate::tui::Size;

    use super::{
        AgentState, ContextUsage, InputBlock, PaletteItem, SCROLLBACK_LINES, Scrollback, TuiView,
        ViewFacts, compose, estimate_tokens_from_chars, filtered_palette, format_tokens, meta_line,
        palette_lines, selector_lines, status_line, top_line,
    };

    /// 空闲期的提示词（调用方给的那一条）。
    const IDLE_HINT: &str = "PgUp/PgDn scroll";

    /// 忙碌期的提示词。
    const BUSY_HINT: &str = "Ctrl+C cancels";

    /// 金帧里的结束行（`render::format_task_end` 的 normal 口径）。
    const DONE_LINE: &str = "● Done · 0.1s · 1 tool · task 019a1b2c-3d4e";

    /// `2026-10-15T12:00:00Z` 的 epoch 毫秒（与 slash 侧用例同源）。
    const STARTED_AT: i64 = 1_792_065_600_000;

    // ---- 用例助手 ----

    /// 固定事实快照：顶/底状态行的逐字用例与金帧共用同一份。
    fn facts() -> ViewFacts {
        ViewFacts {
            version: env!("CARGO_PKG_VERSION"),
            workspace: "/tmp/ws".to_string(),
            kernel: "native".to_string(),
            session: "new".to_string(),
            mode: OutputMode::Normal,
            interaction_mode: InteractionMode::Normal,
            theme: Theme::Dark,
            color: false,
            depth: ColorDepth::Basic16,
            budget: 200_000,
            history_messages: 0,
            stats: None,
            usage: None,
            native: true,
            model: "gpt-4o-mini".to_string(),
            provider: "OpenAI".to_string(),
            capabilities: ignis_runtime::caps::ModelCapabilities::static_resolve(
                &ignis_runtime::ModelConfig::default(),
            ),
            context: None,
            tools: Vec::new(),
            skills: Vec::new(),
            memory: None,
        }
    }

    /// 金帧的固定视图（两份尺寸共用）：顶/底状态行取 `top_line`/`status_line` 的逐字输出，
    /// 视口两行，输入区单行 `❯ 你好`（空输入不再留续行/提示行），光标停在文本末尾
    /// （`❯ ` 2 列 + 文本 4 列）。
    fn golden_view(cols: usize, rows: usize) -> TuiView {
        let facts = facts();
        TuiView {
            size: Size { cols, rows },
            color: false,
            depth: ColorDepth::Basic16,
            theme: Theme::Dark,
            top: top_line(&facts),
            meta: meta_line(&facts, AgentState::Ready, 0, None, cols),
            viewport: vec!["答复一".to_string(), DONE_LINE.to_string()],
            input: InputBlock {
                lines: vec!["❯ 你好".to_string()],
                hint: String::new(),
                cursor: Some((0, 6)),
            },
            status: status_line(&facts, IDLE_HINT),
        }
    }

    /// 按帧协议把「若干行内容」拼成整帧字节（金帧用例里用显式行表 + 这一段协议拼装）：
    /// 藏光标 → 归位 → 每行 内容+`\x1b[K`（行间 `\r\n`）→ 绝对定位 → 显光标。
    fn pinned_frame(rows: &[String], cursor: (usize, usize), show: bool) -> String {
        let mut frame = String::from("\x1b[?25l\x1b[H");
        for (index, row) in rows.iter().enumerate() {
            frame.push_str(row);
            frame.push_str("\x1b[K");
            if index + 1 < rows.len() {
                frame.push_str("\r\n");
            }
        }
        frame.push_str(&format!("\x1b[{};{}H", cursor.0, cursor.1));
        if show {
            frame.push_str("\x1b[?25h");
        }
        frame
    }

    /// 帧的骨架（用例解析用）。
    struct Parsed {
        /// 每行内容（已剥掉 `\x1b[K` 与协议头尾）。
        rows: Vec<String>,
        /// 光标绝对定位（1 基）。
        cursor: (usize, usize),
        /// 有没有写「显光标」那一条。
        show: bool,
    }

    /// 按协议**解析**一帧（解析本身顺带断言骨架：头、每行 `\x1b[K`、尾部定位与显光标）。
    fn parse_frame(frame: &str) -> Parsed {
        let body = frame
            .strip_prefix("\x1b[?25l\x1b[H")
            .unwrap_or_else(|| panic!("帧头不对：{frame:?}"));
        let show = body.ends_with("\x1b[?25h");
        let body = body.strip_suffix("\x1b[?25h").unwrap_or(body);
        let start = body
            .rfind("\x1b[")
            .unwrap_or_else(|| panic!("缺少定位序列：{frame:?}"));
        let (head, position) = body.split_at(start);
        assert!(position.ends_with('H'), "定位序列形状不对：{position:?}");
        // 去掉 `\x1b[` 与终结的 `H`
        let mut parts = position[2..position.len() - 1].split(';');
        let row = parts.next().and_then(|text| text.parse().ok());
        let col = parts.next().and_then(|text| text.parse().ok());
        let (Some(row), Some(col)) = (row, col) else {
            panic!("定位序列形状不对：{position:?}");
        };
        assert!(parts.next().is_none(), "定位序列多了参数：{position:?}");

        let mut chunks: Vec<&str> = head.split("\x1b[K").collect();
        assert_eq!(
            chunks.pop(),
            Some(""),
            "帧尾必须停在一条清行序列之后：{frame:?}"
        );
        let rows: Vec<String> = chunks
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| match index {
                0 => chunk.to_string(),
                // 行间分隔：`split` 把 `\r\n` 留在了后一段的开头
                _ => chunk
                    .strip_prefix("\r\n")
                    .unwrap_or_else(|| panic!("行间缺少 `\\r\\n`：{chunk:?}"))
                    .to_string(),
            })
            .collect();
        Parsed {
            rows,
            cursor: (row, col),
            show,
        }
    }

    /// 剥掉所有 CSI 序列（用例里量可见宽度/内容；帧里的转义序列都是 CSI）。
    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            if chars.peek() == Some(&'[') {
                chars.next();
                for end in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&end) {
                        break;
                    }
                }
            }
        }
        out
    }

    /// 帧里的 SGR 序列（`\x1b[` + 参数 + `m`）：`color=false` 时必须一条都没有。
    fn sgr_sequences(text: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find('\u{1b}') {
            let tail = &rest[start + 1..];
            let Some(body) = tail.strip_prefix('[') else {
                rest = tail;
                continue;
            };
            let end = body
                .find(|c: char| ('\u{40}'..='\u{7e}').contains(&c))
                .unwrap_or(body.len());
            if body[end..].starts_with('m') {
                found.push(format!("\u{1b}[{}m", &body[..end]));
            }
            rest = &body[end..];
        }
        found
    }

    /// 一条会话行（`/sessions` 读的同一形状）。
    fn summary(id: &str, title: &str, updated_at: i64, message_count: usize) -> SessionSummary {
        SessionSummary {
            session: SessionRecord {
                id: id.to_string(),
                title: title.to_string(),
                workspace: "/tmp/ws".into(),
                kernel: "native".into(),
                model: None,
                created_at: updated_at,
                updated_at,
            },
            message_count,
        }
    }

    // ---- 帧协议 ----

    /// 5 行的极小视图逐字节钉死整帧（协议本身的金帧，不借任何拼帧助手）。
    #[test]
    fn frame_protocol_is_pinned_at_five_rows() {
        let view = TuiView {
            size: Size { cols: 10, rows: 5 },
            color: false,
            depth: ColorDepth::Basic16,
            theme: Theme::Dark,
            top: "T".to_string(),
            meta: "M".to_string(),
            viewport: Vec::new(),
            input: InputBlock {
                lines: vec!["❯ a".to_string(), "… b".to_string()],
                hint: "hint".to_string(),
                cursor: Some((1, 0)),
            },
            status: "S".to_string(),
        };
        // 输入区首行在 5 行版式里是第 2 行（0 基 1），它的第 2 行（0 基 1）就是第 3 行、列 1
        assert_eq!(
            compose(&view),
            "\x1b[?25l\x1b[H\
             T\x1b[K\r\n\
             ❯ a\x1b[K\r\n\
             … b\x1b[K\r\n\
             hint\x1b[K\r\n\
             S\x1b[K\
             \x1b[3;1H\
             \x1b[?25h"
        );

        // 藏光标（`cursor = None`）：落到最后一行的行尾，且**不写**显光标那一条
        let hidden = TuiView {
            input: InputBlock {
                lines: vec!["❯ a".to_string(), "… b".to_string()],
                hint: "hint".to_string(),
                cursor: None,
            },
            ..view
        };
        assert_eq!(
            compose(&hidden),
            "\x1b[?25l\x1b[H\
             T\x1b[K\r\n\
             ❯ a\x1b[K\r\n\
             … b\x1b[K\r\n\
             hint\x1b[K\r\n\
             S\x1b[K\
             \x1b[5;2H"
        );
    }

    /// 命令面板：过滤大小写不敏感、选中行带 `> `、空结果有明确说明。
    #[test]
    fn palette_filters_and_marks_the_selected_action() {
        let items = vec![
            PaletteItem {
                label: "/status — Show status".into(),
                insert: "/status".into(),
            },
            PaletteItem {
                label: "/brainstorming — skill".into(),
                insert: "/brainstorming ".into(),
            },
        ];
        let lines = palette_lines(&items, "BRAIN", 0, 10);
        assert!(lines[0].starts_with("Command Palette"), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("> /brainstorming")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("/status")),
            "{lines:?}"
        );

        let empty = palette_lines(&items, "zzz", 0, 10);
        assert!(
            empty.iter().any(|line| line.contains("no matching")),
            "{empty:?}"
        );
        assert_eq!(filtered_palette(&items, "").len(), 2);
    }

    /// 运行状态行必须紧贴输入区上方：顶行 → 内容 → 分隔线 → 运行状态 → 输入 → 分隔线 → 底行。
    #[test]
    fn meta_row_sits_immediately_above_the_input() {
        let view = golden_view(80, 24);
        let parsed = parse_frame(&compose(&view));
        let rule = "─".repeat(80);

        assert_eq!(parsed.rows[0], view.top, "顶行仍在最上面");
        assert_eq!(parsed.rows[1], "答复一", "内容紧接顶行（不再被状态行隔开）");
        assert_eq!(parsed.rows[19], rule, "输入区上方的分隔线");
        assert_eq!(parsed.rows[20], view.meta, "运行状态行紧贴分隔线");
        assert_eq!(parsed.rows[21], view.input.lines[0], "输入区紧随其后");
        assert_eq!(parsed.rows[22], rule, "输入区下方的分隔线");
        assert_eq!(parsed.rows[23], view.status, "底行仍在最后");
    }

    /// 空输入只占一行：`❯` 之后应直接是输入区下方的分隔线，而不是空白续行/提示行。
    #[test]
    fn empty_input_does_not_leave_blank_rows_below_the_prompt() {
        let view = golden_view(80, 24);
        assert_eq!(view.input.height(), 1, "单行输入只占一行");
        let parsed = parse_frame(&compose(&view));
        let input = parsed
            .rows
            .iter()
            .position(|row| row == &view.input.lines[0])
            .expect("输入首行必须在帧里");
        assert_eq!(
            parsed.rows[input + 1],
            "─".repeat(80),
            "空输入后应直接是分隔线，而不是空白续行/提示行"
        );
    }

    /// 80×24 的金帧：顶行 / 视口 18 行 / 分隔线 / 运行状态行 / 输入 1 行 / 分隔线 / 底行。
    #[test]
    fn golden_frame_at_80x24() {
        let view = golden_view(80, 24);
        let rule = "─".repeat(80);
        let mut rows = vec![
            view.top.clone(),
            "答复一".to_string(),
            DONE_LINE.to_string(),
        ];
        // 内容区 18 行：2 行内容 + 16 行补空
        rows.extend(std::iter::repeat_n(String::new(), 18 - 2));
        rows.push(rule.clone());
        rows.push(view.meta.clone());
        rows.extend(view.input.lines.iter().cloned());
        rows.push(rule);
        rows.push(view.status.clone());
        assert_eq!(rows.len(), view.size.rows);

        // 光标：输入区首行是第 22 行（1 基），文本末尾在第 7 列（`❯ ` 2 列 + 文本 4 列 + 1）
        let expected = pinned_frame(&rows, (22, 7), true);
        assert_eq!(compose(&view), expected);

        // 不变量：行数 == rows、每行显示宽度 ≤ cols、屏内换行只有 `\r\n`
        assert_eq!(expected.matches("\x1b[K").count(), view.size.rows);
        assert_eq!(expected.matches('\n').count(), view.size.rows - 1);
        for row in &rows {
            assert!(display_width(row) <= view.size.cols, "{row:?}");
        }
        assert!(
            view.status.len() < 80,
            "底行在 80 列下必须完整：{}",
            view.status
        );
    }

    /// 40×12 的金帧：内容区 6 行（2 行补空），顶/运行/底行与超宽的视口行都被截断。
    #[test]
    fn golden_frame_at_40x12() {
        let view = golden_view(40, 12);
        let rule = "─".repeat(40);
        let mut rows = vec![
            truncate_to_width(&view.top, 40).to_string(),
            "答复一".to_string(),
            truncate_to_width(DONE_LINE, 40).to_string(),
        ];
        rows.extend(std::iter::repeat_n(String::new(), 6 - 2));
        rows.push(rule.clone());
        rows.push(truncate_to_width(&view.meta, 40).to_string());
        rows.extend(view.input.lines.iter().cloned());
        rows.push(rule);
        rows.push(truncate_to_width(&view.status, 40).to_string());
        assert_eq!(rows.len(), view.size.rows);

        // 光标：输入区首行是第 10 行（1 基），文本末尾在第 7 列
        let expected = pinned_frame(&rows, (10, 7), true);
        assert_eq!(compose(&view), expected);

        assert_eq!(expected.matches("\x1b[K").count(), view.size.rows);
        assert_eq!(expected.matches('\n').count(), view.size.rows - 1);
        for row in &rows {
            assert!(display_width(row) <= view.size.cols, "{row:?}");
        }
        // 截断逐字：被切掉的是各自的尾巴
        assert!(view.top.starts_with(&rows[0]), "顶行必须是被切掉尾巴的前缀");
        assert_eq!(
            truncate_to_width(&view.status, 40),
            rows[11].as_str(),
            "底行 40 列下的截断口径"
        );
    }

    /// 全尺寸表（列 0..=6 × 行 0..=9）：不 panic、恰好 `rows` 行、光标落在 `1..=rows`/`1..=cols`。
    #[test]
    fn compose_is_total_at_any_size() {
        for cols in 0..=6 {
            for rows in 0..=9 {
                let view = TuiView {
                    size: Size { cols, rows },
                    // 上色路径一起过：SGR 也会被挤进被截断的行
                    color: true,
                    depth: ColorDepth::Basic16,
                    theme: Theme::Light,
                    top: "IGNIS 很长的顶部状态行".to_string(),
                    meta: "IGNIS 很长的运行状态行".to_string(),
                    viewport: vec!["你".repeat(20), "ab".to_string(), String::new()],
                    input: InputBlock {
                        lines: vec!["❯ 你好世界".to_string(), "… 续行".to_string()],
                        hint: "hint".to_string(),
                        cursor: Some((1, 3)),
                    },
                    status: "ctx 0/200000 chars · last - · mode normal".to_string(),
                };
                let frame = compose(&view);
                let parsed = parse_frame(&frame);
                let what = format!("{cols}x{rows}");
                assert_eq!(parsed.rows.len(), rows, "{what}：帧必须恰好 rows 行");
                assert_eq!(
                    parsed.show,
                    view.input.cursor.is_some(),
                    "{what}：有光标才写显光标那一条"
                );
                let (row, col) = parsed.cursor;
                assert!(
                    (1..=rows.max(1)).contains(&row),
                    "{what}：光标行 {row} 越界"
                );
                assert!(
                    (1..=cols.max(1)).contains(&col),
                    "{what}：光标列 {col} 越界"
                );
                for line in &parsed.rows {
                    assert!(
                        display_width(&plain(line)) <= cols,
                        "{what}：行宽超限 {line:?}"
                    );
                    assert!(!line.contains('\n'), "{what}：行内容里不许有换行");
                }
            }
        }
    }

    /// 100 字符（200 列）的一行 @ 40 列：每行 ≤ 40 且宽字符只按整格切。
    #[test]
    fn compose_truncates_to_the_width_and_never_emits_a_longer_line() {
        let long = "你".repeat(100);
        let view = TuiView {
            size: Size { cols: 40, rows: 6 },
            color: false,
            depth: ColorDepth::Basic16,
            theme: Theme::Dark,
            top: long.clone(),
            meta: long.clone(),
            viewport: vec![long.clone()],
            input: InputBlock {
                lines: vec![long.clone()],
                hint: String::new(),
                // 列坐标远超面板宽度：必须被夹在 1..=cols
                cursor: Some((0, 199)),
            },
            status: long.clone(),
        };
        let parsed = parse_frame(&compose(&view));
        assert_eq!(parsed.rows.len(), 6);
        for row in &parsed.rows {
            assert!(display_width(row) <= 40, "行宽超限：{row:?}");
            assert!(long.starts_with(row.as_str()), "截断必须是前缀：{row:?}");
        }
        // 40 列正好 20 个宽字符：第 21 个放不下，停在它前面（宁少一列，不切半格）
        assert_eq!(parsed.rows[0], "你".repeat(20));
        assert_eq!(display_width(&parsed.rows[0]), 40);
        assert_eq!(parsed.cursor, (5, 40), "光标列被夹在 1..=cols");

        // 奇数宽度：41 列下只能放 20 个宽字符（40 列），绝不切半格
        let odd = TuiView {
            size: Size { cols: 41, rows: 6 },
            ..view
        };
        let parsed = parse_frame(&compose(&odd));
        assert_eq!(parsed.rows[0], "你".repeat(20));
        assert_eq!(
            display_width(&parsed.rows[0]),
            40,
            "宽字符放不下就停在它前面"
        );
    }

    // ---- 颜色 ----

    /// `color=false`：帧里一条 SGR 都没有（定位与清行不是颜色，照常写）。
    #[test]
    fn compose_without_color_emits_no_sgr() {
        let mut view = golden_view(80, 24);
        view.color = false;
        view.input.cursor = None;
        let frame = compose(&view);
        assert!(sgr_sequences(&frame).is_empty(), "{frame:?}");
        // 定位与清行照旧；藏光标时定位落在最后一行的行尾
        assert_eq!(frame.matches("\x1b[K").count(), 24, "{frame:?}");
        assert!(
            frame.contains(&format!("\x1b[24;{}H", display_width(&view.status) + 1)),
            "{frame:?}"
        );
        assert!(!frame.contains("\x1b[?25h"), "{frame:?}");
    }

    /// `color=false` 时内容里的 SGR 也一并净化掉：无色帧不补行尾重置，`\x1b[8m`（隐藏）/`\x1b[7m`
    /// （反显）这类属性一旦进帧就会跨 `\r\n` 刷到后面的视口行、输入区与两条状态行——所以无色帧
    /// 的内容行里一个 ESC 都不许有（帧协议自己的定位/清行/光标序列不算内容）。
    #[test]
    fn compose_without_color_strips_captured_sgr() {
        let mut view = golden_view(80, 24);
        view.color = false;
        view.input.cursor = None;
        // 视口塞进隐藏与反显（`capture::sanitize` 的放行面认得它们）：文本留下，序列一条也不许进帧。
        view.viewport = vec![
            "\u{1b}[8mhidden".to_string(),
            "\u{1b}[7mreverse\u{1b}[0m".to_string(),
        ];
        let frame = compose(&view);
        assert!(!frame.contains("\u{1b}[8m"), "{frame:?}");
        assert!(!frame.contains("\u{1b}[7m"), "{frame:?}");
        assert!(
            sgr_sequences(&frame).is_empty(),
            "无色帧里一条 SGR 都不许有：{frame:?}"
        );

        // 视口两行落在 rows[1..=2]（顶行之后、输入区分隔线之前）：序列去掉、文本逐字留下。
        let parsed = parse_frame(&frame);
        assert_eq!(parsed.rows[1], "hidden", "隐藏属性丢掉、文本逐字留下");
        assert_eq!(parsed.rows[2], "reverse", "反显属性丢掉、文本逐字留下");
        for row in &parsed.rows[1..=2] {
            assert!(!row.contains('\u{1b}'), "{row:?} 里漏进了 SGR");
        }

        // 上色档口径不变：同样的内容在 `color=true` 下原样穿过（放行面只在无色档收紧）。
        view.color = true;
        let colored = compose(&view);
        assert!(colored.contains("\u{1b}[8mhidden"), "{colored:?}");
        assert!(colored.contains("\u{1b}[7mreverse\u{1b}[0m"), "{colored:?}");
    }

    /// `color=true`：chrome 用调色板——顶行的字标（首个 ` · ` 之前、以 `IGNIS v` 开头的那一段）
    /// 与输入区首行标记 `❯ ` 走 `bold + brand`，顶/底状态行与提示行 dim，每行以重置收尾。
    #[test]
    fn compose_with_color_uses_the_palette() {
        let mut view = golden_view(80, 24);
        view.color = true;
        view.input.lines.push("… 续行".to_string());
        view.input.hint = "hint".to_string();
        let frame = compose(&view);

        // 顶行：`{bold}{brand}IGNIS v…{RESET}{dim} · 其余`（basic 16 档的 brand 槽落黄 `33m`）
        assert!(
            frame.contains(&format!(
                "\x1b[1m\x1b[33mIGNIS v{}\x1b[0m\x1b[2m · /tmp/ws · kernel native · session new\
                 \x1b[0m\x1b[K",
                env!("CARGO_PKG_VERSION")
            )),
            "{frame:?}"
        );
        // 底行与提示行 dim（dark 的 dim 是 `\x1b[2m`）
        assert!(
            frame.contains(&format!("\x1b[2m{}\x1b[0m\x1b[K", view.status)),
            "{frame:?}"
        );
        assert!(frame.contains("\x1b[2mhint\x1b[0m\x1b[K"), "{frame:?}");
        // 输入区首行：标记 bold + 品牌色、标记之后的正文不着色；续行既不 dim 也不 bold
        assert!(
            frame.contains("\x1b[1m\x1b[33m❯ \x1b[0m你好\x1b[0m\x1b[K"),
            "{frame:?}"
        );
        assert!(frame.contains("\r\n… 续行\x1b[0m\x1b[K"), "{frame:?}");
        // 每行都以「重置 + 清行」收尾（被截断的行不会把颜色漏到下一行）
        assert_eq!(frame.matches("\x1b[0m\x1b[K").count(), 24, "{frame:?}");
        assert!(!sgr_sequences(&frame).is_empty());

        // 品牌槽跟着 `view.depth`：真彩档的字标换成品牌橙的 RGB 槽
        let mut truecolor = view.clone();
        truecolor.depth = ColorDepth::True;
        let true_frame = compose(&truecolor);
        assert!(
            true_frame.contains(&format!(
                "\x1b[1m\x1b[38;2;247;131;64mIGNIS v{}",
                env!("CARGO_PKG_VERSION")
            )),
            "{true_frame:?}"
        );

        // 顶行不以 `IGNIS v` 开头的头部（含根本没有 ` · ` 分隔符的行）：整行 dim，口径不变
        for top in [
            "T · x".to_string(),
            format!("IGNIS v{}", env!("CARGO_PKG_VERSION")),
        ] {
            let mut plain_top = view.clone();
            plain_top.top = top.clone();
            assert!(
                compose(&plain_top).contains(&format!("\x1b[2m{top}\x1b[0m\x1b[K")),
                "{top:?}"
            );
        }

        // light 主题：dim 换亮灰（浅色背景上 `2m` 几乎看不见），字号里那两段照旧上色
        let mut light = view.clone();
        light.theme = Theme::Light;
        let light_frame = compose(&light);
        assert!(light_frame.contains('\u{1b}'), "上色帧必须有 SGR");
        assert!(!light_frame.contains("\x1b[2m"), "{light_frame:?}");
        let dim = crate::render::palette(Theme::Light, light.depth).dim;
        assert!(
            light_frame.contains(&format!(
                "\x1b[1m\x1b[33mIGNIS v{}\x1b[0m{dim} · /tmp/ws · kernel native · session new",
                env!("CARGO_PKG_VERSION")
            )),
            "{light_frame:?}"
        );
    }

    // ---- 状态行 ----

    /// 顶/底状态行的逐字口径：usage 三态、三档模式、两种 hint。
    #[test]
    fn top_line_and_status_line_are_pinned() {
        let facts = facts();
        assert_eq!(
            top_line(&facts),
            format!(
                "IGNIS v{} · /tmp/ws · kernel native · session new",
                env!("CARGO_PKG_VERSION")
            )
        );

        // native + 还没跑过任务：`last -`；上下文已移到输入区上方的 meta 行
        assert_eq!(
            status_line(&facts, IDLE_HINT),
            "mode Normal · output normal · last - · PgUp/PgDn scroll"
        );
        // native + 有值
        let mut used = facts.clone();
        used.usage = Some(Usage {
            input_tokens: 1234,
            output_tokens: 567,
            estimated: false,
        });
        assert_eq!(
            status_line(&used, BUSY_HINT),
            "mode Normal · output normal · last in=1234 out=567 · Ctrl+C cancels"
        );
        // 外置内核：usage 一律 `n/a`（有值、没值都不展示数字）
        let mut external = used.clone();
        external.native = false;
        external.mode = OutputMode::Debug;
        assert_eq!(
            status_line(&external, BUSY_HINT),
            "mode Normal · output debug · last n/a · Ctrl+C cancels"
        );
        let mut external_empty = facts.clone();
        external_empty.native = false;
        assert_eq!(
            status_line(&external_empty, IDLE_HINT),
            "mode Normal · output normal · last n/a · PgUp/PgDn scroll"
        );

        // 模式字面量与 `ui.output_mode` 的 serde 名一致；char 预算只留在 `/context`
        let mut measured = facts.clone();
        measured.stats = Some(PromptStats {
            facts_injected: 2,
            skills_listed: 3,
            system_chars: 100,
            history_chars: 4096,
        });
        measured.budget = 100_000;
        measured.mode = OutputMode::Minimal;
        assert_eq!(
            status_line(&measured, IDLE_HINT),
            "mode Normal · output minimal · last - · PgUp/PgDn scroll"
        );

        // 运行状态行：模型 / 上下文 / 状态 / 耗时；宽度分档。
        assert_eq!(
            meta_line(&facts, AgentState::Ready, 0, None, 120),
            "✦ gpt-4o-mini │ ctx -/? [----------] -- │ ● Ready"
        );
        let mut measured = facts.clone();
        measured.context = Some(ContextUsage::new(12_500, false, Some(128_000)));
        assert_eq!(
            meta_line(
                &measured,
                AgentState::Thinking,
                0,
                Some(Duration::from_millis(4200)),
                120
            ),
            "✦ gpt-4o-mini │ ctx 12.5K/128K [█░░░░░░░░░] 10% │ ◈ Thinking │ 4.2s"
        );
        assert_eq!(
            meta_line(&measured, AgentState::Thinking, 1, None, 70),
            "ctx 12.5K/128K [█░░░░░░░░░] 10% │ ◇ Thinking"
        );
        assert_eq!(
            meta_line(&measured, AgentState::Waiting, 0, None, 70),
            "ctx 12.5K/128K [█░░░░░░░░░] 10% │ ◇ Waiting"
        );
        assert_eq!(
            meta_line(&measured, AgentState::Scheduled, 0, None, 70),
            "ctx 12.5K/128K [█░░░░░░░░░] 10% │ ◷ Scheduled"
        );
        let mut plan = measured.clone();
        plan.interaction_mode = InteractionMode::Plan;
        assert_eq!(
            meta_line(&plan, AgentState::Thinking, 0, None, 120),
            "✦ gpt-4o-mini │ ctx 12.5K/128K [█░░░░░░░░░] 10% │ Plan │ ◈ Thinking"
        );
        plan.interaction_mode = InteractionMode::Auto;
        assert_eq!(
            status_line(&plan, IDLE_HINT),
            "mode Auto · output normal · last - · PgUp/PgDn scroll"
        );
        assert_eq!(
            meta_line(&measured, AgentState::Failed, 0, None, 50),
            "✗ Failed │ ctx 12.5K/128K"
        );
        // 估算值必须带 `≈`；窗口未知时百分比 `--`、进度条全空。
        let estimated = ContextUsage::new(12_500, true, None);
        assert_eq!(estimated.segment(10), "ctx ≈12.5K/? [----------] --");
        assert_eq!(estimated.percent_word(), "--");
        // 窗口已知、用量未知：显示窗口但不把 0 当真实占用。
        let no_usage = ContextUsage::unknown(Some(128_000));
        assert_eq!(no_usage.segment(10), "ctx -/128K [----------] --");
        assert_eq!(no_usage.percent_word(), "--");
    }

    /// token 短写与无 Usage 时的估算口径。
    #[test]
    fn token_words_and_estimates_are_pinned() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1K");
        assert_eq!(format_tokens(12_500), "12.5K");
        assert_eq!(format_tokens(128_000), "128K");
        assert_eq!(format_tokens(1_048_576), "1M");
        // 0.4 token/char 的保守估算；估算值必须由调用方标 `≈`。
        assert_eq!(estimate_tokens_from_chars(0), 0);
        assert_eq!(estimate_tokens_from_chars(1), 1);
        assert_eq!(estimate_tokens_from_chars(10), 4);
        assert_eq!(estimate_tokens_from_chars(12_000), 4_800);
    }

    // ---- 滚动区 ----

    /// 上限 2000 行：推 2005 行只剩最后 2000 行，第一行是第 6 行。
    #[test]
    fn scrollback_caps_at_two_thousand_lines_and_keeps_the_newest() {
        assert_eq!(SCROLLBACK_LINES, 2000, "上限逐字钉住");
        let mut scrollback = Scrollback::new();
        assert!(scrollback.is_empty());
        for index in 0..SCROLLBACK_LINES + 5 {
            scrollback.push_text(&format!("line {index}\n"));
        }
        assert_eq!(scrollback.len(), SCROLLBACK_LINES);

        let viewport = scrollback.viewport(SCROLLBACK_LINES);
        assert_eq!(viewport.len(), SCROLLBACK_LINES);
        assert_eq!(viewport[0], "line 5", "最旧的 5 行被丢掉");
        assert_eq!(
            viewport[SCROLLBACK_LINES - 1],
            format!("line {}", SCROLLBACK_LINES + 4)
        );
    }

    /// 没有换行的尾巴留在 `pending`：下一次摄入或 `flush_pending` 才落地。
    #[test]
    fn scrollback_keeps_the_unterminated_tail_until_the_next_newline() {
        let mut scrollback = Scrollback::new();
        scrollback.push_text("a\nb");
        assert_eq!(scrollback.len(), 1);
        assert_eq!(scrollback.viewport(5), ["a"]);

        // 下一次摄入的换行把尾巴落地
        scrollback.push_text("\n");
        assert_eq!(scrollback.len(), 2);
        assert_eq!(scrollback.viewport(5), ["a", "b"]);

        // 跨 tick 断在行中的两段拼成同一行
        scrollback.push_text("续");
        scrollback.push_text("完\n");
        assert_eq!(scrollback.viewport(5), ["a", "b", "续完"]);

        // 没有换行的尾巴要显式收尾（任务结束）
        scrollback.push_text("没有换行的尾巴");
        assert_eq!(scrollback.len(), 3, "尾巴还在 pending 里");
        scrollback.flush_pending();
        assert_eq!(scrollback.len(), 4);
        assert_eq!(
            scrollback.viewport(5).last().map(String::as_str),
            Some("没有换行的尾巴")
        );
        // 空 pending 的收尾不再产出行
        scrollback.flush_pending();
        assert_eq!(scrollback.len(), 4);
        // 空行也是行（正文里的空行要看得见）
        scrollback.push_text("\n\n");
        assert_eq!(scrollback.viewport(3), ["没有换行的尾巴", "", ""]);
    }

    /// T3 裁定：摄入时 `\t` 按 8 列制表位展开（**仅显示层**；提交文本与渲染字节不变）。
    ///
    /// 终端对制表符用的是它自己的制表位，宽度口径（[`display_width`]）却控制字符按 0 列算：
    /// 不展开的话含 Tab 的行在截断与列对齐上都会算错。展开在「一行落地」时做，列号按显示宽度
    /// 累计（宽字符占 2 列），所以半行跨 tick 也不影响结果。
    #[test]
    fn tabs_expand_to_the_next_eight_column_stop_on_ingest() {
        let mut scrollback = Scrollback::new();
        // `a` 在第 1 列 → 补 7 个空格到第 8 列
        scrollback.push_text("a\tb\n");
        assert_eq!(scrollback.viewport(1), [format!("a{}b", " ".repeat(7))]);
        assert_eq!(
            display_width(&scrollback.viewport(1)[0]),
            9,
            "展开之后宽度就是真实列数"
        );

        // 宽字符按显示列算：`你` 占 2 列 → 补 6 个空格
        scrollback.push_text("你\tb\n");
        assert_eq!(scrollback.viewport(2)[1], format!("你{}b", " ".repeat(6)));

        // 行首的 Tab、正好落在制表位上的 Tab、连续两个 Tab
        scrollback.push_text("\tb\n");
        assert_eq!(scrollback.viewport(3)[2], format!("{}b", " ".repeat(8)));
        scrollback.push_text("12345678\tb\n");
        assert_eq!(
            scrollback.viewport(4)[3],
            format!("12345678{}b", " ".repeat(8))
        );
        scrollback.push_text("a\t\tb\n");
        assert_eq!(
            scrollback.viewport(5)[4],
            format!("a{}b", " ".repeat(7 + 8))
        );
        assert!(
            !scrollback
                .viewport(5)
                .iter()
                .any(|line| line.contains('\t')),
            "落地的行里不该再有制表符：{:?}",
            scrollback.viewport(5)
        );

        // 半行（跨 tick 的两段拼起来）同样在落地时展开
        scrollback.push_text("x\ty");
        assert_eq!(scrollback.len(), 5, "还没换行：尾巴留在 pending 里");
        scrollback.push_text("\n");
        assert_eq!(scrollback.viewport(6)[5], format!("x{}y", " ".repeat(7)));
    }

    /// 翻页：`scroll_up` 的 offset 与视口底部；到底不越界；`scroll_to_bottom` 回 0。
    #[test]
    fn scrollback_scrolls_by_page_and_end_returns_to_the_bottom() {
        let mut scrollback = Scrollback::new();
        for index in 1..=100 {
            scrollback.push_text(&format!("line {index}\n"));
        }
        assert_eq!(scrollback.viewport(3), ["line 98", "line 99", "line 100"]);

        scrollback.scroll_up(5);
        assert_eq!(scrollback.offset(), 5);
        assert_eq!(scrollback.viewport(3), ["line 93", "line 94", "line 95"]);

        // 到底（最旧一行贴顶）：offset 停在 len-1，视口只剩最旧那一行
        scrollback.scroll_up(usize::MAX);
        assert_eq!(scrollback.offset(), 99);
        assert_eq!(scrollback.viewport(3), ["line 1"]);

        // 往下翻：到 0 即贴底，越界不 panic
        scrollback.scroll_down(4);
        assert_eq!(scrollback.offset(), 95);
        scrollback.scroll_down(usize::MAX);
        assert_eq!(scrollback.offset(), 0);
        assert_eq!(scrollback.viewport(2), ["line 99", "line 100"]);
        scrollback.scroll_down(1);
        assert_eq!(scrollback.offset(), 0);

        // End 回到底部
        scrollback.scroll_up(7);
        assert_eq!(scrollback.offset(), 7);
        scrollback.scroll_to_bottom();
        assert_eq!(scrollback.offset(), 0);
        assert_eq!(scrollback.viewport(1), ["line 100"]);

        // 视口 count 超过行数 / 为 0 都不越界；空滚动区怎么滚都不动
        assert_eq!(scrollback.viewport(500).len(), 100);
        assert!(scrollback.viewport(0).is_empty());
        let mut empty = Scrollback::new();
        empty.scroll_up(3);
        assert_eq!(empty.offset(), 0);
        assert!(empty.viewport(3).is_empty());
        empty.scroll_down(3);
        empty.scroll_to_bottom();
        empty.flush_pending();
        assert!(empty.is_empty());
    }

    // ---- 选择器 ----

    /// 选择器：复用 `/sessions` 的表格语义，只在行首加选择前缀（表头也带前缀）。
    #[test]
    fn selector_lines_prefix_the_cursor_row() {
        let newer = "019a1b2c-2222-7f00-8000-000000000002";
        let older = "019a1b2c-1111-7f00-8000-000000000001";
        let rows = [
            summary(newer, "修复配置路径", STARTED_AT + 1_000, 4),
            summary(older, "你好", STARTED_AT, 2),
        ];
        let current = Some(older);
        let table = slash::sessions_lines(&rows, current);

        let lines = selector_lines(&rows, current, 1);
        assert_eq!(lines.len(), table.len());
        // 表头也带前缀：未选中行两个空格，选中行 `> `
        assert_eq!(lines[0], format!("  {}", table[0]));
        assert!(lines[0].starts_with("  ID"), "{:?}", lines[0]);
        assert_eq!(lines[1], format!("  {}", table[1]));
        assert_eq!(lines[2], format!("> {}", table[2]));
        // 当前会话的 `*` 与短 id 原样保留（只是整体右移两列）
        assert!(
            lines[2].contains(&format!("*{}", &older[..13])),
            "{:?}",
            lines[2]
        );
        assert!(lines[1].contains(&newer[..13]), "{:?}", lines[1]);

        // 光标在第 0 行、没有当前会话时同样只动前缀（表宽跟着 `*` 变，得用同一份表对照）
        let plain_table = slash::sessions_lines(&rows, None);
        let first = selector_lines(&rows, None, 0);
        assert_eq!(first[1], format!("> {}", plain_table[1]));
        assert_eq!(first[2], format!("  {}", plain_table[2]));
        assert!(!first.iter().any(|line| line.contains('*')), "{first:?}");

        // 空表回 `(no sessions)`（没有可选项，不挂前缀）
        assert_eq!(selector_lines(&[], None, 0), ["(no sessions)"]);
    }
}
