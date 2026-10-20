//! 流式渲染：把内核事件写成终端可读的输出。
//!
//! 三个聚焦文件分工：`width.rs` 管显示宽度与折行，`markdown.rs` 管「哪些字符是什么样式」
//! （[`MdStream`]），本模块把两者装配成 [`Renderer`]——模式（minimal/normal/debug）、主题调色板
//! （含 [`ColorDepth`] 色阶探测与品牌档）、工具耗时配对、结束行与短 id 都在这里。
//!
//! **写序纪律**：正文片段即时写出；工具行、推理块首与 [`Renderer::finish`] 一律从列 0 开始
//! （写之前先把挂起的行收束）。
//!
//! **折行纪律**：`width == usize::MAX`（非 tty、管道、e2e）不插入物理换行，保持每行可 grep；
//! tty 下只对**前缀类块**（列表/引用/围栏正文）按物理行折行并保持续行对齐，普通段落交给终端
//! 软换行。折行的唯一口径在 `width.rs`，Renderer 只负责把「一条物理行的分块样式」按折行结果
//! 重新落位。
//!
//! `saw_schedule_create` 是「本轮出现过 `schedule_create`」的信号，命令层在任务结束后读它决定
//! 要不要提示自拉起。

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ignis_core::config::{ColorChoice, OutputMode, Theme};
use ignis_runtime::{RunStatus, RuntimeEvent, Usage};
use serde_json::Value;

use self::markdown::{MdStream, Segment, Style};
use self::width::wrap;

// 宽度/折行模块（Task 6 落地，本模块消费）。声明与入口一律 `pub`：本模块之外还有明确消费者
// ——P2 的输入编辑器按显示宽度定位光标（spec §6.4），P5 的 TUI 由面板给出宽度、复用同一套
// 口径（spec §6.10），收紧可见性会挡住这两处复用。
pub mod width;

// Markdown 子集与流式装配（Task 6 落地，本模块消费）。同样一律 `pub`：P5 的 TUI 复用同一套
// markdown/width（spec §6.10 的复用要求，不复制第二套逻辑）。
pub mod markdown;

/// ANSI：重置。每段着色自带收尾，颜色不会漏到下一段。
const RESET: &str = "\x1b[0m";

/// tty 宽度探测失败（或终端报 0 列）时的回退列数。
const DEFAULT_WIDTH: usize = 80;

/// 工具失败输出（debug 下成功输出也走这条）最多展示的字符数。
const ERROR_PREVIEW_CHARS: usize = 200;

/// `remember` 确认行里事实文本最多展示的字符数。
const MEMORY_TEXT_CHARS: usize = 60;

/// 短 id 的长度：`task list`、结束行与 `task show` 的候选名单共用同一口径。
const SHORT_ID_CHARS: usize = 13;

/// 建过调度任务的标志所认的元工具名：任务结束后据此决定要不要提示自拉起。
const SCHEDULE_CREATE: &str = "schedule_create";

/// `remember` 元工具名：它的成功结果固定打一行确认（决策 #22）。
const REMEMBER: &str = "remember";

/// 推理块的行首标记（normal，以及 minimal 的摘要行）。
const REASONING_PREFIX: &str = "· ";

/// debug 模式下的推理块标记：带上通道名，与正文同屏也不混淆。
const REASONING_PREFIX_DEBUG: &str = "[reasoning] ";

/// 渲染风味：经典 REPL 逐字节不变；TUI 才启用新的工具行/结束行字形。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RenderFlavor {
    /// 经典 REPL / 管道 / e2e：`→` / `  ✓` / `● Done`（既有字节契约）。
    #[default]
    Classic,
    /// TUI：`⚙` / `✓` / `✗` 与 `✓ Done` 摘要。
    Tui,
}

/// 渲染选项：命令层按 `config.ui` + stdout 的 tty 状态装配。
///
/// `width == usize::MAX` 表示不插入物理换行（非 tty / 管道 / e2e），其余取值是每行的目标列数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendererOptions {
    /// 输出模式。
    pub mode: OutputMode,
    /// 是否输出 ANSI 颜色（由 [`resolve_color`] 定）。
    pub color: bool,
    /// 配色主题。
    pub theme: Theme,
    /// 终端色阶（由 [`color_depth`] 按终端现场定）：调色板的品牌档按它取值。
    pub depth: ColorDepth,
    /// 每行目标显示宽度；`usize::MAX` 表示不折行。
    pub width: usize,
}

/// 终端色阶：启动时（或每轮任务前）定一次，决定调色板里的品牌档取哪一级 ANSI。
///
/// 判定见 [`color_depth`]：探测错不致命，降级方向永远朝更保守（真彩 → 256 → 基本色 → 无色）
/// ——误判只影响观感，不影响功能。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    /// 真彩（`COLORTERM=truecolor|24bit`）：`38;2;r;g;b`。
    True,
    /// 256 色（`TERM` 含 `256color`）：`38;5;n`。
    Ansi256,
    /// 基本 16 色（既没有真彩也没有 256 色的现场）：用 `30..37` 这一档。
    Basic16,
    /// 不上色（`resolve_color` 的结论为假）：调用方按它门控，一个 SGR 都不写。
    None,
}

/// 色阶探测：`color` 为假恒 [`ColorDepth::None`]（`NO_COLOR` 非空 / `ui.color=never` / 非 tty
/// 都已在 [`resolve_color`] 里收敛成「不上色」，这里不再重判）；否则 `COLORTERM` 为
/// `truecolor`/`24bit`（大小写不敏感）给真彩，`TERM` 含 `256color` 给 256 色，其余（含两个都缺）
/// 给基本 16 色。
///
/// 两个参数给 `None` 一律按「没设置」处理：探测只做保守降级，绝不 panic、绝不猜高。取值怎么从
/// 环境读（有损解码）由调用方决定——命令层与 TUI 都按「存在即算已设置」的同一口径转发。
pub fn color_depth(color: bool, colorterm: Option<&str>, term: Option<&str>) -> ColorDepth {
    if !color {
        return ColorDepth::None;
    }
    let truecolor = colorterm.is_some_and(|value| {
        value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
    });
    if truecolor {
        return ColorDepth::True;
    }
    // tmux / screen 各有自己的 TERM 前缀（`screen-256color`、`tmux-256color`），认的只是这一截
    if term.is_some_and(|value| value.contains("256color")) {
        return ColorDepth::Ansi256;
    }
    ColorDepth::Basic16
}

/// 主题调色板：dim/bold/error/accent 四档 + 品牌色与两个状态色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// 暗色（工具行、推理、引用/围栏正文）。
    pub dim: &'static str,
    /// 加粗（`**粗体**` 与标题）。
    pub bold: &'static str,
    /// 错误（失败的工具行与其输出首行）。
    pub error: &'static str,
    /// 强调色（行内代码）。
    pub accent: &'static str,
    /// 品牌色（火焰橙 `#F78340`；浅色主题加深保证对比度）：prompt `❯`、字标与状态点。
    pub brand: &'static str,
    /// 成功/就绪（`● Ready`、结束行的 `● Done`）。
    pub ok: &'static str,
    /// 警示（未读告警行、`Aborted`/`Limit reached` 的状态点）。
    pub warn: &'static str,
}

/// 主题调色板：`dim` 与 `brand` 分档，其余槽位与主题/色阶无关。
///
/// 只有 `dim` 分主题：浅色背景上 ANSI 暗色（`2m`）几乎看不见，换成亮灰（`90m`）。`brand` 分主题
/// （浅色主题用加深的橙，深色底上对比度才够）也分色阶（真彩 `38;2;…` / 256 `38;5;n` / 基本色
/// 落黄 `33`）：色阶探测与「无色零 SGR」由调用方门控，这里只负责给出该档的 ANSI 串。
pub fn palette(theme: Theme, depth: ColorDepth) -> Palette {
    let dim = match theme {
        Theme::Dark => "\x1b[2m",
        Theme::Light => "\x1b[90m",
    };
    Palette {
        dim,
        bold: "\x1b[1m",
        error: "\x1b[31m",
        accent: "\x1b[36m",
        brand: brand_slot(theme, depth),
        ok: "\x1b[32m",
        warn: "\x1b[33m",
    }
}

/// 品牌橙按主题 × 色阶取值：深色底 `#F78340`、浅色底加深成 `#D96A2B`；256 档的色号由
/// [`rgb_to_ansi256`] 算（两处必须一致）；基本色与不上色（[`ColorDepth::None`]）都落黄 `33`
/// ——`None` 只是「调用方不会再上色」，槽位仍给最保守的那一档，形状上不带空串。
fn brand_slot(theme: Theme, depth: ColorDepth) -> &'static str {
    match (theme, depth) {
        (Theme::Dark, ColorDepth::True) => "\x1b[38;2;247;131;64m",
        (Theme::Dark, ColorDepth::Ansi256) => "\x1b[38;5;209m",
        (Theme::Light, ColorDepth::True) => "\x1b[38;2;217;106;43m",
        (Theme::Light, ColorDepth::Ansi256) => "\x1b[38;5;166m",
        (_, ColorDepth::Basic16 | ColorDepth::None) => "\x1b[33m",
    }
}

/// xterm 256 色立方档的六个电平（`16..=231` 那一段就是它们的 6³ 组合）。
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// 一个通道值 → 立方档下标：按 xterm 立方档 `[0,95,135,175,215,255]` 取最近的档。
fn cube_index(value: u8) -> usize {
    if value < 48 {
        0
    } else if value < 115 {
        1
    } else {
        (value as usize - 35) / 40
    }
}

