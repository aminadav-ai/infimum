# Infimum Lang & `infimum-web` Native Capsule Reference Guide

This document is the canonical language, GPU protocol, standard library, and CLI toolchain specification for writing, verifying, compiling, and running native GPU applications (`.ifm` -> `.ifw`) in **Infimum Lang** on the **`infimum-web`** native Rust + Vulkan/wgpu runtime (0% HTML/CSS/JS, 0% Electron).

---

## 0. Built-In Standard Library (`use "std/prelude.ifm";`) — USE THIS FIRST!

The `infimum` compiler embeds a **100% Z3-verified standard library** (zero external files needed, works inside any sandbox). Start every app with:
```infimum
use "std/prelude.ifm";
```
This gives you all of the following immediately (or import individual modules `std/math.ifm`, `std/color.ifm`, `std/sys_mem.ifm`, `std/mem.ifm`, `std/gfx.ifm`, `std/text.ifm`, `std/audio.ifm`, `std/input.ifm`, `std/fmt.ifm`, `std/theme.ifm`, `std/icons.ifm`, `std/ui.ifm`):

- **`std/math.ifm`:** `clamp_i64(v, lo, hi)`, `clamp_byte(v)`, `min_i64(a, b)`, `max_i64(a, b)`, `abs_i64(v)`, `point_in_rect(px, py, rx, ry, rw, rh)`, `lerp_i64(a, b, t1000)`, `ease_out_quad(t1000)`, `ease_out_cubic(t1000)`, `ease_in_out_quad(t1000)`, `rng_next(seed)`, `rng_range(seed, bound)`, **`isqrt(n)`**, **`sin_1024(ang_4096)`** (`-1024..1024`), **`cos_1024(ang_4096)`** (`-1024..1024`), plus Q16.16 `fixed` helpers (`fixed`, `fixed_to_int`, `fixed_raw`, `fixed_from_raw`, `fixed_abs`, `fixed_min`, `fixed_max`, `fixed_clamp`, `fixed_lerp`, `fixed_sin`, `fixed_cos`).
- **`std/color.ifm`:** `pack_rgba(r, g, b, a)`, `rgba_r/g/b/a(c)`, `with_alpha(c, a)`, `lerp_rgba(c1, c2, t1000)`, `darken_rgba(c, pct)`, `lighten_rgba(c, pct)`.
- **`std/sys_mem.ifm`:** Single source of truth for the segmented linear memory map (System ABI `0..65535`, GPU stream `65536..705535`, auto-placed user & module layouts `705536+`). Key constants: `SYS_CURSOR_KIND_WORD (11)`, `SYS_PRIM_COUNT_WORD (12)`, `SYS_SCRATCH_CURSOR_WORD (16)`, `SYS_STR_SCRATCH_WORD (17)`, `SYS_HOST_RET_WORD (62)`, `SYS_CLIP_RECTS_WORD (64)`, `SYS_FORMA_MAILBOX_WORD (128)`, `SYS_INPUT_BASE_WORD (192)`, `SYS_FONT_METRICS_FLAG_WORD (256)`, `SYS_SCRATCH_BASE_WORD (4096)`, `SYS_GPU_PRIMS_WORD (65536)`, `SYS_USER_LAYOUT_START_WORD (705536)`.
- **`std/mem.ifm`:** Deterministic zero-GC memory allocators & host return register (Z3-verified contracts; unified error return `-1`):
  - **Per-frame bump scratch (`4096..65535`, ~240 KiB, auto-reset in `gfx_begin_frame`):** `scratch_reset()`, `scratch_used()`, `scratch_remaining()`, `scratch_alloc(words) -> addr | -1`, `scratch_calloc(words) -> addr | -1` (`1..=4096`).
  - **Fixed-capacity bump arena (over any `words[N]` buffer + 1-word `cursor` field):** `arena_reset(cursor_addr)`, `arena_used(cursor_addr, cap)`, `arena_remaining(cursor_addr, cap)`, `arena_alloc(base, cap, cursor_addr, words) -> addr | -1`, `arena_push_strbuf(base, cap, cursor_addr, src_ptr, max_chars) -> addr | -1`.
  - **Fixed-slot object pool (`flags: words[N]`, up to 1024 slots):** `pool_init(flags_addr, cap)`, `pool_alloc(flags_addr, cap) -> idx | -1`, `pool_free(flags_addr, idx, cap)`, `pool_is_alive(flags_addr, idx, cap) -> bool`, `pool_count(flags_addr, cap)`.
  - **Host capability return code:** `host_ret() -> i64` (reads `mem[SYS_HOST_RET_WORD]` written by `do Net.call(...)` / `do Fs.call(...)` / `do Gpu.call(...)`).
