#!/usr/bin/env python3
"""火焰字模生成器（生成期工具，不参与构建、不进 CI）。

把 `assets/ignis-flame-transparent.png` 采样成半块字符画，写出
`apps/cli/src/brand/flame.rs`（编译期常量：运行时零解码、零文件 IO）。

用法：

    python3 tools/gen-flame-art.py            # 重新生成 apps/cli/src/brand/flame.rs
    python3 tools/gen-flame-art.py --check    # 只复算比对，不写文件（不一致报错退出 2）

管线（只用标准库 `struct` / `zlib`）：

1. **解码 PNG**：支持 8-bit RGBA（color type 6）与 8-bit RGB（color type 2），非隔行。
   其余形态（位深 / 色型 / 隔行）显式报错并附 IHDR 字节——不猜，交给人工判断。
   反滤波按 PNG 规范逐扫描线做（None / Sub / Up / Average / Paeth）。
2. **内容 bbox**：alpha ≥ ALPHA_THRESHOLD（64）的像素才算内容，取紧致包围盒；
   RGB（无色型）素材没有 alpha，按全部不透明处理。
3. **采样**：bbox 缩放采样到 20×13（Full）与 12×8（Compact）两个网格，每格上下两像素。
   每个半格的采样框里，alpha ≥ 64 的像素占比 ≥ COVERAGE_THRESHOLD（0.5）判不透明，颜色
   取这些像素的**均值**（box 平均取色）。
4. **输出**：每格 `(上像素, 下像素)`，`0` = 透明，其余 `0xFF_RR_GG_BB`；文件头写明
   「生成物，勿手改」与重生成命令。

网格档位是按素材实际长宽比（内容 bbox ≈ 0.72，半块格≈正方形像素）选的：13 行 ≈ 26 个
采样行、20 列与之相比略宽（≈7% 横向拉伸），观感上可接受（设计 §6.1 起的 20×13 / 12×8）。
"""

from __future__ import annotations

import struct
import sys
import zlib
from pathlib import Path

# 素材与生成物路径（脚本按仓库根定位，从哪调用都一致）。
ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets" / "ignis-flame-transparent.png"
TARGET = ROOT / "apps" / "cli" / "src" / "brand" / "flame.rs"

# alpha 阈值：半透明边缘像素按原色取用（素材为透明底，边缘近似可接受）。
ALPHA_THRESHOLD = 64
# 采样框覆盖率阈值：过半像素不透明才算这一半格有内容（否则那半格留空）。
COVERAGE_THRESHOLD = 0.5

# 两档网格：(名称, 列数, 行数)——行数是半块字符的行数（每行 2 个像素行）。
TIERS = (("FULL", 20, 13), ("COMPACT", 12, 8))

# PNG 色型 → 通道数。
CHANNELS = {0: 1, 2: 3, 4: 2, 6: 4}


def fail(message: str) -> None:
    """报错退出（错误信息中文：这是给人工看的生成期工具）。"""
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(2)


