<!--
  Infimum.Forma Engineering Specification
  Copyright (c) 2026 Yehuda Aminadav
  SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial
-->

# Infimum.Forma — Spatial-Interactive UI Layer Specification

`Infimum.Forma` is the fourth architectural layer of the Infimum language stack:

1. **Infimum.Telos** — Formal intent and Z3 contracts (`req` / `ens`).
2. **Infimum.Flux** — Deterministic logic, algebraic effect capabilities, and content-addressed semantic graph.
3. **Infimum.Forma** — Declarative spatial-interactive UI layer (compiled replacement for HTML + CSS + manual hit-testing).
4. **Infimum.Strata** — Linear memory, unified 1-Draw-Call GPU primitive stream, and direct Wasm/ELF machine code.

`Infimum.Forma` is a **pure compile-time frontend**. It introduces **zero** DOM, **zero** CSS cascade, **zero** JavaScript, and **zero** runtime overhead in `infimum-web`. Every `forma` block is statically verified by Z3 and lowered directly into ordinary `Infimum.Flux` / `Infimum.Strata` functions (`FnDef`) that emit 8-word GPU primitives (`push_prim`) and hardware clip rects (`set_clip_rect`).

---

## 1. Grammar

A `forma` declaration may appear at top level in any `.ifm` source file (including files imported via `use`):

```ebnf
FormaDecl   := ["export"] "forma" Ident "(" ParamList? ")"
               ContractClause*
               [ EffectArrow ReturnType ]
               ContractClause*
               "{" FormaNode "}"

ContractClause := ("req" | "ens") Expr
EffectArrow    := "-[" EffectList? "]->" | "->"

FormaNode   := NodeKind [ "(" PropList? ")" ] [ "{" FormaNode* "}" ]

NodeKind    := "Box" | "Col" | "Row" | "Stack" | "Scroll" | "Overlay"
             | "Button" | "Dropdown" | "Spacer" | "Divider" | "Rect" | "Slot" | "Text"

PropList    := [ StrLit ","? ] Prop ("," Prop)* ","?
Prop        := PropName ":" PropValue

PropName    := "x" | "y" | "w" | "h" | "min_w" | "min_h" | "grow"
             | "pad" | "pad_x" | "pad_y" | "gap" | "align"
             | "bg" | "hover_bg" | "border" | "border_col" | "radius" | "shadow"
             | "clip" | "id" | "on_click" | "visible"
             | "render" | "on_wheel"
             | "text" | "size" | "style" | "color"

AlignVal    := "start" | "center" | "end" | "stretch"
```

- If the return signature is omitted, it defaults to `-[Mem]-> i64` (returning the clicked `on_click` action ID, or `0` if no interactive node was clicked).
- `Slot(render: fn_name [, on_wheel: wheel_fn])` statically binds custom `Infimum.Flux` rendering/event functions with signature `(rx: i64, ry: i64, rw: i64, rh: i64, clip_id: i64) -[Mem]-> unit`.

---

## 2. Deterministic 2-Pass Layout Model (`measure` / `arrange`)

Layout resolves in **one deterministic frame** with zero recursive layout loops and zero multi-frame convergence:

For every container node with outer rectangle `(rx, ry, rw, rh)`:

- Padding:
  - `pad_x = prop.pad_x ?? prop.pad ?? 0`
  - `pad_y = prop.pad_y ?? prop.pad ?? 0`
- Inner content box:
  - `ix = rx + pad_x`, `iy = ry + pad_y`
  - `iw = rw - 2 * pad_x`, `ih = rh - 2 * pad_y`

`Overlay` children are excluded from their parent's normal flow (`Row`/`Col` do not allocate flow space or gaps to `Overlay`s).

### Pass 1 — `measure`
- In `Row`:
  - Each normal child with explicit `w` contributes `w` to `fixed_w`.
  - Each normal child without explicit `w` (or with `grow > 0`) contributes `g = grow ?? 1` to `total_grow`.
  - `total_gap = max(0, n - 1) * gap`.
  - `rem_w = iw - fixed_w - total_gap`.
- In `Col`: exact symmetric dual along the Y axis (`fixed_h`, `total_grow`, `total_gap`, `rem_h`).

