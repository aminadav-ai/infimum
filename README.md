# Infimum — Verified AI-Native Programming Stack, Sandbox Runtime & IDE

[![Infimum CI](https://github.com/aminadav-ai/infimum/actions/workflows/ci.yml/badge.svg)](https://github.com/aminadav-ai/infimum/actions/workflows/ci.yml)
[![License: AGPL-3.0 / Commercial](https://img.shields.io/badge/License-AGPL--3.0%20%7C%20Commercial-blue.svg)](LICENSE)
[![Compiler Output: Apache-2.0](https://img.shields.io/badge/Output%20License-Apache--2.0-green.svg)](LICENSE)
[![Platform: Linux | WebGL2](https://img.shields.io/badge/Platform-Linux%20%7C%20WebGL2%20(Wasm)-orange.svg)](https://infimum-lang.org)

**Official Portal & Interactive Sandbox:** [**https://infimum-lang.org**](https://infimum-lang.org)

---

**Infimum** is a vertically integrated, formally verified programming language and execution platform engineered specifically for the age of autonomous AI coders.

When Large Language Models generate code, traditional compilers only check syntactic and basic type correctness — runtime crashes, edge-case arithmetic bugs, security leaks, and memory overflows slip straight into production. **Infimum eliminates these risks mathematically at compile time with zero runtime overhead.**

```
   ┌─────────────────────────────────────────────────────────────┐
   │                    AI Agent / Developer                     │
   └──────────────────────────────┬──────────────────────────────┘
                                  │ .ifm source
                                  ▼
   ┌─────────────────────────────────────────────────────────────┐
   │                   Infimum Compiler (Rust)                   │
   │  ┌───────────────────────┐       ┌───────────────────────┐  │
   │  │   Infimum.Telos       │       │    Infimum.Flux       │  │
   │  │   Z3 Formal Verifier  │       │ Capability Passports  │  │
   │  │ (req/ens, div0, loops)│       │ (Secret<T> & Taint)   │  │
   │  └───────────────────────┘       └───────────────────────┘  │
   │  ┌───────────────────────┐       ┌───────────────────────┐  │
   │  │   Infimum.Forma       │       │    Infimum.Strata     │  │
   │  │ Z3-Proven Native UI   │       │ Direct Wasm/ELF (0-rs)│  │
   │  └───────────────────────┘       └───────────────────────┘  │
   └──────────────────────────────┬──────────────────────────────┘
                                  │ .ifw capsule (17–108 KB)
                                  ▼
   ┌─────────────────────────────────────────────────────────────┐
   │                Zero-Chromium Native Runtime                 │
   │       infimum-web (wgpu Vulkan/GL/WebGL2 + wasmi fuel)      │
   └─────────────────────────────────────────────────────────────┘
```

---

## Key Pillars

### 1. Mathematical Guarantee of Correctness (`Infimum.Telos`)
Every function, loop, and layout contract can be verified using the built-in **Z3 SMT solver**. Division by zero, array out-of-bounds, contract regressions, and integer overflows are proven impossible *before a single byte of Wasm is emitted*. If a contract fails, Infimum returns a human- and LLM-friendly counterexample showing the exact inputs that trigger the violation.

### 2. Unforgeable Capability Passports & Taint Tracking (`Infimum.Flux`)
Pure capability-based security. A function cannot access the network, filesystem, or audio device unless declared in its effect signature (`-[Net("api.mybank.com"), Audit]->`). Data wrapped in `Secret<T>` cannot be leaked or logged without explicit scoped `reveal(...)` permissions. Every compiled `.ifw` capsule carries a cryptographic capability passport verified before byte 0 executes.

### 3. Sub-Second Direct Compilation (`Infimum.Strata`)
No heavy LLVM or `rustc` toolchain required. The Infimum direct compiler produces tiny, deterministic WebAssembly (`.ifw`) capsules in **100–300 ms**. A complete graphical desktop app or game weighs **17 to 108 KB** (3 orders of magnitude smaller than Electron).

### 4. Zero-Chromium Native UI & Runtime (`infimum-web`)
Forget 150 MB Chromium runtimes. Infimum apps run on `infimum-web`, a lightweight native Rust engine powered by `wgpu` (Vulkan/Metal/OpenGL) and `wasmi` per-frame fuel metering. Interactive widgets, SDF 2D text, and 3D PBR meshes are rendered in single-digit milliseconds with native startup times under 10 ms.

### 5. Built for AI Agents & MCP
Infimum provides a first-class Model Context Protocol server via `infimum mcp`, allowing Claude, Cursor, OpenAI, or local agents to verify, compile, and execute sandboxed code with deterministic feedback loops.

---

## Code at a Glance

### Formally Verified Business Logic (`Telos` + `Flux`)

```infimum
// Leaf node with explicit scoped network capability and audit logging
fn send_bank_wire(token: Secret<str>, net_amount: i64) -[Net("api.mybank.com"), Audit]-> bool
  req net_amount > 0
  ens result == true
{
  do Audit.record(net_amount);
  do Net.post(reveal("api.mybank.com", token), net_amount);
  true
}

// Fee calculation with contract bounds
fn calc_fee(amount: i64, rate_bps: i64) -> i64
  req amount >= 0 && rate_bps >= 0 && rate_bps <= 5000
  ens result >= 0 && result <= amount / 2
{
  (amount * rate_bps) / 10000
}

// Z3 modularly proves that `net > 0` holds before `send_bank_wire` is called!
fn checkout(auth_token: Secret<str>, total: i64) -[Net("api.mybank.com"), Audit]-> bool
  req total > 100
  ens result == true
{
  let fee = calc_fee(total, 250);
  let net = total - fee;
  send_bank_wire(auth_token, net)
}
```

### Declarative Native UI with Zero CSS (`Forma`)

```infimum
use forma::prelude::*;

forma CounterApp {
  state count: i32 = 0;

  layout {
    Column(gap: 16, align: Center, padding: 24) {
      Text(format("Clicks: {}", self.count), size: 24, color: #ffffff);
      Row(gap: 8) {
        Button("-1", on_click: || self.count -= 1);
        Button("+1", on_click: || self.count += 1);
      }
    }
  }
}
```

---

## Comparison: Infimum vs. Traditional AI Stacks

| Feature | Infimum Stack | Python / Node Sandbox | Electron + JS |
| :--- | :--- | :--- | :--- |
| **Logic Verification** | **Z3 SMT Formal Proofs** | None (Runtime exceptions) | None (Runtime crashes) |
| **Executable Bundle Size** | **17 KB – 108 KB** | 50 MB – 500 MB (Virtualenv) | 150 MB – 300 MB |
| **Cold Start Latency** | **< 10 ms** | 300 ms – 1500 ms | 2000 ms – 4000 ms |
| **Capability Sandboxing** | **Hardware/Passport Level** | OS Process (Docker / gVisor) | Insecure (Node integration) |
| **Secret Leak Protection** | **`Secret<T>` Taint Tracker** | None | None |
| **Compilation Speed** | **100–300 ms** (Direct) | Interpreted / JIT | Transpiler + Bundler (slow) |
| **Renderer Overhead** | **Zero Chromium** (wgpu SDF) | Headless OS only | Full Blink / Chromium DOM |

---

## Monorepo Architecture

```
ProjectInfimum/infimum/
├── crates/
│   ├── infimum-lang/     # Compiler, Z3 verifier, Forma UI engine, MCP server, stdlib
│   └── infimum-web/      # Native wgpu / WebGL2 sandbox runtime (wasmi fuel metering)
└── apps/
    └── infimum-code/     # 108 KB Native AI Studio (.ifw) & agent daemon (infimum-agentd)
```

| Component | Path | Description |
| :--- | :--- | :--- |
| **Compiler & Verifier** | [`crates/infimum-lang`](crates/infimum-lang) | `infimum` CLI, AST, Z3 Telos engine, Wasm/ELF emitters, MCP server, and `std/`. |
| **Runtime Engine** | [`crates/infimum-web`](crates/infimum-web) | Pure-Rust lightweight runtime with Vulkan/GL/WebGL2 SDF rendering and capability sandbox. |
| **Infimum Code IDE** | [`apps/infimum-code`](apps/infimum-code) | Native GPU IDE built entirely in Infimum, backed by bubblewrap-sandboxed agentd. |

---

## Getting Started

### Prerequisites

- **Rust:** `stable` toolchain (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- **Z3 SMT Solver:** `libz3-dev` or `z3` binary on `PATH` (`sudo apt install z3 libz3-dev` on Debian/Ubuntu)

### 1. Build the Entire Workspace

```bash
cargo build --release --workspace
```

This builds three core binaries:
- `target/release/infimum` (CLI, compiler, Z3 verifier, MCP server)
- `target/release/infimum-web` (Native sandbox capsule runtime)
- `target/release/infimum-agentd` (Sandboxed agent execution daemon)

### 2. Verify and Compile an Infimum File

```bash
# Verify mathematical contracts via Z3 SMT solver
./target/release/infimum verify crates/infimum-lang/examples/payment_demo.ifm

# Compile to a compact WebAssembly capsule (.ifw)
./target/release/infimum compile-wasm crates/infimum-lang/examples/payment_demo.ifm -o payment.ifw
```

### 3. Run a Native Capsule

```bash
# Run interactive showcase capsule in high-performance wgpu window
./target/release/infimum-web crates/infimum-lang/examples/showcase/color-lines/color_lines.ifw
```

### 4. Connect to AI Agents via MCP

Add `infimum` to your Claude Desktop or Cursor configuration:

```json
{
  "mcpServers": {
    "infimum": {
      "command": "/absolute/path/to/target/release/infimum",
      "args": ["mcp"]
    }
  }
}
```

---

## Live Demonstrations

Explore real interactive `.ifw` capsules compiled directly from Infimum Lang running inside the WebGL2 browser runtime:

- 🎮 **Color Lines Showcase:** [infimum-lang.org/player/?app=color-lines](https://infimum-lang.org/player/?app=color-lines)
- 🌌 **Ceres Drift Procedural Demo:** [infimum-lang.org/player/?app=ceres-drift](https://infimum-lang.org/player/?app=ceres-drift)
- 🌀 **Slipgate Retro FPS Showcase:** [infimum-lang.org/player/?app=slipgate](https://infimum-lang.org/player/?app=slipgate)
- 🦁 **Lion Herd Ecosystem Simulation:** [infimum-lang.org/player/?app=lion-herd](https://infimum-lang.org/player/?app=lion-herd)
- ⚫ **Gomoku Five-in-a-Row:** [infimum-lang.org/player/?app=gomoku](https://infimum-lang.org/player/?app=gomoku)

Or try the full in-browser IDE: **[infimum-lang.org/code/](https://infimum-lang.org/code/)**

---

## Licensing

- **Core Platform & Runtime (Open Source):** Licensed under the **GNU AGPL-3.0** (see [`LICENSE`](LICENSE)).
- **Compiler Output & Stdlib Exception:** Your code and compiled `.ifw` / `.wasm` capsules are **100% yours**. The standard library (`std/`), runtime UI widgets, and compiler-generated glue code are licensed under **Apache-2.0** with explicit license exceptions guaranteeing that your proprietary applications are never tainted by AGPL copyleft.
- **Commercial Licensing:** Closed-source enterprise distributions, private SaaS embedding, and OEM agreements are available via **Infimum Commercial License**. Inquiries: **`licensing@infimum-lang.org`**.
