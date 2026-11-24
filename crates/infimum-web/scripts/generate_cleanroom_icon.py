#!/usr/bin/env python3
"""
100% Clean-Room Procedural Mathematical Icon Generator for Infimum Code.
Copyright (c) 2026 Yehuda Aminadav. Zero external assets, zero stock art, zero AI datasets.
Synthesizes multi-resolution RGBA PNGs purely from analytical SDF equations.
"""

import math
import struct
import zlib
from pathlib import Path


def write_rgba_png(path: Path, width: int, height: int, rgba_bytes: bytes) -> None:
    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    raw_rows = bytearray()
    stride = width * 4
    for y in range(height):
        raw_rows.append(0)  # Filter type 0 (None)
        raw_rows.extend(rgba_bytes[y * stride : (y + 1) * stride])

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw_rows), 9))
        + chunk(b"IEND", b"")
    )
    path.write_bytes(png)


def sd_round_box(px: float, py: float, bx: float, by: float, r: float) -> float:
    qx = abs(px) - bx + r
    qy = abs(py) - by + r
    out = math.hypot(max(qx, 0.0), max(qy, 0.0))
    return min(max(qx, qy), 0.0) + out - r


def sd_segment(px: float, py: float, ax: float, ay: float, bx: float, by: float) -> float:
    pax, pay = px - ax, py - ay
    bax, bay = bx - ax, by - ay
    denom = max(bax * bax + bay * bay, 1e-6)
    h = max(0.0, min(1.0, (pax * bax + pay * bay) / denom))
    return math.hypot(pax - bax * h, pay - bay * h)


def smoothstep(e0: float, e1: float, x: float) -> float:
    t = max(0.0, min(1.0, (x - e0) / (e1 - e0)))
    return t * t * (3.0 - 2.0 * t)


def render_icon(size: int) -> bytes:
    buf = bytearray(size * size * 4)
    inv = 2.0 / size
    aa = 2.4 / size

    # 3 Connected Semantic Graph Vertices (Apex, Bottom-Left, Bottom-Right) + Center Core
    v_top = (0.0, -0.44)
    v_bl = (-0.42, 0.34)
    v_br = (0.42, 0.34)
    v_mid = (0.0, 0.05)

    edges = [
        (v_top, v_bl),
        (v_top, v_br),
        (v_bl, v_br),
        (v_top, v_mid),
        (v_bl, v_mid),
        (v_br, v_mid),
    ]
    nodes = [
        (v_top[0], v_top[1], 0.135, (52, 211, 153)),   # Emerald Z3 Apex
        (v_bl[0], v_bl[1], 0.12, (82, 152, 255)),      # Sapphire Flux Left
        (v_br[0], v_br[1], 0.12, (82, 152, 255)),      # Sapphire Strata Right
        (v_mid[0], v_mid[1], 0.095, (250, 204, 21)),   # Golden Core Proof Node
    ]

    idx = 0
    for py_i in range(size):
        ny = (py_i + 0.5) * inv - 1.0
        for px_i in range(size):
            nx = (px_i + 0.5) * inv - 1.0

            # 1. Rounded Squircle Body
            d_box = sd_round_box(nx, ny, 0.88, 0.88, 0.36)
            alpha_box = 1.0 - smoothstep(-aa, aa, d_box)
            if alpha_box <= 0.001:
                idx += 4
                continue

            # Deep obsidian-sapphire radial + diagonal gradient
            grad_t = max(0.0, min(1.0, (nx * 0.45 + ny * 0.65 + 0.85) / 1.7))
            r = 11.0 * (1.0 - grad_t) + 24.0 * grad_t
            g = 18.0 * (1.0 - grad_t) + 41.0 * grad_t
            b = 36.0 * (1.0 - grad_t) + 78.0 * grad_t

            # Subtle ambient emerald-blue inner aura
            aura = math.exp(-3.2 * (nx * nx + (ny - 0.02) * (ny - 0.02)))
            r += 16.0 * aura
            g += 58.0 * aura
            b += 92.0 * aura

            # Outer crisp metallic rim
            rim = (1.0 - smoothstep(-0.045, -0.015, d_box)) * smoothstep(-0.085, -0.045, d_box)
            r = r * (1.0 - 0.55 * rim) + 96.0 * 0.55 * rim
            g = g * (1.0 - 0.55 * rim) + 165.0 * 0.55 * rim
            b = b * (1.0 - 0.55 * rim) + 250.0 * 0.55 * rim

            # 2. Glowing Faceted Prism & Graph Edges
            d_edge = 999.0
            for (ax, ay), (bx, by) in edges:
                d_edge = min(d_edge, sd_segment(nx, ny, ax, ay, bx, by))

            glow = math.exp(-18.0 * max(d_edge, 0.0)) * 0.55
            r += 52.0 * glow
            g += 195.0 * glow
            b += 245.0 * glow

            line_cov = 1.0 - smoothstep(0.030 - aa, 0.030 + aa, d_edge)
            if line_cov > 0.0:
                lt = max(0.0, min(1.0, (ny + 0.45) / 0.85))
                lr = 52.0 * (1.0 - lt) + 82.0 * lt
                lg = 211.0 * (1.0 - lt) + 152.0 * lt
                lb = 153.0 * (1.0 - lt) + 255.0 * lt
                r = r * (1.0 - line_cov) + lr * line_cov
                g = g * (1.0 - line_cov) + lg * line_cov
                b = b * (1.0 - line_cov) + lb * line_cov

            # 3. Crisp Luminous Graph Nodes with White Specular Centers
            for cx, cy, rad, (nr, ng, nb) in nodes:
                dn = math.hypot(nx - cx, ny - cy)
                ncov = 1.0 - smoothstep(rad - aa, rad + aa, dn)
                if ncov > 0.0:
                    r = r * (1.0 - ncov) + nr * ncov
                    g = g * (1.0 - ncov) + ng * ncov
                    b = b * (1.0 - ncov) + nb * ncov
                icov = 1.0 - smoothstep(rad * 0.42 - aa, rad * 0.42 + aa, dn)
                if icov > 0.0:
                    r = r * (1.0 - icov) + 248.0 * icov
                    g = g * (1.0 - icov) + 252.0 * icov
                    b = b * (1.0 - icov) + 255.0 * icov

            buf[idx + 0] = min(255, max(0, int(round(r))))
            buf[idx + 1] = min(255, max(0, int(round(g))))
            buf[idx + 2] = min(255, max(0, int(round(b))))
            buf[idx + 3] = min(255, max(0, int(round(alpha_box * 255.0))))
            idx += 4

    return bytes(buf)


def main() -> None:
    web_dir = Path(__file__).resolve().parent.parent / "web"
    icons_dir = web_dir / "icons"
    icons_dir.mkdir(parents=True, exist_ok=True)
    for sz in (512, 256, 192, 128, 64, 48):
        out = icons_dir / f"icon-{sz}.png"
        write_rgba_png(out, sz, sz, render_icon(sz))
        print(f"Generated {out} ({out.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
