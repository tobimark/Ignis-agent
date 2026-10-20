//! TUI 的大型 IGNIS 品牌横幅：ANSI Shadow 字模 + 橙金渐变 + 三档响应。
//!
//! 与经典 REPL 的 `brand_block` 完全分开：这里只服务 TUI 的实时欢迎面板，按终端宽度/内容高度
//! 选档；`ColorDepth::None`（无色/哑终端）零 SGR，`Basic16` 用黄字，真彩按列做橙→金渐变。
//! 每行都不含换行，调用方负责居中与截断。

use std::fmt::Write as _;

use ignis_core::config::Theme;

use crate::render::{ColorDepth, palette, rgb_to_ansi256};

/// ANSI Shadow 字模（33 列 × 6 行）。大档水平加倍成 66 列。
const SHADOW: [&str; 6] = [
    "██╗ ██████╗ ███╗   ██╗██╗███████╗",
    "██║██╔════╝ ████╗  ██║██║██╔════╝",
    "██║██║  ███╗██╔██╗ ██║██║███████╗",
    "██║██║   ██║██║╚██╗██║██║╚════██║",
    "██║╚██████╔╝██║ ╚████║██║███████║",
    "╚═╝ ╚═════╝ ╚═╝  ╚═══╝╚═╝╚══════╝",
];

/// 渐变的起点（火焰橙）与终点（琥珀金）。
const GRADIENT_START: (u8, u8, u8) = (255, 107, 26);
const GRADIENT_END: (u8, u8, u8) = (255, 194, 75);

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const YELLOW: &str = "\x1b[33m";

/// 横幅档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerTier {
    /// 66 列、6 行 + 两行副标题（宽 ≥140 且高度足够）。
    Large,
    /// 33 列、6 行 + 一行副标题（宽 ≥100 且高度足够）。
    Medium,
    /// 单行渐变大字标 + 版本（宽 ≥60）。
    Small,
    /// 单行 `IGNIS vX`（窄窗/矮窗）。
    Minimal,
}

/// 按终端宽度与内容区高度选档：高度不够时不硬塞 6 行字模。
pub fn tier(cols: usize, content_rows: usize) -> BannerTier {
    if cols >= 140 && content_rows >= 24 {
        BannerTier::Large
    } else if cols >= 100 && content_rows >= 18 {
        BannerTier::Medium
    } else if cols >= 60 && content_rows >= 8 {
        BannerTier::Small
    } else {
        BannerTier::Minimal
    }
}

/// 合成横幅行：字模 + 副标题。所有行都不超过档位宽度（Large 66 / Medium 33 / Small ~20）。
pub fn lines(tier: BannerTier, version: &str, depth: ColorDepth, theme: Theme) -> Vec<String> {
    match tier {
        BannerTier::Large => {
            let mut rows = art_lines(true, depth);
            rows.push(dim("Your Heraclitus.", depth, theme));
            rows.push(dim(
                &format!("v{version} · Ever-Living Fire."),
                depth,
                theme,
            ));
            rows
        }
        BannerTier::Medium => {
            let mut rows = art_lines(false, depth);
            rows.push(dim(&format!("Your Heraclitus. · v{version}"), depth, theme));
            rows
        }
        BannerTier::Small => vec![format!(
            "{} {}",
            gradient_text("IGNIS", depth),
            dim(&format!("v{version}"), depth, theme)
        )],
        BannerTier::Minimal => vec![brand_text(&format!("IGNIS v{version}"), depth, theme)],
    }
}

/// 字模行：`expanded` 时每个字符水平加倍（大档），逐行做渐变。
fn art_lines(expanded: bool, depth: ColorDepth) -> Vec<String> {
    let width = SHADOW
        .iter()
        .map(|line| {
            if expanded {
                line.chars().count() * 2
            } else {
                line.chars().count()
            }
        })
        .max()
        .unwrap_or(0);
    SHADOW
        .iter()
        .map(|line| {
            let line = if expanded {
                expand(line)
            } else {
                (*line).to_string()
            };
            gradient_line(&line, width, depth)
        })
        .collect()
}

