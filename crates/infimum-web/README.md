# `infimum-web` — Native `.ifw` Capsule Runtime & Browser (Zero Chromium / Zero JS)

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)

`infimum-web` is the native Rust + `wgpu` (Vulkan/GL) + `winit` (Wayland/X11) + `wasmi` runtime for **Infimum** (`.ifw` / `.wasm`) applications and 3D games — completely eliminating Chromium, Electron, HTML, CSS, and JavaScript.

## Core Architecture

1. **Pre-Execution `infimum.passport` Gatekeeper (`src/passport.rs`)**
   - Parses the embedded `infimum.passport` custom section **before** instantiating WebAssembly.
   - Enforces a strict deny-by-default capability policy (`Gpu`, `Mem`, `Net(domain)`, `Fs(root)`, `Proc`).
2. **Deterministic `wasmi` Fuel Sandbox (`src/runtime.rs`)**
   - Executes `.ifw` capsules with a per-frame instruction fuel budget to prevent infinite-loop freezes.
   - Reads the unified 1-draw-call 2D SDF stream, 16 hardware clip rects, and native 3D mesh VBOs directly from linear memory.
3. **1-Draw-Call 2D SDF + HiDPI Unicode Renderer (`src/gpu.rs`, `src/atlas.rs`, `src/shader.wgsl`)**
   - Renders rounded rectangles with soft drop shadows/borders, gradients, antialiased lines, 3D raymarched orbs, and crisp HiDPI Unicode glyphs (ASCII + full Russian Cyrillic + `₪`, `€`, `✓`, `→`) in a single instanced GPU pass.
   - Supports native cursor styles (`Default`, `Pointer`, `Text`, `EwResize`).
4. **Native Vulkan/wgpu 3D Mesh + 1K Photo-PBR `Texture2DArray` (`src/mesh3d.wgsl`, `src/textures3d.rs`)**
   - Hardware 4×4 MVP, frustum clipping, `Depth32Float` Z-buffer, close-range cotangent TBN normal mapping, roughness shading, procedural sky & fog, and up to **24 dynamic point lights**.
   - Built-in 12-layer procedural PBR `Texture2DArray` that automatically upgrades to external 1K photo-PBR texture packs (`textures/<layer>_<name>_diff/norm/rough.{jpg,png}`) decoded asynchronously on a background thread behind an instant animated 2D SDF loading splash screen.
5. **Procedural Audio & Wayland Desktop Integration (`src/audio.rs`, `src/main.rs`)**
   - Built-in zero-asset synthesizer with 16 SFX presets + custom FM/waveform tones.
   - Layout-independent physical WASD + raw 360° pointer lock on Wayland (works seamlessly across keyboard layouts, including Cyrillic).
   - Automatic per-capsule window icons (`<name>.png` / `icon.png`) and GNOME Wayland `.desktop` taskbar/dock icon registration.

## Quick Start

```bash
cargo build --release

# Launch any .ifw capsule (e.g. examples from infimum-code)
./target/release/infimum-web ../infimum-lang/examples/showcase/color-lines/color_lines.ifw
./target/release/infimum-web ../infimum-lang/examples/showcase/lion-herd/lion_herd.ifw
./target/release/infimum-web ../infimum-lang/examples/showcase/slipgate/slipgate.ifw
```

Headless passport + frame smoke check (no display required):

```bash
./target/release/infimum-web --headless-check ../infimum-lang/examples/showcase/color-lines/color_lines.ifw
```

## License

Copyright (c) 2026 Yehuda Aminadav. Dual-licensed under **GNU AGPL-3.0** OR **Infimum Commercial License** (`licensing@infimum-lang.org`). See the root [LICENSE](../../LICENSE).