/// 归一化到 256 色（xterm 调色板）：立方候选与灰阶候选各算一个，取平方距离更近的那个（平手取
/// 立方）。
///
/// - 立方候选 = `(levels[r6], levels[g6], levels[b6])`，色号 `16 + 36*r6 + 6*g6 + b6`；
/// - 灰阶候选 = 平均亮度落到的灰阶档 `i`（夹在 `0..=23`），色号 `232 + i`，灰度值 `8 + 10*i`。
///
/// 品牌橙的两个 256 槽位常量（[`palette`]）就是拿它算的：改这里必须同步改常量，反之亦然。
pub fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let (r6, g6, b6) = (cube_index(r), cube_index(g), cube_index(b));
    let cube = (CUBE_LEVELS[r6], CUBE_LEVELS[g6], CUBE_LEVELS[b6]);
    let cube_code = 16 + 36 * r6 + 6 * g6 + b6;

    // 灰阶档 i = clamp((平均亮度 - 8) / 10, 0, 23)：上界取 230 是因为 247/10 会算出 24（越界）
    let average = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let gray_index = average.saturating_sub(8).min(230) / 10;
    let gray_value = (8 + 10 * gray_index) as u8;
    let gray_code = 232 + gray_index;
    let gray = (gray_value, gray_value, gray_value);

    if color_distance((r, g, b), gray) < color_distance((r, g, b), cube) {
        gray_code as u8
    } else {
        cube_code as u8
    }
}

/// 两个 RGB 的平方距离（通道差的平方和；最大 3×255²，`u32` 足够）。
fn color_distance(from: (u8, u8, u8), to: (u8, u8, u8)) -> u32 {
    let channel = |a: u8, b: u8| {
        let delta = i32::from(a) - i32::from(b);
        (delta * delta) as u32
    };
    channel(from.0, to.0) + channel(from.1, to.1) + channel(from.2, to.2)
}

/// 是否上色：`NO_COLOR` 非空恒 `false`（优先级最高，含 `Always`；空串按 NO_COLOR 的约定视为
/// 未设置）；`Never` 恒 `false`；`Always` 恒 `true`；`Auto` 跟 stdout 的 tty 状态。
pub fn resolve_color(choice: ColorChoice, is_tty: bool, no_color: Option<&str>) -> bool {
    if no_color.is_some_and(|value| !value.is_empty()) {
        return false;
    }
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => is_tty,
    }
}

/// 短 id：uuid 的前 13 个字符（按字符截断，不碰字节下标）。
///
/// `task list` 的 id 列、结束行与 `task show` 的歧义候选共用这一口径：同一个任务在三个地方
/// 显示同一个短 id。
pub fn short_id(id: &str) -> String {
    id.chars().take(SHORT_ID_CHARS).collect()
}

/// 每轮任务开始时定下的行宽。
///
/// stdout 不是 tty 时给 `usize::MAX`（不插入物理换行：管道 / 脚本 / e2e 要的是每行可 grep）；
/// 是 tty 时向终端要 `TIOCGWINSZ`，拿不到（ioctl 失败或报 0 列）退回 [`DEFAULT_WIDTH`]。
pub fn width_for_stdout(is_tty: bool) -> usize {
    if is_tty {
        probe_width().unwrap_or(DEFAULT_WIDTH)
    } else {
        usize::MAX
    }
}

