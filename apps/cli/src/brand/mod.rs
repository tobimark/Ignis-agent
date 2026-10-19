//! 品牌启动屏：火焰字模、三段响应（Full / Compact / Minimal）与状态块。
//!
//! 本模块是**纯函数层**：不读环境、不碰文件、不写终端——`TERM` / `COLORTERM` / `NO_COLOR`
//! 与 stdout 的 tty 状态都由命令层读好传进来（[`startup_style`] 的入参就是那几个现场值），
//! 因此每一段输出都可以在单测里逐字节断言（三档宽度黄金帧见本文件测试）。
//!
//! **字模数据**：[`flame::FULL`]（20×13）与 [`flame::COMPACT`]（12×8）由
//! `tools/gen-flame-art.py` 从 `assets/ignis-flame-transparent.png` 生成（生成期工具，
//! 不参与构建），运行时零解码、零文件 IO。
//!
//! **档位规则**（[`tier`]）：stdout 不是 tty 或终端不支持（`TERM` 缺失 / 空 / `dumb`）恒
//! [`Tier::Minimal`]；否则 `cols >= 100` 给 Full、`cols >= 60` 给 Compact，其余 Minimal。
//!
//! **哑终端保守规则**（[`startup_style`]）：`TERM` 缺失 / 空 / `dumb` 时启动屏恒 Minimal 且
//! [`ColorDepth::None`]——即使 `ui.color=always` 也一个 SGR 都不写。
//!
//! **零 SGR 门控纪律**（T1 评审固化）：调色板的槽位在 [`ColorDepth::None`] 下仍有非空串
//! （形状上不留空档），但**消费者一律按 `depth != None` 门控**——本模块里所有 `brand` /
//! `dim` / `ok` / `warn` 包裹都只发生在有色阶时。
//!
//! **状态块**（[`status_block`]）：逐字承接旧 `repl::banner_lines` 的五/六行语义，只多一层
//! 着色——`● Ready` 整体绿、未读告警行整体黄，`Workspace:` / `Runtime:` / `Session:` 三行
//! 是稳定锚点子串，保持纯文本。

use std::path::Path;

use ignis_core::config::{Theme, UiConfig};

use crate::render::{ColorDepth, color_depth, palette, resolve_color};

pub mod flame;

/// ANSI：重置。每段着色自带收尾（上色档的行尾也要收一次）。
const RESET: &str = "\u{1b}[0m";

/// ANSI：归位默认背景色。只设了前景的半格必须收一次，否则上一格的背景色会漏进来。
const BG_DEFAULT: &str = "\u{1b}[49m";

/// [`Tier::Full`] 的最小列数。
const FULL_MIN_COLS: usize = 100;

/// [`Tier::Compact`] 的最小列数。
const COMPACT_MIN_COLS: usize = 60;

/// 火焰块与右侧文字之间的间隔列数。
const GAP: &str = "  ";

/// Full 档的标语（`dim` 档）。
const TAGLINE: &str = "Your Personal Autonomous Agent";

/// 剪影字形：上下都不透明。
const BLOCK_FULL: char = '█';

/// 剪影字形：只有上半。
const BLOCK_TOP: char = '▀';

/// 剪影字形：只有下半。
const BLOCK_BOTTOM: char = '▄';

/// 火焰字模：一格一对像素（上半 / 下半）。
///
/// `0` = 透明，其余 = `0xFF_RR_GG_BB`。数据由 `tools/gen-flame-art.py` 生成（生成期工具，
/// 不参与构建），运行时零解码、零文件 IO。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Art {
    /// 列数（每行的格数）。
    pub cols: usize,
    /// 行数（半块字符的行数；每行覆盖上下两条像素行）。
    pub rows: usize,
    /// `cols * rows` 个格，按行优先排列。
    pub cells: &'static [(u32, u32)],
}

/// 启动屏档位：宽度与终端能力共同决定（判定见 [`tier`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// 宽 ≥ 100 列：20×13 火焰 + 字标 + 标语 + 状态块。
    Full,
    /// 宽 60–99 列：12×8 火焰 + 字标 + 状态块。
    Compact,
    /// 窄窗 / 非 tty / 哑终端：单行字标。
    Minimal,
}

