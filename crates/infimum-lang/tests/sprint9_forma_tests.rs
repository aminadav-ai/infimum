// Copyright (c) 2026 Infimum Language Contributors
// License: Apache-2.0

use infimum_lang::forma::verify_module_formas;
use infimum_lang::parser::{parse_module, parse_module_from_path};
use infimum_lang::store::GraphStore;
use infimum_lang::strata_wasm::build_strata_wasm_from_module_with_cache;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_workspace(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "infimum_sprint9_{tag}_{}_{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_forma_z3_proves_valid_composer_bar_layout() {
    let src = r#"
fn draw_input_text(_rx: i64, _ry: i64, _rw: i64, _rh: i64, _clip: i64) -[Mem]-> unit { () }
fn handle_input_wheel(_rx: i64, _ry: i64, _rw: i64, _rh: i64, _clip: i64) -[Mem]-> unit { () }
fn draw_attach_icon(_rx: i64, _ry: i64, _rw: i64, _rh: i64, _clip: i64) -[Mem]-> unit { () }
fn draw_model_label(_rx: i64, _ry: i64, _rw: i64, _rh: i64, _clip: i64) -[Mem]-> unit { () }
fn draw_send_icon(_rx: i64, _ry: i64, _rw: i64, _rh: i64, _clip: i64) -[Mem]-> unit { () }

forma ComposerBar(x: i64, y: i64, w: i64, h: i64, theme_bg: i64, border_col: i64)
  req w >= 280 && h >= 80
  -[Mem]-> i64
{
  Box(x: x, y: y, w: w, h: h, radius: 12, border: 1, bg: theme_bg, border_col: border_col, pad: 10) {
    Col(gap: 8, grow: 1) {
      Scroll(id: 3, grow: 1, clip: 3) {
        Slot(render: draw_input_text, on_wheel: handle_input_wheel)
      }
      Row(h: 34, gap: 8, align: center) {
        Button(id: 101, w: 32, h: 32, radius: 8, bg: theme_bg, hover_bg: border_col, on_click: 11) {
          Slot(render: draw_attach_icon)
        }
        Dropdown(id: 102, w: 170, h: 30, radius: 8, bg: theme_bg, hover_bg: border_col, on_click: 12) {
          Slot(render: draw_model_label)
        }
        Spacer(grow: 1)
        Button(id: 103, w: 34, h: 34, radius: 17, bg: theme_bg, hover_bg: border_col, on_click: 1) {
          Slot(render: draw_send_icon)
        }
      }
    }
  }
}
"#;

    let module = parse_module(src).expect("valid ComposerBar forma should parse");
    assert_eq!(module.formas.len(), 1);

    let forma_reports =
        verify_module_formas(&module).expect("Z3 should prove all geometry invariants");
    assert_eq!(forma_reports.len(), 1);
    assert_eq!(forma_reports[0].name, "ComposerBar");
    assert!(forma_reports[0].obligations_proved > 0);
    assert!(forma_reports[0].static_prim_bound < 80_000);

    let ws = temp_workspace("valid_composer");
    let store = GraphStore::open(&ws).unwrap();
    let fn_reports = store
        .verify_module_with_cache(&module, false)
        .expect("lowered ComposerBar should pass full Telos+Flux verification");
    assert!(fn_reports.iter().any(|r| r.alias == "ComposerBar"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_forma_z3_catches_row_overflow_with_counterexample() {
    // Row(w: 200, gap: 10) with three buttons of w: 80 => 80 + 10 + 80 + 10 + 80 = 260 > 200.
    let src = r#"
forma BrokenRow(bg_col: i64)
  -[Mem]-> i64
{
  Row(x: 0, y: 0, w: 200, h: 40, gap: 10) {
    Button(id: 1, w: 80, h: 32, bg: bg_col, on_click: 1)
    Button(id: 2, w: 80, h: 32, bg: bg_col, on_click: 2)
    Button(id: 3, w: 80, h: 32, bg: bg_col, on_click: 3)
  }
}
"#;

    let module = parse_module(src).expect("syntactically valid forma should parse");
    let err = verify_module_formas(&module).expect_err("Z3 must catch Row overflow");
    assert_eq!(err.code, "E090_FORMA_LAYOUT_VIOLATION");
    assert_eq!(err.phase, "forma-verify");
    assert!(
        err.message.contains("Bounds overflow"),
        "expected bounds overflow message, got: {}",
        err.message
    );
    assert!(
        !err.counterexample.is_empty(),
        "Z3 must provide a concrete counterexample"
    );

    // Also ensure GraphStore::verify_module_with_cache blocks it at compile time.
    let ws = temp_workspace("row_overflow");
    let store = GraphStore::open(&ws).unwrap();
    let store_err = store
        .verify_module_with_cache(&module, false)
        .expect_err("GraphStore must reject overflowing forma");
    assert_eq!(store_err.code, "E090_FORMA_LAYOUT_VIOLATION");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_forma_z3_catches_button_collapse_non_degeneracy() {
    // When w == 200: pad=10 leaves iw=180; two fixed 90px spacers leave 0px for the flex Button!
    let src = r#"
forma CollapsingButton(w: i64, bg_col: i64)
  req w >= 200
  -[Mem]-> i64
{
  Box(x: 0, y: 0, w: w, h: 48, pad: 10) {
    Row(gap: 0) {
      Spacer(w: 90)
      Button(id: 1, grow: 1, h: 28, bg: bg_col, on_click: 1)
      Spacer(w: 90)
    }
  }
}
"#;

    let module = parse_module(src).expect("syntactically valid forma should parse");
    let err = verify_module_formas(&module)
        .expect_err("Z3 must catch Button collapsing to w <= 0 at w == 200");
    assert_eq!(err.code, "E090_FORMA_LAYOUT_VIOLATION");
    assert!(
        err.message.contains("Non-degeneracy violated"),
        "expected non-degeneracy violation, got: {}",
        err.message
    );
    assert_eq!(
        err.counterexample.get("w").map(String::as_str),
        Some("200"),
        "expected Z3 counterexample w = 200, got: {:?}",
        err.counterexample
    );
}

#[test]
fn test_forma_demo_compiles_to_ifw_and_passes_infimum_web_headless_check() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let demo_path = manifest_dir.join("examples/forma_ui_demo.ifm");
    let module = parse_module_from_path(&demo_path).expect("forma_ui_demo.ifm should parse");
    assert_eq!(module.formas.len(), 2);

    let ws = temp_workspace("forma_demo_axw");
    let out_axw = ws.join("forma_ui_demo.ifw");
    let mut store = GraphStore::open(&ws).unwrap();

    let artifact =
        build_strata_wasm_from_module_with_cache(&mut store, &module, &out_axw, false)
            .expect("forma_ui_demo.ifm should verify via Z3 and compile to .ifw");
    assert!(artifact.wasm_size_bytes > 0);
    assert!(artifact.exported_functions.contains(&"step_ui".to_string()));
    assert!(artifact
        .exported_functions
        .contains(&"init_app".to_string()));

    let wasm_bytes = fs::read(&out_axw).expect("compiled .ifw should exist");
    assert!(
        wasm_bytes.starts_with(b"\0asm"),
        "expected valid WebAssembly binary header"
    );

    let infimum_web_bin = std::env::var_os("INFIMUM_WEB_BIN")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            manifest_dir
                .parent()
                .map(|p| p.join("infimum-web/target/release/infimum-web"))
        })
        .filter(|p| p.is_file());

    if let Some(infimum_web_bin) = infimum_web_bin {
        let output = Command::new(&infimum_web_bin)
            .arg(&out_axw)
            .arg("--headless-check")
            .output()
            .expect("failed to execute infimum-web --headless-check");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "infimum-web --headless-check failed:\nstdout: {stdout}\nstderr: {stderr}"
        );
        assert!(
            stdout.contains("HEADLESS_OK"),
            "expected HEADLESS_OK in output, got:\n{stdout}"
        );

        // Parse `Prims=<N>` and verify real GPU primitives were emitted (> 0).
        let prims: usize = stdout
            .split('|')
            .find_map(|part| part.trim().strip_prefix("Prims="))
            .and_then(|num| num.trim().parse().ok())
            .unwrap_or(0);
        assert!(
            prims > 0,
            "expected Prims > 0 from Forma 1-Draw-Call stream, got stdout:\n{stdout}"
        );
    } else {
        eprintln!(
            "Skipping infimum-web --headless-check (sibling infimum-web binary not present in this environment)"
        );
    }

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_forma2_fit_content_and_justify_space_between() {
    // Reproduces the Infimum Code Native sidebar footer ("Настройки" + "119.2 МБ")
    // using Forma 2.0 `w: fit`, `h: fit`, and `justify: space_between`.
    let src = r#"
forma SidebarFooter(sb_w: i64, bg_col: i64, text_col: i64, green_col: i64)
  req sb_w >= 280
  -[Mem]-> i64
{
  Row(x: 0, y: 0, w: sb_w, h: 42, pad_x: 12, justify: space_between, align: center, bg: bg_col) {
    Button(id: 1, w: fit, h: fit, pad_x: 8, pad_y: 6, radius: 6, bg: bg_col, on_click: 1) {
      Text("Настройки", size: 14, color: text_col)
    }
    Text("119.2 МБ", size: 13, color: green_col)
  }
}
"#;

    let module = parse_module(src).expect("Forma 2.0 SidebarFooter should parse");
    let reports = verify_module_formas(&module)
        .expect("Z3 should prove SidebarFooter with w:fit, h:fit, justify:space_between");
    assert_eq!(reports.len(), 1);
    assert!(reports[0].obligations_proved > 0);

    let ws = temp_workspace("forma2_sidebar_footer");
    let store = GraphStore::open(&ws).unwrap();
    let fn_reports = store
        .verify_module_with_cache(&module, false)
        .expect("lowered Forma 2.0 SidebarFooter should pass Telos+Flux verification");
    assert!(fn_reports.iter().any(|r| r.alias == "SidebarFooter"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_forma2_shrink_and_ellipsis_prevents_overflow() {
    // Reproduces the "Ход работы" bar with arbitrary dynamic summary text on the left
    // and fixed right label ("развернуть"). `shrink: 1` guarantees no overflow for ANY string.
    let src = r#"
forma TimelineHeader(bar_w: i64, summary: str, bg_col: i64, text_col: i64, accent_col: i64)
  req bar_w >= 240
  -[Mem]-> i64
{
  Row(x: 0, y: 0, w: bar_w, h: 32, pad_x: 12, gap: 8, align: center, bg: bg_col) {
    Text(summary, size: 12, color: text_col, shrink: 1, overflow: ellipsis)
    Spacer(grow: 1)
    Text("развернуть", size: 12, color: accent_col)
  }
}
"#;

    let module = parse_module(src).expect("Forma 2.0 TimelineHeader should parse");
    let reports = verify_module_formas(&module)
        .expect("Z3 must prove TimelineHeader with shrink:1 + overflow:ellipsis never overflows");
    assert_eq!(reports.len(), 1);
    assert!(reports[0].obligations_proved > 0);
}

#[test]
fn test_forma2_z3_catches_unbounded_dynamic_text_overflow() {
    // Without `shrink: 1` or `max_w`, an arbitrary dynamic string inside a fixed-width Row
    // CAN overflow the right edge — Z3 must catch this at compile time!
    let src = r#"
forma UnsafeTimelineHeader(bar_w: i64, summary: str, bg_col: i64, text_col: i64, accent_col: i64)
  req bar_w >= 240
  -[Mem]-> i64
{
  Row(x: 0, y: 0, w: bar_w, h: 32, pad_x: 12, gap: 8, align: center, bg: bg_col) {
    Text(summary, size: 12, color: text_col)
    Spacer(grow: 1)
    Text("развернуть", size: 12, color: accent_col)
  }
}
"#;

    let module = parse_module(src).expect("UnsafeTimelineHeader should parse");
    let err = verify_module_formas(&module)
        .expect_err("Z3 must catch unbounded dynamic Text overflow in Row");
    assert_eq!(err.code, "E090_FORMA_LAYOUT_VIOLATION");
    assert!(
        err.message.contains("Bounds overflow"),
        "expected bounds overflow message, got: {}",
        err.message
    );
}

#[test]
fn test_forma2_full_capsule_compiles_and_runs_headless() {
    let src = r#"
forma Forma2Demo(vw: i64, vh: i64, summary: str)
  req vw >= 320 && vh >= 240
  -[Mem]-> i64
{
  Col(x: 0, y: 0, w: vw, h: vh, pad: 16, gap: 12, bg: 4279309080) {
    Row(h: 36, pad_x: 12, gap: 8, align: center, radius: 8, bg: 4280296747) {
      Text(summary, size: 13, color: 4293849848, shrink: 1, overflow: ellipsis)
      Spacer(grow: 1)
      Text("развернуть", size: 13, color: 4284193791)
    }
    Box(grow: 1, pad: 12, radius: 8, bg: 4279769635) {
      Text("Длинный многострочный текст в Infimum.Forma 2.0 с автоматическим переносом строк внутри контейнера.", size: 14, color: 4293849848, grow: 1, overflow: wrap)
    }
    Row(h: 40, pad_x: 12, justify: space_between, align: center, radius: 8, bg: 4280296747) {
      Button(id: 1, w: fit, h: fit, pad_x: 10, pad_y: 6, radius: 6, bg: 4281415488, on_click: 1) {
        Text("Настройки", size: 14, color: 4294967295)
      }
      Text("119.2 МБ", size: 13, color: 4281856625)
    }
  }
}

export fn init_app() -[Mem]-> unit {
  ()
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, m_down: i64, m_clicked: i64, wheel_dy: i64, key_char: i64, key_special: i64) -[Mem, Gpu]-> i64 {
  mem_store(12, 0);
  mem_store(48100, mx);
  mem_store(48101, my);
  mem_store(48102, m_clicked);
  mem_store(48103, wheel_dy);
  let w = if vw < 320 { 320 } else { vw };
  let h = if vh < 240 { 240 } else { vh };
  let _ = Forma2Demo(w, h, "Ход работы: 76 шагов (посл. 32) за 4 мин 30 с — очень длинный текст");
  mem_load(12)
}

export fn get_cmd_byte_offset() -[Mem]-> i64 { 65536 * 8 }
export fn get_clips_byte_offset() -[Mem]-> i64 { 48000 * 8 }
export fn get_prim_count() -[Mem]-> i64 { mem_load(12) }
export fn get_cursor_kind() -[Mem]-> i64 { 0 }
export fn ifw_unicode_atlas() -[Mem]-> i64 { 1 }
"#;

    let module = parse_module(src).expect("Forma2Demo should parse");
    let ws = temp_workspace("forma2_full_capsule");
    let out_axw = ws.join("forma2_demo.ifw");
    let mut store = GraphStore::open(&ws).unwrap();

    let artifact =
        build_strata_wasm_from_module_with_cache(&mut store, &module, &out_axw, false)
            .expect("Forma2Demo should verify via Z3 and compile to .ifw");
    assert!(artifact.wasm_size_bytes > 0);

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let infimum_web_bin = std::env::var_os("INFIMUM_WEB_BIN")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            manifest_dir
                .parent()
                .map(|p| p.join("infimum-web/target/release/infimum-web"))
        })
        .filter(|p| p.is_file());

    if let Some(infimum_web_bin) = infimum_web_bin {
        let output = Command::new(&infimum_web_bin)
            .arg(&out_axw)
            .arg("--headless-check")
            .output()
            .expect("failed to execute infimum-web --headless-check");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("HEADLESS_OK"),
            "Forma2Demo headless-check failed:\nstdout: {stdout}\nstderr: {stderr}"
        );
        let prims: usize = stdout
            .split('|')
            .find_map(|part| part.trim().strip_prefix("Prims="))
            .and_then(|num| num.trim().parse().ok())
            .unwrap_or(0);
        assert!(
            prims > 20,
            "expected > 20 GPU primitives from Forma2Demo text+rects, got {prims}\n{stdout}"
        );
    }

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_std_stage2_ui_kit_z3_and_headless_exec() {
    let src = r#"
use "std/prelude.ifm";

export fn init_app() -[Mem]-> unit {
  ()
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, _m_down: i64, m_clicked: i64, _wheel_dy: i64, _key_char: i64, _key_special: i64) -[Mem, Gpu]-> i64 {
  mem_store(12, 0);
  let w = clamp_i64(vw, 320, 2560);
  let h = clamp_i64(vh, 240, 1440);
  let th = THEME_DARK;
  let sz = theme_scale(14, 100);
  ui_rect(0, 0, w, h, 0, 0, 0, theme_bg(th), 0, 0);
  ui_card(16, 16, w - 32, h - 32, th, 10, 0);
  let bw = ui_badge(28, 28, "Forma 2.0", 13, theme_accent(), pack_rgba(255, 255, 255, 255), 0);
  let _ = fmt_duration(28 + bw + 12, 32, 2705, 13, theme_text(th), theme_muted(th), 0);
  let _ = fmt_mb(28, 64, 1192, 13, theme_success(), theme_success(), 0);
  let _ = fmt_text_ellipsis(28, 90, 180, "Длинная строка, которая должна аккуратно обрезаться с многоточием", sz, 0, theme_text(th), 0);
  let _ = ui_button(28, 120, 140, 34, "Настройки", sz, th, true, mx, my, m_clicked, 0);
  ui_progress_bar(28, 168, 220, 10, 76, 100, theme_surf2(th), theme_accent(), 0);
  ui_scrollbar_v(w - 26, 24, h - 48, 120, 600, h - 48, th, false, 0);
  ui_divider(28, 192, w - 56, th, 0);
  icon_folder(28, 204, 16, theme_warn(), 0);
  icon_check(60, 212, 6, theme_success(), 0);
  icon_spinner(88, 212, 8, 15, theme_accent(), 0);
  mem_load(12)
}

export fn get_cmd_byte_offset() -[Mem]-> i64 { GPU_CMD_BYTE_OFFSET }
export fn get_clips_byte_offset() -[Mem]-> i64 { GPU_CLIPS_BYTE_OFFSET }
export fn get_prim_count() -[Mem]-> i64 { mem_load(12) }
export fn get_cursor_kind() -[Mem]-> i64 { 0 }
export fn ifw_unicode_atlas() -[Mem]-> i64 { 1 }
"#;

    let ws = temp_workspace("std_stage2_ui_kit");
    let src_path = ws.join("std_ui_demo.ifm");
    let out_axw = ws.join("std_ui_demo.ifw");
    fs::write(&src_path, src).unwrap();

    let module = parse_module_from_path(&src_path).expect("std Stage 2 UI kit demo should parse");
    let mut store = GraphStore::open(&ws).unwrap();
    let artifact =
        build_strata_wasm_from_module_with_cache(&mut store, &module, &out_axw, false)
            .expect("std Stage 2 UI kit demo should pass Z3 verification and compile to .ifw");
    assert!(artifact.wasm_size_bytes > 0);

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let infimum_web_bin = std::env::var_os("INFIMUM_WEB_BIN")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            manifest_dir
                .parent()
                .map(|p| p.join("infimum-web/target/release/infimum-web"))
        })
        .filter(|p| p.is_file());

    if let Some(infimum_web_bin) = infimum_web_bin {
        let output = Command::new(&infimum_web_bin)
            .arg(&out_axw)
            .arg("--headless-check")
            .output()
            .expect("failed to execute infimum-web --headless-check");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("HEADLESS_OK"),
            "std Stage 2 UI kit headless-check failed:\nstdout: {stdout}\nstderr: {stderr}"
        );
    }

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_std_stage3_text_edit_and_markdown_z3_and_headless_exec() {
    let src = r##"
use "std/prelude.ifm";
use "std/text_edit.ifm";
use "std/markdown.ifm";

layout DemoMem at 0 {
  cursor_kind: word at 11,
  prim_count: word at 12,
  edit_buf: words[512] at 1000,
  md_buf: words[512] at 2000,
}

export fn init_app() -[Mem]-> unit {
  let mut p = 0;
  p = tbuf_insert_char(DemoMem::edit_buf, 500, p, 'H');
  p = tbuf_insert_char(DemoMem::edit_buf, 500, p, 'i');
  p = tbuf_insert_char(DemoMem::edit_buf, 500, p, '!');
  let _ = tbuf_backspace(DemoMem::edit_buf, 500, p, p);
  do Mem.store_str(2000, "# Title\n- **Bold** `code`\n---\nHello");
  ()
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, _m_down: i64, _m_clicked: i64, _wheel_dy: i64, _key_char: i64, _key_special: i64) -[Mem, Gpu]-> i64 {
  mem_store(12, 0);
  let w = clamp_i64(vw, 320, 2560);
  let h = clamp_i64(vh, 240, 1440);
  let th = THEME_DARK;
  ui_rect(0, 0, w, h, 0, 0, 0, theme_bg(th), 0, 0);
  let _ = tbuf_measure_wrapped_h(DemoMem::edit_buf, 500, w - 40, 14, 20, 0);
  let _ = tbuf_hit_wrapped(DemoMem::edit_buf, 500, mx, my, 20, 20, w - 40, 14, 20, 0);
  let eh = tbuf_draw_wrapped(
    DemoMem::edit_buf, 500, 20, 20, w - 40, 14, 20, 0, theme_text(th),
    0, 1, theme_accent(), 2, theme_accent(), 0, 0, h
  );
  let _ = md_measure_h(DemoMem::md_buf, 500, w - 40, 14, 20, th);
  let _ = md_draw(DemoMem::md_buf, 500, 20, 32 + eh, w - 40, 14, 20, th, 0, 0, h);
  mem_load(12)
}

export fn get_cmd_byte_offset() -[Mem]-> i64 { GPU_CMD_BYTE_OFFSET }
export fn get_clips_byte_offset() -[Mem]-> i64 { GPU_CLIPS_BYTE_OFFSET }
export fn get_prim_count() -[Mem]-> i64 { mem_load(12) }
export fn get_cursor_kind() -[Mem]-> i64 { 0 }
export fn ifw_unicode_atlas() -[Mem]-> i64 { 1 }
"##;

    let ws = temp_workspace("std_stage3_text_md");
    let src_path = ws.join("std_stage3_demo.ifm");
    let out_axw = ws.join("std_stage3_demo.ifw");
    fs::write(&src_path, src).unwrap();

    let module = parse_module_from_path(&src_path).expect("std Stage 3 demo should parse");
    let mut store = GraphStore::open(&ws).unwrap();
    let artifact =
        build_strata_wasm_from_module_with_cache(&mut store, &module, &out_axw, false)
            .expect("std Stage 3 text_edit + markdown should pass Z3 and compile to .ifw");
    assert!(artifact.wasm_size_bytes > 0);

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let infimum_web_bin = std::env::var_os("INFIMUM_WEB_BIN")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            manifest_dir
                .parent()
                .map(|p| p.join("infimum-web/target/release/infimum-web"))
        })
        .filter(|p| p.is_file());

    if let Some(infimum_web_bin) = infimum_web_bin {
        let output = Command::new(&infimum_web_bin)
            .arg(&out_axw)
            .arg("--headless-check")
            .output()
            .expect("failed to execute infimum-web --headless-check");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("HEADLESS_OK"),
            "std Stage 3 headless-check failed:\nstdout: {stdout}\nstderr: {stderr}"
        );
    }

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_forma_interactive_widgets_toggle_slider_text_input_z3_and_headless() {
    let src = r#"
use "std/prelude.ifm";

layout WidgetDemo {
  dark_mode: word,
  font_scale: word,
  tick: word,
  api_key: words[128],
}

forma WidgetScreen(vw: i64, vh: i64, dark: bool)
  req vw >= 640 && vw <= 4096 && vh >= 480 && vh <= 2160
  -[Mem]-> i64
{
  Box(x: 0, y: 0, w: vw, h: vh, bg: 0xFF0F172A, align: center) {
    Col(w: 440, h: 240, pad: 20, gap: 16, bg: 0xFF1E293B, border: 1, border_col: 0xFF334155, radius: 12) {
      Row(h: 32, gap: 12, align: center) {
        Text("Тёмная тема", grow: 1, size: 15, color: 0xFFF8FAFC)
        Toggle(id: 10, checked: dark, bind: WidgetDemo::dark_mode, on_change: 101)
      }
      Row(h: 32, gap: 12, align: center) {
        Text("Масштаб", w: 100, size: 15, color: 0xFFF8FAFC)
        Slider(id: 11, grow: 1, bind: WidgetDemo::font_scale, min: 80, max: 200, step: 5, on_change: 102)
      }
      TextInput(id: 12, grow: 1, h: 36, buf: WidgetDemo::api_key, max_len: 120, mask: true, placeholder: "Вставьте ключ...", on_change: 103, on_submit: 104)
    }
  }
}

export fn init_app(seed: i64, vw: i64, vh: i64) -[Mem]-> i64 {
  WidgetDemo.dark_mode = 1;
  WidgetDemo.font_scale = 120;
  WidgetDemo.tick = 0;
  0
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, md: i64, mc: i64, wd: i64, kc: i64, ks: i64) -[Mem, Gpu]-> i64 {
  WidgetDemo.tick += 1;
  forma_begin_frame(vw, vh, mx, my, mc, wd);
  forma_set_input(md, kc, ks, WidgetDemo.tick);
  let sw = clamp_i64(vw, 640, 4096);
  let sh = clamp_i64(vh, 480, 2160);
  let _act = WidgetScreen(sw, sh, WidgetDemo.dark_mode != 0);
  let cur = forma_cursor();
  if cur != 0 { gfx_set_cursor(cur); }
  gfx_end_frame()
}
"#;

    let ws = temp_workspace("forma_widgets_demo");
    let src_path = ws.join("widgets_demo.ifm");
    let out_ifw = ws.join("widgets_demo.ifw");
    fs::write(&src_path, src).unwrap();

    let module = parse_module_from_path(&src_path).expect("Forma widgets demo should parse");
    let mut store = GraphStore::open(&ws).unwrap();
    let artifact =
        build_strata_wasm_from_module_with_cache(&mut store, &module, &out_ifw, false)
            .expect("Forma Toggle/Slider/TextInput should pass Z3 and compile to .ifw");
    assert!(artifact.wasm_size_bytes > 0);

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let infimum_web_bin = std::env::var_os("INFIMUM_WEB_BIN")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            manifest_dir
                .parent()
                .map(|p| p.join("infimum-web/target/release/infimum-web"))
        })
        .filter(|p| p.is_file());

    if let Some(infimum_web_bin) = infimum_web_bin {
        let output = Command::new(&infimum_web_bin)
            .arg(&out_ifw)
            .arg("--headless-check")
            .output()
            .expect("failed to execute infimum-web --headless-check");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("HEADLESS_OK"),
            "Forma widgets headless-check failed:\nstdout: {stdout}\nstderr: {stderr}"
        );
    }

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_forma_slider_z3_rejects_invalid_min_ge_max() {
    let src = r#"
forma BadSlider(vw: i64, vh: i64)
  req vw >= 300 && vh >= 100
  -[Mem]-> i64
{
  Box(x: 0, y: 0, w: vw, h: vh) {
    Slider(w: 200, h: 24, min: 100, max: 50)
  }
}
"#;
    let module = parse_module(src).expect("should parse");
    let err = infimum_lang::forma::verify_forma(&module.formas[0])
        .expect_err("Z3 must reject Slider with min >= max");
    assert!(
        err.message.contains("min") && err.message.contains("max"),
        "unexpected Z3 message: {}",
        err.message
    );
}