- **`std/gfx.ifm`:** `GPU_CMD_BYTE_OFFSET`, `GPU_CLIPS_BYTE_OFFSET`, **`gfx_begin_frame(vw, vh)`**, **`forma_begin_frame(vw, vh, mx, my, m_clicked, wheel_dy)`**, **`forma_set_pointer(mx, my, m_clicked, wheel_dy)`**, **`forma_set_input(m_down, key_char, key_special, tick)`**, **`forma_wheel_dy()`**, **`forma_hovered()`**, **`forma_cursor()`** (`0`=Default, `1`=Pointer, `2`=Text), **`forma_focus_id()`**, **`forma_set_focus(id)`**, **`forma_sel_range_len()`**, **`forma_paste_text(src_ptr, src_len)`**, **`forma_copy_text(dst_ptr, dst_cap)`**, **`gfx_set_cursor(kind)`**, **`gfx_end_frame()`**, `set_clip_rect(cid, x0, y0, x1, y1)` (16 hardware clip slots `0..15`; slot `0` is auto-set to full viewport), `push_prim(...)` (up to 80,000 primitives/frame), `ui_rect(...)`, `ui_line(...)`, `ui_circle(...)`, **`ui_image(...)`** (slot `0`), **`ui_image_slot(..., slot, clip)`** (uploaded RGBA image slot `0..15`), and **Hardware 3D/PBR WGSL Primitives (1 Draw Call!)**:
  - **`ui_grad_rect(x, y, w, h, radius, mode, shadow_px, col1, col2, clip)`** — smooth GPU gradient rect (`mode`: `0`=vertical, `1`=horizontal, `2`=diagonal, `3`=radial) + soft shadow.
  - **`ui_sphere_3d(cx, cy, radius, material, roughness, shadow_px, base_rgba, rim_rgba, clip)`** — real per-pixel 3D PBR raymarched sphere (`material`: `0`=crystal glass with caustics, `1`=metallic chrome, `2`=marble ceramic; `roughness`: `0..15`; `rim_rgba`: `0` for auto).
  - **`ui_orb(cx, cy, radius, base_rgba, clip)`** — 1-primitive shortcut for a glossy 3D crystal jewel sphere.
  - **`ui_glow(cx, cy, radius, falloff_x10, core_rgba, outer_rgba, clip)`** — soft radial bloom/light halo.
  - **`ui_tri(x0, y0, x1, y1, x2, y2, col1, col2, light_1000, clip)`** — anti-aliased shaded 3D polygon triangle.
  - **`ui_quad_3d(x0, y0, x1, y1, x2, y2, x3, y3, col1, col2, light_1000, clip)`** — 2-triangle flat shaded quad (no depth buffer).
  - **Native Hardware 3D Mesh + PBR Texture2DArray + 24 Dynamic Point Lights (`use "std/mesh3d.ifm";`, RECOMMENDED for 3D games / FPS / 3D worlds!):**
    - Zero CPU projection or manual frustum clipping! `infimum-web` performs hardware 4×4 MVP transform, frustum clipping, `Depth32Float` Z-buffering, close-range cotangent TBN normal mapping, roughness shading, distance fog, procedural sky, and up to **24 dynamic point lights** in Vulkan/wgpu.
    - Call `read_infimum_std("mesh3d.ifm")` to inspect the exact source. Key functions:
      - `mesh3d_begin(cam_x, cam_y, cam_z, yaw, pitch, fov_deg, sky_mode, fog_mode)` — starts a 3D frame (`yaw` in `0..4095`, `pitch` in `-240..240`, `fov_deg` in `45..110`; `sky_mode`: `0`=dark gothic, `1`=crimson storm, `2`=sunlit daylight; `fog_mode`: `0`=horizon haze, `1`=dark fog, `2`=crimson fog).
      - `mesh3d_add_light(x, y, z, rad, r, g, b, inten)` — adds a dynamic point light (up to `24`/frame; `inten` in `0..512`).
      - `mesh3d_quad(is_vm, x0,y0,z0,u0,v0, x1,y1,z1,u1,v1, x2,y2,z2,u2,v2, x3,y3,z3,u3,v3, nx,ny,nz, mat, rgba, ao, emissive)` — arbitrary 3D quad (`nx,ny,nz` in `-127..127`, UV in `1024 = 1.0` tile, `ao` in `0..255`).
      - `mesh3d_box(is_vm, x0, y0, z0, x1, y1, z1, mat, rgba, ao, emissive, uv_scale)` — axis-aligned 6-face 3D box with automatic outward normals & world-scale UVs.
      - `mesh3d_limb(cx, cy, cz, yaw, lx0, ly0, lz0, lx1, ly1, lz1, mat, rgba, ao, emissive)` — Y-rotated 6-face 3D box for articulated characters, monsters, animals, drones, and pickups.
      - `mesh3d_rafter_x(x0, y0, x1, y1, z0, z1, thick_y, mat, rgba, ao)` — sloped architectural roof rafter / diagonal beam.
      - `is_vm`: `0` = world mesh (with depth & fog), `1` = first-person viewmodel (camera-space weapons/hands, no fog).
      - **Auto-placed memory map:** `std/mesh3d.ifm` declares an auto-placed `layout Mesh3dMem { ... }` (packed cleanly alongside user `layout`s starting at `SYS_USER_LAYOUT_START_WORD = 705536`), and the compiler automatically exports `get_mesh3d_byte_offset()` for `infimum-web`. 2D/UI apps that don't import `std/mesh3d.ifm` pay 0 bytes for 3D VBOs, and memory collisions are mathematically impossible.
    - **12-Layer PBR `Texture2DArray` & External 1K Photo-PBR Textures:**
      - Works out-of-the-box with zero assets using 12 built-in seamless procedural 256×256 diffuse + normal/roughness mipmapped layers (`mat = 0..11`).
      - Upgrade any slot `0..11` to 1K CC0 photo-PBR textures via `textures.json` (see Section 4) or local `textures/<layer>_<name>_{diff,norm,rough}.{jpg,png}`.
  - **Legacy Screen-Space Z-Buffered 3D (`kind = 9` in `std/gfx.ifm`):** `ui_tri_3d(...)` and `ui_quad_3d_mat(...)` remain available for simple screen-projected quads (`MAT_SOLID=0`..`MAT_SHADOW_CAUSTIC=13`; `FOG_DARK=0`, `FOG_RED=1`, `FOG_NONE=2`), but prefer `std/mesh3d.ifm` for new 3D games.
- **`std/text.ifm`:** `codepoint_to_atlas(cp)`, `glyph_px(cp, size, style)`, `ui_glyph(...)`, `ui_text(x, y, s, size, style, rgba, clip)`, `ui_text_width(s, size, style)`, **`ui_text_centered(cx, y, s, size, style, rgba, clip)`**, **`ui_int(x, y, val, size, style, rgba, clip)`** (full 64-bit `i64`), **`ui_int_width(val, size, style)`**, **`ui_int_centered(cx, y, val, size, style, rgba, clip)`**, **`ui_fixed(x, y, val, decimals, size, style, rgba, clip)`**, **`ui_fixed_width(val, decimals, size, style)`**, and zero-heap compile-time formatting:
  - **`ui_fmt(x, y, "Turn {turn + 1} — score {Game.score} | Y: {pos_y:.1}", size, style, rgba, clip)`**
  - **`ui_fmt_width("Turn {turn + 1}", size, style)`** — exact pixel width of a formatted string.
  - **`ui_fmt_centered(cx, y, "Moves: {Game.move_count}", size, style, rgba, clip)`** — automatically centers a formatted string around `cx`.
  *(All use exact TrueType metrics uploaded to words `256..1023`: ASCII + Russian Cyrillic + `₪`/`€`/`→`; `style`: `0`=Regular, `1`=Bold, `2`=Mono.)*
