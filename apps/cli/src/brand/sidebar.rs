//! TUI 侧边火焰：`assets/ignis-sidebar-truecolor.ans` 的 truecolor ANSI art。
//!
//! 这份图来自用户提供的 `ignis-sidebar-truecolor.ans`：11 行 × 16 列，字符是半块/四分之一块，
//! 颜色由逐字符的 `38;2;r;g;b` 前景区分。TUI 欢迎面板按可用高度采样行：
//! 空间足够时显示完整 11 行；80×24 等紧凑布局只保留 4 行。
//!
//! 经典 REPL 的 `brand_block` 仍使用 `flame.rs` 的像素字模，两者互不影响：
//! 这里只服务 TUI 的实时欢迎面板。

use crate::render::{ColorDepth, rgb_to_ansi256};

/// 用户提供的 ANSI art（编译期嵌入，运行时零文件 IO）。
const SIDEBAR_ANSI: &str = include_str!("../../../../assets/ignis-sidebar-truecolor.ans");

/// 原始图的行数。
pub const ROWS: usize = 11;

/// 原始图的可见列数。
pub const COLS: usize = 16;

/// 按 `max_rows` 采样并降级颜色：True 保留 truecolor，Ansi256 转 256 色，
/// Basic16 / None 退化为无 SGR 的剪影。
pub fn rows(max_rows: usize, depth: ColorDepth) -> Vec<String> {
    let source: Vec<&str> = SIDEBAR_ANSI
        .lines()
        .filter(|line| !line.is_empty())
        .collect();
    if source.is_empty() || max_rows == 0 {
        return Vec::new();
    }
    sample_indices(source.len(), max_rows)
        .into_iter()
        .map(|index| render_line(source[index], depth))
        .collect()
}

/// 在 `total` 行里均匀取 `max_rows` 行（包含首尾，保留火焰底部）。
fn sample_indices(total: usize, max_rows: usize) -> Vec<usize> {
    if max_rows >= total {
        return (0..total).collect();
    }
    if max_rows == 1 {
        return vec![total / 2];
    }
    (0..max_rows)
        .map(|index| index * (total - 1) / (max_rows - 1))
        .collect()
}

fn render_line(line: &str, depth: ColorDepth) -> String {
    match depth {
        ColorDepth::True => line.to_string(),
        ColorDepth::Ansi256 => truecolor_to_256(line),
        ColorDepth::Basic16 | ColorDepth::None => strip_ansi(line),
    }
}

/// `38;2;r;g;b` → `38;5;n`；其余 CSI（reset 等）原样保留。
fn truecolor_to_256(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('\u{1b}') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some(body) = tail.strip_prefix("\u{1b}[38;2;")
            && let Some(end) = body.find('m')
        {
            let mut rgb = body[..end]
                .split(';')
                .filter_map(|value| value.parse::<u8>().ok());
            if let (Some(red), Some(green), Some(blue)) = (rgb.next(), rgb.next(), rgb.next()) {
                out.push_str(&format!(
                    "\u{1b}[38;5;{}m",
                    rgb_to_ansi256(red, green, blue)
                ));
                rest = &body[end + 1..];
                continue;
            }
        }
        let end = csi_end(tail).unwrap_or(tail.len());
        out.push_str(&tail[..end]);
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// 去掉全部 CSI 序列，保留可见字符（剪影档）。
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('\u{1b}') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = csi_end(tail).unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// `text` 开头是一条 CSI 时返回它结束后的字节下标。
fn csi_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(1) != Some(&b'[') {
        return None;
    }
    let mut index = 2;
    while let Some(&byte) = bytes.get(index) {
        if (0x40..=0x7e).contains(&byte) {
            return Some(index + 1);
        }
        if !(0x20..=0x3f).contains(&byte) {
            return None;
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{COLS, ROWS, SIDEBAR_ANSI, rows};
    use crate::render::ColorDepth;
    use crate::render::width::visible_width;

    /// 资产形状：11 行、每行可见 16 列。
    #[test]
    fn sidebar_asset_has_the_expected_geometry() {
        let lines: Vec<&str> = SIDEBAR_ANSI
            .lines()
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(lines.len(), ROWS);
        for line in lines {
            assert_eq!(visible_width(&super::strip_ansi(line)), COLS, "{line:?}");
        }
    }

    /// 高度采样与颜色降级：True 保留 truecolor，256 转色号，Basic16/None 零 SGR。
    #[test]
    fn sidebar_rows_sample_to_the_budget_and_degrade_colors() {
        for max_rows in [11, 8, 6, 4, 3] {
            let art = rows(max_rows, ColorDepth::True);
            assert_eq!(art.len(), max_rows.min(ROWS));
            assert!(
                art.iter()
                    .any(|line| line.contains('█') || line.contains('▄') || line.contains('▀')),
                "{max_rows}: {art:?}"
            );
            for line in &art {
                assert!(visible_width(line) <= COLS, "{max_rows}: {line:?}");
                assert!(line.contains("\u{1b}[38;2;"), "{max_rows}: {line:?}");
            }
        }

        let ansi256 = rows(4, ColorDepth::Ansi256);
        assert!(
            ansi256.iter().all(|line| line.contains("\u{1b}[38;5;")),
            "{ansi256:?}"
        );
        assert!(
            ansi256.iter().all(|line| !line.contains("\u{1b}[38;2;")),
            "{ansi256:?}"
        );

        for depth in [ColorDepth::Basic16, ColorDepth::None] {
            let art = rows(4, depth);
            assert_eq!(art.len(), 4);
            assert!(
                art.iter().all(|line| !line.contains('\u{1b}')),
                "{depth:?}: {art:?}"
            );
            assert!(art.iter().all(|line| visible_width(line) <= COLS));
        }

        assert!(rows(0, ColorDepth::True).is_empty());
        assert_eq!(
            rows(99, ColorDepth::None).len(),
            ROWS,
            "预算超过原图时给完整图"
        );
    }
}
