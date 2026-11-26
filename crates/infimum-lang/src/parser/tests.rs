mod struct_layout_tests {
    use super::super::*;

    #[test]
    fn parses_and_lowers_structs_and_indexed_arrays() {
        let src = r#"
struct Sheep {
  x: word,
  z: word,
  vx: word,
  alive: word,
}

layout GameMem at 0 {
  tick: word,
  hero: Sheep,
  flock: Sheep[8] at 20,
  board: words[16] at 100,
}

fn step(i: i64) -[Mem]-> i64 req i >= 0 && i < 8 {
  GameMem.tick += 1;
  GameMem.hero.x = 42;
  GameMem.hero.vx += 3;
  GameMem.flock[i].x += GameMem.flock[i].vx;
  GameMem.board[i] = Sheep::SIZE + GameMem::flock::stride + GameMem::flock::count;
  GameMem.flock[i].x + GameMem.board[i] + GameMem.hero.x
}
"#;
        let module = parse_module(src).expect("struct + indexed layout should parse cleanly");
        assert_eq!(module.layouts.len(), 1);
        let l = &module.layouts[0];
        let flock = l.fields.iter().find(|f| f.name == "flock").unwrap();
        assert_eq!(flock.offset, 20);
        assert_eq!(flock.len, 32); // 8 * 4 words
        assert!(flock.is_array);
    }

    #[test]
    fn unknown_struct_field_lists_available_fields_in_hint() {
        let src = r#"
struct Vec2 {
  x: word,
  y: word,
}
layout M at 0 {
  p: Vec2,
}
fn bad() -[Mem]-> i64 {
  M.p.z
}
"#;
        let err = parse_module(src).expect_err("should fail on unknown subfield");
        assert!(err.repair_hint.contains("x, y"), "hint was: {}", err.repair_hint);
    }

    #[test]
    fn auto_layout_places_safely_above_gpu_prims_and_packs_multiple_layouts() {
        // Headless profile (no std/gfx): starts at 65_536 right above FrameScratch
        let headless_src = r#"
layout GameState {
  score: word,
  board: words[10],
}
layout UiState {
  hover: word,
}
fn f() -[Mem]-> i64 {
  GameState.score = 1;
  UiState.hover = 2;
  GameState.score + UiState.hover
}
"#;
        let hm = parse_module(headless_src).expect("headless auto layouts should parse");
        assert_eq!(hm.layouts[0].base, 65_536);
        assert_eq!(hm.layouts[0].fields[0].offset, 65_536);
        assert_eq!(hm.layouts[1].base, 65_547);
        assert_eq!(hm.layouts[1].fields[0].offset, 65_547);

        // GPU profile (with GPU_CMD_BYTE_OFFSET / std/gfx): starts at 705_536 above gpu_prims
        let gpu_src = r#"
const GPU_CMD_BYTE_OFFSET: i64 = 262144;
layout GameState {
  score: word,
  board: words[10],
}
layout UiState {
  hover: word,
}
fn f() -[Mem]-> i64 {
  GameState.score = 1;
  UiState.hover = 2;
  GameState.score + UiState.hover
}
"#;
        let gm = parse_module(gpu_src).expect("gpu auto layouts should parse");
        assert_eq!(gm.layouts[0].base, 705_536);
        assert_eq!(gm.layouts[0].fields[0].offset, 705_536);
        assert_eq!(gm.layouts[1].base, 705_547);
        assert_eq!(gm.layouts[1].fields[0].offset, 705_547);
    }

    #[test]
    fn gpu_layout_overwriting_prim_count_word_12_is_rejected() {
        let src = r#"
const GPU_CMD_BYTE_OFFSET: i64 = 262144;
layout Bad at 0 {
  cells: words[20],
}
fn f() -[Mem]-> i64 { Bad.cells[0] }
"#;
        let err = parse_module(src).expect_err("overwriting word 12 must fail");
        assert_eq!(err.code, "E066_RESERVED_RUNTIME_ADDR");
    }

    #[test]
    fn layout_exceeding_64mib_linear_memory_is_rejected() {
        let src = r#"
layout TooBig at 16770000 {
  buf: words[10000],
}
fn f() -[Mem]-> i64 { TooBig.buf[0] }
"#;
        let err = parse_module(src).expect_err("layout past 16,777,216 words must fail");
        assert_eq!(err.code, "E067_LAYOUT_OUT_OF_MEMORY");
    }

    #[test]
    fn auto_exports_are_generated_for_gpu_capsules() {
        let src = r#"
const GPU_CMD_BYTE_OFFSET: i64 = 262144;
const GPU_CLIPS_BYTE_OFFSET: i64 = 256;
export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, md: i64, mc: i64, wd: i64, kc: i64, ks: i64) -[Mem, Gpu]-> i64 {
  do Gpu.submit(0, 0);
  0
}
"#;
        let module = parse_module(src).expect("minimal GPU capsule should auto-export");
        for name in [
            "ifw_abi_version",
            "ifw_unicode_atlas",
            "get_cmd_byte_offset",
            "get_clips_byte_offset",
            "get_font_metrics_byte_offset",
            "get_input_byte_offset",
            "get_prim_count",
            "get_cursor_kind",
        ] {
            assert!(
                module.functions.iter().any(|f| f.exported && f.name == name),
                "missing auto-export {name}"
            );
        }
    }

    #[test]
    fn std_mem_scratch_arena_pool_and_16mib_auto_sizing_work() {
        let src = r#"
use "std/mem.ifm";

layout BigApp {
  arena_cur: word,
  arena_buf: words[2500000],
  pool_flags: words[16],
}

export fn run_mem_demo() -[Mem]-> i64 {
  scratch_reset();
  let s1 = scratch_alloc(32);
  let s2 = scratch_alloc(16);
  arena_reset(BigApp::arena_cur);
  let a1 = arena_alloc(BigApp::arena_buf, 2500000, BigApp::arena_cur, 100);
  pool_init(BigApp::pool_flags, 16);
  let p0 = pool_alloc(BigApp::pool_flags, 16);
  let p1 = pool_alloc(BigApp::pool_flags, 16);
  pool_free(BigApp::pool_flags, p0, 16);
  let p2 = pool_alloc(BigApp::pool_flags, 16);
  if s1 == 4096 && s2 == 4128 && a1 == BigApp::arena_buf && p0 == 0 && p1 == 1 && p2 == 0 {
    1
  } else {
    0
  }
}
"#;
        let files = BTreeMap::from([("main.ifm".to_string(), src.to_string())]);
        let (wasm, meta) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("std/mem.ifm + 16MiB layout should compile");
        assert!(meta.ok);
        // Verify Wasm Memory section requests 256 pages (16 MiB) because BigApp ends > 2,097,152 words.
        // Memory section id = 5: [0x05, size, 0x01, 0x00, 0x80, 0x02] (256 in ULEB128 = 0x80 0x02)
        assert!(
            wasm.windows(6).any(|w| w == [0x05, 0x04, 0x01, 0x00, 0x80, 0x02]),
            "expected Wasm Memory section with 256 pages (16 MiB)"
        );
    }

    #[test]
    fn for_break_continue_desugar_and_typecheck_cleanly() {
        let src = r#"
fn sum_filtered(n: i64) -> i64 {
  let mut sum = 0;
  for i in 0..10 {
    if i == 3 { continue; }
    if i == 7 { break; }
    sum += i;
  }
  for j in 1..=3 {
    sum += j;
  }
  for k in 0..n max 16 {
    if k == 2 { break; }
    sum += 1;
  }
  sum
}
"#;
        let module = parse_module(src).expect("for/break/continue should parse and lower");
        assert_eq!(module.functions.len(), 1);
        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), src.to_string());
        let (wasm_bytes, meta) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("typecheck + light verify + wasm compile should pass for/break/continue");
        assert!(!wasm_bytes.is_empty());
        assert_eq!(meta.compiled_nodes, 1);
    }

    #[test]
    fn break_outside_loop_is_rejected() {
        let src = r#"
fn bad() -> i64 {
  break;
  0
}
"#;
        let err = parse_module(src).expect_err("break outside loop must fail");
        assert_eq!(err.code, "E067_BREAK_OUTSIDE_LOOP");
    }

    #[test]
    fn field_invariants_propagate_on_reads_and_reject_invalid_writes() {
        let good_src = r#"
struct Cell {
  color: word inv 1..=7,
}
layout Board {
  turn: word inv 1..=2,
  cells: Cell[9],
}
fn div_by_turn(x: i64) -[Mem]-> i64 {
  Board.turn = 2;
  Board.cells[0].color = 5;
  x / Board.turn + 100 / Board.cells[0].color
}
"#;
        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), good_src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("field invariants should prove non-zero divisors without clamp");
        assert!(!wasm.is_empty());

        let bad_src = r#"