- **`std/audio.ifm`:** Zero-asset procedural sound effects (uses `-[Gpu]->`, already available in `step_ui`, no audio files needed!):
  - **Convenience helpers:** `sfx_clack()`, `sfx_hop()`, `sfx_click()`, `sfx_spawn()`, `sfx_clear()`, `sfx_error()`, `sfx_win()`.
  - **All 18 built-in SFX presets (`sfx_play(preset, pitch_pct, vol_pct)`, `pitch_pct` in `40..250`, `vol_pct` in `0..100`):**
    - UI & Board (`1..7`): `SFX_CLACK=1`, `SFX_HOP=2`, `SFX_CLICK=3`, `SFX_SPAWN=4`, `SFX_CLEAR=5`, `SFX_ERROR=6`, `SFX_WIN=7`
    - 3D Action / FPS (`8..14`): `SFX_SHOTGUN=8`, `SFX_MONSTER_HIT=9`, `SFX_MONSTER_DIE=10`, `SFX_PLAYER_HURT=11`, `SFX_STEP=12`, `SFX_GATE=13`, `SFX_JUMP=14`
    - Animals (`15..16`): `SFX_LION_ROAR=15`, `SFX_SHEEP_BAA=16`
    - Sci-Fi / Space (`17..18`): `SFX_LASER=17` (crisp sci-fi plasma laser), `SFX_MISSILE=18` (homing missile launch whoosh)
  - **Custom synth:** `sfx_tone(freq_hz, dur_ms, wave)` (`freq_hz` in `60..4000`, `dur_ms` in `10..1500`, `wave`: `0`=sine, `1`=triangle, `2`=square, `3`=glass bell FM).
- **`std/input.ifm`:** Real-time held-key bitmask (layout-independent physical keys — works even on Russian/Cyrillic keyboard layouts!), FPS mouse look, and canonical `enum KeySpecial`:
  - Held physical keys: `key_held(bit)`, `keys_held_mask()` with `KEY_W`, `KEY_A`, `KEY_S`, `KEY_D`, `KEY_UP/DOWN/LEFT/RIGHT`, `KEY_Q`, `KEY_E`, `KEY_SPACE`, `KEY_SHIFT`, `KEY_R`, `KEY_Z`, `KEY_Y`, `KEY_F`, `KEY_CTRL`, `KEY_ALT`, `KEY_1..KEY_9`.
  - 360° relative mouse look & pointer lock: `mouse_dx()`, `mouse_dy()`, `pointer_locked()`, `set_pointer_lock(bool)` (`Esc` always unlocks).
  - Canonical `enum KeySpecial` (`None=0`, `Backspace=1`, `Enter=2`, `Escape=3`, `Tab=4`, `ArrowLeft=5..Delete=11`, `ShiftEnter=12`, `CtrlA=20..CtrlX=23`, `ShiftLeft=25..ShiftEnd=30`, `RightClick=31`, `CtrlHome=32..CtrlShiftEnd=35`) — see Section 3.
- **`std/fmt.ifm`:** Exact-width formatters: `fmt_fixed_1(x, y, tenths, sz, st, col, clip)`, `fmt_fixed_1_width(...)`, `fmt_fixed_2(x, y, hundredths, ...)`, `fmt_mb(x, y, tenths_mb, sz, col_num, col_unit, clip)` (`"119.2 MB"`), `fmt_mb_width(...)`, `fmt_duration(x, y, tenths, sz, col_num, col_unit, clip)` (`"Mm Ss"` when `>= 60s`, else `"X.Xs"`), `fmt_duration_width(...)`, `fmt_text_ellipsis(x, y, max_w, s, sz, st, col, clip)`.
- **`std/theme.ifm`:** `THEME_DARK` (`0`), `THEME_LIGHT` (`1`), `theme_scale(px, scale_pct)`, `theme_bg(th)`, `theme_side(th)`, `theme_surf(th)`, `theme_surf2(th)`, `theme_border(th)`, `theme_text(th)`, `theme_muted(th)`, `theme_heading(th)`, `theme_code_bg(th)`, `theme_code_pill(th)`, `theme_code_text(th)`, `theme_accent()`, `theme_accent_hover()`, `theme_success()`, `theme_warn()`, `theme_danger()`.
- **`std/icons.ifm`:** Vector UI icons: `icon_chevron_down`, `icon_chevron_right`, `icon_plus`, `icon_close`, `icon_check`, `icon_folder`, `icon_copy`, `icon_send`, `icon_spinner(cx, cy, r, tick, col, clip)`.
- **`std/ui.ifm`:** Reusable themed widgets: `ui_card`, `ui_divider`, `ui_badge(x, y, s, sz, bg, fg, clip) -> width`, `ui_button(x, y, w, h, s, sz, th, primary, mx, my, m_clicked, clip) -> bool`, `ui_progress_bar`, `ui_scrollbar_v`.
- **`std/buf.ifm`:** Z3-safe UTF-32 buffer & zero-GC `str` ergonomics (`mem[buf]` = length, `mem[buf + 1 + i]` = Unicode codepoint): `buf_len`, `buf_clear`, `buf_push_cp`, `buf_push_int`, `buf_push_json_escaped`, `buf_eq_slice`, `buf_find_cp`, **`str_len(s)`**, **`str_char(s, idx)`**, **`buf_push_str(buf, max_len, s)`**, **`buf_set_str(buf, max_len, s)`**, **`buf_eq_str(buf, s)`**, **`buf_has_str(buf, s)`**, **`buf_starts_with(buf, s)`**, **`buf_ends_with(buf, s)`**, **`slice_eq_str(raw_ptr, len, s)`**.
- **`std/json.ifm`** *(opt-in: `use "std/json.ifm";`)*: Zero-allocation UTF-32 JSON scanner/decoder: `json_skip_ws`, `json_skip_string`, `json_skip_value`, `json_parse_int`, `json_decode_string` (supports `\n`, `\r`, `\t`, `\"`, `\\`, and `\uXXXX` Unicode escapes), `json_find_key`.
- **`std/text_edit.ifm`** *(opt-in: `use "std/text_edit.ifm";`)*: Reusable UTF-32 text buffer editor & word-wrapper (100% parity across measure, hit-test, and render): `tbuf_len`, `tbuf_insert_char`, `tbuf_delete_range`, `tbuf_backspace`, `tbuf_word_lo`, `tbuf_word_hi`, `tbuf_measure_wrapped_h`, `tbuf_hit_wrapped`, `tbuf_draw_wrapped`.
- **`std/markdown.ifm`** *(opt-in: `use "std/markdown.ifm";`)*: Full GFM UTF-32 Markdown layout, hit-test & render engine (headings `#..####`, bold `**`, inline code pills, fenced code blocks, GFM pipe tables `|...|`, blockquotes `>`, bullet & numbered lists, HR, word-wrapping, selection highlight): `md_layout`, `md_measure_h`, `md_hit`, `md_draw`, `md_draw_sel`.