/// 探测 stdout 的终端列数；不是终端 / ioctl 失败 / 报 0 列时给 `None`。
///
/// 每轮任务开始探测一次（不在每个事件里问终端）：运行中途改窗口不会重排已经写出的行。
#[cfg(unix)]
fn probe_width() -> Option<usize> {
    use std::os::fd::AsRawFd;

    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: `size` 是本函数独占的一块 `winsize`；`TIOCGWINSZ` 只往里写，失败时返回非 0，
    // 我们不读它写坏的内容（`ok` 为 false 时直接给 None）。
    let ok =
        unsafe { libc::ioctl(std::io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_col > 0).then_some(size.ws_col as usize)
}

/// 非 unix：没有 `TIOCGWINSZ` 这条路，一律回退默认宽度。
#[cfg(not(unix))]
fn probe_width() -> Option<usize> {
    None
}

/// 耗时：与结束行同一口径（`{:.1}s`，秒的小数一位）。
fn format_elapsed(elapsed: Duration) -> String {
    format!("{:.1}s", elapsed.as_secs_f64())
}

/// 按**字符**截断到 `max` 个字符（按字节切会切坏 UTF-8）：超长时末位换成 `…`，总长不超过 `max`。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 输出预览：首行 + 按字符截断到 [`ERROR_PREVIEW_CHARS`]。
///
/// 失败的工具结果只露首行：全文在 JSONL 与 `ignis task show` 里，终端不该被一段错误输出刷屏。
fn preview_first_line(output: Option<&str>) -> Option<String> {
    let first = output?.lines().next().unwrap_or_default();
    if first.is_empty() {
        return None;
    }
    Some(truncate_chars(first, ERROR_PREVIEW_CHARS))
}

/// `remember` 的成功确认行：`  ✓ memory: {key} ({scope}) {文本 ≤60 字符}`。
///
/// 三个字段都从该 `call_id` 的 `ToolExec.args` 取；缺任何一个就给 `None`，调用方退回普通成功行
/// （旧版本内核的参数形状可能不同）。
fn memory_line(args: &Value) -> Option<String> {
    let key = args.get("key")?.as_str()?;
    let scope = args.get("scope")?.as_str()?;
    let text = args.get("text")?.as_str()?;
    Some(format!(
        "  ✓ memory: {key} ({scope}) {}",
        truncate_chars(text, MEMORY_TEXT_CHARS)
    ))
}

/// 一条待配对的工具调用：`ToolResult` 到达时按 `call_id`（或 FIFO）取回它，算耗时、取摘要与参数。
struct PendingTool {
    /// 到达序号：`VecDeque` 里按它删中间项，`by_id` 里按它反查。
    seq: u64,
    /// 本轮调用的 id（旧日志为空串）。
    call_id: String,
    /// 工具名（`remember` 的确认行与未配对结果都看它）。
    name: String,
    /// 开始行的摘要。
    summary: String,
    /// 调用参数（`remember` 的确认行从 key/scope/text 取）。
    args: Value,
    /// 开始时刻：成功/失败行的 `· 0.3s` 是它到结果到达的实测耗时。
    started: Instant,
}

/// 正文通道的行状态。
enum Line {
    /// 行首：下一笔片段决定这行是「直接流式」还是「折行缓冲」。
    Start,
    /// 直接流出的行（普通段落/标题）：片段一到就写，不整行缓冲。
    Streaming,
    /// 折行缓冲的行（前缀类块）：攒满一条物理行，行末按宽度折行写出。
    Buffered(Vec<(String, Style)>),
}

/// 事件渲染器：把事件流写成终端输出。
///
/// 一个任务一个实例：状态（Markdown 装配器、挂起的行、待配对的工具表）都只服务本轮。
pub struct Renderer {
    /// 渲染目标：命令层给 stdout，测试给可读回的缓冲区。
    out: Box<dyn Write + Send>,
    /// 渲染选项（模式/颜色/主题/宽度）。
    options: RendererOptions,
    /// 渲染风味（经典路径默认 Classic；TUI 用 `with_flavor` 切换）。
    flavor: RenderFlavor,
    /// 主题调色板（由 `options.theme` 定，构造时算一次）。
    palette: Palette,
    /// 本轮事件里是否出现过 `schedule_create`（命令层在任务结束后读它决定自拉起提示）。
    saw_schedule_create: Arc<AtomicBool>,
    /// 正文通道的 Markdown 装配器。
    md: MdStream,
    /// 正文通道当前的物理行状态。
    line: Line,
    /// 打开的推理块已收的字符数；`None` 表示当前没有打开的块（minimal 的摘要行要它）。
    reasoning: Option<usize>,
    /// 待配对的工具调用，按到达顺序（空 `call_id` 的 FIFO 兜底靠它）。
    pending: VecDeque<PendingTool>,
    /// `call_id` → 到达序号（有 id 的调用优先按 id 命中）。
    by_id: HashMap<String, u64>,
    /// 下一个到达序号。
    seq: u64,
    /// 最后一个写出的正文字符（ANSI 不计）：列 0 纪律与 `finish` 的补换行都看它。
    /// `None` 表示本轮还什么都没写（`finish` 因此不会凭空补一个空行）。
    last: Option<char>,
}

impl Renderer {
    /// 装配一个渲染器：`out` 可注入（测试传可读回的缓冲区）。
    pub fn new(
        out: Box<dyn Write + Send>,
        options: RendererOptions,
        saw_schedule_create: Arc<AtomicBool>,
    ) -> Self {
        Self {
            out,
            options,
            flavor: RenderFlavor::Classic,
            palette: palette(options.theme, options.depth),
            saw_schedule_create,
            md: MdStream::new(),
            line: Line::Start,
            reasoning: None,
            pending: VecDeque::new(),
            by_id: HashMap::new(),
            seq: 0,
            last: None,
        }
    }

    /// 切换渲染风味（TUI 在装配会话时调用；经典路径不调用，保持逐字节不变）。
    pub fn with_flavor(mut self, flavor: RenderFlavor) -> Self {
        self.flavor = flavor;
        self
    }

    /// 渲染一个事件。
    ///
    /// `Usage` 与 `Done` 不在此渲染：结束行由命令层用 [`format_task_end`] 打印（用量在那里按模式
    /// 决定要不要露）。
    pub fn on_event(&mut self, ev: &RuntimeEvent) {
        // 推理块在同一通道的下一笔事件处收束：内容已经流式写出，收束只是补换行 / 打摘要行
        if !matches!(ev, RuntimeEvent::ReasoningDelta { .. }) {
            self.end_reasoning();
        }
        match ev {
            RuntimeEvent::TextDelta { text } => self.push_answer(text),
            RuntimeEvent::ReasoningDelta { text } => self.push_reasoning(text),
            RuntimeEvent::ToolExec {
                name,
                summary,
                args,
                call_id,
            } => self.push_tool_exec(name, summary, args, call_id),
            RuntimeEvent::ToolResult {
                name,
                ok,
                output,
                call_id,
            } => self.push_tool_result(name, *ok, output.as_deref(), call_id),
            RuntimeEvent::Usage { .. } | RuntimeEvent::Done { .. } => {}
        }

        // 流式输出必须逐事件冲洗：增量卡在行缓冲里就不再是「流式」（写失败多为 EPIPE，忽略）
        let _ = self.out.flush();
    }

    /// 任务收束：冲刷正文装配器与挂起的行，并保证输出以 `\n` 结尾（结束行不粘在正文尾巴上）。
    ///
    /// 本轮什么都没写时不补换行：只有围栏标记行的流产出零片段（`MdStream` 的已知形态），
    /// 凭空多一个空行会把结束行推下去一行。
    pub fn finish(&mut self) {
        for segment in self.md.finish() {
            self.render_segment(&segment);
        }
        self.end_reasoning();
        self.close_line();
        if self.last.is_some_and(|c| c != '\n') {
            self.write_char('\n');
        }
        let _ = self.out.flush();
    }

    /// 结束行的状态点着色：把 `● {状态词}` **整段**包上槽位色，再原样接上剩余部分。
    ///
    /// 色档按终态给（`Success` 绿 / `Failed` 红 / `Aborted` 与 `LimitReached` 黄）：状态词与
    /// [`status_word`] 同源，不另立一份状态表。着色范围含 `●` 与状态词两者：`● Done` 是终端里
    /// 可直接复制去 grep / `ignis task show` 的锚点，夹断成两段 SGR 就不再是连续子串。
    ///
    /// `color=false`、或行不以 `● {该状态的状态词}` 开头（正文行、别的状态的结束行）时原样
    /// 返回：一个 SGR 都不写，非 tty 的 e2e 输出因此逐字不变。
    pub fn paint_status_dot(&self, line: &str, status: RunStatus) -> String {
        if !self.options.color {
            return line.to_string();
        }
        // 经典路径认 `● Done`，TUI 认 `✓ Done` / `✗ Failed` / `◼ Aborted` / `! Limit reached`。
        let classic = format!("● {}", status_word(status));
        let tui = format!("{} {}", task_glyph(status), status_word(status));
        let anchor = if line.starts_with(&classic) {
            classic
        } else if line.starts_with(&tui) {
            tui
        } else {
            // 前缀对不上就不认领：正文里出现的 `● Done` 只是普通文字
            return line.to_string();
        };
        let rest = &line[anchor.len()..];
        let color = match status {
            RunStatus::Success => self.palette.ok,
            RunStatus::Failed => self.palette.error,
            RunStatus::Aborted | RunStatus::LimitReached => self.palette.warn,
        };
        format!("{color}{anchor}{RESET}{rest}")
    }

    /// 正文增量：过 `MdStream` 后逐片段写出。
    fn push_answer(&mut self, text: &str) {
        for segment in self.md.feed(text) {
            self.render_segment(&segment);
        }
    }

    /// 渲染一个正文片段：换行段断行，其余按当前行的形态流式写出或攒进折行缓冲。
    fn render_segment(&mut self, segment: &Segment) {
        if segment.text == "\n" {
            self.end_answer_line();
            return;
        }
        if segment.text.is_empty() {
            return;
        }
        match std::mem::replace(&mut self.line, Line::Start) {
            // 已经是折行缓冲的行：后续片段继续攒（形态由行首那一刻定）
            Line::Buffered(mut chunks) => {
                chunks.push((segment.text.clone(), segment.style));
                self.line = Line::Buffered(chunks);
            }
            Line::Streaming => {
                self.line = Line::Streaming;
                self.write_styled(&segment.text, segment.style);
            }
            Line::Start => {
                if self.options.width != usize::MAX && is_prefixed(segment) {
                    self.line = Line::Buffered(vec![(segment.text.clone(), segment.style)]);
                } else {
                    self.line = Line::Streaming;
                    self.write_styled(&segment.text, segment.style);
                }
            }
        }
    }

    /// 一条物理行结束：折行缓冲的按宽度折行写出，其余补一个换行。
    fn end_answer_line(&mut self) {
        if let Line::Buffered(chunks) = std::mem::replace(&mut self.line, Line::Start) {
            self.flush_prefixed_line(chunks);
            return;
        }
        // 空行（Markdown 的段落分隔）也是合法的一行：照样输出空行
        self.write_char('\n');
    }

    /// 收束挂起的物理行：折行缓冲的按宽度写出，流式行什么都不做（内容早已写出）。
    fn close_line(&mut self) {
        if let Line::Buffered(chunks) = std::mem::replace(&mut self.line, Line::Start) {
            self.flush_prefixed_line(chunks);
        }
    }

    /// 保证下一笔输出从列 0 开始：折行缓冲先收束，停在行中的补一个换行。
    fn ensure_line_start(&mut self) {
        self.close_line();
        if self.last.is_some_and(|c| c != '\n') {
            self.write_char('\n');
        }
    }

    /// 把一条前缀类块的物理行按宽度折行写出：续行按块的挂起缩进对齐。
    ///
    /// 折行的唯一入口是 [`width::wrap`]（空白优先、超长串硬断、宽字符 2 列），这里只做两件事：
    /// 给续行加缩进、把「字符 → 样式」按折行结果重新落位（`wrap` 只丢断点处的空白，非空白字符
    /// 一一对应，所以从游标处往前找同名字符即可）。
    fn flush_prefixed_line(&mut self, chunks: Vec<(String, Style)>) {
        let styled: Vec<(char, Style)> = chunks
            .iter()
            .flat_map(|(text, style)| text.chars().map(move |c| (c, *style)))
            .collect();
        let indent = hanging_indent(&styled);
        let body = &styled[indent.min(styled.len())..];
        let body_text: String = body.iter().map(|(c, _)| *c).collect();
        let lines = wrap(&body_text, self.options.width.saturating_sub(indent));

        if lines.is_empty() {
            // 空内容（只有列表标记或空白）：标记不能丢，按原文（去尾空白）打一行
            let raw: String = styled.iter().map(|(c, _)| *c).collect();
            let raw = raw.trim_end();
            if !raw.is_empty() {
                let kept = raw.chars().count();
                let mut cursor = 0;
                let runs = map_styles(&styled[..kept], &mut cursor, raw);
                self.write_runs(&runs);
            }
            self.write_char('\n');
            return;
        }

        let mut cursor = 0;
        for (index, line) in lines.iter().enumerate() {
            if index == 0 {
                // 首行带标记（列表的 `- ` / `* `）：标记自身照它的样式写出
                let marker = &styled[..indent.min(styled.len())];
                let runs = runs_of(marker);
                self.write_runs(&runs);
            } else if indent > 0 {
                // 续行按标记宽度缩进：与内容起点对齐（缩进是排版空白，不上色）
                let pad = " ".repeat(indent);
                self.write_styled(&pad, Style::Plain);
            }
            let runs = map_styles(body, &mut cursor, line);
            self.write_runs(&runs);
            self.write_char('\n');
        }
    }

    /// 上色写出若干「同风格的连续字符」。
    fn write_runs(&mut self, runs: &[(String, Style)]) {
        for (text, style) in runs {
            self.write_styled(text, *style);
        }
    }

    /// 推理增量：normal/debug 流式（块首带前缀、dim），minimal 只累计字数（块结束时打摘要行）。
    fn push_reasoning(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let fresh = self.reasoning.is_none();
        let count = self.reasoning.unwrap_or(0) + text.chars().count();
        self.reasoning = Some(count);
        if self.options.mode == OutputMode::Minimal {
            return;
        }
        if fresh {
            // 推理块从列 0 开始：前面挂起的正文行先收束
            self.ensure_line_start();
            let prefix = match self.options.mode {
                OutputMode::Debug => REASONING_PREFIX_DEBUG,
                _ => REASONING_PREFIX,
            };
            self.write_styled(prefix, Style::Dim);
        }
        self.write_styled(text, Style::Dim);
    }

    /// 推理块收束：minimal 打一行 dim 摘要，其余模式保证块后从列 0 起（与正文/工具行分开）。
    fn end_reasoning(&mut self) {
        let Some(count) = self.reasoning.take() else {
            return;
        };
        if self.options.mode == OutputMode::Minimal {
            if count > 0 {
                let line = format!("{REASONING_PREFIX}thinking ({count} chars)");
                self.ensure_line_start();
                self.write_styled(&line, Style::Dim);
                self.write_char('\n');
            }
            return;
        }
        self.ensure_line_start();
    }

    /// 工具开始：记「发起过」的标志、进配对表，normal/debug 打一行开始行。
    fn push_tool_exec(&mut self, name: &str, summary: &str, args: &Value, call_id: &str) {
        // 记「发起过」而不是「建成了」：工具可能被审批拒掉，但用户想建调度任务的意图已经明确，
        // 任务结束后照样该提示一次守护进程没在跑
        if name == SCHEDULE_CREATE {
            self.saw_schedule_create.store(true, Ordering::Relaxed);
        }
        let tool = PendingTool {
            seq: self.seq,
            call_id: call_id.to_string(),
            name: name.to_string(),
            summary: summary.to_string(),
            args: args.clone(),
            started: Instant::now(),
        };
        self.seq = self.seq.saturating_add(1);
        if !tool.call_id.is_empty() {
            self.by_id.insert(tool.call_id.clone(), tool.seq);
        }
        self.pending.push_back(tool);

        // minimal 不逐行展示工具（只由结束行的计数体现）
        if self.options.mode == OutputMode::Minimal {
            return;
        }
        let mut line = match self.flavor {
            RenderFlavor::Classic => format!("→ {summary}"),
            // TUI：工具名在前、摘要随后；状态行同时显示 Executing
            RenderFlavor::Tui if summary.is_empty() => format!("⚙ {name}"),
            RenderFlavor::Tui => format!("⚙ {name} — {summary}"),
        };
        if self.options.mode == OutputMode::Debug && !call_id.is_empty() {
            line.push_str(&format!(" [{call_id}]"));
        }
        self.ensure_line_start();
        self.write_styled(&line, Style::Dim);
        self.write_char('\n');
    }

    /// 工具结果：取回配对的开始行（`call_id` 优先、空 id 走 FIFO），normal/debug 打结果行。
    fn push_tool_result(&mut self, name: &str, ok: bool, output: Option<&str>, call_id: &str) {
        // 配对状态与模式无关：minimal 只是不写工具行，配对表仍按同一规则收束
        let paired = self.pair(call_id);
        if self.options.mode == OutputMode::Minimal {
            return;
        }

        // `remember` 的成功结果固定一行确认（参数取自开始行的 args；缺字段退回普通成功行）
        if ok
            && let Some(tool) = &paired
            && tool.name == REMEMBER
            && let Some(line) = memory_line(&tool.args)
        {
            self.ensure_line_start();
            self.write_styled(&line, Style::Dim);
            self.write_char('\n');
            return;
        }

        // 配对得上用开始行的摘要 + 实测耗时；配不上只打结果行，拿工具名兜底摘要
        let mark = if ok { "✓" } else { "✗" };
        let (label, elapsed) = match &paired {
            Some(tool) => (tool.summary.clone(), format_elapsed(tool.started.elapsed())),
            None => (name.to_string(), String::new()),
        };
        let mut line = match self.flavor {
            RenderFlavor::Classic => {
                if elapsed.is_empty() {
                    format!("  {mark} {label}")
                } else {
                    format!("  {mark} {label} · {elapsed}")
                }
            }
            // TUI：工具名 + 明确的终态词；摘要不再出现在结果行（开始行已经给过）。
            RenderFlavor::Tui => {
                let state = if ok { "Done" } else { "Failed" };
                if elapsed.is_empty() {
                    format!("{mark} {name} — {state}")
                } else {
                    format!("{mark} {name} — {state} · {elapsed}")
                }
            }
        };
        if self.options.mode == OutputMode::Debug && !call_id.is_empty() {
            line.push_str(&format!(" [{call_id}]"));
        }

        self.ensure_line_start();
        let color = if !ok {
            self.palette.error
        } else if self.flavor == RenderFlavor::Tui {
            // TUI 的成功结果用绿色；经典路径保持 dim（既有字节与观感不变）
            self.palette.ok
        } else {
            self.palette.dim
        };
        self.write_colored(&line, color);
        self.write_char('\n');
        // 失败行的第二行（debug 下成功行也给首行）：输出首行按字符截断
        if (!ok || self.options.mode == OutputMode::Debug)
            && let Some(preview) = preview_first_line(output)
        {
            self.write_colored(&format!("    {preview}"), color);
            self.write_char('\n');
        }
    }

    /// 取一条待配对的工具调用：`call_id` 优先，空 id 退回 FIFO（旧日志两侧都没有 id）。
    fn pair(&mut self, call_id: &str) -> Option<PendingTool> {
        if call_id.is_empty() {
            let tool = self.pending.pop_front()?;
            if !tool.call_id.is_empty() {
                self.by_id.remove(&tool.call_id);
            }
            return Some(tool);
        }
        let seq = self.by_id.remove(call_id)?;
        let index = self.pending.iter().position(|tool| tool.seq == seq)?;
        self.pending.remove(index)
    }

    /// 按调色板的一档上色写出（工具行/推理/摘要行；`color=false` 时零 ANSI）。
    fn write_colored(&mut self, text: &str, color: &str) {
        if self.options.color {
            let _ = write!(self.out, "{color}{text}{RESET}");
        } else {
            let _ = self.out.write_all(text.as_bytes());
        }
        self.note_written(text);
    }

    /// 按 Markdown 片段的样式上色写出。
    fn write_styled(&mut self, text: &str, style: Style) {
        match self.style_color(style) {
            Some(color) => self.write_colored(text, color),
            None => {
                let _ = self.out.write_all(text.as_bytes());
                self.note_written(text);
            }
        }
    }

    /// 写出一个字符（换行与列表标记的收尾用；不带颜色）。
    fn write_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let _ = self.out.write_all(c.encode_utf8(&mut buf).as_bytes());
        self.last = Some(c);
    }

    /// 记住最后一个写出的正文字符（ANSI 转义不算内容）。
    fn note_written(&mut self, text: &str) {
        if let Some(c) = text.chars().last() {
            self.last = Some(c);
        }
    }

    /// Markdown 样式 → 调色板；`color=false` 与 `Plain` 都不上色。
    fn style_color(&self, style: Style) -> Option<&'static str> {
        if !self.options.color {
            return None;
        }
        match style {
            Style::Plain => None,
            Style::Bold => Some(self.palette.bold),
            Style::InlineCode => Some(self.palette.accent),
            Style::Dim => Some(self.palette.dim),
        }
    }
}

