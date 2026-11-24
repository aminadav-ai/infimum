# Infimum Color Lines 98 — 3D Glass & Marble Puzzle (`17.4 KB` `.ifw` · `0 B` external assets)

**Live Browser Preview:** [https://infimum-lang.org/player/?app=color-lines](https://infimum-lang.org/player/?app=color-lines)  
**Powered by:** [Infimum (`infimum-lang`)](https://github.com/aminadav-ai/infimum-lang) + [`infimum-web`](https://github.com/aminadav-ai/infimum-web)

**Color Lines 98** is a 3D tabletop puzzle written 100% in **Infimum Lang** using typed Infimum `struct`s (zero raw `mem_load`/`mem_store`), compiled directly to a **`17.4 KB` `.ifw` capsule** (`color_lines.ifw`, `17,808` bytes) with **zero external texture or audio assets**.

---

## Asset & Binary Breakdown

| Component | Size / Details |
| :--- | :--- |
| **Compiled `.ifw` Capsule (`color_lines.ifw`)** | **`17.4 KB`** (`17,808` bytes) — game rules, BFS pathfinding, 128-turn Time-Travel undo, 3D rendering, and procedural audio |
| **External Texture Assets** | **`0 B`** — 100% procedural glass & veined marble rendering |
| **External Audio Assets** | **`0 B`** — 100% procedural audio (`std/audio.ifm`) |

---

## Build & Run Locally

```bash
# Compile .ifw capsule
infimum compile-wasm src/main.ifm -o color_lines.ifw

# Run natively via infimum-web (Vulkan/wgpu)
infimum-web color_lines.ifw
```

---

## License

Copyright (c) 2026 Yehuda Aminadav. Licensed under the **Apache License 2.0** (see [`../LICENSE`](../LICENSE) and [`../NOTICE`](../NOTICE)).