def read_png(path: Path) -> tuple[int, int, bytes]:
    """解码 PNG → `(宽, 高, RGBA 字节流)`；只认 8-bit RGB / RGBA 非隔行。"""
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        fail(f"{path} 不是 PNG（缺 8 字节签名）")
    pos = 8
    header = None
    idat = bytearray()
    while pos + 8 <= len(data):
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        kind = data[pos + 4 : pos + 8]
        chunk = data[pos + 8 : pos + 8 + length]
        if kind == b"IHDR":
            header = chunk
        elif kind == b"IDAT":
            idat += chunk
        elif kind == b"IEND":
            break
        pos += 12 + length
    if header is None or len(header) != 13:
        fail(f"{path} 缺 IHDR")
    width, height, depth, color_type, compression, filter_method, interlace = struct.unpack(
        ">IIBBBBB", header
    )
    if depth != 8 or color_type not in (2, 6) or interlace != 0:
        fail(
            f"{path} 的 IHDR 不是「8-bit RGB/RGBA 非隔行」：IHDR={header.hex()}"
            f"（位深 {depth} / 色型 {color_type} / 隔行 {interlace}）"
        )
    channels = CHANNELS[color_type]
    raw = zlib.decompress(bytes(idat))

    stride = width * channels
    if len(raw) < height * (stride + 1):
        fail(f"{path} 的 IDAT 长度不足（期望 ≥ {height * (stride + 1)}，实得 {len(raw)}）")
    pixels = bytearray(height * stride)
    previous = bytearray(stride)
    cursor = 0
    for y in range(height):
        kind = raw[cursor]
        cursor += 1
        line = bytearray(raw[cursor : cursor + stride])
        cursor += stride
        # 反滤波：左侧 a、上方 b、左上方 c（每条扫描线按 bpp 偏移取）
        for i in range(stride):
            a = line[i - channels] if i >= channels else 0
            b = previous[i]
            c = previous[i - channels] if i >= channels else 0
            if kind == 0:
                pass
            elif kind == 1:
                line[i] = (line[i] + a) & 0xFF
            elif kind == 2:
                line[i] = (line[i] + b) & 0xFF
            elif kind == 3:
                line[i] = (line[i] + (a + b) // 2) & 0xFF
            elif kind == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                predictor = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + predictor) & 0xFF
            else:
                fail(f"{path} 出现未知滤波类型 {kind}（第 {y} 行）")
        pixels[y * stride : (y + 1) * stride] = line
        previous = line

    # 统一成 RGBA：RGB 素材补 alpha=255（没有透明背景，整幅都是内容）。
    if channels == 4:
        return width, height, bytes(pixels)
    rgba = bytearray(width * height * 4)
    for index in range(width * height):
        rgba[index * 4 : index * 4 + 3] = pixels[index * channels : index * channels + 3]
        rgba[index * 4 + 3] = 255
    return width, height, bytes(rgba)


def content_bbox(width: int, height: int, rgba: bytes) -> tuple[int, int, int, int]:
    """alpha ≥ 阈值的内容包围盒（`min_x, min_y, max_x, max_y`，闭区间）。"""
    min_x, min_y, max_x, max_y = width, height, -1, -1
    for y in range(height):
        row = y * width * 4
        for x in range(width):
            if rgba[row + x * 4 + 3] >= ALPHA_THRESHOLD:
                min_x = min(min_x, x)
                max_x = max(max_x, x)
                min_y = min(min_y, y)
                max_y = max(max_y, y)
    if max_x < 0:
        fail("素材里没有 alpha ≥ 阈值的像素（整幅都透明？）")
    return min_x, min_y, max_x, max_y


def sample_half(
    width: int,
    rgba: bytes,
    x0: int,
    x1: int,
    y0: int,
    y1: int,
) -> int:
    """采样一个半格 → `0`（透明）或 `0xFF_RR_GG_BB`（不透明半格的平均色）。"""
    total = 0
    opaque = 0
    red = green = blue = 0
    for y in range(y0, y1):
        row = y * width * 4
        for x in range(x0, x1):
            index = row + x * 4
            total += 1
            if rgba[index + 3] >= ALPHA_THRESHOLD:
                opaque += 1
                red += rgba[index]
                green += rgba[index + 1]
                blue += rgba[index + 2]
    if total == 0 or opaque < total * COVERAGE_THRESHOLD:
        return 0
    return (
        0xFF000000
        | (round(red / opaque) << 16)
        | (round(green / opaque) << 8)
        | round(blue / opaque)
    )