layout Board {
  turn: word inv 1..=2,
}
fn bad_write() -[Mem]-> i64 {
  Board.turn = 99;
  Board.turn
}
"#;
        let mut bad_files = BTreeMap::new();
        bad_files.insert("main.ifm".to_string(), bad_src.to_string());
        let err = crate::wasm_api::compile_project_in_memory("main.ifm", &bad_files)
            .expect_err("storing 99 into inv 1..=2 must fail");
        assert_eq!(err.code, "E054_FIELD_INVARIANT_VIOLATED");
    }

    #[test]
    fn builtin_test_blocks_run_and_report_pass_and_fail() {
        let src = r#"
layout Board {
  cells: words[9] inv 0..=2,
}
fn sum_range(n: i64) -> i64 {
  let mut s = 0;
  for i in 1..=n max 32 {
    s += i;
  }
  s
}
test "sum_range and board layout" {
  assert(sum_range(5) == 15);
  Board.cells[4] = 2;
  assert(Board.cells[4] == 2);
}
test "intentional failure" {
  assert(sum_range(3) == 999);
}
"#;
        let module = parse_module(src).expect("test blocks should parse");
        let results = crate::eval::run_module_tests(&module);
        assert_eq!(results.len(), 2);
        assert!(results[0].passed, "first test should pass: {:?}", results[0]);
        assert!(!results[1].passed, "second test should fail: {:?}", results[1]);
        assert!(results[1].fail_line > 0);
    }

    #[test]
    fn tuples_return_destructure_field_access_eval_and_wasm_compile() {
        let src = r#"
fn divmod_18(idx: i64) -> (i64, i64)
  req idx >= 0
{
  (idx / 18, idx % 18)
}

export fn sum_coords(idx: i64) -> i64
  req idx >= 0
{
  let (r, c) = divmod_18(idx);
  let r2 = divmod_18(idx).0;
  let c2 = (10, 20).1;
  r + c + r2 + c2
}

test "divmod_18 and destructuring" {
  let (r, c) = divmod_18(40);
  assert(r == 2);
  assert(c == 4);
  assert(sum_coords(40) == 28);
}
"#;
        let module = parse_module(src).expect("tuples should parse and lower");
        let results = crate::eval::run_module_tests(&module);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "tuple test failed: {:?}", results[0]);

        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("tuple multi-value Wasm compilation should succeed");
        assert!(!wasm.is_empty());
    }

    #[test]
    fn forma_nested_components_if_else_and_for_expand_and_compile() {
        let src = r#"
forma PillBtn(label: str, act_id: i64) -[Mem]-> i64 {
  Button(w: 96, h: 36, bg: 0xFF334455, hover_bg: 0xFF445566, radius: 8, on_click: act_id, align: center) {
    Text(label, w: 80, h: 20, size: 14, style: 1, color: 0xFFFFFFFF, align: center)
  }
}

forma MainPanel(vw: i64, vh: i64, show_extra: bool)
  req vw >= 640 && vw <= 4096 && vh >= 480 && vh <= 2160
  -[Mem]-> i64
{
  Box(x: 0, y: 0, w: vw, h: vh, bg: 0xFF111318, align: center) {
    Col(w: 420, h: 240, pad: 16, gap: 10, bg: 0xFF1D2230, radius: 12, align: center) {
      Row(w: 360, h: 40, gap: 8, align: center) {
        for i in 0..3 {
          PillBtn("Btn", i + 1)
        }
      }
      if show_extra {
        PillBtn("Extra", 10)
      } else {
        Text("Hidden", w: 120, h: 24, size: 14, style: 0, color: 0xFF99AABB, align: center)
      }
    }
  }
}
"#;
        let module = parse_module(src).expect("Forma components + if/else + for should expand");
        assert_eq!(module.formas.len(), 2);
        let main_forma = module
            .formas
            .iter()
            .find(|f| f.name == "MainPanel" || f.name.ends_with("__MainPanel"))
            .unwrap();
        let col = &main_forma.root.children[0];
        let row = &col.children[0];
        // `for i in 0..3 { PillBtn(...) }` expanded into 3 Button nodes!
        assert_eq!(row.children.len(), 3);
        assert_eq!(row.children[0].kind, crate::forma::FormaNodeKind::Button);
        // `if show_extra { PillBtn(...) } else { Text(...) }` expanded into 2 children with opposite visible guards!
        assert_eq!(col.children.len(), 3);
        assert!(col.children[1].props.visible.is_some());
        assert!(col.children[2].props.visible.is_some());
    }

    #[test]
    fn mod_pub_namespaces_isolate_private_helpers_and_block_external_private_access() {
        let ok_src = r#"
mod alpha {
  const SECRET: i64 = 10;
  pub const SCALE: i64 = 2;
  fn helper(x: i64) -> i64 { x + SECRET }
  pub fn compute(x: i64) -> i64 { helper(x) * SCALE }
}

mod beta {
  const SECRET: i64 = 100;
  fn helper(x: i64) -> i64 { x + SECRET }
  pub fn compute(x: i64) -> i64 { helper(x) }
}

export fn run(x: i64) -> i64 {
  alpha::compute(x) + beta::compute(x) + alpha::SCALE
}

test "two modules with identical private helpers do not collide" {
  assert(run(5) == 137);
}
"#;
        let module = parse_module(ok_src).expect("mod/pub namespaces should parse and lower");
        let results = crate::eval::run_module_tests(&module);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "mod test failed: {:?}", results[0]);

        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), ok_src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("mod/pub Wasm compile should pass");
        assert!(!wasm.is_empty());

        // Calling private function externally must fail with E068_PRIVATE_ITEM
        let bad_fn = r#"