/// 启动屏档位：`!stdout_tty || !term_supported` 恒 [`Tier::Minimal`]；否则 `cols >= 100` 给
/// Full、`cols >= 60` 给 Compact，其余 Minimal。
///
/// 两个降级方向都在最保守的那边：非 tty（管道 / 脚本）与哑终端（`TERM` 缺失 / 空 / `dumb`）
/// 不画火焰字模——半块字符与 SGR 都可能变成噪声。
pub fn tier(cols: usize, stdout_tty: bool, term_supported: bool) -> Tier {
    if !stdout_tty || !term_supported {
        return Tier::Minimal;
    }
    if cols >= FULL_MIN_COLS {
        Tier::Full
    } else if cols >= COMPACT_MIN_COLS {
        Tier::Compact
    } else {
        Tier::Minimal
    }
}

/// 启动屏样式：档位 + 色阶 + 主题——本模块其余函数只依赖这三个值，不读环境。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupStyle {
    /// 三档响应中的哪一档。
    pub tier: Tier,
    /// 终端色阶（由 [`crate::render::color_depth`] 定）。
    pub depth: ColorDepth,
    /// 配色主题。
    pub theme: Theme,
}

/// 启动屏样式：把终端现场值收敛成 [`StartupStyle`]（环境读取由命令层做，这里只做判定）。
///
/// `term_supported` 为假（`TERM` 缺失 / 空 / `dumb`）时**哑终端保守规则**生效：恒
/// `{ tier: Minimal, depth: None }`——品牌块零 SGR，即使 `ui.color=always`。否则颜色策略交给
/// [`resolve_color`]（`NO_COLOR` / `ui.color` / tty 三者的既有口径）、色阶交给
/// [`color_depth`]，档位交给 [`tier`]。
pub fn startup_style(
    ui: &UiConfig,
    cols: usize,
    stdout_tty: bool,
    term_supported: bool,
    colorterm: Option<&str>,
    term: Option<&str>,
    no_color: Option<&str>,
) -> StartupStyle {
    if !term_supported {
        return StartupStyle {
            tier: Tier::Minimal,
            depth: ColorDepth::None,
            theme: ui.theme,
        };
    }
    let color = resolve_color(ui.color, stdout_tty, no_color);
    StartupStyle {
        tier: tier(cols, stdout_tty, term_supported),
        depth: color_depth(color, colorterm, term),
        theme: ui.theme,
    }
}

/// 字模逐格呈现（每行一条字符串，不带换行）。
///
/// 每格按「上下两个像素」选字形：都不透明 → `▀`（前景取上、背景取下）；仅上 → `▀`（只设前景）；
/// 仅下 → `▄`（前景取下、背景归位默认）；全透明 → 空格（上色档先 reset，确保前一格的背景色不
/// 漏进来）。行内先截到最后一个「有不透明半格」的单元，行尾不补透明格。
///
/// 色阶：`True` 用 `38;2;r;g;b` / `48;2;r;g;b`、`Ansi256` 用 `38;5;n` / `48;5;n`
/// （n 由 [`crate::render::rgb_to_ansi256`] 算），两种情况只设前景的格都补一条背景归位；
/// 这两档的行尾再补一条 reset。`Basic16` 与 `None` 走**剪影**（`█` / `▀` / `▄` / 空格），
/// 一个 SGR 都不写——字模是任意 RGB，基本 16 色表达不了，画成彩色反而难看。
pub fn art_rows(art: &Art, depth: ColorDepth) -> Vec<String> {
    (0..art.rows)
        .map(|row| art_row(art, row, depth).0)
        .collect()
}