def build_grid(
    width: int,
    rgba: bytes,
    bbox: tuple[int, int, int, int],
    cols: int,
    rows: int,
) -> list[tuple[int, int]]:
    """bbox → `cols × rows` 网格；每格 `(上像素, 下像素)`。"""
    min_x, min_y, max_x, max_y = bbox
    box_width = max_x - min_x + 1
    box_height = max_y - min_y + 1
    cells: list[tuple[int, int]] = []
    for row in range(rows):
        for col in range(cols):
            x0 = min_x + (col * box_width) // cols
            x1 = max(x0 + 1, min_x + ((col + 1) * box_width) // cols)
            top0 = min_y + (2 * row * box_height) // (2 * rows)
            top1 = max(top0 + 1, min_y + ((2 * row + 1) * box_height) // (2 * rows))
            bottom0 = min_y + ((2 * row + 1) * box_height) // (2 * rows)
            bottom1 = max(bottom0 + 1, min_y + ((2 * row + 2) * box_height) // (2 * rows))
            cells.append(
                (
                    sample_half(width, rgba, x0, x1, top0, top1),
                    sample_half(width, rgba, x0, x1, bottom0, bottom1),
                )
            )
    return cells


def render_source(grids: dict[str, list[tuple[int, int]]]) -> str:
    """`flame.rs` 的全文。"""
    lines = [
        "//! 火焰字模（生成物）：由 `tools/gen-flame-art.py` 生成，勿手改。",
        "//!",
        "//! 重新生成：`python3 tools/gen-flame-art.py`；只复算比对：`python3 tools/gen-flame-art.py --check`。",
        "//!",
        "//! 数据源：`assets/ignis-flame-transparent.png`——按 alpha ≥ 64 求内容 bbox，缩放采样成",
        "//! 两个网格（每格 = 上下两个像素，采样框过半不透明才算有内容，颜色取框内不透明像素均值）。",
        "//! 每格 `(上像素, 下像素)`；`0` = 透明，其余 = `0xFF_RR_GG_BB`。",
        "//!",
        "//! 数据块带 `#[rustfmt::skip]`：形状（一行一格）就是 `--check` 的复算口径，`cargo fmt` 不改它。",
        "",
        "use super::Art;",
        "",
    ]
    for name, cols, rows in TIERS:
        lines.append(f"/// {name} 档字模（{cols}×{rows}）。")
        # 生成物的形状（一行一格）要能被 `--check` 逐字节复算：让 rustfmt 别重排这一段
        lines.append("#[rustfmt::skip]")
        lines.append(f"pub static {name}: Art = Art {{")
        lines.append(f"    cols: {cols},")
        lines.append(f"    rows: {rows},")
        lines.append("    cells: &[")
        for row in range(rows):
            cells = grids[name][row * cols : (row + 1) * cols]
            body = ", ".join(
                f"(0x{top:08X}, 0x{bottom:08X})" for top, bottom in cells
            )
            lines.append(f"        {body}, // 第 {row + 1} 行")
        lines.append("    ],")
        lines.append("};")
        lines.append("")
    return "\n".join(lines)


def preview(grid: list[tuple[int, int]], cols: int, rows: int) -> str:
    """剪影预览（目视用：半块字符在终端里的观感只能靠看）。"""
    out = []
    for row in range(rows):
        line = ""
        for top, bottom in grid[row * cols : (row + 1) * cols]:
            line += "█" if top and bottom else "▀" if top else "▄" if bottom else " "
        out.append(line)
    return "\n".join(out)


def main() -> int:
    if not SOURCE.exists():
        fail(f"缺素材 {SOURCE}")
    width, height, rgba = read_png(SOURCE)
    bbox = content_bbox(width, height, rgba)
    grids = {name: build_grid(width, rgba, bbox, cols, rows) for name, cols, rows in TIERS}

    print(f"素材 {SOURCE.name}：{width}×{height}，内容 bbox {bbox}（alpha ≥ {ALPHA_THRESHOLD}）")
    for name, cols, rows in TIERS:
        grid = grids[name]
        opaque = sum(1 for top, bottom in grid if top or bottom)
        print(f"\n{name}（{cols}×{rows}，不透明格 {opaque}/{len(grid)}）：")
        print(preview(grid, cols, rows))

    source = render_source(grids)
    if "--check" in sys.argv[1:]:
        current = TARGET.read_text(encoding="utf-8") if TARGET.exists() else ""
        if current != source:
            fail(f"{TARGET} 与复算结果不一致（跑一次不带参数的重生成）")
        print(f"\n{TARGET} 与复算结果一致")
        return 0
    TARGET.parent.mkdir(parents=True, exist_ok=True)
    TARGET.write_text(source, encoding="utf-8")
    print(f"\n已写出 {TARGET}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
