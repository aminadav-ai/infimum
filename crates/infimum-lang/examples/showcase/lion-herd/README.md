# Infimum Lion Herd — 3D PBR Savanna Shepherd (`19.6 KB` `.ifw` + `9.1 MB` PBR textures)

**Live Browser Preview:** [https://infimum-lang.org/player/?app=lion-herd](https://infimum-lang.org/player/?app=lion-herd)  
**Powered by:** [Infimum (`infimum-lang`)](https://github.com/aminadav-ai/infimum-lang) + [`infimum-web`](https://github.com/aminadav-ai/infimum-web)

**Lion Herd** is a native 3D savanna shepherd game written 100% in **Infimum Lang** (`src/main.ifm`, `src/logic.ifm`, `src/render.ifm`, `src/abi.ifm`) and compiled directly to a **`19.6 KB` `.ifw` capsule** (`lion_herd.ifw`, `19,580` bytes) with zero `rustc` and zero JS runtime.

---

## Asset & Binary Breakdown

| Component | Size / Details |
| :--- | :--- |
| **Compiled `.ifw` Capsule (`lion_herd.ifw`)** | **`19.6 KB`** (`19,580` bytes) — all game logic, flocking AI, 3D mesh generation, and procedural audio |
| **External PBR Textures (`textures/`)** | **`9.1 MB`** (`9,542,969` bytes) — 5× 1K photo-PBR texture sets (ground, bark, grass, rock, wood; diffuse/normal/roughness) from Poly Haven (CC0 1.0) |
| **Audio Assets** | **`0 B`** external files — 100% procedural SFX (`std/audio.ifm`, roar/bleat presets) |
| **3D Scene** | Sculpted 24-part lion, 14-part fluffy sheep, savanna plateau, trees, and corral rendered natively via Vulkan/wgpu |

---

## Build & Run Locally

```bash
# Compile .ifw capsule
infimum compile-wasm src/main.ifm -o lion_herd.ifw

# Run natively via infimum-web (Vulkan/wgpu)
infimum-web lion_herd.ifw
```

---

## License

Copyright (c) 2026 Yehuda Aminadav. Licensed under the **Apache License 2.0** (see [`../LICENSE`](../LICENSE)). Bundled Poly Haven 1K PBR textures are **CC0 1.0 Public Domain** (see [`../NOTICE`](../NOTICE)).
