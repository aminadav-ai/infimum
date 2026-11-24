# Infimum (`infimum-lang` v0.2)

[![Infimum CI](https://github.com/aminadav-ai/infimum-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/aminadav-ai/infimum-lang/actions/workflows/ci.yml)
[![Deploy Showcase](https://github.com/aminadav-ai/infimum-lang/actions/workflows/pages.yml/badge.svg)](https://aminadav-ai.github.io/infimum-lang/)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)

> **Official Ecosystem Portal:** **[https://infimum-lang.org](https://infimum-lang.org)**  
> **Native Capsule Runtime (Vulkan/wgpu):** **[`aminadav-ai/infimum-web`](https://github.com/aminadav-ai/infimum-web)** *(Apache-2.0)*  
> **Native AI-First IDE & Examples:** **[`aminadav-ai/infimum-code`](https://github.com/aminadav-ai/infimum-code)** *(PolyForm Noncommercial 1.0.0)*  
> **Live Interactive Compiler & Taint Inspector:** **[https://aminadav-ai.github.io/infimum-lang/](https://aminadav-ai.github.io/infimum-lang/)**

**Infimum** is an AI-native programming language stack built for a world where autonomous AI agents write software and humans verify **intent, mathematical guarantees, and capability boundaries** rather than auditing tens of thousands of lines of code.

---

## The Four Sub-Languages of `infimum-lang`

| Sub-Language | Role | Highlights |
| :--- | :--- | :--- |
| **`Infimum.Telos`** | **Formal Verification (Z3 SMT)** | Compile-time preconditions (`req`), postconditions (`ens`), non-zero divisor proofs, bounded loops (`while cond max N`), vacuous-contract guards, and LLM-friendly counterexamples & repair hints. |
| **`Infimum.Flux`** | **Content-Addressed Effect Graph** | De Bruijn normalized, SHA-256 content-addressed functions with semantic deduplication, explicit capability arrows `-[Mem, Gpu, Net("..."), Fs]->`, `Secret<T>` taint pipes, and unforgeable `infimum.passport` manifests. |
| **`Infimum.Forma`** | **Declarative Z3-Verified UI Layout** | Zero-HTML/CSS native UI (`Box`, `Row`, `Col`, `Stack`, `Scroll`, `Overlay`, `Button`, `Dropdown`, `Slot`, `Text`) with compile-time Z3 overflow proofs, `fit-content`, `justify`/`align`, and TrueType metrics. |
| **`Infimum.Strata`** | **Typed Memory & Direct Native/Wasm Emission** | Typed `layout` with `struct`, `Struct[N]`, and `words[N]` (`AppMem.flock[i].x += vx;`, zero manual offset math, compile-time overlap checks), direct zero-`rustc` `.ifw`/`.wasm` emission (~100–300 ms) and `1.45 KB` Linux `x86_64` ELF64 binaries. |

---

## Embedded Z3-Verified Standard Library (`std/`)

Every binary compiled by `infimum` has zero-dependency access to the built-in standard library (`use "std/prelude.ifm";`):

- **`std/mesh3d.ifm`:** Hardware Vulkan/wgpu 3D Mesh pipeline (`mesh3d_begin`, `mesh3d_box`, `mesh3d_limb`, `mesh3d_quad`, `mesh3d_rafter_x`, up to 24 dynamic point lights, 12-layer PBR `Texture2DArray`, viewmodels).
- **`std/gfx.ifm`:** 1-draw-call SDF UI primitives (`ui_rect`, `ui_grad_rect`, `ui_line`, `ui_circle`, `ui_sphere_3d`, `ui_orb`, `ui_glow`) and hardware clip rects.
- **`std/audio.ifm`:** Zero-asset procedural audio synthesizer with 16 built-in SFX presets (`SFX_CLACK`..`SFX_WIN`, FPS combat SFX, `SFX_LION_ROAR`, `SFX_SHEEP_BAA`) plus custom `sfx_tone`.
- **`std/input.ifm`:** Layout-independent physical held-key bitmask (`key_held(KEY_W/A/S/D/...)`), 360° relative mouse look (`mouse_dx/dy`), and pointer lock (`set_pointer_lock`).
- **`std/math.ifm`, `std/color.ifm`, `std/text.ifm`, `std/fmt.ifm`, `std/theme.ifm`, `std/icons.ifm`, `std/ui.ifm`, `std/buf.ifm`, `std/json.ifm`, `std/text_edit.ifm`, `std/markdown.ifm`.**

---

## Flagship `.ifw` Showcase Apps (in [`infimum-lang/examples/showcase`](https://github.com/aminadav-ai/infimum-lang/tree/main/examples/showcase))

| Capsule | Source | Binary Size (+ External Assets) | Highlights |
| :--- | :--- | :--- | :--- |
| **Infimum Code** | [`infimum-code/capsule`](https://github.com/aminadav-ai/infimum-code/tree/main/capsule) | **`108 KB` `.ifw`** (`0 B` assets) | Full native AI IDE running on `infimum-web` + isolated `infimum-agentd` daemon |
| **SlipGate** | [`examples/slipgate`](https://github.com/aminadav-ai/infimum-lang/tree/main/examples/showcase/slipgate) | **`27.6 KB` `.ifw`** + `12.7 MB` PBR | 6-zone Quake-inspired 3D citadel FPS, 7× 1K photo-PBR texture sets (CC0), 24 dynamic lights |
| **Lion Herd** | [`examples/lion-herd`](https://github.com/aminadav-ai/infimum-lang/tree/main/examples/showcase/lion-herd) | **`19.6 KB` `.ifw`** + `9.1 MB` PBR | 3D savanna shepherd with sculpted 24-part lion, 14-part fluffy sheep, 5× 1K PBR sets (CC0), roar SFX |
| **Color Lines 98** | [`examples/color-lines`](https://github.com/aminadav-ai/infimum-lang/tree/main/examples/showcase/color-lines) | **`17.4 KB` `.ifw`** (`0 B` assets) | 3D refractive glass spheres on veined marble, written with typed `struct`s (0 raw `mem_load`/`mem_store`) |

---

## Quick Start (Local CLI)

### Prerequisites
- Rust (`stable` toolchain)
- `z3` SMT solver binary on `PATH` (or `~/.local/bin/z3`)

```bash
# Build release CLI
cargo build --release

# Verify Z3 contracts, types, effects & layout non-overlap
./target/release/infimum verify examples/payment_demo.ifm

# Compile a GUI/3D capsule directly to .ifw (zero rustc!)
./target/release/infimum compile-wasm my_app.ifm -o my_app.ifw

# Inspect an unforgeable Security Passport
./target/release/infimum ingest examples/payment_demo.ifm
./target/release/infimum passport checkout

# Start the built-in Stdio MCP Server (for Antigravity, Cursor, Claude Code, etc.)
./target/release/infimum mcp

# Run full test suite
cargo test
```

See [`INFIMUM_LANG_GUIDE.md`](https://github.com/aminadav-ai/infimum-code/blob/main/docs/INFIMUM_LANG_GUIDE.md) in `infimum-code` for the complete language reference.

---

## License

Copyright (c) 2026 Yehuda Aminadav. Dual-licensed under **GNU AGPL-3.0** OR **Infimum Commercial License** (`licensing@infimum-lang.org`), with **Compiler Output & Standard Library Exception** (`std/` and `examples/` are licensed under **Apache-2.0**). See the root [LICENSE](../../LICENSE) and [CONTRIBUTING.md](../../CONTRIBUTING.md).