### Infimum.Forma 2.1 Layout & Interactive Widgets
Inside `forma Name(...) { ... }`, containers (`Box`, `Col`, `Row`, `Stack`, `Scroll`, `Overlay`, `Button`, `Dropdown`) can be arbitrarily nested and support:
- `w: fit` / `h: fit` (CSS `fit-content` measured automatically from children/text)
- `justify: start | center | end | space_between | space_around` (main-axis distribution in `Row`/`Col`)
- `align: start | center | end | stretch` (cross-axis alignment)
- `grow`, `shrink`, `min_w`, `min_h`, `max_w`, `max_h`, `pad`, `pad_x`, `pad_y`, `gap`
- `Text(expr, size: ..., style: ..., color: ..., shrink: 1, overflow: visible | clip | ellipsis | wrap)` — accepts both string literals and `str` variables!
- **Built-in Z3-verified interactive widgets (Forma 2.1):**
  - **`Toggle(id: N, w: 46, h: 24, checked: bool_expr, bind: Layout::field, bg: ..., border_col: ..., accent: ..., on_change: Act)`** — animated pill switch (`bind:` auto-toggles `0/1` in memory, or handle `on_change:`).
  - **`Slider(id: N, w: 180, h: 24, bind: Layout::field, min: 0, max: 100, step: 5, bg: ..., border_col: ..., accent: ..., on_change: Act)`** — smooth drag-captured horizontal slider (`Z3` proves `min < max`!).
  - **`TextInput(id: N, grow: 1, h: 34, buf: Layout::str_buf, max_len: 120, mask: false, placeholder: "...", size: 14, bg: ..., border_col: ..., accent: ..., color: ..., on_change: Act, on_submit: Act)`** — single-line UTF-32 text input with click-to-caret hit-testing, drag & `Shift+Arrows`/`Ctrl+A` range selection, `Ctrl+C`/`Ctrl+X`/`Ctrl+V` clipboard support (via `forma_copy_text`/`forma_paste_text`), smooth horizontal scroll, and full password masking (`mask: true` hides all chars and refuses copy).
- **Composable custom components + `if`/`else` + static `for` inside `forma`:**
  - Call any other `forma` component directly inside a container: `ScoreCard("Player", score)` (inlined and Z3-verified!).
  - Conditional UI: `if cond { Widget(...) } else { Other(...) }`.
  - Static unrolled loops: `for i in 0..3 { PillBtn("Tab", i + 1) }` (up to 256 iterations, Z3-verified!).
  - `Slot(render: fn_name, on_wheel: wheel_fn)` inside `mod` automatically resolves module-local functions.

**Do NOT redefine** stdlib functions in your project when using `use "std/prelude.ifm";`!

---

## 1. The Four Sub-Languages of Infimum Lang

1. **Telos (Formal Verification — Z3 SMT):**
   - Preconditions (`req`) and postconditions (`ens`) on functions and `forma` components.
   - All `while` loops MUST have a static upper bound: `while cond max N { ... }`.
   - Use `clamp_i64(v, lo, hi)` or `inv lo..=hi` on layout fields to prove bounds and non-zero divisors to Z3.
2. **Flux (Effect System):**
   - Pure functions omit effects: `fn add(a: i64, b: i64) -> i64 { a + b }`.
   - Effectful functions declare effects: `-[Mem]->`, `-[Mem, Gpu]->`, `-[Mem, Gpu, Fs, Net]->`.
   - Built-in memory ops: `mem_load(word_idx)`, `mem_store(word_idx, val)`.
   - Submitting GPU frame at end of `step_ui`: `do Gpu.submit(prim_count, cursor_kind);` (or simply `gfx_end_frame()`).
3. **Forma (Declarative Z3-Verified UI Layout — PREFERRED for UI & Text!):**
   - `forma ComponentName(params...) req <constraints> -[Mem]-> i64 { ... }`
   - Layout & leaf widgets: `Box`, `Row`, `Col`, `Stack`, `Spacer`, `Divider`, `Rect`, `Button`, `Dropdown`, `Scroll`, `Overlay`, `Slot(render: fn_name)`, `Toggle`, `Slider`, `TextInput`, and built-in **`Text("...", size: 24, style: 0, color: rgba, align: center)`**.
   - **`Text(...)` automatically uses exact TrueType font metrics from `infimum-web` (words `256..1023`), supports ASCII + Cyrillic + `₪`/`€`/`→`, horizontal alignment (`start`/`center`/`end`), vertical centering inside its box, and optional pill background (`bg`, `radius`, `border`, `border_col`).**
   - Returns the triggered `on_click` / `on_change` / `on_submit` action `i64` (`0` if none).
   - **Zero magic addresses with `std/gfx.ifm`:** Call `forma_begin_frame(w, h, mx, my, m_clicked, wheel_dy);` and `forma_set_input(m_down, key_char, key_special, tick);` before invoking your `forma` screen, then `gfx_set_cursor(forma_cursor());` after!