### Pass 2 — `arrange`
- **`Row`**: lays out children left-to-right starting at `cur_x = ix`. Fixed-width children take `w`; flex children take `(rem_w * g) / total_grow`, with the final flex child absorbing any integer division remainder (`rem_w - allocated`) so total width is exact to the pixel. Cross-axis `ch = child.h ?? ih`, `cy = align_pos(iy, ih, ch, align)`.
- **`Col`**: symmetric top-to-bottom placement starting at `cur_y = iy`.
- **`Box` / `Stack` / `Scroll` / `Button` / `Dropdown`**: each normal child takes `cw = child.w ?? iw`, `ch = child.h ?? ih`, aligned inside `(ix, iy, iw, ih)` (or explicit `x`/`y` when specified).
- **`Overlay`**: positioned from its own `(x, y, w, h)` (defaulting to parent's `(rx, ry, rw, rh)` when omitted).

Cross-axis alignment helper `align_pos(start, avail, size, align)`:
- `start` / `stretch`: `start`
- `center`: `start + (avail - size) / 2`
- `end`: `start + (avail - size)`

---

## 3. Single Source of Truth: Layout + Clipping + Hit-Test + Render + `Overlay`

Each node's computed `(rx, ry, rw, rh, clip_id)` is shared identically across all four concerns:

1. **Clipping (`set_clip_rect`)**:
   - Hardware clip table lives at linear memory word `48000 + (cid & 15) * 4` (`x0, y0, x1, y1`, byte offset `192_000`), matching `infimum-web`.
   - When a node specifies `clip: C`, its clip rectangle is intersected with its parent's clip rectangle and written to slot `C & 15`.
2. **Priority Hit-Testing**:
   - Mouse coordinates `(mx, my)` and click flag `m_clicked` are read either from explicit `forma` parameters (`mx`, `my`, `m_clicked`) or from the `Infimum.Forma` linear memory input mailbox (`48100: mx`, `48101: my`, `48102: m_clicked`, `48103: wheel_dy`, `48104: hovered_id`).
   - **Overlay Priority**: visible `Overlay` subtrees are hit-tested **first** (top-most first). If an `Overlay` or any interactive descendant inside it contains `(mx, my)`, background hit-testing is suppressed so modals and dropdowns always intercept pointer events.
   - Interactive nodes (`Button`, `Dropdown`) switch their fill color from `bg` to `hover_bg` when hovered, and set the returned action ID to `on_click` (falling back to `id`) when hovered and `m_clicked != 0`.
3. **1-Draw-Call GPU Emission (`push_prim`)**:
   - Primitive counter at word `12` (`mem_load(12)`), primitive buffer at word `65536 + idx * 8` (byte offset `262_144`, capacity `80_000` primitives, 32 bytes each).
   - SDF Rounded Rect (`kind = 0`): `pmeta = 0 | (clamp(radius, 0, 120) << 4) | (clamp(border, 0, 15) << 12) | (clamp(shadow, 0, 60) << 16)`.
   - **Deferred Overlay Paint**: normal tree primitives are emitted first; all visible `Overlay` subtrees are emitted in a deferred top pass so they always render above the rest of the UI in the same single GPU draw call.

---

## 4. Formal Z3 UI Invariants (`Telos × Forma`)

Before any `forma` block is accepted by `infimum verify`, `infimum ingest`, or `infimum compile-wasm`, the compiler builds a symbolic SMT-LIB2 model of the layout tree under the block's `req` preconditions and proves four invariants via Z3:

1. **Non-Degeneracy**: For every visible interactive node (`Button`, `Dropdown`, `Scroll`), `rw > 0 && rh > 0` (and `rw >= min_w`, `rh >= min_h` if specified).
2. **No Sibling Overlap**:
   - Inside every `Row`: for every adjacent pair of flow children `(a, b)`, `a.x + a.w <= b.x`.
   - Inside every `Col`: for every adjacent pair of flow children `(a, b)`, `a.y + a.h <= b.y`.
3. **Bounds & Clip Containment**:
   - Inside every non-`Scroll`, non-`Overlay` container, every flow child stays within the parent's inner padded bounds: `cx >= ix && cy >= iy && cx + cw <= ix + iw && cy + ch <= iy + ih`.
   - Every nested `clip` rectangle is contained within its parent clip rectangle and non-inverted (`cx0 <= cx1 && cy0 <= cy1`).
4. **Primitive Budget Bound**:
   - The static upper bound on GPU primitives emitted by the `forma` tree is strictly less than `80_000` (`FORMA_MAX_PRIMS`).

Any violation fails compilation with `E090_FORMA_LAYOUT_VIOLATION` and includes Z3's concrete counterexample (input parameters and offending node geometry).