/// 品牌块：按档位合成启动屏上半段（火焰 + 字标 [+ 标语]）。
///
/// - [`Tier::Minimal`]：单行字标 `IGNIS v{version}`。
/// - [`Tier::Compact`]：`COMPACT` 字模的 8 行，第 1 行右侧放字标（去标语）。
/// - [`Tier::Full`]：`FULL` 字模的 13 行，第 1 行右侧字标、第 2 行右侧标语。
///
/// 字标与标语在 `depth != None` 时分别用 `bold + brand` 与 `dim` 包一层；`None` 档整块零 SGR
/// （字模本身就是剪影）。状态块见 [`status_block`]：两段分开是因为品牌块零 DB 依赖、可以
/// 先打，状态块要等 `-c` 恢复与会话计数读完之后才准。
pub fn brand_block(version: &str, style: &StartupStyle) -> Vec<String> {
    let wordmark = wordmark(version, style);
    match style.tier {
        Tier::Minimal => vec![wordmark],
        Tier::Compact => flame_with_text(&flame::COMPACT, style, &wordmark, None),
        Tier::Full => flame_with_text(&flame::FULL, style, &wordmark, Some(TAGLINE)),
    }
}

/// 状态块的事实来源：工作区、内核、技能/记忆计数、会话标签与未读告警行。
///
/// `session_label` 由调用方算（`new` 或 13 字符短 id，与 `/status` 共用同一 helper）；
/// `alerts` 已是整行文案（含 `⚠ …`），`None`（没有未读 / 计数读失败）时不追加那一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusFacts<'a> {
    /// 工作区根目录。
    pub workspace: &'a Path,
    /// 生效内核名（`native` / `codex` / `claude`）。
    pub kernel: &'a str,
    /// 已加载技能条数。
    pub skills: usize,
    /// active 事实条数（0 也照实显示 `0 facts`——记忆是「有就注入」的可选段，不是开关）。
    pub memory: usize,
    /// 会话标签（`new` 或短 id）。
    pub session_label: &'a str,
    /// 未读告警整行文案（没有时 `None`）。
    pub alerts: Option<&'a str>,
}

/// 状态块：逐字承接旧 `repl::banner_lines` 的五/六行语义，只多一层着色。
///
/// 五行固定：`● Ready` / `Workspace: {path}` / `Runtime: {kernel} · Skills: {n} · Memory: {n} facts`
/// / `Session: {label}`，`alerts` 为 `Some` 时追加第 6 行。着色只包整段：`● Ready` 走 `ok`、
/// 告警行走 `warn`；`Workspace:` / `Runtime:` / `Session:` 三行保持纯文本（锚点子串：下游测试与
/// TUI 断言靠它们定位，键名/值的局部上色会让这些锚点分家）。
pub fn status_block(style: &StartupStyle, facts: &StatusFacts) -> Vec<String> {
    let slots = palette(style.theme, style.depth);
    let mut lines = vec![
        wrap("● Ready", style, slots.ok),
        format!("Workspace: {}", facts.workspace.display()),
        format!(
            "Runtime: {} · Skills: {} · Memory: {} facts",
            facts.kernel, facts.skills, facts.memory
        ),
        format!("Session: {}", facts.session_label),
    ];
    if let Some(alerts) = facts.alerts {
        lines.push(wrap(alerts, style, slots.warn));
    }
    lines
}

/// 字标 `IGNIS v{version}`：有色阶时加粗 + 品牌色包一层（无色阶时纯文本）。
fn wordmark(version: &str, style: &StartupStyle) -> String {
    let slots = palette(style.theme, style.depth);
    wrap(
        &format!("IGNIS v{version}"),
        style,
        &format!("{}{}", slots.bold, slots.brand),
    )
}

/// 火焰块 + 右侧文字（第 1 行字标、第 2 行标语）。
///
/// 右侧文字的左边缘固定在 `art.cols + 2` 列：行尾被截断成短行的那些行按 `art.cols` 补空格，
/// 字标与标语在每一档里都落在同一列上（剪影档与上色档的可见宽度都是「格数」）。
fn flame_with_text(
    art: &Art,
    style: &StartupStyle,
    wordmark: &str,
    tagline: Option<&str>,
) -> Vec<String> {
    (0..art.rows)
        .map(|row| {
            let (mut line, cells) = art_row(art, row, style.depth);
            let text = match row {
                0 => Some(wordmark),
                1 => tagline,
                _ => None,
            };
            if let Some(text) = text {
                line.push_str(&" ".repeat(art.cols.saturating_sub(cells)));
                line.push_str(GAP);
                line.push_str(text);
            }
            line
        })
        .collect()
}