/// 水平加倍：每个字符重复两次（`╗` 变成 `╗╗`，笔画更厚、宽度翻倍）。
fn expand(line: &str) -> String {
    let mut out = String::with_capacity(line.len() * 2);
    for ch in line.chars() {
        out.push(ch);
        out.push(ch);
    }
    out
}

/// 一行按列做橙→金渐变；空格不写 SGR（但占用渐变位置）。
fn gradient_line(line: &str, width: usize, depth: ColorDepth) -> String {
    match depth {
        ColorDepth::None => line.to_string(),
        ColorDepth::Basic16 => format!("{BOLD}{YELLOW}{line}{RESET}"),
        ColorDepth::True | ColorDepth::Ansi256 => {
            let denominator = width.saturating_sub(1).max(1);
            let mut out = String::with_capacity(line.len() * 4);
            let mut last: Option<(u8, u8, u8)> = None;
            let mut colored = false;
            for (column, ch) in line.chars().enumerate() {
                if ch == ' ' {
                    out.push(ch);
                    continue;
                }
                let rgb = gradient_color(column, denominator);
                if last != Some(rgb) {
                    match depth {
                        ColorDepth::True => {
                            let _ = write!(out, "\x1b[38;2;{};{};{}m", rgb.0, rgb.1, rgb.2);
                        }
                        ColorDepth::Ansi256 => {
                            let _ =
                                write!(out, "\x1b[38;5;{}m", rgb_to_ansi256(rgb.0, rgb.1, rgb.2));
                        }
                        ColorDepth::Basic16 | ColorDepth::None => {}
                    }
                    last = Some(rgb);
                    colored = true;
                }
                out.push(ch);
            }
            if colored {
                out.push_str(RESET);
            }
            out
        }
    }
}

/// 渐变取样：整数插值，避免浮点误差与 clippy 的浮点比较。
fn gradient_color(column: usize, denominator: usize) -> (u8, u8, u8) {
    let t = column.min(denominator);
    let mix = |start: u8, end: u8| -> u8 {
        ((u16::from(start) * (denominator as u16 - t as u16) + u16::from(end) * t as u16)
            / denominator as u16) as u8
    };
    (
        mix(GRADIENT_START.0, GRADIENT_END.0),
        mix(GRADIENT_START.1, GRADIENT_END.1),
        mix(GRADIENT_START.2, GRADIENT_END.2),
    )
}

/// 单行渐变大字标（Small 档）。
pub fn gradient_text(text: &str, depth: ColorDepth) -> String {
    gradient_line(text, text.chars().count(), depth)
}

/// 品牌色加粗文本（Minimal 档）。
fn brand_text(text: &str, depth: ColorDepth, theme: Theme) -> String {
    if depth == ColorDepth::None {
        return text.to_string();
    }
    let slots = palette(theme, depth);
    format!("{}{}{}{}", slots.bold, slots.brand, text, RESET)
}

/// dim 文本（副标题）。
fn dim(text: &str, depth: ColorDepth, theme: Theme) -> String {
    if depth == ColorDepth::None {
        return text.to_string();
    }
    let slots = palette(theme, depth);
    format!("{}{}{}", slots.dim, text, RESET)
}

#[cfg(test)]
mod tests {
    use super::{BannerTier, art_lines, lines, tier};
    use crate::render::ColorDepth;
    use crate::render::width::visible_width;
    use ignis_core::config::Theme;

    #[test]
    fn tier_covers_the_breakpoints() {
        assert_eq!(tier(180, 40), BannerTier::Large);
        assert_eq!(tier(140, 24), BannerTier::Large);
        assert_eq!(tier(139, 40), BannerTier::Medium);
        assert_eq!(tier(100, 18), BannerTier::Medium);
        assert_eq!(tier(99, 30), BannerTier::Small);
        assert_eq!(tier(80, 24), BannerTier::Small);
        assert_eq!(tier(60, 8), BannerTier::Small);
        assert_eq!(tier(59, 24), BannerTier::Minimal);
        assert_eq!(tier(180, 20), BannerTier::Medium, "高度不足时降档");
        assert_eq!(tier(80, 6), BannerTier::Minimal, "矮窗不硬塞多行");
    }