/// 这一笔片段是否属于「前缀类块」：列表（`- `/`* ` 开头）或暗色块（引用、围栏正文、分隔线）。
///
/// 只有这些块在 tty 下按宽度折行：它们的续行需要跟着块的缩进，普通段落交给终端软换行。
fn is_prefixed(segment: &Segment) -> bool {
    segment.style == Style::Dim || segment.text.starts_with("- ") || segment.text.starts_with("* ")
}

/// 前缀类块的续行缩进：列表行留出标记宽度（`- `/`* ` 两个字符，续行与内容起点对齐），
/// 其余（引用/围栏正文：`MdStream` 已经吞掉标记）缩进 0 列。
fn hanging_indent(styled: &[(char, Style)]) -> usize {
    match (
        styled.first().map(|(c, _)| *c),
        styled.get(1).map(|(c, _)| *c),
    ) {
        (Some('-'), Some(' ')) | (Some('*'), Some(' ')) => 2,
        _ => 0,
    }
}

/// 折行后的行文本 → 「连续同风格的字符段」。
///
/// `wrap` 只在断点处丢空白、其余字符序不变，所以从游标处往前找同名字符即可还原样式；找不到
/// （理论上不会发生）就当正文样式，绝不吞字符。
fn map_styles(chars: &[(char, Style)], cursor: &mut usize, line: &str) -> Vec<(String, Style)> {
    let mut runs: Vec<(String, Style)> = Vec::new();
    for c in line.chars() {
        // 跳过被断点吞掉的空白
        while chars
            .get(*cursor)
            .is_some_and(|(ch, _)| *ch != c && ch.is_whitespace())
        {
            *cursor += 1;
        }
        let mut style = Style::Plain;
        if let Some((ch, found)) = chars.get(*cursor)
            && *ch == c
        {
            style = *found;
            *cursor += 1;
        }
        match runs.last_mut() {
            Some((text, last)) if *last == style => text.push(c),
            _ => runs.push((c.to_string(), style)),
        }
    }
    runs
}

/// 一段已排定的字符 → 「连续同风格的字符段」（首行的列表标记用）。
fn runs_of(chars: &[(char, Style)]) -> Vec<(String, Style)> {
    chars.iter().fold(Vec::new(), |mut runs, (c, style)| {
        match runs.last_mut() {
            Some((text, last)) if last == style => text.push(*c),
            _ => runs.push((c.to_string(), *style)),
        }
        runs
    })
}

/// 结束行：`● Done · 2.1s · 3 tools · task 019a1b2c-3d4e`（模式决定信息量）；工具计数按
/// 单复数出词：恰好一次是 `1 tool`，其余（含 0）是 `N tools`。
///
/// - minimal：`● {状态} · {耗时}`（只留正文与一行收尾）。
/// - normal：加工具计数与**短 id**（13 字符）——失败/触限的运行要能直接从终端复制 id 去
///   `ignis task show`，不必再翻 SQLite 或 JSONL。
/// - debug：在 `task` 前再加 `tokens in=… out=…`（估算值前加 `~`），且 task 用完整 id。
///
/// `skills` 非空时 normal/debug 追加 `· skills: a,b`（按首次加载顺序）：minimal 连它一起省掉。
pub fn format_task_end(
    id: &str,
    status: RunStatus,
    elapsed: Duration,
    usage: &Usage,
    skills: &[String],
    tool_calls: usize,
    mode: OutputMode,
) -> String {
    let elapsed = format_elapsed(elapsed);
    let mut line = format!("● {} · {elapsed}", status_word(status));
    // 恰好一次调用用单数：`· 1 tool` / `· 3 tools`
    let tools = if tool_calls == 1 { "tool" } else { "tools" };
    match mode {
        OutputMode::Minimal => {}
        OutputMode::Normal => {
            line.push_str(&format!(" · {tool_calls} {tools} · task {}", short_id(id)));
        }
        OutputMode::Debug => {
            let mark = if usage.estimated { "~" } else { "" };
            line.push_str(&format!(
                " · {tool_calls} {tools} · tokens {mark}in={} {mark}out={} · task {id}",
                usage.input_tokens, usage.output_tokens
            ));
        }
    }
    if mode != OutputMode::Minimal && !skills.is_empty() {
        line.push_str(&format!(" · skills: {}", skills.join(",")));
    }
    line
}

/// TUI 的终态字形（经典路径用 `●`）。
fn task_glyph(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Success => "✓",
        RunStatus::Failed => "✗",
        RunStatus::Aborted => "◼",
        RunStatus::LimitReached => "!",
    }
}