/// 呈现一行：返回「这一行的字符串」与「可见格数」（SGR 零宽，补空格对齐要知道格数）。
fn art_row(art: &Art, row: usize, depth: ColorDepth) -> (String, usize) {
    let ink = Ink::of(depth);
    let start = row * art.cols;
    let cells = art.cells.get(start..start + art.cols).unwrap_or_default();
    // 行尾先截到最后一个「有不透明半格」的单元：短行就是短行，不补透明格
    let cells = &cells[..cells
        .iter()
        .rposition(|(top, bottom)| *top != 0 || *bottom != 0)
        .map_or(0, |last| last + 1)];
    let mut line = String::new();
    for (top, bottom) in cells {
        match ink {
            Some(ink) => ink.push_cell(&mut line, *top, *bottom),
            None => line.push(silhouette(*top, *bottom)),
        }
    }
    if ink.is_some() {
        line.push_str(RESET);
    }
    (line, cells.len())
}

/// 剪影字形：全透明空格、仅上 `▀`、仅下 `▄`、上下都不透明 `█`。
fn silhouette(top: u32, bottom: u32) -> char {
    match (top, bottom) {
        (0, 0) => ' ',
        (0, _) => BLOCK_BOTTOM,
        (_, 0) => BLOCK_TOP,
        (_, _) => BLOCK_FULL,
    }
}

/// 逐格上色的两种口径（剪影档不经这里）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ink {
    /// 真彩：`38;2;r;g;b` / `48;2;r;g;b`。
    True,
    /// 256 色：`38;5;n` / `48;5;n`。
    Ansi256,
}

impl Ink {
    /// 色阶 → 上色口径；`Basic16` / `None` 给 `None`（走剪影，零 SGR）。
    fn of(depth: ColorDepth) -> Option<Self> {
        match depth {
            ColorDepth::True => Some(Self::True),
            ColorDepth::Ansi256 => Some(Self::Ansi256),
            ColorDepth::Basic16 | ColorDepth::None => None,
        }
    }

    /// 写出一格（调用方保证不透明与透明的情形已分流）。
    fn push_cell(&self, line: &mut String, top: u32, bottom: u32) {
        match (top, bottom) {
            (0, 0) => {
                // 全透明：先收回上一格留下的背景色，再打一个空格
                line.push_str(RESET);
                line.push(' ');
            }
            (0, bottom) => {
                line.push_str(&self.sequence(38, bottom));
                line.push_str(BG_DEFAULT);
                line.push(BLOCK_BOTTOM);
            }
            (top, 0) => {
                line.push_str(&self.sequence(38, top));
                line.push_str(BG_DEFAULT);
                line.push(BLOCK_TOP);
            }
            (top, bottom) => {
                line.push_str(&self.sequence(38, top));
                line.push_str(&self.sequence(48, bottom));
                line.push(BLOCK_TOP);
            }
        }
    }

