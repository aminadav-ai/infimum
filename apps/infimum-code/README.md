# Infimum Code — Native AI-First IDE, Isolated Agent Daemon & `.ifw` Examples

[![Infimum Code CI](https://github.com/aminadav-ai/infimum-code/actions/workflows/ci.yml/badge.svg)](https://github.com/aminadav-ai/infimum-code/actions/workflows/ci.yml)

**Ecosystem Portal:** [**https://infimum-lang.org**](https://infimum-lang.org) • **Compiler:** [`aminadav-ai/infimum-lang`](https://github.com/aminadav-ai/infimum-lang) • **Native Runtime:** [`aminadav-ai/infimum-web`](https://github.com/aminadav-ai/infimum-web)

**Infimum Code** is a zero-Electron, native AI coding environment whose entire UI is compiled from **Infimum Lang** (`capsule/infimum_code.ifw`, **`108 KB`**) and rendered natively on Vulkan/wgpu inside [`infimum-web`](https://github.com/aminadav-ai/infimum-web), communicating over authenticated loopback HTTP (`127.0.0.1` + 256-bit Bearer token) with the isolated **`infimum-agentd`** Rust daemon.

> **Privacy & Local-First Guarantee:** Zero telemetry, 100% local execution, API keys never leave your machine (`~/.env` with `0600` permissions), direct provider calls with no cloud proxy, and strict single-folder `bubblewrap` (`bwrap`) sandboxing.

---

## Key Capabilities

1. **Pure-Infimum Native GPU UI (`capsule/src/*.ifm` -> `capsule/infimum_code.ifw`):**
   - Built with `Infimum.Forma` + 1-draw-call SDF GPU rendering, dark/light themes, scalable HiDPI Cyrillic/Latin typography (`80%–200%`), draggable sidebar width splitter, Markdown & table rendering, live "Ход работы" agent timeline, image paste (`Ctrl+V`), and delete confirmation dialogs for projects and individual conversations.
2. **Multi-Provider LLM Menu & In-App API Key Management:**
   - Cascading provider submenus: **Google Gemini**, **OpenAI (`GPT-4.1`, `GPT-4.1 mini`, `o4-mini`)**, **DeepSeek**, **OpenRouter**, **Groq**, **Z.AI**, and **Mistral**.
   - Enter, save (`~/.env` with `0600` permissions), and live-test provider API keys directly inside Settings — no manual config editing required.
3. **Strict Single-Folder Sandbox & Semantic Infimum Tools (`daemon/src/agent.rs`):**
   - Terminal commands run inside `bubblewrap` (`bwrap`) locked strictly to the active project folder with sensitive files (`.env`, `*.key`) blocked.
   - Built-in semantic & Infimum tools for AI agents: `get_ifm_symbol`, `replace_ifm_symbol`, `find_ifm_callers`, `read_infimum_guide`, `read_infimum_std` (including `mesh3d.ifm`, `audio.ifm`, `input.ifm`), `read_infimum_template` (`ui`, `game2d`, `game3d`), `verify_infimum_file`, `compile_infimum_wasm`, and `run_infimum_capsule`.
4. **Apache-2.0 Stdio MCP Server (now in `infimum-lang` via `infimum mcp`):**
   - Exposes `list_symbols`, `get_symbol`, `replace_symbol`, `find_callers`, `infimum_verify`, and `infimum_compile_wasm` for both `.ifm` and `.rs` files to external MCP clients such as Antigravity, Cursor, and Claude Code.
5. **Bundled Native `.ifw` Showcase Games (`examples/`):**
   - [**`examples/color-lines/`**](examples/color-lines) (`17.4 KB` `.ifw`, **`0 B` external assets**) — 3D refractive glass & marble Color Lines 98 written with typed Infimum `struct`s (zero raw `mem_load`/`mem_store`), BFS pathfinding, procedural audio, and 128-turn Time-Travel.
   - [**`examples/lion-herd/`**](examples/lion-herd) (`19.6 KB` `.ifw` code + **`9.1 MB`** external textures) — 3D PBR savanna shepherd with 24-part sculpted lion, 14-part fluffy sheep, 5× 1K Poly Haven CC0 PBR texture sets, and procedural roar/bleat SFX.
   - [**`examples/slipgate/`**](examples/slipgate) (`27.6 KB` `.ifw` code + **`12.7 MB`** external textures) — 6-zone Quake-inspired 3D citadel FPS with 7× 1K Poly Haven CC0 PBR texture sets, 24 dynamic lights, Gold Key gate, and procedural FPS audio.

---

## Quick Start (Linux)

```bash
# 1. From the monorepo root, build the workspace (`infimum`, `infimum-web`, `infimum-agentd`)
cargo build --release --workspace

# 2. Launch Infimum Code
./apps/infimum-code/bin/infimum-code
```

---

## License

Copyright (c) 2026 Yehuda Aminadav. Dual-licensed under **GNU AGPL-3.0** OR **Infimum Commercial License** (`licensing@infimum-lang.org`). See the root [LICENSE](../../LICENSE).

All showcase examples in [`crates/infimum-lang/examples/`](../../crates/infimum-lang/examples/) are licensed under **Apache License 2.0**, and bundled Poly Haven PBR textures are **CC0 1.0 Public Domain**.