/// TUI 的结束行：`✓ Done · 3.2s · 4 tools · task 019a1b2c-3d4e`。
///
/// 信息量与经典 [`format_task_end`] 一致（minimal/normal/debug 三档、单复数、skills 追加），
/// 只换字形；经典路径不调用它，逐字节契约不受影响。
pub fn format_task_end_tui(
    id: &str,
    status: RunStatus,
    elapsed: Duration,
    usage: &Usage,
    skills: &[String],
    tool_calls: usize,
    mode: OutputMode,
) -> String {
    let elapsed = format_elapsed(elapsed);
    let mut line = format!("{} {} · {elapsed}", task_glyph(status), status_word(status));
    let tools = if tool_calls == 1 { "tool" } else { "tools" };
    match mode {
        OutputMode::Minimal => {}
        OutputMode::Normal => {
            line.push_str(&format!(" · {tool_calls} {tools} · task {}", short_id(id)));
        }
        OutputMode::Debug => {
            let mark = if usage.estimated { "~" } else { "" };
            line.push_str(&format!(
                " · {tool_calls} {tools} · tokens {mark}in={} {mark}out={} · task {id}",
                usage.input_tokens, usage.output_tokens
            ));
        }
    }
    if mode != OutputMode::Minimal && !skills.is_empty() {
        line.push_str(&format!(" · skills: {}", skills.join(",")));
    }
    line
}