    /// 一个像素 → SGR 序列：`38`/`48` + 「`2;r;g;b`（真彩）」或「`5;n`（256 色）」。
    fn sequence(&self, base: u8, pixel: u32) -> String {
        let (r, g, b) = ((pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8);
        match self {
            Self::True => format!("\u{1b}[{base};2;{r};{g};{b}m"),
            Self::Ansi256 => format!(
                "\u{1b}[{base};5;{}m",
                crate::render::rgb_to_ansi256(r, g, b)
            ),
        }
    }
}

/// 上色包裹：`prefix` 是调色板的一档（字标是 `bold + brand` 拼两段），尾部固定收一条 reset。
///
/// 门控约定（T1 评审固化）：[`ColorDepth::None`] 的槽位仍有非空串，但消费者一律按
/// `depth != None` 门控——哑终端与 `NO_COLOR` 现场必须零 SGR。
fn wrap(text: &str, style: &StartupStyle, prefix: &str) -> String {
    if style.depth == ColorDepth::None {
        return text.to_string();
    }
    format!("{prefix}{text}{RESET}")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ignis_core::config::{ColorChoice, OutputMode, Theme};

    use super::*;

    // ---- 档位与样式 ----

    #[test]
    fn tier_covers_the_three_bands() {
        assert_eq!(tier(200, true, true), Tier::Full);
        assert_eq!(tier(100, true, true), Tier::Full);
        assert_eq!(tier(99, true, true), Tier::Compact);
        assert_eq!(tier(60, true, true), Tier::Compact);
        assert_eq!(tier(59, true, true), Tier::Minimal);
        assert_eq!(tier(200, false, true), Tier::Minimal, "非 tty");
        assert_eq!(tier(200, true, false), Tier::Minimal, "哑终端");
    }

    #[test]
    fn startup_style_folds_dumb_and_no_color_to_zero_sgr() {
        let ui = UiConfig {
            color: ColorChoice::Always,
            theme: Theme::Dark,
            output_mode: OutputMode::Normal,
        };
        for (supported, no_color) in [(false, None), (true, Some("1"))] {
            let style = startup_style(
                &ui,
                200,
                true,
                supported,
                Some("truecolor"),
                Some("xterm"),
                no_color,
            );
            assert_eq!(style.depth, ColorDepth::None);
        }
        let (supported, no_color) = (false, None);
        assert_eq!(
            startup_style(
                &ui,
                200,
                true,
                supported,
                Some("truecolor"),
                Some("xterm"),
                no_color
            )
            .tier,
            Tier::Minimal,
            "哑终端恒 Minimal（即使 ui.color=always）"
        );
        let style = startup_style(&ui, 200, true, true, Some("truecolor"), Some("xterm"), None);
        assert_eq!(style.depth, ColorDepth::True, "COLORTERM 命中真彩");

        // 档位现场值：宽度与 tty 分别管档位，色阶交给 `resolve_color` + `color_depth`
        let style = startup_style(&ui, 80, true, true, Some("truecolor"), Some("xterm"), None);
        assert_eq!((style.tier, style.depth), (Tier::Compact, ColorDepth::True));
        let style = startup_style(
            &ui,
            200,
            false,
            true,
            Some("truecolor"),
            Some("xterm"),
            None,
        );
        assert_eq!(
            (style.tier, style.depth),
            (Tier::Minimal, ColorDepth::True),
            "非 tty 只降档位，颜色策略仍由 resolve_color 定"
        );

        // `never` / 主题都照传
        let never = UiConfig {
            color: ColorChoice::Never,
            theme: Theme::Light,
            output_mode: OutputMode::Minimal,
        };
        let style = startup_style(&never, 200, true, true, Some("truecolor"), None, None);
        assert_eq!(
            (style.tier, style.depth, style.theme),
            (Tier::Full, ColorDepth::None, Theme::Light)
        );
    }

    // ---- 字模几何 ----

    /// 两档字模的几何与像素形状：格数自洽、有内容也不全不透明、非零像素都带 `0xFF` 不透明前缀。
    #[test]
    fn flame_art_geometry_is_consistent() {
        assert_eq!((flame::FULL.cols, flame::FULL.rows), (20, 13));
        assert_eq!((flame::COMPACT.cols, flame::COMPACT.rows), (12, 8));

        for art in [&flame::FULL, &flame::COMPACT] {
            assert_eq!(art.cells.len(), art.cols * art.rows, "格数必须自洽");
            let opaque = art
                .cells
                .iter()
                .filter(|(top, bottom)| *top != 0 || *bottom != 0)
                .count();
            assert!(opaque > 0, "字模不能是全透明的空图");
            assert!(opaque < art.cells.len(), "字模要留出透明背景");
            for (top, bottom) in art.cells {
                for pixel in [*top, *bottom] {
                    assert!(
                        pixel == 0 || pixel >= 0xFF_00_00_00,
                        "像素值形状：{pixel:#010x}（0 之外一律 0xFF_RR_GG_BB）"
                    );
                }
            }
        }
    }

    // ---- 逐格呈现 ----

    /// 夹具（2×1）：(不透明红, 不透明蓝) + (不透明绿, 透明下)。
    const FIXTURE: Art = Art {
        cols: 2,
        rows: 1,
        cells: &[(0xFF_FF_00_00, 0xFF_00_00_FF), (0xFF_00_FF_00, 0)],
    };

    /// 夹具（3×1）：中间一列全透明（有色阶时要用 reset + 空格把前一格的背景色收回来）。
    const GAP_FIXTURE: Art = Art {
        cols: 3,
        rows: 1,
        cells: &[(0xFF_FF_00_00, 0xFF_00_00_FF), (0, 0), (0xFF_00_FF_00, 0)],
    };

    /// 夹具（2×1）：末列全透明——行尾先截到最后一个「有不透明半格」的单元，不补透明格。
    const TRAILING_FIXTURE: Art = Art {
        cols: 2,
        rows: 1,
        cells: &[(0xFF_FF_00_00, 0xFF_00_00_FF), (0, 0)],
    };

    /// 夹具（1×1）：只有下半像素——`▄` 走下半色。
    const BOTTOM_FIXTURE: Art = Art {
        cols: 1,
        rows: 1,
        cells: &[(0, 0xFF_FF_00_00)],
    };

    #[test]
    fn art_rows_renders_every_cell_kind_in_all_four_depths() {
        // 真彩：上下不透明 → fg(上) + bg(下) + ▀；仅上 → fg(上) + 归位默认背景 + ▀；
        // 仅下 → fg(下) + 归位默认背景 + ▄；行尾补一条 reset
        assert_eq!(
            art_rows(&FIXTURE, ColorDepth::True),
            ["\u{1b}[38;2;255;0;0m\u{1b}[48;2;0;0;255m▀\u{1b}[38;2;0;255;0m\u{1b}[49m▀\u{1b}[0m"]
        );
        assert_eq!(
            art_rows(&BOTTOM_FIXTURE, ColorDepth::True),
            ["\u{1b}[38;2;255;0;0m\u{1b}[49m▄\u{1b}[0m"]
        );
        // 256 色：色号由 `rgb_to_ansi256` 算（红 196 / 蓝 21 / 绿 46）
        assert_eq!(
            art_rows(&FIXTURE, ColorDepth::Ansi256),
            ["\u{1b}[38;5;196m\u{1b}[48;5;21m▀\u{1b}[38;5;46m\u{1b}[49m▀\u{1b}[0m"]
        );
        // 基本色与不上色都是剪影：一个 SGR 都不写
        assert_eq!(art_rows(&FIXTURE, ColorDepth::Basic16), ["█▀"]);
        assert_eq!(art_rows(&FIXTURE, ColorDepth::None), ["█▀"]);
        assert_eq!(art_rows(&BOTTOM_FIXTURE, ColorDepth::None), ["▄"]);
    }

    #[test]
    fn art_rows_resets_the_background_across_a_transparent_cell() {
        // 透明格：有色阶时 reset + 空格（前一格的背景色不漏进来），无色阶时就是一个空格
        assert_eq!(
            art_rows(&GAP_FIXTURE, ColorDepth::True),
            [
                "\u{1b}[38;2;255;0;0m\u{1b}[48;2;0;0;255m▀\u{1b}[0m \u{1b}[38;2;0;255;0m\u{1b}[49m▀\u{1b}[0m"
            ]
        );
        assert_eq!(
            art_rows(&GAP_FIXTURE, ColorDepth::Ansi256),
            ["\u{1b}[38;5;196m\u{1b}[48;5;21m▀\u{1b}[0m \u{1b}[38;5;46m\u{1b}[49m▀\u{1b}[0m"]
        );
        assert_eq!(art_rows(&GAP_FIXTURE, ColorDepth::None), ["█ ▀"]);
    }

    #[test]
    fn art_rows_truncates_the_trailing_transparent_cells() {
        // 行尾不补透明格：短行就是短行（右侧文字的对齐由 `brand_block` 自己补空格）
        assert_eq!(
            art_rows(&TRAILING_FIXTURE, ColorDepth::True),
            ["\u{1b}[38;2;255;0;0m\u{1b}[48;2;0;0;255m▀\u{1b}[0m"]
        );
        assert_eq!(art_rows(&TRAILING_FIXTURE, ColorDepth::Basic16), ["█"]);
    }

    // ---- 品牌块 ----

    /// 字标在不上色时是纯文本；`Minimal` 就是这一行。
    #[test]
    fn brand_block_minimal_is_the_single_wordmark_line() {
        let plain = StartupStyle {
            tier: Tier::Minimal,
            depth: ColorDepth::None,
            theme: Theme::Dark,
        };
        assert_eq!(brand_block("9.9.9", &plain), ["IGNIS v9.9.9"]);

        let colored = StartupStyle {
            tier: Tier::Minimal,
            depth: ColorDepth::True,
            theme: Theme::Dark,
        };
        assert_eq!(
            brand_block("9.9.9", &colored),
            ["\u{1b}[1m\u{1b}[38;2;247;131;64mIGNIS v9.9.9\u{1b}[0m"]
        );
    }

    #[test]
    fn brand_block_full_carries_the_wordmark_the_tagline_and_the_flame() {
        let style = StartupStyle {
            tier: Tier::Full,
            depth: ColorDepth::None,
            theme: Theme::Dark,
        };
        let lines = brand_block("9.9.9", &style);
        assert_eq!(lines.len(), flame::FULL.rows, "Full 档是字模的 13 行");
        assert!(lines[0].contains("IGNIS v9.9.9"), "{:?}", lines[0]);
        assert!(
            lines[1].contains("Your Personal Autonomous Agent"),
            "{:?}",
            lines[1]
        );
        assert!(
            lines.iter().all(|line| !line.contains('\u{1b}')),
            "无色阶零 SGR"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains('█') || line.contains('▀') || line.contains('▄')),
            "必须真的画出剪影字形：{lines:?}"
        );
        // 右侧文字落在 art.cols + 2 列：左侧（含行尾截断的短行）按 art.cols 补空格对齐
        let wordmark_left = lines[0]
            .strip_suffix("  IGNIS v9.9.9")
            .unwrap_or_else(|| panic!("第 1 行右侧是字标：{:?}", lines[0]));
        assert_eq!(
            wordmark_left.chars().count(),
            flame::FULL.cols,
            "{:?}",
            lines[0]
        );
        let tagline_left = lines[1]
            .strip_suffix("  Your Personal Autonomous Agent")
            .unwrap_or_else(|| panic!("第 2 行右侧是标语：{:?}", lines[1]));
        assert_eq!(
            tagline_left.chars().count(),
            flame::FULL.cols,
            "{:?}",
            lines[1]
        );
    }

