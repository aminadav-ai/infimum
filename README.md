# Infimum — Verified AI-Native Programming Stack, Sandbox Runtime & Native IDE

[![Infimum CI](https://github.com/aminadav-ai/infimum/actions/workflows/ci.yml/badge.svg)](https://github.com/aminadav-ai/infimum/actions/workflows/ci.yml)
[![License: AGPL-3.0 / Commercial](https://img.shields.io/badge/License-AGPL--3.0%20%7C%20Commercial-blue.svg)](LICENSE)

**Official Portal:** [**https://infimum-lang.org**](https://infimum-lang.org)

**Infimum** is a vertically integrated, Z3-verified programming language, capability-sandboxed Wasm runtime, and native AI-first IDE designed for safely compiling and executing **AI-generated code, plugins, interactive widgets, and micro-applications**.

- **Compile-time Z3 Formal Verification (`Infimum.Telos`):** preconditions (`req`), postconditions (`ens`), non-zero divisor proofs, bounded loops, and LLM-friendly counterexamples.
- **Unforgeable Capability Passports & Taint Tracking (`Infimum.Flux`):** `Secret<T>` leak prevention and pre-execution `infimum.passport` verification before byte 0 executes.
- **Z3-Verified Declarative Native UI (`Infimum.Forma`):** zero-HTML/CSS/JS UI layout with compile-time overflow proofs and interactive widgets.
- **Typed Memory & Direct Wasm/Native Emission (`Infimum.Strata`):** ~100–300 ms zero-`rustc` compilation to tiny `.ifw`/`.wasm` capsules (17–108 KB) and native Linux ELF64 binaries.
- **Zero-Chromium Native & WebGL2 Runtime (`infimum-web`):** Rust + `wgpu` (Vulkan/GL/WebGL2) + `wasmi` per-frame fuel sandbox + 1-draw-call SDF 2D & 3D PBR renderer.
- **Native AI-First IDE (`infimum-code`):** 108 KB pure-Infimum GPU UI + local `bubblewrap`-sandboxed Rust agent daemon (`infimum-agentd`) + built-in MCP server (`infimum mcp`).

---

## Monorepo Structure

| Path | Component | Description |
| :--- | :--- | :--- |
| [`crates/infimum-lang`](crates/infimum-lang) | **Compiler, Verifier & MCP** | `infimum` CLI, Z3 SMT verifier, `Forma` UI compiler, direct Wasm/ELF emitter, `infimum mcp`, plus `std/` and `examples/` (`Apache-2.0`). |
| [`crates/infimum-web`](crates/infimum-web) | **Capsule Sandbox Runtime** | `infimum-web` native Vulkan/wgpu & WebGL2 runtime with pre-execution `infimum.passport` gatekeeper and `wasmi` fuel sandbox. |
| [`apps/infimum-code`](apps/infimum-code) | **Native AI-First IDE** | Zero-Electron 108 KB `.ifw` IDE + isolated `infimum-agentd` Rust daemon. |

---

## Quick Start

### Prerequisites
- Rust (`stable` toolchain)
- `z3` SMT solver on `PATH` (or `~/.local/bin/z3`)

```bash
# Build all workspace binaries (infimum, infimum-web, infimum-agentd)
cargo build --release --workspace

# Run full workspace test suite
cargo test --workspace

# Verify and compile an Infimum module
./target/release/infimum verify crates/infimum-lang/examples/payment_demo.ifm
./target/release/infimum compile-wasm my_app.ifm -o my_app.ifw

# Launch a capsule in infimum-web
./target/release/infimum-web crates/infimum-lang/examples/showcase/color-lines/color_lines.ifw

# Launch Infimum Code IDE (Linux)
./apps/infimum-code/bin/infimum-code
```

---

## Licensing (Simple 3-Line Summary)

- **Open Source, Evaluation & Local Development:** Free under **GNU AGPL-3.0** (see [`LICENSE`](LICENSE)).
- **Your Code & Compiled `.ifw`/`.wasm` Capsules:** **100% yours** — `std/` and `examples/` are under **Apache-2.0**, and `LICENSE` includes a **Compiler Output & Stdlib Exception** so your compiled applications are never subject to AGPL.
- **Closed-Source Commercial Products & SaaS Embedding:** Available under a simple **Infimum Commercial License** — email **`licensing@infimum-lang.org`**.