    #[test]
    fn art_fits_its_tier_and_never_emits_newlines() {
        let medium = art_lines(false, ColorDepth::None);
        assert_eq!(medium.len(), 6);
        assert!(
            medium.iter().all(|line| visible_width(line) <= 33),
            "{medium:?}"
        );

        let large = art_lines(true, ColorDepth::None);
        assert_eq!(large.len(), 6);
        assert!(
            large.iter().all(|line| visible_width(line) <= 66),
            "{large:?}"
        );
        assert!(
            visible_width(&large[0]) > visible_width(&medium[0]),
            "大档必须比中档宽"
        );
        for line in medium.iter().chain(large.iter()) {
            assert!(!line.contains('\n'));
        }
    }

    #[test]
    fn no_color_emits_zero_sgr() {
        for tier in [
            BannerTier::Large,
            BannerTier::Medium,
            BannerTier::Small,
            BannerTier::Minimal,
        ] {
            let rows = lines(tier, "9.9.9", ColorDepth::None, Theme::Dark);
            assert!(!rows.is_empty(), "{tier:?}");
            for row in rows {
                assert!(!row.contains('\u{1b}'), "{tier:?}: {row:?}");
            }
        }
    }

    #[test]
    fn truecolor_uses_the_orange_gold_gradient() {
        let rows = lines(BannerTier::Large, "9.9.9", ColorDepth::True, Theme::Dark);
        let flame = rows[..6].join("\n");
        assert!(flame.contains("\x1b[38;2;255;"), "{flame:?}");
        assert!(flame.contains("\x1b[38;2;"), "{flame:?}");
        assert!(flame.contains('█'), "{flame:?}");
        assert!(flame.ends_with("\x1b[0m"), "行尾必须 reset：{flame:?}");

        // 256 色档用 `38;5;n`，不是真彩。
        let rows = lines(
            BannerTier::Medium,
            "9.9.9",
            ColorDepth::Ansi256,
            Theme::Dark,
        );
        assert!(rows[0].contains("\x1b[38;5;"), "{:?}", rows[0]);
        assert!(!rows[0].contains("\x1b[38;2;"), "{:?}", rows[0]);
    }

    /// 品牌文案：The Living Intelligence / Autonomous Agent 已替换为新的火焰叙事。
    #[test]
    fn taglines_use_the_new_brand_copy() {
        let medium = lines(BannerTier::Medium, "9.9.9", ColorDepth::None, Theme::Dark);
        assert_eq!(
            medium.last().map(String::as_str),
            Some("Your Heraclitus. · v9.9.9")
        );
        let large = lines(BannerTier::Large, "9.9.9", ColorDepth::None, Theme::Dark);
        let joined = large.join("\n");
        assert!(joined.contains("Your Heraclitus."), "{joined}");
        assert!(joined.contains("v9.9.9 · Ever-Living Fire."), "{joined}");
        assert!(!joined.contains("The Living Intelligence"), "{joined}");
        assert!(!joined.contains("Autonomous Agent"), "{joined}");
    }

    #[test]
    fn small_and_minimal_degrade_to_a_single_wordmark() {
        let small = lines(BannerTier::Small, "9.9.9", ColorDepth::None, Theme::Dark);
        assert_eq!(small.len(), 1);
        assert!(small[0].starts_with("IGNIS"), "{small:?}");
        assert!(small[0].contains("v9.9.9"), "{small:?}");

        let minimal = lines(BannerTier::Minimal, "9.9.9", ColorDepth::None, Theme::Dark);
        assert_eq!(minimal, ["IGNIS v9.9.9"]);
    }

    #[test]
    fn visible_width_ignores_gradient_sgr() {
        let rows = lines(BannerTier::Large, "9.9.9", ColorDepth::True, Theme::Dark);
        for row in &rows[..6] {
            assert!(
                visible_width(row) <= 66,
                "SGR 不能算进宽度：{}",
                visible_width(row)
            );
        }
    }
}