mod m {
  fn hidden_fn() -> i64 { 42 }
}
fn main() -> i64 { m::hidden_fn() }
"#;
        let err = parse_module(bad_fn).expect_err("calling private mod fn must fail");
        assert_eq!(err.code, "E068_PRIVATE_ITEM");

        // Reading private constant externally must fail with E068_PRIVATE_ITEM
        let bad_const = r#"
mod m {
  const SECRET: i64 = 42;
}
fn main() -> i64 { m::SECRET }
"#;
        let err2 = parse_module(bad_const).expect_err("reading private mod const must fail");
        assert_eq!(err2.code, "E068_PRIVATE_ITEM");

        // File-based `use "ai.ifm" as ai;` in-memory (Online Studio)
        let mut proj = BTreeMap::new();
        proj.insert(
            "ai.ifm".to_string(),
            "const BONUS: i64 = 7;\nfn hidden(x: i64) -> i64 { x + BONUS }\npub fn pick(x: i64) -> i64 { hidden(x) * 2 }\n".to_string(),
        );
        proj.insert(
            "main.ifm".to_string(),
            "use \"ai.ifm\" as ai;\nexport fn play(x: i64) -> i64 { ai::pick(x) }\n".to_string(),
        );
        let (wasm2, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &proj)
            .expect("file-based use ... as should compile in memory");
        assert!(!wasm2.is_empty());
    }

    #[test]
    fn ui_fmt_desugars_zero_heap_into_ui_text_and_ui_int() {
        let src = r#"
use "std/prelude.ifm";

layout Game {
  score: word,
  speed: fixed,
}

export fn draw_hud(turn: i64, vel: fixed) -[Mem]-> i64 {
  Game.score = 42;
  Game.speed = vel;
  let w = ui_fmt_width("v={Game.speed} ({vel:.1})", 16, 1);
  let _ = ui_fmt_centered(320, 40, "Ход {turn + 1} — счёт {Game.score} — v={Game.speed} ({vel:.1})", 16, 1, pack_rgba(255, 255, 255, 255), 0);
  w
}
"#;
        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("ui_fmt should desugar and compile cleanly");
        assert!(!wasm.is_empty());
    }

    #[test]
    fn fixed_q16_16_type_arithmetic_layout_and_safety_work() {
        let ok_src = r#"
use "std/math.ifm";

const GRAVITY: fixed = 9.81;
const DT: fixed = 0.5;

struct Body {
  y: fixed inv 0.0..=1000.0,
  vy: fixed,
}

layout Physics {
  bodies: Body[4],
  wind: fixed,
}

export fn step_fall(v0: fixed, dt: fixed) -> fixed
  req dt > 0.0
{
  let v1 = v0 + GRAVITY * dt;
  v1 / 2.0
}

export fn init_physics() -[Mem]-> i64 {
  Physics.wind = -0.25;
  for i in 0..4 {
    if i >= 0 {
      Physics.bodies[i].y = fixed(10 + i);
      Physics.bodies[i].vy = 1.5;
    }
  }
  fixed_to_int(Physics.bodies[2].y + Physics.bodies[2].vy * 2)
}

test "fixed Q16.16 arithmetic, layout fields, and trig" {
  let _ = init_physics();
  assert(fixed_to_int(Physics.bodies[0].y) == 10);
  assert(fixed_to_int(Physics.bodies[2].y + Physics.bodies[2].vy * 2) == 15);
  assert(fixed_to_int(1.5 * 4.0) == 6);
  assert(fixed_to_int(7.5 / 2.5) == 3);
  assert(fixed_to_int(fixed_abs(-3.5)) == 3);
  assert(fixed_sin(1024) == 1.0);
  assert(fixed_cos(0) == 1.0);
  assert(step_fall(0.0, DT) > 2.4 && step_fall(0.0, DT) < 2.5);
}
"#;
        let module = parse_module(ok_src).expect("fixed Q16.16 module should parse and lower");
        let results = crate::eval::run_module_tests(&module);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "fixed test failed: {:?}", results[0]);

        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), ok_src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("fixed Q16.16 Wasm compile should pass");
        assert!(!wasm.is_empty());

        // Type mismatch: cannot add raw i64 and fixed without fixed(...)
        let bad_mix = "fn bad(a: i64) -> fixed { a + 1.5 }\n";
        let mut f_bad = BTreeMap::new();
        f_bad.insert("main.ifm".to_string(), bad_mix.to_string());
        let err = crate::wasm_api::compile_project_in_memory("main.ifm", &f_bad)
            .expect_err("mixing i64 + fixed must fail typecheck");
        assert_eq!(err.code, "E060_TYPE_MISMATCH");

        // Division by 0.0 must be caught statically
        let bad_div = "fn bad_div(a: fixed) -> fixed { a / 0.0 }\n";
        let mut f_div = BTreeMap::new();
        f_div.insert("main.ifm".to_string(), bad_div.to_string());
        let err_div = crate::wasm_api::compile_project_in_memory("main.ifm", &f_div)
            .expect_err("dividing fixed by 0.0 must fail");
        assert_eq!(err_div.code, "E053_POSSIBLE_DIV_BY_ZERO");
    }

    #[test]
    fn value_structs_construct_pass_return_mutate_layout_and_wasm_compile() {
        let src = r#"
use "std/prelude.ifm";

struct Vec3 {
  x: fixed,
  y: fixed,
  z: fixed,
}

layout World {
  hero: Vec3,
  pts: Vec3[4],
}

fn vec_add(a: Vec3, b: Vec3) -> Vec3 {
  Vec3 { x: a.x + b.x, y: a.y + b.y, z: a.z + b.z }
}

fn vec_dot(a: Vec3, b: Vec3) -> fixed {
  a.x * b.x + a.y * b.y + a.z * b.z
}

fn safe_ratio(v: Vec3) -> fixed
  req v.y != 0.0
{
  v.x / v.y
}

export fn init_app() -[Mem]-> i64 {
  let start = Vec3 { z: 3.0, x: 1.0, y: 2.0 };
  World.hero = start;
  World.pts[1] = vec_add(start, Vec3 { x: 0.5, y: 1.5, z: 2.5 });
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, md: i64, mc: i64, wd: i64, kc: i64, ks: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  gfx_begin_frame(w, h);
  let mut p = World.hero;
  p.x += 0.5;
  World.hero = p;
  ui_fmt(12, 12, "Pos: ({p.x:.1}, {p.y:.1}, {p.z:.1})", 14, 1, pack_rgba(255, 255, 255, 255), 0);
  gfx_end_frame()
}

test "value structs math, mutation, and layout roundtrip" {
  let _ = init_app();
  let mut v: Vec3 = World.hero;
  assert(v.x == 1.0 && v.y == 2.0 && v.z == 3.0);
  v.x += 2.5;
  v.z = 10.0;
  assert(v.x == 3.5 && v.z == 10.0);
  let sum = vec_add(v, World.pts[1]);
  assert(sum.x == 5.0 && sum.y == 5.5 && sum.z == 15.5);
  assert(vec_add(Vec3 { x: 1.0, y: 2.0, z: 3.0 }, Vec3 { x: 4.0, y: 5.0, z: 6.0 }).y == 7.0);
  assert(vec_dot(Vec3 { x: 1.0, y: 2.0, z: 3.0 }, Vec3 { x: 2.0, y: 3.0, z: 4.0 }) == 20.0);
  assert(safe_ratio(Vec3 { x: 9.0, y: 3.0, z: 0.0 }) == 3.0);
}
"#;
        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("value-struct Wasm compile should pass");
        assert!(!wasm.is_empty());

        let module = parse_module(src).expect("value-struct module should parse");
        let results = crate::eval::run_module_tests(&module);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "value-struct test failed: {:?}", results[0]);

        // Missing field error
        let bad_missing = "struct P { x: i64, y: i64 }\nfn f() -> P { P { x: 1 } }\n";
        let err_m = parse_module(bad_missing).expect_err("missing field must fail");
        assert_eq!(err_m.code, "E063_MISSING_FIELD");

        // Unknown field error
        let bad_unk = "struct P { x: i64, y: i64 }\nfn f() -> P { P { x: 1, y: 2, z: 3 } }\n";
        let err_u = parse_module(bad_unk).expect_err("unknown field must fail");
        assert_eq!(err_u.code, "E063_UNKNOWN_FIELD");

        // Duplicate field error
        let bad_dup = "struct P { x: i64, y: i64 }\nfn f() -> P { P { x: 1, x: 2, y: 3 } }\n";
        let err_d = parse_module(bad_dup).expect_err("duplicate field must fail");
        assert_eq!(err_d.code, "E063_DUPLICATE_FIELD");
    }
}