    #[test]
    fn brand_block_compact_drops_the_tagline_and_keeps_the_flame_silhouette_plain() {
        let style = StartupStyle {
            tier: Tier::Compact,
            depth: ColorDepth::Basic16,
            theme: Theme::Dark,
        };
        let lines = brand_block("9.9.9", &style);
        assert_eq!(lines.len(), flame::COMPACT.rows, "Compact 档是字模的 8 行");
        assert!(lines[0].contains("IGNIS v9.9.9"), "{:?}", lines[0]);
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("Your Personal Autonomous Agent")),
            "Compact 不带标语：{lines:?}"
        );
        // 基本色档字模走剪影（零 SGR），只有字标那一行带着色（加粗 + 品牌黄）
        assert!(lines[0].contains("\u{1b}[1m\u{1b}[33m"), "{:?}", lines[0]);
        for line in &lines[1..] {
            assert!(!line.contains('\u{1b}'), "{line:?}");
        }
        let wordmark_left = lines[0]
            .strip_suffix("  \u{1b}[1m\u{1b}[33mIGNIS v9.9.9\u{1b}[0m")
            .unwrap_or_else(|| panic!("第 1 行右侧是字标：{:?}", lines[0]));
        assert_eq!(
            wordmark_left.chars().count(),
            flame::COMPACT.cols,
            "{:?}",
            lines[0]
        );
    }

    // ---- 状态块（承接旧 `repl::banner_lines` 的五/六行语义） ----

    /// 状态块的最小夹具。
    fn facts<'a>(alerts: Option<&'a str>) -> StatusFacts<'a> {
        StatusFacts {
            workspace: Path::new("/tmp/ws"),
            kernel: "native",
            skills: 3,
            memory: 2,
            session_label: "new",
            alerts,
        }
    }

    #[test]
    fn status_block_reproduces_the_locked_lines_without_color() {
        let style = StartupStyle {
            tier: Tier::Compact,
            depth: ColorDepth::None,
            theme: Theme::Dark,
        };
        assert_eq!(
            status_block(&style, &facts(None)),
            [
                "● Ready".to_string(),
                "Workspace: /tmp/ws".to_string(),
                "Runtime: native · Skills: 3 · Memory: 2 facts".to_string(),
                "Session: new".to_string(),
            ]
        );

        // 0 facts 照实显示（记忆是「有就注入」的可选段，不是开关）
        let zero = StatusFacts {
            memory: 0,
            ..facts(None)
        };
        assert_eq!(
            status_block(&style, &zero)[2],
            "Runtime: native · Skills: 3 · Memory: 0 facts"
        );
        assert!(
            !status_block(&style, &zero)
                .iter()
                .any(|l| l.contains("off"))
        );
    }

    #[test]
    fn status_block_appends_the_alerts_line_only_when_there_is_one() {
        let style = StartupStyle {
            tier: Tier::Compact,
            depth: ColorDepth::None,
            theme: Theme::Dark,
        };
        let lines = status_block(
            &style,
            &facts(Some("⚠ 4 unread alerts — run `ignis alerts`")),
        );
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert_eq!(lines[4], "⚠ 4 unread alerts — run `ignis alerts`");
        assert_eq!(lines[3], "Session: new", "会话行仍在第 5 行");
    }

    /// 生成物形状的字节锚点：Compact 档（60–99 列窗口，含设计稿点名的 80 列）在不上色现场的
    /// 实际输出逐字节钉在这里——字模或布局一改，这条测试必须先红。设计 §7 P1 验收里的
    /// 「三档宽度黄金帧（含 80 列 Compact）」由此成为可回归的字节基线。
    #[test]
    fn compact_brand_block_matches_the_golden_frame() {
        let style = StartupStyle {
            tier: Tier::Compact,
            depth: ColorDepth::None,
            theme: Theme::Dark,
        };
        assert_eq!(
            brand_block("9.9.9", &style),
            [
                "    █▄▄       IGNIS v9.9.9",
                "    ████▄",
                "   ███████▄",
                " ██████ ▀▀█▄",
                "██████▀    █",
                "█████",
                "▀████",
                "  ▀▀█▄▄",
            ]
        );
    }

    #[test]
    fn status_block_colors_only_the_ready_line_and_the_alerts_line() {
        let style = StartupStyle {
            tier: Tier::Full,
            depth: ColorDepth::True,
            theme: Theme::Dark,
        };
        let lines = status_block(
            &style,
            &facts(Some("⚠ 4 unread alerts — run `ignis alerts`")),
        );
        assert!(
            lines[0].contains("\u{1b}[32m● Ready\u{1b}[0m"),
            "{:?}",
            lines[0]
        );
        // 告警行整体被 warn 包一层
        assert_eq!(
            lines[4],
            "\u{1b}[33m⚠ 4 unread alerts — run `ignis alerts`\u{1b}[0m"
        );
        // 锚点子串保护：三行文案不着色（下游测试与 TUI 断言靠它们定位）
        for text in [
            "Workspace: /tmp/ws",
            "Runtime: native · Skills: 3 · Memory: 2 facts",
            "Session: new",
        ] {
            let line = lines
                .iter()
                .find(|line| line.contains(text))
                .unwrap_or_else(|| panic!("缺行：{text}"));
            assert_eq!(line, text, "锚点行必须是纯文本");
            assert!(!line.contains('\u{1b}'), "{line:?}");
        }
    }
}