4. **Strata (Direct Wasm `.ifw` Compilation & Segmented Linear Memory):**
   - **Strict Pascal-Unit Module System (`pub`, `use`, `pub use`, `as`, `mod`):**
     - **Private by default (unit encapsulation):** Every `.ifm` file is an isolated compilation unit. Declarations without `pub` (`const`, `enum`, `struct`, `layout`, `forma`, `fn`, `mod`) are strictly private to their file and never collide across files. Mark public API items with `pub` (`pub const`, `pub enum`, `pub struct`, `pub layout`, `pub forma`, `pub fn`, `pub mod`).
     - **Three canonical `use` forms** (prefix any with `pub use` to re-export into the current file's public interface — there are **zero implicit transitive leaks**):
       - Flat public interface import: `use "abi.ifm";` (or `pub use "abi.ifm";`)
       - Namespace import: `use "maze.ifm" as maze;` — access all 6 entity kinds via `maze::CONST`, `maze::Phase::Escape`, `maze::Vec2 { ... }`, `maze::Vec2::SIZE`, `maze::BoardMem.cells[i]`, `maze::BoardMem::cells::count`, `maze::step()`, `maze::Card(...)`, `Slot(render: maze::draw)`.
       - Selective import with optional alias: `use "std/text.ifm" { ui_text, ui_text_centered as draw_centered };` (original name is hidden when aliased).
     - **Inline namespaces:** `mod ai { const DEPTH: i64 = 3; fn score(x: i64) -> i64 { x * DEPTH } pub fn pick(x: i64) -> i64 { score(x) } }` (add `pub mod ai { ... }` to export namespace `ai` from the file).
     - **Fail-loud diagnostics (zero shadowing):** Any collision between two distinct visible symbols raises `E024_DUPLICATE_SYMBOL` (naming both files & lines); duplicate module/item aliases raise `E069_DUPLICATE_ALIAS`; unknown selective imports raise `E075_UNKNOWN_IMPORT`; accessing a non-`pub` item across a module boundary raises `E068_PRIVATE_ITEM`. Diamond imports of the same symbol/layout are automatically deduplicated.
   - Constants: `const BOARD_N: i64 = 9;`
   - Enums: `enum CursorKind { Default = 0, Pointer = 1, Text = 2, EwResize = 3 }`.
   - **Canonical Segmented Memory Map (`std/sys_mem.ifm`, auto-sized Wasm 8 → 16 → 32 → 64 MiB):**
     | Word Range | Segment | Notes |
     |---|---|---|
     | `0..63` | Core System Registers | `11`=`cursor_kind`, `12`=`prim_count`, `16`=`scratch_cursor`, `17`=`str_scratch`, `62`=`host_ret` |
     | `64..127` | Hardware Clip Rects | `16 × 4` words (`GPU_CLIPS_BYTE_OFFSET = 256`) |
     | `128..191` | Forma Mailbox & Widget State | Pointer, focus, drag, caret, selection, scroll |
     | `192..223` | Host Input Mailbox | `keys_held`, `mouse_dx`, `mouse_dy`, `pointer_lock` (`byte offset 768`) |
     | `256..1023` | TrueType Font Metrics | `256`=flag, `257+`=advance table (`byte offset 1024`) |
     | `4096..65535` | `FrameScratch` Bump Arena | `~240 KiB` per-frame scratch, auto-reset in `gfx_begin_frame` |
     | `65536..705535` | Unified GPU Primitive Stream | `80,000 × 8` words (`2.5 MiB`, `GPU_CMD_BYTE_OFFSET = 262144`) |
     | `705536+` | Auto-Placed User & Module `layout`s | Packed sequentially without overlap (including `Mesh3dMem` if `std/mesh3d.ifm` is imported) |
   - **Omit `at N` (`layout AppState { ... }`)** — the compiler automatically places all user and library layouts starting at `SYS_USER_LAYOUT_START_WORD (705536)` and auto-sizes Wasm linear memory. Manual `at 0` overlapping `11` or `12` is rejected (`E066_RESERVED_RUNTIME_ADDR`).
   - **Auto-generated `.ifw` exports:** when using `std/gfx.ifm` + `export fn step_ui(...)`, the compiler automatically generates all **7 standard `.ifw` runtime exports**: `ifw_unicode_atlas`, `get_cmd_byte_offset`, `get_clips_byte_offset`, `get_font_metrics_byte_offset`, `get_input_byte_offset`, `get_prim_count`, and `get_cursor_kind` — plus **`get_mesh3d_byte_offset`** (8th export) whenever `std/mesh3d.ifm` is imported!
   - Typed linear memory layout + **`struct` & indexed arrays** (compile-time proven non-overlapping, zero manual offset math):
     ```infimum
     struct Sheep {
       x: word,
       z: word,
       vx: word,
       alive: word,
     }

     // Auto-placed safely at 705536+ (zero magic addresses!)
     layout AppState {
       tick: word,
       score: word,
       hero: Sheep,              // single struct (4 words) -> AppState.hero.x
       flock: Sheep[32],         // struct array (32 * 4 = 128 words) -> AppState.flock[i].x
       board: words[81],         // indexed word array -> AppState.board[i]
     }
     ```
   - Field & array access (all support `=`, `+=`, `-=`, `*=`, `|=`, `&=`):
     - Scalar: `AppState.tick += 1;`
     - Indexed word array: `AppState.board[i] = 5;`
     - Single struct: `AppState.hero.x += 10;`
     - Indexed struct array: `AppState.flock[i].x += AppState.flock[i].vx;`
     - Compile-time metadata constants: `Sheep::SIZE`, `AppState::flock` (base word address), `AppState::flock::stride`, `AppState::flock::count`, `AppState::flock::len`, `AppState::flock::end`.

---

## 2. Core Infimum Syntax Notes

- Types: `i64`, **`fixed` (deterministic Q16.16 fixed-point, scale = 65536)**, `bool`, `str`, `unit`, **value `struct`s** (zero-heap stack/register structs), and **tuples** `(i64, i64)` / `(i64, i64, bool)`:
  - **Value `struct`s (Zero-Heap, Wasm Multi-Value, Z3-Verified):**
    - Any declared `struct` (`struct Vec3 { x: fixed, y: fixed, z: fixed }`) works both inside `layout` AND by value on the stack/registers.
    - Construct: `let mut v = Vec3 { x: 1.0, y: 2.0, z: 3.0 };`.
    - Pass & return by value: `fn vec_add(a: Vec3, b: Vec3) -> Vec3 { Vec3 { x: a.x + b.x, y: a.y + b.y, z: a.z + b.z } }`.
    - Field access & mutation: `v.x`, `vec_add(a, b).y`, `v.x += 0.5;`, Z3 contracts on fields (`req v.y != 0.0`), and whole-struct `layout` copy (`let h = World.hero; World.hero = h;`).
  - **Deterministic `fixed` (Q16.16, Z3-compatible, zero float drift):**
    - Decimal literals: `1.5`, `0.25`, `9.81`, `-0.5`; constants: `const GRAVITY: fixed = 9.81;`.
    - Fields in `struct` / `layout`: `y: fixed inv 0.0..=1000.0`, `vels: fixed[16]`.
    - Arithmetic: `fixed + fixed`, `fixed - fixed`, `fixed * fixed` (auto `/ 65536`), `fixed / fixed` (auto `* 65536`, Z3 proves divisor `!= 0.0`), `fixed * i64`, `fixed / i64`, comparisons `<`, `<=`, `==`. Mixing `i64 + fixed` is a compile-time error (`E060_TYPE_MISMATCH`).
    - Zero-heap formatting via `ui_fixed(x, y, val, decimals, ...)` or directly inside `ui_fmt(x, y, "Y: {y} | V: {vy:.1}", ...)`.
  - Return multiple values with zero heap overhead: `fn divmod(n: i64) -> (i64, i64) { (n / 18, n % 18) }`; destructure with `let (r, c) = divmod(idx);`.
- **Field invariants (`inv lo..=hi` / `inv lo..hi`) in `struct` and `layout`:**
  - `turn: word inv 1..=2`, `board: words[324] inv 0..=2`.
  - Checked on every write (`E054_FIELD_INVARIANT_VIOLATED`), automatically assumed by Z3 on every read (eliminates redundant `clamp_i64`).
- **Built-in unit tests (`test "name" { assert(...); }`):**
  - Run via `infimum test main.ifm` (desktop) or `test_ifm` (Online Studio); automatically stripped from `.ifw` binaries (0 bytes overhead).
- Variables: `let x = 10;`, mutable `let mut i = 0; i += 1;`.
- Conditionals: `if cond { a } else if cond2 { b } else { c }` (trailing `;` after block `if`, `while`, `for`, `match` is optional).
- Pattern matching: `match state { 0 => { ... }, 1 => { ... }, _ => {} }`.
- Bounded loops (`for`, `while`, `break`, `continue`):
  - `for i in 0..81 { ... }` (constant range = automatic static bound for Z3), `for i in 1..=5 { ... }`, or dynamic `for i in 0..count max 128 { ... }`.
- Character literals: `'A'`, `' '`, `'\n'` (evaluate to Unicode codepoint `i64`).
- Bitwise ops: `|`, `&`, `<<`, `>>`, `^`. Modulo & division: `%`, `/` (always guard divisor `> 0` for Z3).

---

## 3. Capsule Exports, Clipboard Protocol & `KeySpecial` Table

### Required Capsule Exports
When using `use "std/prelude.ifm";` (or `use "std/gfx.ifm";`), your main `.ifm` file ONLY needs to export **`init_app`** and **`step_ui`**. Both `init_app()` (0 args) and `init_app(seed: i64, vw: i64, vh: i64)` (3 args) are supported. The compiler automatically generates all **7 standard `.ifw` runtime exports** (`ifw_unicode_atlas`, `get_cmd_byte_offset`, `get_clips_byte_offset`, `get_font_metrics_byte_offset`, `get_input_byte_offset`, `get_prim_count`, `get_cursor_kind`) plus `get_mesh3d_byte_offset` for 3D apps.

```infimum
export fn init_app() -[Mem]-> i64 { 1 }

export fn step_ui(
  vw: i64, vh: i64, mx: i64, my: i64,
  m_down: i64, m_clicked: i64, wheel_dy: i64,
  key_char: i64, key_special: i64
) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  forma_begin_frame(w, h, mx, my, m_clicked, wheel_dy);
  forma_set_input(m_down, key_char, key_special, AppState.tick);
  // ... run Forma screen or emit GPU primitives ...
  gfx_set_cursor(forma_cursor());
  gfx_end_frame()
}
```

### Optional Host Clipboard & Action Exports (for `TextInput` / Editors)
To enable native OS / browser clipboard (`Ctrl+C`, `Ctrl+X`, `Ctrl+V`, image paste) in your capsule, declare two UTF-32 mailbox arrays in your `layout` (e.g. `copy_mb: words[4100]`, `paste_mb: words[4100]`) and export:
- `export fn ifw_get_copy_mailbox_word() -> i64 { AppState::copy_mb }` — word address where capsule writes copied UTF-32 text (`len` at `[0]`, chars at `[1..]`).
- `export fn ifw_get_paste_mailbox_word() -> i64 { AppState::paste_mb }` — word address where host writes pasted UTF-32 text before calling `ifw_on_paste_text`.
- `export fn ifw_on_paste_text(ptr: i64, len: i64) -[Mem]-> i64 { forma_paste_text(ptr, len) }` — called by host on `Ctrl+V` / `Shift+Insert`.
- `export fn ifw_get_copy_text() -[Mem]-> i64 { forma_copy_text(AppState::copy_mb, 4096) }` — called by host on `Ctrl+C` / `Ctrl+X`; returns copied char count.
- `export fn ifw_on_paste_image(w: i64, h: i64) -[Mem]-> i64` *(optional)* — notified when host uploads a pasted image to GPU image slot.
- `export fn ifw_pop_host_action() -[Mem]-> i64` *(optional)* — polled each frame for host-side commands (`1`=Copy, `2`=Paste, `3`=Cut, `4`=PickImage, `5`=ClearImage).

### `enum KeySpecial` (`std/input.ifm`, delivered via `step_ui(..., key_special)`)
| Code | Variant | Code | Variant | Code | Variant |
|---:|---|---:|---|---:|---|
| `0` | `KeySpecial::None` | `9` | `KeySpecial::Home` | `25` | `KeySpecial::ShiftLeft` |
| `1` | `KeySpecial::Backspace` | `10` | `KeySpecial::End` | `26` | `KeySpecial::ShiftRight` |
| `2` | `KeySpecial::Enter` | `11` | `KeySpecial::Delete` | `27` | `KeySpecial::ShiftUp` |
| `3` | `KeySpecial::Escape` | `12` | `KeySpecial::ShiftEnter` | `28` | `KeySpecial::ShiftDown` |
| `4` | `KeySpecial::Tab` | `20` | `KeySpecial::CtrlA` | `29` | `KeySpecial::ShiftHome` |
| `5` | `KeySpecial::ArrowLeft` | `21` | `KeySpecial::CtrlC` | `30` | `KeySpecial::ShiftEnd` |
| `6` | `KeySpecial::ArrowRight` | `22` | `KeySpecial::CtrlV` | `31` | `KeySpecial::RightClick` |
| `7` | `KeySpecial::ArrowUp` | `23` | `KeySpecial::CtrlX` | `32..35` | `CtrlHome`, `CtrlEnd`, `CtrlShiftHome`, `CtrlShiftEnd` |
| `8` | `KeySpecial::ArrowDown` | | | | |

---

## 4. CLI Toolchain, Compilation & Running Capsules

The real Infimum compiler binary is **`infimum`** (NOT `infimumc`), and the runtime player is the native Rust + wgpu binary **`infimum-web`** (NOT TypeScript/npm).

### Recommended Agent Tools (in Infimum Code)
1. **Verify Z3 contracts & types:** `verify_infimum_file(path)` (`infimum verify <file.ifm>`)
2. **Run built-in unit tests:** `infimum test <file.ifm>` (or `test_ifm` in Online Studio)
3. **Compile `.ifm` -> `.ifw` (Strata WebAssembly capsule):** `compile_infimum_wasm(path, output)` (`infimum compile-wasm <file.ifm> -o <file.ifw>` — always use `compile-wasm`, NOT `build-strata-wasm`, for GUI capsules so all `ifw_*` exports are preserved)
4. **Smoke-check or launch `.ifw` in `infimum-web`:**
   - Headless check: `run_infimum_capsule(path, headless: true)`
   - Launch native GPU window: `run_infimum_capsule(path, headless: false)`
   - **Per-capsule icons:** place a 256×256 PNG named `<stem>.png` or `icon.png` next to `<stem>.ifw`.
5. **Export Rust ABI from a shared `abi.ifm` layout:** `infimum export-abi <abi.ifm> --rust -o <out.rs>`

### 3D PBR Textures (`textures.json` & Cloud PBR Hub — Zero Bundled Assets)
- Every 3D capsule using `std/mesh3d.ifm` renders immediately with 12 built-in procedural PBR texture layers (`0..11`).
- To upgrade any slot (`0..11`) to real 1K CC0 photo-PBR textures (Diffuse + Normal + Roughness) without bundling megabytes of images, place a declarative `textures.json` next to your `.ifw` capsule:
  ```json
  {
    "1": "stone_brick_wall_001",
    "3": "metal_plate_02",
    "6": "stone_tiles",
    "8": "oak_wood_planks"
  }
  ```
- **Agent tools:** `search_pbr_textures(query)` searches 800+ CC0 Poly Haven textures, `set_pbr_texture(slot, texture_id)` binds a slot in `textures.json`.

---

## 5. Complete Copy-Ready Examples (Z3-Verified)

### Example 1: Interactive `Infimum.Forma` 2.1 App (`Toggle` + `Slider` + `TextInput` + Clipboard + Zero Magic Addresses)
```infimum
use "std/prelude.ifm";
use "std/buf.ifm";

enum Action {
  None = 0,
  Inc = 1,
  Reset = 2,
  ToggleTurbo = 3,
}

layout AppState {
  tick: word,
  counter: word inv 0..=999999,
  turbo: word inv 0..=1,
  intensity: word inv 0..=100,
  name_buf: words[40],
  copy_mb: words[128],
  paste_mb: words[128],
}

forma ActionBtn(label: str, action: i64, bg_col: i64, hover_col: i64) -[Mem]-> i64 {
  Button(w: 154, h: 42, bg: bg_col, hover_bg: hover_col, radius: 8, on_click: action, align: center) {
    Text(label, w: 120, h: 24, size: 15, style: 1, color: pack_rgba(255, 255, 255, 255), align: center)
  }
}

forma MainScreen(vw: i64, vh: i64, turbo: i64)
  req vw >= 640 && vw <= 4096 && vh >= 480 && vh <= 2160 && turbo >= 0 && turbo <= 1
  -[Mem]-> i64
{
  Box(x: 0, y: 0, w: vw, h: vh, bg: pack_rgba(17, 19, 24, 255), align: center) {
    Col(w: 460, h: 340, pad: 24, gap: 14, bg: pack_rgba(28, 32, 42, 255), border: 1, border_col: pack_rgba(91, 155, 255, 255), radius: 14, align: center) {
      Text("Infimum.Forma 2.1", w: 380, h: 38, size: 26, style: 1, color: pack_rgba(248, 250, 255, 255), align: center)
      Row(w: 400, h: 28, gap: 12, align: center) {
        Text("Turbo Mode:", w: 120, h: 20, size: 14, style: 1, color: pack_rgba(220, 230, 245, 255))
        Toggle(id: 10, w: 46, h: 24, checked: turbo == 1, on_change: Action::ToggleTurbo, bg: pack_rgba(45, 52, 68, 255), border_col: pack_rgba(74, 86, 112, 255), accent: pack_rgba(59, 130, 246, 255))
      }
      Row(w: 400, h: 28, gap: 12, align: center) {
        Text("Intensity:", w: 90, h: 20, size: 14, style: 1, color: pack_rgba(220, 230, 245, 255))
        Slider(id: 11, w: 260, h: 24, bind: AppState::intensity, min: 0, max: 100, step: 5, bg: pack_rgba(45, 52, 68, 255), border_col: pack_rgba(74, 86, 112, 255), accent: pack_rgba(59, 130, 246, 255))
      }
      TextInput(id: 12, w: 400, h: 36, buf: AppState::name_buf, max_len: 32, placeholder: "Type or paste (Ctrl+V)...", bg: pack_rgba(18, 21, 29, 255), border_col: pack_rgba(64, 74, 96, 255), accent: pack_rgba(91, 155, 255, 255), color: pack_rgba(248, 250, 255, 255), size: 14, on_submit: Action::Inc)
      Row(w: 320, h: 42, gap: 12, align: center) {
        ActionBtn("Increment", Action::Inc, pack_rgba(59, 130, 246, 255), pack_rgba(96, 165, 250, 255))
        ActionBtn("Reset", Action::Reset, pack_rgba(44, 50, 64, 255), pack_rgba(64, 72, 92, 255))
      }
    }
  }
}

export fn init_app() -[Mem]-> i64 {
  AppState.tick = 0;
  AppState.counter = 0;
  AppState.turbo = 1;
  AppState.intensity = 60;
  buf_clear(AppState::name_buf);
  1
}

export fn step_ui(
  vw: i64, vh: i64, mx: i64, my: i64,
  m_down: i64, m_clicked: i64, wheel_dy: i64,
  key_char: i64, key_special: i64
) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  AppState.tick += 1;

  forma_begin_frame(w, h, mx, my, m_clicked, wheel_dy);
  forma_set_input(m_down, key_char, key_special, AppState.tick);
  let act = MainScreen(w, h, AppState.turbo);
  if act == Action::Inc {
    let step = if AppState.turbo == 1 { 5 } else { 1 };
    AppState.counter = clamp_i64(AppState.counter + step, 0, 999999);
    sfx_click();
  } else if act == Action::Reset {
    AppState.counter = 0;
    AppState.intensity = 60;
    buf_clear(AppState::name_buf);
    sfx_clack();
  } else if act == Action::ToggleTurbo {
    AppState.turbo = 1 - AppState.turbo;
    sfx_click();
  }
  ui_fmt_centered(w / 2, h / 2 + 190, "Count: {AppState.counter}  |  Intensity: {AppState.intensity}%", 15, 1, pack_rgba(248, 250, 255, 255), 0);
  gfx_set_cursor(forma_cursor());
  gfx_end_frame()
}

export fn ifw_get_copy_mailbox_word() -> i64 { AppState::copy_mb }
export fn ifw_get_paste_mailbox_word() -> i64 { AppState::paste_mb }
export fn ifw_on_paste_text(ptr: i64, len: i64) -[Mem]-> i64 { forma_paste_text(ptr, len) }
export fn ifw_get_copy_text() -[Mem]-> i64 { forma_copy_text(AppState::copy_mb, 120) }
```

### Example 2: Combining `Infimum.Forma` HUD + Custom `Slot(render: draw_canvas)` for Games / Graphics
Use `Slot(render: fn_name)` whenever you need dynamic grids, animated orbs, charts, or custom 2D/3D rendering:
```infimum
// Slot render callback MUST have signature: (rx: i64, ry: i64, rw: i64, rh: i64, clip_id: i64) -[Mem]-> unit
fn draw_canvas(rx: i64, ry: i64, rw: i64, rh: i64, clip_id: i64) -[Mem]-> unit {
  let cx = rx + rw / 2;
  let cy = ry + rh / 2;
  ui_orb(cx, cy, 36, pack_rgba(52, 211, 153, 255), clip_id);
}

forma GameShell(vw: i64, vh: i64)
  req vw >= 640 && vw <= 4096 && vh >= 480 && vh <= 2160
  -[Mem]-> i64
{
  Col(x: 0, y: 0, w: vw, h: vh, bg: pack_rgba(17, 19, 23, 255)) {
    Row(h: 52, pad_x: 16, gap: 12, bg: pack_rgba(22, 25, 31, 255), align: center) {
      Text("My Game", w: 200, h: 28, size: 20, style: 1, color: pack_rgba(248, 250, 255, 255))
      Spacer()
      Button(w: 110, h: 32, bg: pack_rgba(44, 50, 64, 255), hover_bg: pack_rgba(64, 72, 92, 255), radius: 6, on_click: 1, align: center) {
        Text("New Game", h: 20, size: 14, style: 1, color: pack_rgba(255, 255, 255, 255), align: center)
      }
    }
    Box(grow: 1, pad: 16) {
      Slot(render: draw_canvas)
    }
  }
}
```

### Common Pitfalls to Avoid
- **Never** invent fake stubs like `fn str_len(s: str) -> i64 { 0 }` or `fn Gpu_submit(...) {}` — use `Infimum.Forma` `Text(...)`, `ui_fmt(...)` / `ui_fmt_centered(...)`, and `gfx_begin_frame` / `forma_begin_frame` / `gfx_end_frame`.
- **Never** write `layout AppMem at 0` — always use auto-placed `layout AppState { ... }` so user state is placed cleanly at `705536+` and never overlaps the System ABI + `FrameScratch` (`0..65535`) or `gpu_prims` (`65536..705535`).
- **Always** check `Col`/`Row` child sizes: `sum(child_h) + (n - 1) * gap + 2 * pad <= parent_h` so Z3 proves zero overflow (`E090_FORMA_LAYOUT_VIOLATION`).
- **Always** bind a memory field to a local `let` before checking its range and passing it to a function with `req` (two separate field loads are independent to Z3 unless the field has an `inv` range).
- **Do** use `inv lo..=hi` on `layout`/`struct` fields so Z3 knows their bounds on every read, and use composable `forma` subcomponents, `if`/`else`, `for` loops, and Forma 2.1 `Toggle` / `Slider` / `TextInput` widgets.
