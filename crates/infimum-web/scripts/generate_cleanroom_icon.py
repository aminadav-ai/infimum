#!/usr/bin/env python3
"""
Generate canonical Infimum application icons matching infimum-site/public/favicon.svg.

Design (64x64 viewBox):
- Rounded square plate: x=2, y=2, w=60, h=60, rx=14, fill=#0b0e14, stroke=#263042 (width=2)
- Upper white circle:   cx=32, cy=24, r=10, fill=#ffffff
- Lower white pill bar: x=14, y=43, w=36, h=6, rx=3, fill=#ffffff

Uses pure Python stdlib with 8x8 supersampling (64 samples/pixel) for crisp anti-aliased
PNGs across all standard desktop/PWA sizes, plus writes the canonical SVG.
"""

import math
import shutil
import struct
import zlib
from pathlib import Path

SVG_CONTENT = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64">
  <rect x="2" y="2" width="60" height="60" rx="14" fill="#0b0e14" stroke="#263042" stroke-width="2"/>
  <circle cx="32" cy="24" r="10" fill="#ffffff"/>
  <rect x="14" y="43" width="36" height="6" rx="3" fill="#ffffff"/>
</svg>
"""

BG_COLOR = (0x0B, 0x0E, 0x14, 0xFF)
STROKE_COLOR = (0x26, 0x30, 0x42, 0xFF)
FG_COLOR = (0xFF, 0xFF, 0xFF, 0xFF)


def sd_rounded_box(px: float, py: float, cx: float, cy: float, hx: float, hy: float, r: float) -> float:
    """Exact signed distance to a centered rounded rectangle (negative inside)."""
    qx = abs(px - cx) - (hx - r)
    qy = abs(py - cy) - (hy - r)
    ox = max(qx, 0.0)
    oy = max(qy, 0.0)
    return math.hypot(ox, oy) + min(max(qx, qy), 0.0) - r


def sd_circle(px: float, py: float, cx: float, cy: float, r: float) -> float:
    return math.hypot(px - cx, py - cy) - r


def over(dst: tuple[float, float, float, float], src: tuple[float, float, float, float]) -> tuple[float, float, float, float]:
    """Porter-Duff source-over compositing in straight RGBA (0..255)."""
    sa = src[3] / 255.0
    da = dst[3] / 255.0
    out_a = sa + da * (1.0 - sa)
    if out_a <= 1e-6:
        return (0.0, 0.0, 0.0, 0.0)
    inv_a = 1.0 / out_a
    r = (src[0] * sa + dst[0] * da * (1.0 - sa)) * inv_a
    g = (src[1] * sa + dst[1] * da * (1.0 - sa)) * inv_a
    b = (src[2] * sa + dst[2] * da * (1.0 - sa)) * inv_a
    return (r, g, b, out_a * 255.0)


def sample_point(u: float, v: float) -> tuple[float, float, float, float]:
    """Evaluate exact SVG layers at viewBox coordinates (u, v) in [0, 64]."""
    # Base rounded rect: x=2..62, y=2..62 => center=(32,32), half=(30,30), rx=14
    # SVG stroke-width=2 is centered on the path: outer at +1, inner at -1.
    d_plate = sd_rounded_box(u, v, 32.0, 32.0, 30.0, 30.0, 14.0)

    col = (0.0, 0.0, 0.0, 0.0)
    if d_plate <= 0.0:
        col = (float(BG_COLOR[0]), float(BG_COLOR[1]), float(BG_COLOR[2]), 255.0)
    if abs(d_plate) <= 1.0:
        col = over(col, (float(STROKE_COLOR[0]), float(STROKE_COLOR[1]), float(STROKE_COLOR[2]), 255.0))

    # White circle: cx=32, cy=24, r=10
    d_circle = sd_circle(u, v, 32.0, 24.0, 10.0)
    # White pill bar: x=14..50, y=43..49 => center=(32,46), half=(18,3), rx=3
    d_bar = sd_rounded_box(u, v, 32.0, 46.0, 18.0, 3.0, 3.0)

    if d_circle <= 0.0 or d_bar <= 0.0:
        col = over(col, (float(FG_COLOR[0]), float(FG_COLOR[1]), float(FG_COLOR[2]), 255.0))

    return col


def render_icon(size: int, ss: int = 8) -> bytes:
    """Render size x size RGBA8 icon with ss x ss supersampling."""
    scale = 64.0 / size
    inv_ss = 1.0 / ss
    inv_n = 1.0 / (ss * ss)
    out = bytearray(size * size * 4)

    for py in range(size):
        row_off = py * size * 4
        for px in range(size):
            # Fast-path check at pixel center for completely flat interior/exterior pixels
            cu = (px + 0.5) * scale
            cv = (py + 0.5) * scale
            d_plate = sd_rounded_box(cu, cv, 32.0, 32.0, 30.0, 30.0, 14.0)
            d_circle = sd_circle(cu, cv, 32.0, 24.0, 10.0)
            d_bar = sd_rounded_box(cu, cv, 32.0, 46.0, 18.0, 3.0, 3.0)
            margin = scale * 1.5

            if d_plate > 1.0 + margin:
                # Fully transparent outside plate
                continue
            if d_plate < -1.0 - margin and d_circle > margin and d_bar > margin:
                # Fully inside dark background plate
                idx = row_off + px * 4
                out[idx : idx + 4] = bytes(BG_COLOR)
                continue
            if d_circle < -margin or d_bar < -margin:
                # Fully inside white foreground glyph
                idx = row_off + px * 4
                out[idx : idx + 4] = bytes(FG_COLOR)
                continue

            # Supersample boundary pixels (premultiplied accumulation)
            acc_r = acc_g = acc_b = acc_a = 0.0
            for sy in range(ss):
                v = (py + (sy + 0.5) * inv_ss) * scale
                for sx in range(ss):
                    u = (px + (sx + 0.5) * inv_ss) * scale
                    r, g, b, a = sample_point(u, v)
                    af = a / 255.0
                    acc_r += r * af
                    acc_g += g * af
                    acc_b += b * af
                    acc_a += af

            avg_a = acc_a * inv_n
            idx = row_off + px * 4
            if avg_a > 1e-6:
                inv_avg_a = 1.0 / (avg_a * ss * ss)
                out[idx] = max(0, min(255, int(round(acc_r * inv_avg_a))))
                out[idx + 1] = max(0, min(255, int(round(acc_g * inv_avg_a))))
                out[idx + 2] = max(0, min(255, int(round(acc_b * inv_avg_a))))
                out[idx + 3] = max(0, min(255, int(round(avg_a * 255.0))))
    return bytes(out)


def write_png(path: Path, width: int, height: int, rgba: bytes) -> None:
    def chunk(tag: bytes, data: bytes) -> bytes:
        crc = zlib.crc32(tag + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", crc)

    raw = bytearray()
    stride = width * 4
    for y in range(height):
        raw.append(0)  # Filter type 0 (None)
        raw.extend(rgba[y * stride : (y + 1) * stride])

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(png)


def main() -> None:
    crate_root = Path(__file__).resolve().parent.parent
    repo_root = crate_root.parent.parent
    out_dir = crate_root / "icons"
    out_dir.mkdir(parents=True, exist_ok=True)

    svg_path = out_dir / "icon.svg"
    svg_path.write_text(SVG_CONTENT, encoding="utf-8")
    print(f"Generated {svg_path}")

    for sz in (512, 256, 192, 128, 64, 48):
        ss = 8 if sz <= 256 else 4
        rgba = render_icon(sz, ss=ss)
        out_path = out_dir / f"icon-{sz}.png"
        write_png(out_path, sz, sz, rgba)
        print(f"Generated {out_path} ({sz}x{sz})")

    capsule_icon = repo_root / "apps" / "infimum-code" / "capsule" / "icon.png"
    if capsule_icon.parent.is_dir():
        shutil.copy2(out_dir / "icon-256.png", capsule_icon)
        print(f"Synced    {capsule_icon} (256x256)")


if __name__ == "__main__":
    main()