/// 终态的状态词（`minimal` 也用它：状态词不能省）。
fn status_word(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Success => "Done",
        RunStatus::Failed => "Failed",
        RunStatus::Aborted => "Aborted",
        RunStatus::LimitReached => "Limit reached",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ignis_core::config::{ColorChoice, OutputMode, Theme};
    use ignis_runtime::{RunStatus, RuntimeEvent, Usage};
    use serde_json::json;

    use super::*;

    /// 结束行里的任务 id（与真实任务 id 同形，断言直接引用它）。
    const TASK_ID: &str = "019a1b2c-3d4e-7f00-8000-000000000001";

    /// 非 tty 的口径：不插入物理换行。
    const NO_WRAP: usize = usize::MAX;

    /// 写入落到共享缓冲区的测试写入器：`Renderer` 持有 `out`，需要从外部读回内容。
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).expect("UTF-8 渲染输出")
        }
    }

    /// TUI 风味的渲染器：无色、不折行，便于逐字断言。
    fn renderer_tui(mode: OutputMode) -> (Renderer, SharedBuf) {
        let (renderer, buf, _) = renderer_with(mode, false, Theme::Dark, NO_WRAP);
        (renderer.with_flavor(RenderFlavor::Tui), buf)
    }

    /// 测试用渲染器：模式/颜色/主题/宽度都显式给（不探测终端），色阶固定基本 16 色
    /// （要别的色阶的用例另加带 `depth` 的变体）。
    fn renderer_with(
        mode: OutputMode,
        color: bool,
        theme: Theme,
        width: usize,
    ) -> (Renderer, SharedBuf, Arc<AtomicBool>) {
        let buf = SharedBuf::default();
        let flag = Arc::new(AtomicBool::new(false));
        let renderer = Renderer::new(
            Box::new(buf.clone()),
            RendererOptions {
                mode,
                color,
                theme,
                depth: ColorDepth::Basic16,
                width,
            },
            Arc::clone(&flag),
        );
        (renderer, buf, flag)
    }

    /// 默认主题（dark）下的测试渲染器。
    fn renderer(
        mode: OutputMode,
        color: bool,
        width: usize,
    ) -> (Renderer, SharedBuf, Arc<AtomicBool>) {
        renderer_with(mode, color, Theme::Dark, width)
    }

    /// 跑一串事件（末尾 `finish`），返回渲染出的文本。
    fn render(mode: OutputMode, color: bool, width: usize, events: &[RuntimeEvent]) -> String {
        let (mut renderer, buf, _flag) = renderer(mode, color, width);
        for event in events {
            renderer.on_event(event);
        }
        renderer.finish();
        buf.text()
    }

    /// 正文增量。
    fn text(delta: &str) -> RuntimeEvent {
        RuntimeEvent::TextDelta {
            text: delta.to_string(),
        }
    }

    /// 推理增量。
    fn reasoning(delta: &str) -> RuntimeEvent {
        RuntimeEvent::ReasoningDelta {
            text: delta.to_string(),
        }
    }

    /// 工具开始事件。
    fn tool_exec(summary: &str, call_id: &str) -> RuntimeEvent {
        RuntimeEvent::ToolExec {
            name: "fs_read".to_string(),
            summary: summary.to_string(),
            args: json!({ "path": "a.txt" }),
            call_id: call_id.to_string(),
        }
    }

    /// 工具结果事件。
    fn tool_result(name: &str, ok: bool, output: Option<&str>, call_id: &str) -> RuntimeEvent {
        RuntimeEvent::ToolResult {
            name: name.to_string(),
            ok,
            output: output.map(str::to_string),
            call_id: call_id.to_string(),
        }
    }

    /// `remember` 的工具开始事件（参数可换，用来验确认行的字段回退）。
    fn remember_exec(args: serde_json::Value, call_id: &str) -> RuntimeEvent {
        RuntimeEvent::ToolExec {
            name: "remember".to_string(),
            summary: "Remembering project fact".to_string(),
            args,
            call_id: call_id.to_string(),
        }
    }

    // ---- 模式矩阵 ----

    #[test]
    fn normal_mode_streams_reasoning_behind_a_dim_prefix_then_the_answer() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[reasoning("想一下"), reasoning("再想"), text("答案")],
        );
        // 推理分道：块首一个 `· `，内容 dim 流式；正文接着从新的一行开始
        assert_eq!(out, "· 想一下再想\n答案\n");
    }

    #[test]
    fn normal_mode_prints_the_exec_and_result_lines_of_a_paired_tool_call() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[
                tool_exec("Reading a.txt", "call_1"),
                tool_result("fs_read", true, Some("文件内容"), "call_1"),
            ],
        );
        assert!(
            out.starts_with("→ Reading a.txt\n  ✓ Reading a.txt · "),
            "{out}"
        );
        assert!(out.ends_with("s\n"), "{out}");
        // 成功结果不打输出（要看全文用 `ignis task show`）
        assert!(!out.contains("文件内容"), "{out}");
    }

    #[test]
    fn minimal_mode_hides_tool_lines_and_summarizes_thinking() {
        let out = render(
            OutputMode::Minimal,
            false,
            NO_WRAP,
            &[
                reasoning("四字推理"),
                tool_exec("Reading a.txt", "call_1"),
                tool_result("fs_read", true, None, "call_1"),
                text("答案"),
            ],
        );
        // 工具行一行都不出（计数只由结束行体现），推理只留一行 dim 摘要
        assert_eq!(out, "· thinking (4 chars)\n答案\n");
    }

    #[test]
    fn debug_mode_marks_reasoning_and_carries_the_call_id() {
        let out = render(
            OutputMode::Debug,
            false,
            NO_WRAP,
            &[
                reasoning("内心戏"),
                text("答案"),
                tool_exec("Reading a.txt", "call_1"),
                tool_result("fs_read", true, Some("文件内容"), "call_1"),
            ],
        );
        assert!(out.starts_with("[reasoning] 内心戏\n"), "{out}");
        assert!(out.contains("→ Reading a.txt [call_1]\n"), "{out}");
        assert!(out.contains("  ✓ Reading a.txt · "), "{out}");
        // debug 下成功结果也带首行（诊断用；全文仍只在 `ignis task show`）
        assert!(out.contains("\n    文件内容\n"), "{out}");
    }

    // ---- 颜色 ----

    #[test]
    fn color_false_emits_zero_ansi_in_every_mode() {
        let events = [
            reasoning("内心戏"),
            text("# 标题 **粗体** `代码`\n> 引用\n"),
            tool_exec("Reading a.txt", "call_1"),
            tool_result("fs_read", false, Some("boom"), "call_1"),
        ];
        for mode in [OutputMode::Minimal, OutputMode::Normal, OutputMode::Debug] {
            let out = render(mode, false, NO_WRAP, &events);
            assert!(!out.contains('\u{1b}'), "{mode:?} 输出了 ANSI：{out}");
        }
    }

    #[test]
    fn color_uses_the_theme_palette_and_styles_markdown_segments() {
        let out = render(OutputMode::Normal, true, NO_WRAP, &[text("**粗** `代`\n")]);
        assert!(out.contains("\u{1b}[1m粗\u{1b}[0m"), "{out}");
        assert!(out.contains("\u{1b}[36m代\u{1b}[0m"), "{out}");

        // dark 的 dim 是 ANSI 暗色，light 换成亮灰（浅色背景上 2m 几乎看不见）
        let (mut dark, dark_buf, _) = renderer(OutputMode::Normal, true, NO_WRAP);
        dark.on_event(&tool_exec("Reading a.txt", "call_1"));
        assert_eq!(dark_buf.text(), "\u{1b}[2m→ Reading a.txt\u{1b}[0m\n");

        let (mut light, light_buf, _) =
            renderer_with(OutputMode::Normal, true, Theme::Light, NO_WRAP);
        light.on_event(&tool_exec("Reading a.txt", "call_1"));
        assert_eq!(light_buf.text(), "\u{1b}[90m→ Reading a.txt\u{1b}[0m\n");
    }

    // ---- 工具配对 ----

    #[test]
    fn tool_result_pairs_by_call_id_not_first_in_first_out() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[
                tool_exec("First", "call_1"),
                tool_exec("Second", "call_2"),
                tool_result("fs_read", true, None, "call_2"),
            ],
        );
        assert!(out.contains("  ✓ Second · "), "call_id 命中第二条：{out}");
        assert!(!out.contains("  ✓ First"), "{out}");
    }

    #[test]
    fn tool_result_falls_back_to_fifo_when_call_id_is_empty() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[
                tool_exec("First", ""),
                tool_exec("Second", ""),
                tool_result("fs_read", true, None, ""),
                tool_result("fs_read", true, None, ""),
            ],
        );
        // 旧日志（两侧都没有 call_id）按先进先出配对：第一条结果配 First、第二条配 Second
        let first = out.find("  ✓ First").unwrap_or_else(|| panic!("{out}"));
        let second = out.find("  ✓ Second").unwrap_or_else(|| panic!("{out}"));
        assert!(first < second, "{out}");
    }

    #[test]
    fn unpaired_tool_result_uses_the_tool_name_and_no_elapsed() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[tool_result("shell_run", true, None, "call_9")],
        );
        assert_eq!(out, "  ✓ shell_run\n");
    }

    #[test]
    fn failed_tool_result_prints_the_first_line_of_its_output_on_a_second_line() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[
                tool_exec("Running: npm test", "call_1"),
                tool_result("shell_run", false, Some("boom\n附加的第二行"), "call_1"),
            ],
        );
        assert!(out.contains("  ✗ Running: npm test · "), "{out}");
        assert!(out.contains("\n    boom\n"), "{out}");
        assert!(!out.contains("附加的第二行"), "{out}");
    }

    #[test]
    fn tool_error_preview_truncates_the_first_line_to_200_chars() {
        let long = "你".repeat(300);
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[tool_result(
                "shell_run",
                false,
                Some(&format!("{long}\n尾巴")),
                "call_1",
            )],
        );
        let preview = out.lines().last().expect("至少一行");
        assert!(preview.starts_with("    "), "{out}");
        assert_eq!(preview.trim_start().chars().count(), 200, "{out}");
        assert!(preview.trim_start().ends_with('…'), "{out}");
        assert!(!out.contains("尾巴"), "{out}");
    }

    #[test]
    fn remember_success_prints_the_fixed_memory_line() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[
                remember_exec(
                    json!({"scope": "project", "key": "pkg_manager", "text": "这个项目用 uv"}),
                    "call_5",
                ),
                tool_result("remember", true, None, "call_5"),
            ],
        );
        assert!(
            out.ends_with("  ✓ memory: pkg_manager (project) 这个项目用 uv\n"),
            "{out}"
        );
        // 固定的确认行不带耗时
        assert!(!out.contains("这个项目用 uv · "), "{out}");
    }

    #[test]
    fn remember_line_truncates_the_text_to_sixty_chars() {
        let long = "记".repeat(70);
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[
                remember_exec(json!({"scope": "user", "key": "k", "text": long}), "call_5"),
                tool_result("remember", true, None, "call_5"),
            ],
        );
        let line = out.lines().last().expect("至少一行");
        let shown = line
            .split(" (user) ")
            .nth(1)
            .unwrap_or_else(|| panic!("确认行形状不对：{out}"));
        assert_eq!(shown.chars().count(), 60, "{out}");
        assert!(shown.ends_with('…'), "{out}");
    }

    #[test]
    fn remember_with_missing_args_falls_back_to_the_plain_success_line() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[
                remember_exec(json!({"scope": "project"}), "call_5"),
                tool_result("remember", true, None, "call_5"),
            ],
        );
        assert!(out.contains("  ✓ Remembering project fact · "), "{out}");
        assert!(!out.contains("memory:"), "{out}");
    }

    /// 失败路径（`ok = false`）：一行确认都不打，只走既有的 `  ✗ {summary} · {耗时}` 失败行
    /// （首行预览跟在它后面）。
    #[test]
    fn remember_failure_prints_no_confirmation_line() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[
                remember_exec(
                    json!({"scope": "project", "key": "pkg_manager", "text": "这个项目用 uv"}),
                    "call_5",
                ),
                tool_result("remember", false, Some("记忆的 text 不能为空"), "call_5"),
            ],
        );
        assert!(!out.contains("memory:"), "{out}");
        assert!(!out.contains('✓'), "{out}");
        // 失败行照旧（摘要 + 实测耗时 + 首行预览），没有被吞掉
        assert!(out.contains("  ✗ Remembering project fact · "), "{out}");
        assert!(out.contains("    记忆的 text 不能为空"), "{out}");
    }

    // ---- 写序：列 0 纪律与 finish ----

    #[test]
    fn tool_lines_start_at_column_zero_after_a_partial_text_line() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[
                text("先说话"),
                tool_exec("Reading a.txt", "call_1"),
                text("后半句"),
            ],
        );
        assert_eq!(out, "先说话\n→ Reading a.txt\n后半句\n");
    }

    #[test]
    fn finish_appends_a_newline_only_when_the_stream_stops_mid_line() {
        let (mut mid, mid_buf, _) = renderer(OutputMode::Normal, false, NO_WRAP);
        mid.on_event(&text("没有换行的正文"));
        mid.finish();
        assert_eq!(mid_buf.text(), "没有换行的正文\n");

        let (mut done, done_buf, _) = renderer(OutputMode::Normal, false, NO_WRAP);
        done.on_event(&text("已经换行\n"));
        done.finish();
        assert_eq!(done_buf.text(), "已经换行\n");
    }

    #[test]
    fn finish_does_not_fabricate_a_blank_line_for_a_fence_only_turn() {
        // 只有围栏标记行的流产出零片段（Task 6 的已知形态）：finish 不该凭空补一个空行
        let (mut fence_only, fence_buf, _) = renderer(OutputMode::Normal, false, NO_WRAP);
        fence_only.on_event(&text("```rust\n"));
        fence_only.finish();
        assert_eq!(fence_buf.text(), "");
    }

    #[test]
    fn tool_lines_end_an_open_reasoning_block() {
        let out = render(
            OutputMode::Normal,
            false,
            NO_WRAP,
            &[reasoning("想"), tool_exec("Reading a.txt", "call_1")],
        );
        assert_eq!(out, "· 想\n→ Reading a.txt\n");
    }

    // ---- 宽度与折行 ----

    #[test]
    fn non_tty_width_never_inserts_physical_breaks() {
        assert_eq!(width_for_stdout(false), usize::MAX);
        let out = render(
            OutputMode::Normal,
            false,
            width_for_stdout(false),
            &[text("- alpha beta gamma\n")],
        );
        assert_eq!(out, "- alpha beta gamma\n");
    }

    #[test]
    fn tty_width_is_a_real_column_count_with_the_documented_fallback() {
        // 用例里 stdout 可能是管道（ioctl 失败）也可能是终端：两种情形都必须是「有列数的折行」
        // 而不是 `usize::MAX`（那条路只留给非 tty）
        let width = width_for_stdout(true);
        assert!(width > 0 && width != usize::MAX, "{width}");
        assert_eq!(DEFAULT_WIDTH, 80);
    }

    #[test]
    fn explicit_width_wraps_list_lines_and_aligns_the_continuation() {
        let out = render(
            OutputMode::Normal,
            false,
            10,
            &[text("- alpha beta gamma\n")],
        );
        assert_eq!(out, "- alpha\n  beta\n  gamma\n");
    }

    #[test]
    fn explicit_width_wraps_dim_blocks_at_the_content_start() {
        // 引用/围栏正文都是 dim 块：MdStream 已吞掉 `> ` 标记，续行与内容起点对齐（0 列缩进）
        let out = render(
            OutputMode::Normal,
            false,
            10,
            &[text("> alpha beta gamma\n")],
        );
        assert_eq!(out, "alpha beta\ngamma\n");
    }

    #[test]
    fn explicit_width_wraps_fence_content_without_touching_the_markers() {
        let out = render(
            OutputMode::Normal,
            false,
            10,
            &[text("```\nalpha beta gamma\n```\n")],
        );
        assert_eq!(out, "alpha beta\ngamma\n");
    }

    #[test]
    fn explicit_width_leaves_plain_paragraphs_to_the_terminal() {
        // 普通段落不插物理换行（终端自己软换行；每行保持可 grep）
        let out = render(
            OutputMode::Normal,
            false,
            6,
            &[text("这是一个很长的普通段落不该被物理折行\n")],
        );
        assert_eq!(out, "这是一个很长的普通段落不该被物理折行\n");
    }

    #[test]
    fn explicit_width_keeps_inline_styles_on_every_wrapped_line() {
        let out = render(
            OutputMode::Normal,
            true,
            10,
            &[text("- **alpha beta gamma**\n")],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert_eq!(lines[0], "- \u{1b}[1malpha\u{1b}[0m", "{out}");
        assert_eq!(lines[1], "  \u{1b}[1mbeta\u{1b}[0m", "{out}");
        assert_eq!(lines[2], "  \u{1b}[1mgamma\u{1b}[0m", "{out}");
    }

    #[test]
    fn explicit_width_maps_styles_back_to_their_characters_across_a_break() {
        // `alpha` 普通、`beta gamma` 加粗：折行把两者的样式各归各位
        let out = render(
            OutputMode::Normal,
            true,
            12,
            &[text("- alpha **beta gamma**\n")],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert_eq!(lines[0], "- alpha \u{1b}[1mbeta\u{1b}[0m", "{out}");
        assert_eq!(lines[1], "  \u{1b}[1mgamma\u{1b}[0m", "{out}");
    }

    #[test]
    fn finish_flushes_a_wrapped_line_that_never_ended() {
        let out = render(OutputMode::Normal, false, 10, &[text("> alpha beta gamma")]);
        assert_eq!(out, "alpha beta\ngamma\n");
    }

    // ---- 计数与标志 ----

    #[test]
    fn schedule_create_flips_the_autostart_flag_in_every_mode() {
        for mode in [OutputMode::Minimal, OutputMode::Normal, OutputMode::Debug] {
            let (mut renderer, _buf, flag) = renderer(mode, false, NO_WRAP);

            // 别的工具（含工具结果）都不该翻这个标志
            renderer.on_event(&tool_exec("Reading a.txt", "call_1"));
            renderer.on_event(&tool_result("schedule_create", true, None, "call_1"));
            assert!(
                !flag.load(Ordering::Relaxed),
                "{mode:?}: 只有 ToolExec 才算数"
            );

            renderer.on_event(&RuntimeEvent::ToolExec {
                name: "schedule_create".to_string(),
                summary: "Running schedule_create".to_string(),
                args: json!({ "name": "disk-watch" }),
                call_id: "call_2".to_string(),
            });
            assert!(flag.load(Ordering::Relaxed), "{mode:?}: 必须翻标志");
        }
    }

    // ---- 函数级契约 ----

    #[test]
    fn resolve_color_covers_the_whole_table() {
        // NO_COLOR 非空恒无色（含 Always）；空串视为未设置（NO_COLOR 的约定）
        assert!(!resolve_color(ColorChoice::Always, true, Some("1")));
        assert!(!resolve_color(ColorChoice::Never, true, Some("1")));
        assert!(resolve_color(ColorChoice::Always, false, Some("")));

        assert!(!resolve_color(ColorChoice::Never, true, None));
        assert!(resolve_color(ColorChoice::Always, false, None));
        assert!(!resolve_color(ColorChoice::Auto, false, None));
        assert!(resolve_color(ColorChoice::Auto, true, None));
        assert!(!resolve_color(ColorChoice::Auto, true, Some("0")));
    }

    #[test]
    fn color_depth_detection_table() {
        // color=false 恒 None（NO_COLOR / ui.color=never 经 resolve_color 收敛到这里）
        assert_eq!(
            color_depth(false, Some("truecolor"), Some("xterm-256color")),
            ColorDepth::None
        );
        assert_eq!(
            color_depth(true, Some("truecolor"), Some("xterm")),
            ColorDepth::True
        );
        assert_eq!(
            color_depth(true, Some("TrueColor"), Some("xterm")),
            ColorDepth::True,
            "大小写不敏感"
        );
        assert_eq!(color_depth(true, Some("24bit"), None), ColorDepth::True);
        assert_eq!(
            color_depth(true, None, Some("screen-256color")),
            ColorDepth::Ansi256
        );
        assert_eq!(color_depth(true, None, Some("xterm")), ColorDepth::Basic16);
        assert_eq!(color_depth(true, None, None), ColorDepth::Basic16);
    }

    #[test]
    fn rgb_to_ansi256_picks_the_nearest_of_cube_and_gray() {
        assert_eq!(rgb_to_ansi256(255, 0, 0), 196);
        assert_eq!(rgb_to_ansi256(0, 0, 0), 16);
        assert_eq!(rgb_to_ansi256(255, 255, 255), 231);
        // 品牌橙两档（Palette 的 256 槽位常量必须与量化函数一致）
        assert_eq!(rgb_to_ansi256(247, 131, 64), 209);
        assert_eq!(rgb_to_ansi256(217, 106, 43), 166);
        // 中性灰走灰阶候选（立方候选比它远）：128 = 第 12 级灰的灰度值
        assert_eq!(rgb_to_ansi256(128, 128, 128), 244);
        assert_eq!(rgb_to_ansi256(130, 130, 130), 244, "灰阶择近不是精确相等");
    }

    /// 槽位矩阵（dark/light × True/Ansi256/Basic16）：dim 分主题、brand 分主题分色阶，其余一律
    /// 与主题/色阶无关。
    #[test]
    fn palette_slots_follow_the_theme_and_depth_matrix() {
        // dim 分主题（浅色背景上 `2m` 几乎看不见，换亮灰）
        assert_eq!(palette(Theme::Dark, ColorDepth::True).dim, "\u{1b}[2m");
        assert_eq!(palette(Theme::Light, ColorDepth::True).dim, "\u{1b}[90m");

        // brand：真彩给品牌橙 RGB（浅色主题加深），256 给立方色号，基本色两主题都落黄
        let brand = |theme: Theme, depth: ColorDepth| palette(theme, depth).brand;
        assert_eq!(
            brand(Theme::Dark, ColorDepth::True),
            "\u{1b}[38;2;247;131;64m"
        );
        assert_eq!(
            brand(Theme::Light, ColorDepth::True),
            "\u{1b}[38;2;217;106;43m"
        );
        assert_eq!(brand(Theme::Dark, ColorDepth::Ansi256), "\u{1b}[38;5;209m");
        assert_eq!(brand(Theme::Light, ColorDepth::Ansi256), "\u{1b}[38;5;166m");
        assert_eq!(brand(Theme::Dark, ColorDepth::Basic16), "\u{1b}[33m");
        assert_eq!(brand(Theme::Light, ColorDepth::Basic16), "\u{1b}[33m");
        // 256 档与量化函数同源（两个主题各锁一次）
        assert_eq!(
            brand(Theme::Dark, ColorDepth::Ansi256),
            format!("\u{1b}[38;5;{}m", rgb_to_ansi256(247, 131, 64))
        );
        assert_eq!(
            brand(Theme::Light, ColorDepth::Ansi256),
            format!("\u{1b}[38;5;{}m", rgb_to_ansi256(217, 106, 43))
        );

        // 状态色与 bold/error/accent 在全部主题 × 色阶组合下都不变
        for theme in [Theme::Dark, Theme::Light] {
            for depth in [ColorDepth::True, ColorDepth::Ansi256, ColorDepth::Basic16] {
                let slots = palette(theme, depth);
                assert_eq!(slots.ok, "\u{1b}[32m", "{theme:?}/{depth:?}");
                assert_eq!(slots.warn, "\u{1b}[33m", "{theme:?}/{depth:?}");
                assert_eq!(slots.bold, "\u{1b}[1m", "{theme:?}/{depth:?}");
                assert_eq!(slots.error, "\u{1b}[31m", "{theme:?}/{depth:?}");
                assert_eq!(slots.accent, "\u{1b}[36m", "{theme:?}/{depth:?}");
            }
        }

        // 不上色（None）与基本色同档：槽位照旧可取，调用方按 `depth != None` 门控
        assert_eq!(
            palette(Theme::Light, ColorDepth::None).brand,
            palette(Theme::Light, ColorDepth::Basic16).brand
        );
    }

    #[test]
    fn short_id_takes_the_first_thirteen_chars() {
        assert_eq!(short_id(TASK_ID), "019a1b2c-3d4e");
        assert_eq!(short_id("short"), "short");
        assert_eq!(short_id(""), "");
        // 按字符截断（按字节切会切坏 UTF-8）
        assert_eq!(short_id(&"你".repeat(20)), "你".repeat(13));
    }

    #[test]
    fn elapsed_and_truncation_helpers_follow_their_contracts() {
        assert_eq!(format_elapsed(Duration::from_millis(0)), "0.0s");
        assert_eq!(format_elapsed(Duration::from_millis(300)), "0.3s");
        assert_eq!(format_elapsed(Duration::from_millis(1234)), "1.2s");

        assert_eq!(truncate_chars("你好", 5), "你好");
        assert_eq!(truncate_chars("你好世界", 3), "你好…");
        assert_eq!(truncate_chars("abcdef", 3).chars().count(), 3);
    }

    // ---- 结束行 ----

    #[test]
    fn task_end_line_covers_the_status_and_mode_matrix() {
        let usage = Usage {
            input_tokens: 1234,
            output_tokens: 567,
            estimated: false,
        };
        let elapsed = Duration::from_millis(2100);

        for (status, word) in [
            (RunStatus::Success, "Done"),
            (RunStatus::Failed, "Failed"),
            (RunStatus::Aborted, "Aborted"),
            (RunStatus::LimitReached, "Limit reached"),
        ] {
            assert_eq!(
                format_task_end(
                    TASK_ID,
                    status,
                    elapsed,
                    &usage,
                    &[],
                    3,
                    OutputMode::Minimal
                ),
                format!("● {word} · 2.1s"),
                "{status:?} minimal"
            );
            assert_eq!(
                format_task_end(TASK_ID, status, elapsed, &usage, &[], 3, OutputMode::Normal),
                format!("● {word} · 2.1s · 3 tools · task 019a1b2c-3d4e"),
                "{status:?} normal"
            );
            assert_eq!(
                format_task_end(TASK_ID, status, elapsed, &usage, &[], 3, OutputMode::Debug),
                format!("● {word} · 2.1s · 3 tools · tokens in=1234 out=567 · task {TASK_ID}"),
                "{status:?} debug"
            );
        }
    }

    #[test]
    fn task_end_line_marks_estimated_tokens_and_appends_skills() {
        let usage = Usage {
            input_tokens: 1234,
            output_tokens: 567,
            estimated: true,
        };
        // 故意不是字典序：追加段必须保留调用方给的顺序
        let skills = vec!["server-health".to_string(), "report-generator".to_string()];

        assert_eq!(
            format_task_end(
                TASK_ID,
                RunStatus::Success,
                Duration::from_secs(2),
                &usage,
                &skills,
                0,
                OutputMode::Debug
            ),
            format!(
                "● Done · 2.0s · 0 tools · tokens ~in=1234 ~out=567 · task {TASK_ID} \
                 · skills: server-health,report-generator"
            )
        );
        // normal 也带 skills 段（在短 id 之后）
        let normal = format_task_end(
            TASK_ID,
            RunStatus::Success,
            Duration::from_secs(2),
            &usage,
            &skills,
            0,
            OutputMode::Normal,
        );
        assert_eq!(
            normal,
            "● Done · 2.0s · 0 tools · task 019a1b2c-3d4e · skills: server-health,report-generator"
        );
        // minimal 只留状态与耗时：没加载过技能时也不多一个空段
        assert!(
            !format_task_end(
                TASK_ID,
                RunStatus::Success,
                Duration::from_secs(2),
                &usage,
                &[],
                0,
                OutputMode::Normal
            )
            .contains("skills")
        );
    }

    /// F2（评审遗留）：工具计数按单复数出词——恰好一次是 `1 tool`，其余（含 0）是 `N tools`，
    /// normal 与 debug 两个模式共用同一口径。
    /// TUI 工具生命周期：`⚙ name — summary` / `✓ name — Done` / `✗ name — Failed`；经典路径不变。
    #[test]
    fn tui_flavor_renders_the_tool_lifecycle_with_names() {
        let (mut renderer, buf) = renderer_tui(OutputMode::Normal);
        renderer.on_event(&RuntimeEvent::ToolExec {
            name: "shell_run".into(),
            summary: "Running ls".into(),
            args: json!({}),
            call_id: "c1".into(),
        });
        assert_eq!(buf.text(), "⚙ shell_run — Running ls\n");
        renderer.on_event(&RuntimeEvent::ToolResult {
            name: "shell_run".into(),
            ok: true,
            output: None,
            call_id: "c1".into(),
        });
        let text = buf.text();
        assert!(
            text.contains("✓ shell_run — Done · "),
            "TUI 成功行必须带工具名与终态词：{text:?}"
        );
        assert!(!text.contains('→'), "TUI 不再用经典箭头：{text:?}");
        assert!(!text.contains("  ✓"), "TUI 结果行不再用经典缩进：{text:?}");

        // 失败行用红叉 + Failed + 输出首行预览。
        let (mut renderer, buf) = renderer_tui(OutputMode::Normal);
        renderer.on_event(&RuntimeEvent::ToolExec {
            name: "fs_write".into(),
            summary: "Writing a.txt".into(),
            args: json!({}),
            call_id: "c2".into(),
        });
        renderer.on_event(&RuntimeEvent::ToolResult {
            name: "fs_write".into(),
            ok: false,
            output: Some("permission denied".into()),
            call_id: "c2".into(),
        });
        let text = buf.text();
        assert!(text.contains("✗ fs_write — Failed · "), "{text:?}");
        assert!(text.contains("    permission denied"), "{text:?}");

        // 经典风味（默认）保持 `→ /   ✓` 的字节契约。
        let (mut classic, classic_buf, _) =
            renderer_with(OutputMode::Normal, false, Theme::Dark, NO_WRAP);
        classic.on_event(&RuntimeEvent::ToolExec {
            name: "shell_run".into(),
            summary: "Running ls".into(),
            args: json!({}),
            call_id: "c3".into(),
        });
        classic.on_event(&RuntimeEvent::ToolResult {
            name: "shell_run".into(),
            ok: true,
            output: None,
            call_id: "c3".into(),
        });
        let classic_text = classic_buf.text();
        assert!(
            classic_text.starts_with("→ Running ls\n"),
            "{classic_text:?}"
        );
        assert!(
            classic_text.contains("  ✓ Running ls · "),
            "{classic_text:?}"
        );
    }

    /// TUI 结束行：✓/✗/◼/! 字形与 classic 相同的信息量。
    #[test]
    fn tui_task_end_line_uses_status_glyphs() {
        let usage = Usage {
            input_tokens: 1234,
            output_tokens: 567,
            estimated: false,
        };
        let elapsed = Duration::from_millis(2100);
        assert_eq!(
            format_task_end_tui(
                TASK_ID,
                RunStatus::Success,
                elapsed,
                &usage,
                &[],
                3,
                OutputMode::Normal
            ),
            "✓ Done · 2.1s · 3 tools · task 019a1b2c-3d4e"
        );
        assert_eq!(
            format_task_end_tui(
                TASK_ID,
                RunStatus::Failed,
                elapsed,
                &usage,
                &[],
                0,
                OutputMode::Minimal
            ),
            "✗ Failed · 2.1s"
        );
        assert_eq!(
            format_task_end_tui(
                TASK_ID,
                RunStatus::Aborted,
                elapsed,
                &usage,
                &[],
                1,
                OutputMode::Normal
            ),
            "◼ Aborted · 2.1s · 1 tool · task 019a1b2c-3d4e"
        );
        let estimated = Usage {
            input_tokens: 1234,
            output_tokens: 567,
            estimated: true,
        };
        assert_eq!(
            format_task_end_tui(
                TASK_ID,
                RunStatus::LimitReached,
                elapsed,
                &estimated,
                &["demo".to_string()],
                2,
                OutputMode::Debug
            ),
            format!(
                "! Limit reached · 2.1s · 2 tools · tokens ~in=1234 ~out=567 · task {TASK_ID} · skills: demo"
            )
        );
    }

    /// TUI 结束行的状态字形也走调色板：`✓ Done` 整段绿色。
    #[test]
    fn tui_status_glyph_is_colored() {
        let buf = SharedBuf::default();
        let renderer = Renderer::new(
            Box::new(buf),
            RendererOptions {
                mode: OutputMode::Normal,
                color: true,
                theme: Theme::Dark,
                depth: ColorDepth::Basic16,
                width: NO_WRAP,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .with_flavor(RenderFlavor::Tui);
        let line = format_task_end_tui(
            TASK_ID,
            RunStatus::Success,
            Duration::from_millis(2100),
            &Usage::default(),
            &[],
            0,
            OutputMode::Normal,
        );
        let painted = renderer.paint_status_dot(&line, RunStatus::Success);
        assert!(painted.starts_with("\x1b[32m✓ Done\x1b[0m"), "{painted:?}");
    }

    #[test]
    fn task_end_line_pluralizes_the_tool_count() {
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 1,
            estimated: false,
        };
        let elapsed = Duration::from_millis(2100);
        let normal = |tools: usize| {
            format_task_end(
                TASK_ID,
                RunStatus::Success,
                elapsed,
                &usage,
                &[],
                tools,
                OutputMode::Normal,
            )
        };

        assert!(normal(1).contains(" · 1 tool · task "), "{}", normal(1));
        assert!(!normal(1).contains("1 tools"), "{}", normal(1));
        assert!(normal(0).contains(" · 0 tools · task "), "{}", normal(0));
        assert!(normal(2).contains(" · 2 tools · task "), "{}", normal(2));

        let debug = format_task_end(
            TASK_ID,
            RunStatus::Success,
            elapsed,
            &usage,
            &[],
            1,
            OutputMode::Debug,
        );
        assert!(debug.contains(" · 1 tool · tokens "), "{debug}");
    }

    // ---- 结束行状态点 ----

    /// 结束行的状态点按终态着色：`● {状态词}` **整段**包上槽位色（锚点子串不被拆开），剩余部分
    /// 逐字保留。输入取 [`format_task_end`] 的真实输出。
    #[test]
    fn paint_status_dot_colors_the_state_word_of_the_task_end_line() {
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 1,
            estimated: false,
        };
        let elapsed = Duration::from_millis(2100);
        let (renderer, _buf, _flag) = renderer(OutputMode::Normal, true, NO_WRAP);

        for (status, word, color) in [
            (RunStatus::Success, "Done", "\u{1b}[32m"),
            (RunStatus::Failed, "Failed", "\u{1b}[31m"),
            (RunStatus::Aborted, "Aborted", "\u{1b}[33m"),
            (RunStatus::LimitReached, "Limit reached", "\u{1b}[33m"),
        ] {
            let line =
                format_task_end(TASK_ID, status, elapsed, &usage, &[], 2, OutputMode::Normal);
            let painted = renderer.paint_status_dot(&line, status);
            let anchor = format!("● {word}");
            assert_eq!(
                painted,
                format!("{color}{anchor}\u{1b}[0m · 2.1s · 2 tools · task 019a1b2c-3d4e"),
                "{status:?}"
            );
            // 锚点整段着色：终端里能直接复制 `● Done` 去 grep，子串不被 SGR 拆开
            assert!(painted.contains(&anchor), "{status:?}: {painted}");
        }
    }

    /// `color=false` 一字不动（非 tty 的 e2e 走这条：结束行输出逐字不变）。
    #[test]
    fn paint_status_dot_is_verbatim_without_color() {
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 1,
            estimated: false,
        };
        let (off, _buf, _flag) = renderer(OutputMode::Normal, false, NO_WRAP);

        for status in [
            RunStatus::Success,
            RunStatus::Failed,
            RunStatus::Aborted,
            RunStatus::LimitReached,
        ] {
            let line = format_task_end(
                TASK_ID,
                status,
                Duration::from_millis(2100),
                &usage,
                &[],
                2,
                OutputMode::Normal,
            );
            let painted = off.paint_status_dot(&line, status);
            assert_eq!(painted, line, "{status:?} 必须逐字相等");
            assert!(!painted.contains('\u{1b}'), "{status:?}: {painted}");
        }
    }

    /// 不以 `● {该状态词}` 开头的行原样返回：正文行（哪怕正文里提到 `● Done`）、缩进过的行与
    /// 别的状态的结束行都不着色（防误伤正文）。
    #[test]
    fn paint_status_dot_leaves_lines_it_does_not_own_untouched() {
        let (renderer, _buf, _flag) = renderer(OutputMode::Normal, true, NO_WRAP);

        for line in [
            // 状态点在正文中间：含 `● Done` 子串但不是结束行
            "总结里写了 ● Done · 2.1s 这一行",
            // 状态点不在列 0：结束行以外的行都不该被认领
            "  ● Done · 2.1s",
            "· thinking (4 chars)",
            "",
        ] {
            assert_eq!(
                renderer.paint_status_dot(line, RunStatus::Success),
                line,
                "{line:?}"
            );
        }

        // 状态词对不上：不许把别的状态词包进这一档色
        let failed = "● Failed · 0.3s";
        assert_eq!(
            renderer.paint_status_dot(failed, RunStatus::Success),
            failed
        );
    }
}
