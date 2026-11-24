# Infimum SlipGate — 3D Citadel FPS (`27.6 KB` `.ifw` + `12.7 MB` PBR textures)

**Live Browser Preview:** [https://infimum-lang.org/player/?app=slipgate](https://infimum-lang.org/player/?app=slipgate)  
**Powered by:** [Infimum (`infimum-lang`)](https://github.com/aminadav-ai/infimum-lang) + [`infimum-web`](https://github.com/aminadav-ai/infimum-web)

**SlipGate** is a 6-zone Quake-inspired 3D citadel first-person shooter written 100% in **Infimum Lang** (`src/main.ifm`, `src/world.ifm`, `src/logic.ifm`, `src/render.ifm`, `src/abi.ifm`) and compiled directly to a **`27.6 KB` `.ifw` capsule** (`slipgate.ifw`, `27,566` bytes) with zero `rustc` and zero third-party game engine dependencies.

---

## Asset & Binary Breakdown

| Component | Size / Details |
| :--- | :--- |
| **Compiled `.ifw` Capsule (`slipgate.ifw`)** | **`27.6 KB`** (`27,566` bytes) — 6-zone level geometry, physics, enemy AI, combat, Gold Key gate, and procedural audio |
| **External PBR Textures (`textures/`)** | **`12.7 MB`** (`13,317,369` bytes) — 7× 1K photo-PBR texture sets (brick, pillar, metal, floor, rock, timber, walnut; diffuse/normal/roughness) from Poly Haven (CC0 1.0) |
| **Audio Assets** | **`0 B`** external files — 100% procedural FPS combat SFX (`std/audio.ifm`) |
| **Lighting & Rendering** | 24 dynamic point lights, hardware Vulkan/wgpu 3D PBR mesh pipeline |

---

## Build & Run Locally

```bash
# Compile .ifw capsule
infimum compile-wasm src/main.ifm -o slipgate.ifw

# Run natively via infimum-web (Vulkan/wgpu)
infimum-web slipgate.ifw
```

---

## License

Copyright (c) 2026 Yehuda Aminadav. Licensed under the **Apache License 2.0** (see [`../LICENSE`](../LICENSE)). Bundled Poly Haven 1K PBR textures are **CC0 1.0 Public Domain** (see [`../NOTICE`](../NOTICE)).
