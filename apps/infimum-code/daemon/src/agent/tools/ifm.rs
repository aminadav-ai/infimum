//! Infimum Lang toolchain, guide, stdlib, starter templates, PBR textures, and binary resolvers.

use crate::policy::CapabilityPolicy;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::sandbox::{is_jailed_in, resolve_in_project};

pub(in crate::agent) const EMBEDDED_INFIMUM_GUIDE: &str =
    include_str!("../../../../../../crates/infimum-lang/docs/INFIMUM_LANG_GUIDE.md");

pub(in crate::agent) fn studio_root_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(home) = env::var("INFIMUM_CODE_HOME") {
        if !home.trim().is_empty() {
            roots.push(PathBuf::from(home));
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(r) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            roots.push(r.join("apps/infimum-code"));
        }
        if let Some(r) = exe.parent().and_then(|p| p.parent()) {
            roots.push(r.to_path_buf());
        }
    }
    roots
}

pub(in crate::agent) fn read_canonical_infimum_guide() -> String {
    for root in studio_root_candidates() {
        if let Some(mono) = root.parent().and_then(|p| p.parent()) {
            let p = mono.join("crates/infimum-lang/docs/INFIMUM_LANG_GUIDE.md");
            if let Ok(text) = fs::read_to_string(&p) {
                if !text.trim().is_empty() {
                    return text;
                }
            }
        }
    }
    EMBEDDED_INFIMUM_GUIDE.to_string()
}

pub(super) fn infimum_template_source(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "ui" => Some((
            "Infimum.Forma 2.1 Interactive Native UI App (Toggle + Slider + TextInput + layout + if/for + ui_fmt_centered + test)",
            r#"use "std/prelude.ifm";

enum Action {
  Inc = 1,
  Reset = 2,
  ToggleTurbo = 3,
  TabBase = 10,
}

layout AppState {
  tick: word,
  counter: word inv 0..=999999,
  active_tab: word inv 0..=2,
  turbo: word inv 0..=1,
  intensity: word inv 0..=100,
  name_buf: vec<word>[64],
}

forma ActionBtn(label: str, act_id: i64, bg_col: i64, hov_col: i64) -[Mem]-> i64 {
  Button(w: 154, h: 42, bg: bg_col, hover_bg: hov_col, radius: 8, on_click: act_id, align: center) {
    Text(label, w: 120, h: 24, size: 15, style: 1, color: pack_rgba(255, 255, 255, 255), align: center)
  }
}

forma MainScreen(vw: i64, vh: i64, tab: i64, turbo: i64)
  req vw >= 640 && vw <= 4096 && vh >= 480 && vh <= 2160 && tab >= 0 && tab <= 2 && turbo >= 0 && turbo <= 1
  -[Mem]-> i64
{
  Box(x: 0, y: 0, w: vw, h: vh, bg: pack_rgba(17, 19, 24, 255), align: center) {
    Col(w: 480, h: 416, pad: 24, gap: 12, bg: pack_rgba(28, 32, 42, 255), border: 1, border_col: pack_rgba(91, 155, 255, 255), radius: 14, align: center) {
      Text("Infimum Native App", w: 380, h: 36, size: 26, style: 1, color: pack_rgba(248, 250, 255, 255), align: center)
      Row(w: 324, h: 32, gap: 8, align: center) {
        for i in 0..3 {
          Button(
            w: 102, h: 32,
            bg: if tab == i { pack_rgba(59, 130, 246, 255) } else { pack_rgba(38, 43, 56, 255) },
            hover_bg: pack_rgba(74, 144, 255, 255),
            radius: 7,
            on_click: Action::TabBase + i,
            align: center
          ) {
            Text(if i == 0 { "Overview" } else if i == 1 { "Controls" } else { "About" }, w: 88, h: 20, size: 13, style: 1, color: pack_rgba(248, 250, 255, 255), align: center)
          }
        }
      }
      Box(w: 420, h: 26, align: center) {
        if tab == 0 {
          Text("Zero magic addresses, 100% Z3-verified Forma 2.1 UI", w: 420, h: 22, size: 14, style: 0, color: pack_rgba(160, 174, 198, 255), align: center)
        } else if tab == 1 {
          Text("Interactive Toggle, Slider & TextInput widgets", w: 420, h: 22, size: 14, style: 0, color: pack_rgba(110, 231, 183, 255), align: center)
        } else {
          Text("Built with Infimum Lang & Strata Wasm", w: 420, h: 22, size: 14, style: 0, color: pack_rgba(253, 224, 71, 255), align: center)
        }
      }
      Row(w: 420, h: 28, gap: 12, align: center) {
        Text("Turbo Mode:", w: 120, h: 20, size: 14, style: 1, color: pack_rgba(220, 230, 245, 255))
        Toggle(id: 20, w: 44, h: 24, checked: turbo == 1, on_change: Action::ToggleTurbo, bg: pack_rgba(45, 52, 68, 255), border_col: pack_rgba(74, 86, 112, 255), accent: pack_rgba(59, 130, 246, 255))
      }
      Row(w: 420, h: 28, gap: 12, align: center) {
        Text("Intensity:", w: 90, h: 20, size: 14, style: 1, color: pack_rgba(220, 230, 245, 255))
        Slider(id: 21, w: 240, h: 24, bind: AppState::intensity, min: 0, max: 100, step: 5, bg: pack_rgba(45, 52, 68, 255), border_col: pack_rgba(74, 86, 112, 255), accent: pack_rgba(59, 130, 246, 255))
      }
      TextInput(id: 22, w: 420, h: 36, buf: AppState::name_buf, max_len: 32, placeholder: "Type a label...", bg: pack_rgba(18, 21, 29, 255), border_col: pack_rgba(64, 74, 96, 255), accent: pack_rgba(91, 155, 255, 255), color: pack_rgba(248, 250, 255, 255), size: 14)
      Row(w: 320, h: 42, gap: 12, align: center) {
        ActionBtn("Action", Action::Inc, pack_rgba(59, 130, 246, 255), pack_rgba(96, 165, 250, 255))
        ActionBtn("Reset", Action::Reset, pack_rgba(44, 50, 64, 255), pack_rgba(64, 72, 92, 255))
      }
    }
  }
}

export fn init_app() -[Mem]-> i64 {
  AppState.tick = 0;
  AppState.counter = 0;
  AppState.active_tab = 0;
  AppState.turbo = 1;
  AppState.intensity = 60;
  AppState.name_buf.clear();
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, m_down: i64, m_clicked: i64, wheel_dy: i64, key_char: i64, key_special: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  AppState.tick += 1;

  forma_begin_frame(w, h, mx, my, m_clicked, wheel_dy);
  forma_set_input(m_down, key_char, key_special, AppState.tick);
  let act = MainScreen(w, h, AppState.active_tab, AppState.turbo);
  if act == Action::Inc {
    let step = if AppState.turbo == 1 { 5 } else { 1 };
    AppState.counter = clamp_i64(AppState.counter + step, 0, 999999);
    sfx_click();
  } else if act == Action::Reset {
    AppState.counter = 0;
    AppState.intensity = 60;
    AppState.name_buf.clear();
    sfx_clack();
  } else if act == Action::ToggleTurbo {
    AppState.turbo = 1 - AppState.turbo;
    sfx_click();
  } else if act >= Action::TabBase && act < Action::TabBase + 3 {
    AppState.active_tab = act - Action::TabBase;
    sfx_click();
  }
  gfx_set_cursor(forma_cursor());
  ui_fmt_centered(w / 2, h / 2 + 228, "Clicks: {AppState.counter}  |  Intensity: {AppState.intensity}%  |  Name len: {AppState.name_buf.len}", 15, 1, pack_rgba(200, 214, 238, 255), 0);
  gfx_end_frame()
}

test "init resets app state" {
  let _ = init_app();
  assert(AppState.counter == 0);
  assert(AppState.active_tab == 0);
  assert(AppState.turbo == 1);
  assert(AppState.intensity == 60);
  assert(AppState.name_buf.len == 0);
}
"#,
        )),
        "game2d" => Some((
            "2D GPU Game Starter (auto-placed layout + vec<Orb>[24] + [Orb] slice + for + ui_fmt + test)",
            r#"use "std/prelude.ifm";

struct Orb {
  x: word,
  y: word,
  vx: word,
  vy: word,
  col: word,
}

layout GameState {
  tick: word,
  score: word inv 0..=999999,
  orbs: vec<Orb>[24],
}

fn step_orbs(orbs: [Orb], w: i64, h: i64) -[Mem, Gpu]-> i64 {
  let mut bounces = 0;
  for i in 0..orbs.len max 24 {
    let mut o = orbs[i];
    o.x += o.vx;
    o.y += o.vy;
    if o.x < 32 || o.x > w - 32 {
      o.vx = 0 - o.vx;
      bounces += 1;
    }
    if o.y < 64 || o.y > h - 32 {
      o.vy = 0 - o.vy;
    }
    orbs[i] = o;
    ui_orb(o.x, o.y, 22, o.col, 0);
  }
  bounces
}

export fn init_app() -[Mem]-> i64 {
  GameState.tick = 0;
  GameState.score = 0;
  GameState.orbs.clear();
  for i in 0..12 {
    let _ = GameState.orbs.try_push(Orb {
      x: 120 + i * 80,
      y: 200 + (i % 4) * 90,
      vx: if i % 2 == 0 { 3 } else { -3 },
      vy: if i % 3 == 0 { 2 } else { -2 },
      col: pack_rgba(60 + i * 15, 190 - i * 8, 240, 255),
    });
  }
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, m_down: i64, m_clicked: i64, wheel_dy: i64, key_char: i64, key_special: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  GameState.tick += 1;
  gfx_begin_frame(w, h);
  ui_grad_rect(0, 0, w, h, 0, 0, 0, pack_rgba(14, 18, 28, 255), pack_rgba(24, 30, 46, 255), 0);

  if m_clicked == 1 {
    let orbs = GameState.orbs.as_slice();
    let mut hit_idx = -1;
    for i in 0..orbs.len max 24 {
      let dx = orbs[i].x - mx;
      let dy = orbs[i].y - my;
      if dx * dx + dy * dy <= 28 * 28 {
        hit_idx = i;
        break;
      }
    }
    if hit_idx >= 0 && hit_idx < GameState.orbs.len {
      let _ = GameState.orbs.swap_remove(hit_idx);
      sfx_clack();
    } else {
      let _ = GameState.orbs.try_push(Orb {
        x: clamp_i64(mx, 40, w - 40),
        y: clamp_i64(my, 72, h - 40),
        vx: if GameState.tick % 2 == 0 { 4 } else { -4 },
        vy: -3,
        col: pack_rgba(120, 210, 255, 255),
      });
      sfx_click();
    }
  }

  let bounces = step_orbs(GameState.orbs.as_slice(), w, h);
  if bounces > 0 {
    GameState.score = clamp_i64(GameState.score + bounces, 0, 999999);
    sfx_hop();
  }
  ui_fmt(20, 18, "2D Infimum Game — Orbs: {GameState.orbs.len} | Bounces: {GameState.score} | Click: Spawn/Remove", 18, 1, pack_rgba(245, 248, 255, 255), 0);
  gfx_end_frame()
}

test "init populates all 12 orbs" {
  let _ = init_app();
  assert(GameState.orbs.len == 12);
  let ok = if GameState.orbs.len == 12 {
    GameState.orbs[0].x == 120 && GameState.orbs[11].x == 1000
  } else {
    false
  };
  assert(ok);
}
"#,
        )),
        "game3d" => Some((
            "3D Vulkan Mesh Starter (auto-placed layout + value struct Vec3 + vec<Pillar>[16] + [Pillar] slice + std/mesh3d + ui_fmt)",
            r#"use "std/prelude.ifm";
use "std/mesh3d.ifm";

struct Vec3 {
  x: fixed,
  y: fixed,
  z: fixed,
}

struct Pillar {
  x: word,
  z: word,
  h: word,
}

layout WorldState {
  tick: word,
  pos: Vec3,
  yaw: word,
  pitch: word inv -220..=220,
  pillars: vec<Pillar>[16],
}

fn vec3_add(a: Vec3, b: Vec3) -> Vec3 {
  Vec3 { x: a.x + b.x, y: a.y + b.y, z: a.z + b.z }
}

fn draw_pillars(pillars: [Pillar]) -[Mem, Gpu]-> unit {
  for i in 0..pillars.len max 16 {
    let p = pillars[i];
    mesh3d_box(0, p.x - 450, 0, p.z - 450, p.x + 450, p.h, p.z + 450, 2, pack_rgba(220, 210, 195, 255), 235, 0, 1400);
  }
}

export fn init_app() -[Mem]-> i64 {
  WorldState.tick = 0;
  WorldState.pos = Vec3 { x: 0.0, y: 1.7, z: -4.5 };
  WorldState.yaw = 0;
  WorldState.pitch = 0;
  WorldState.pillars.clear();
  for i in 0..8 {
    let side = if i % 2 == 0 { -3200 } else { 3200 };
    let _ = WorldState.pillars.try_push(Pillar {
      x: side,
      z: -2000 + (i / 2) * 2600,
      h: 4200,
    });
  }
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, m_down: i64, m_clicked: i64, wheel_dy: i64, key_char: i64, key_special: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  WorldState.tick += 1;
  gfx_begin_frame(w, h);

  if m_clicked == 1 && !pointer_locked() { set_pointer_lock(true); }
  if pointer_locked() {
    WorldState.yaw = (WorldState.yaw + mouse_dx() * 2) & 4095;
    WorldState.pitch = clamp_i64(WorldState.pitch - mouse_dy(), -220, 220);
  }
  let yw = WorldState.yaw;
  let fx = fixed_sin(yw);
  let fz = fixed_cos(yw);
  let mut delta = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
  if key_held(KEY_W) { delta.x += fx * 0.09; delta.z += fz * 0.09; }
  if key_held(KEY_S) { delta.x -= fx * 0.09; delta.z -= fz * 0.09; }
  if key_held(KEY_A) { delta.x -= fz * 0.08; delta.z += fx * 0.08; }
  if key_held(KEY_D) { delta.x += fz * 0.08; delta.z -= fx * 0.08; }
  let pos = vec3_add(WorldState.pos, delta);
  WorldState.pos = pos;

  let cam_x = fixed_to_int(pos.x * 1000.0);
  let cam_y = fixed_to_int(pos.y * 1000.0);
  let cam_z = fixed_to_int(pos.z * 1000.0);
  mesh3d_begin(cam_x, cam_y, cam_z, WorldState.yaw, WorldState.pitch, 75, 2, 0);
  mesh3d_add_light(0, 3600, 1500, 12000, 255, 220, 175, 260);
  mesh3d_box(0, -10000, -200, -10000, 10000, 0, 10000, 6, pack_rgba(190, 185, 175, 255), 220, 0, 2000);
  draw_pillars(WorldState.pillars.as_slice());

  ui_fmt(18, 16, "Pos: ({pos.x:.1}, {pos.z:.1}) m | WASD: Move | Click: Look", 15, 1, pack_rgba(255, 255, 255, 230), 0);
  gfx_end_frame()
}

test "init places 8 pillars and Vec3 camera in meters" {
  let _ = init_app();
  assert(WorldState.pillars.len == 8);
  let ok = if WorldState.pillars.len == 8 {
    WorldState.pillars[0].x == -3200 && WorldState.pillars[1].x == 3200
  } else {
    false
  };
  assert(ok);
  let p = WorldState.pos;
  assert(p.y == 1.7);
  assert(vec3_add(p, Vec3 { x: 1.0, y: 0.3, z: 0.5 }).y == 2.0);
}
"#,
        )),
        _ => None,
    }
}

/// Find the most recently modified `.ifw` capsule in `active_project` (depth <= 2) and append a
/// `[PLAY:<rel_path>]` tag to `reply` so Infimum Code renders a clickable Play button at the end
/// of the assistant message.
pub(in crate::agent) fn append_play_tag_if_axw_present(
    reply: &mut String,
    active_project: Option<&str>,
) {
    if reply.contains("[PLAY:") || reply.trim_start().starts_with("Ошибка") {
        return;
    }
    let Some(proj_str) = active_project.filter(|s| !s.trim().is_empty()) else {
        return;
    };
    let workspace = Path::new(proj_str);
    if !workspace.is_dir() {
        return;
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    let scan_dir = |dir: &Path, prefix: &str, best: &mut Option<(std::time::SystemTime, String)>| {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.is_file() && name.ends_with(".ifw") {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        let rel = if prefix.is_empty() {
                            name.to_string()
                        } else {
                            format!("{prefix}/{name}")
                        };
                        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                            *best = Some((mtime, rel));
                        }
                    }
                }
            }
        }
    };
    scan_dir(workspace, "", &mut best);
    if best.is_none() {
        if let Ok(rd) = fs::read_dir(workspace) {
            for entry in rd.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if path.is_dir()
                    && !name.starts_with('.')
                    && name != "target"
                    && name != "node_modules"
                {
                    scan_dir(&path, name, &mut best);
                }
            }
        }
    }
    if let Some((_, rel)) = best {
        if !reply.ends_with('\n') {
            reply.push('\n');
        }
        reply.push_str(&format!("[PLAY:{rel}]"));
    }
}

pub(crate) fn resolve_infimum_bin(policy: &CapabilityPolicy) -> PathBuf {
    if let Ok(p) = env::var("INFIMUM_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return pb;
        }
    }
    let home = env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("infimum"));
        }
    }
    if let Some(r) = policy.allowed_fs_roots.first() {
        candidates.push(r.join("target/release/infimum"));
        candidates.push(r.join("target/debug/infimum"));
        candidates.push(r.join("infimum-lang/target/release/infimum"));
        candidates.push(r.join("infimum-lang/target/debug/infimum"));
    }
    for root in studio_root_candidates() {
        if let Some(mono) = root.parent().and_then(|p| p.parent()) {
            candidates.push(mono.join("target/release/infimum"));
            candidates.push(mono.join("target/debug/infimum"));
        }
        if let Some(ws) = root.parent() {
            candidates.push(ws.join("infimum-lang/target/release/infimum"));
            candidates.push(ws.join("infimum-lang/target/debug/infimum"));
        }
    }
    if !home.is_empty() {
        candidates.push(PathBuf::from(&home).join(".local/bin/infimum"));
    }
    candidates.push(PathBuf::from("/usr/local/bin/infimum"));
    for c in candidates {
        if c.is_file() {
            return c;
        }
    }
    PathBuf::from("infimum")
}

pub(in crate::agent) fn resolve_infimum_std_dir() -> Option<PathBuf> {
    if let Ok(p) = env::var("INFIMUM_STD_DIR") {
        let pb = PathBuf::from(p);
        if pb.is_dir() {
            return Some(pb);
        }
    }
    if let Ok(p) = env::var("INFIMUM_LANG_DIR") {
        let pb = PathBuf::from(p).join("std");
        if pb.is_dir() {
            return Some(pb);
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(mono) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            let pb = mono.join("crates/infimum-lang/std");
            if pb.is_dir() {
                return Some(pb);
            }
        }
    }
    for root in studio_root_candidates() {
        if let Some(mono) = root.parent().and_then(|p| p.parent()) {
            let pb = mono.join("crates/infimum-lang/std");
            if pb.is_dir() {
                return Some(pb);
            }
        }
        if let Some(ws) = root.parent() {
            let pb = ws.join("infimum-lang/std");
            if pb.is_dir() {
                return Some(pb);
            }
        }
    }
    None
}

pub(in crate::agent) fn resolve_z3_bin() -> Option<PathBuf> {
    let home = env::var("HOME").unwrap_or_default();
    let candidates = [
        PathBuf::from(&home).join(".local/bin/z3"),
        PathBuf::from("/usr/local/bin/z3"),
        PathBuf::from("/usr/bin/z3"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

pub(crate) fn resolve_infimum_web_bin() -> PathBuf {
    if let Ok(p) = env::var("INFIMUM_WEB_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return pb;
        }
    }
    if let Ok(exe) = env::current_exe() {
        if exe.is_file()
            && exe
                .file_name()
                .and_then(|s| s.to_str())
                .map(|n| n == "infimum-web")
                .unwrap_or(false)
        {
            return exe;
        }
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("infimum-web");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    for root in studio_root_candidates() {
        if let Some(mono) = root.parent().and_then(|p| p.parent()) {
            for rel in ["target/release/infimum-web", "target/debug/infimum-web"] {
                let p = mono.join(rel);
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    if let Ok(home) = env::var("HOME") {
        let p = PathBuf::from(home).join(".local/bin/infimum-web");
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("infimum-web")
}

pub(in crate::agent) fn tool_read_infimum_std(args: &Value) -> Value {
    const ALLOWED_STD: &[&str] = &[
        "audio.ifm",
        "buf.ifm",
        "slice.ifm",
        "color.ifm",
        "fmt.ifm",
        "gfx.ifm",
        "icons.ifm",
        "input.ifm",
        "json.ifm",
        "markdown.ifm",
        "math.ifm",
        "mem.ifm",
        "mesh3d.ifm",
        "prelude.ifm",
        "sys_mem.ifm",
        "text.ifm",
        "text_edit.ifm",
        "theme.ifm",
        "ui.ifm",
    ];
    let raw = args
        .get("file")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim();
    if raw.is_empty() || raw == "list" {
        return json!({
            "available_files": ALLOWED_STD,
            "hint": "Call read_infimum_std with file='gfx.ifm' or file='input.ifm' to read any embedded std module."
        });
    }
    let norm = raw.trim_start_matches("std/").trim_start_matches('/');
    let file_name = if norm.ends_with(".ifm") {
        norm.to_string()
    } else {
        format!("{norm}.ifm")
    };
    if !ALLOWED_STD.contains(&file_name.as_str()) {
        return json!({
            "error": format!("Unknown std module '{raw}'. Allowed modules: {:?}", ALLOWED_STD)
        });
    }
    let Some(std_dir) = resolve_infimum_std_dir() else {
        return json!({ "error": "Standard library directory not found (set INFIMUM_LANG_DIR or INFIMUM_STD_DIR)" });
    };
    let std_path = std_dir.join(&file_name);
    match fs::read_to_string(&std_path) {
        Ok(content) => json!({
            "path": format!("std/{file_name}"),
            "content": content,
        }),
        Err(e) => json!({ "error": format!("Failed to read std/{file_name}: {e}") }),
    }
}

pub(in crate::agent) fn tool_read_infimum_template(args: &Value) -> Value {
    let kind = args
        .get("kind")
        .and_then(|s| s.as_str())
        .unwrap_or("list")
        .trim();
    if kind.is_empty() || kind == "list" {
        return json!({
            "available_templates": ["ui", "game2d", "game3d"],
            "hint": "Call read_infimum_template(kind='ui' | 'game2d' | 'game3d') for a Z3-verified starter capsule."
        });
    }
    match infimum_template_source(kind) {
        Some((title, source)) => json!({
            "kind": kind,
            "title": title,
            "source": source,
        }),
        None => json!({
            "error": format!("Unknown template '{kind}'. Available: ui, game2d, game3d")
        }),
    }
}

pub(in crate::agent) fn tool_search_pbr_textures(args: &Value) -> Value {
    let q = args
        .get("query")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim();
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(10)
        .clamp(1, 20);
    let encoded_q: String = q
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect();
    let url = format!("https://infimum-lang.org/api/textures/search?q={encoded_q}&limit={limit}");
    match Command::new("curl")
        .args(["-fsSL", "--max-time", "15", &url])
        .output()
    {
        Ok(o) if o.status.success() => {
            let body = String::from_utf8_lossy(&o.stdout);
            serde_json::from_str::<Value>(&body).unwrap_or_else(|_| json!({ "raw": body }))
        }
        Ok(o) => json!({
            "error": format!("Texture search failed: {}", String::from_utf8_lossy(&o.stderr).trim())
        }),
        Err(e) => json!({ "error": format!("curl failed: {e}") }),
    }
}

pub(in crate::agent) fn tool_set_pbr_texture(default_proj: &Path, args: &Value) -> Value {
    let slot = args.get("slot").and_then(|v| v.as_i64()).unwrap_or(-1);
    let tex_id = args
        .get("texture_id")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !(0..=11).contains(&slot) {
        return json!({ "error": format!("Invalid slot {slot}. Must be 0..11.") });
    }
    if tex_id.is_empty()
        || !tex_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return json!({ "error": format!("Invalid texture_id '{tex_id}'. Must match ^[a-z0-9_]+$.") });
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let cache_dir = PathBuf::from(&home)
        .join(".cache")
        .join("infimum-web")
        .join("textures")
        .join(&tex_id);
    let proj_tex_dir = resolve_in_project(default_proj, "textures");
    if let Err(e) = fs::create_dir_all(&cache_dir).and_then(|_| fs::create_dir_all(&proj_tex_dir)) {
        return json!({ "error": format!("Failed to create texture dirs: {e}") });
    }
    let slot_prefix = format!("{slot}_");
    if let Ok(entries) = fs::read_dir(&proj_tex_dir) {
        for ent in entries.flatten() {
            if let Some(fname) = ent.file_name().to_str() {
                if fname.starts_with(&slot_prefix) {
                    let _ = fs::remove_file(ent.path());
                }
            }
        }
    }
    let mut downloaded = Vec::new();
    for kind in ["diff", "norm", "rough"] {
        let cached_file = cache_dir.join(format!("{kind}.jpg"));
        if !cached_file.is_file() || fs::metadata(&cached_file).map(|m| m.len()).unwrap_or(0) == 0 {
            let url = format!("https://infimum-lang.org/textures/{tex_id}/{kind}.jpg");
            let tmp = cache_dir.join(format!("{kind}.jpg.tmp"));
            let ok = Command::new("curl")
                .args([
                    "-fsSL",
                    "--max-time",
                    "25",
                    "-o",
                    &tmp.display().to_string(),
                    &url,
                ])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok && fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) > 0 {
                let _ = fs::rename(&tmp, &cached_file);
            } else {
                let _ = fs::remove_file(&tmp);
            }
        }
        if cached_file.is_file() {
            let dst = proj_tex_dir.join(format!("{slot}_{tex_id}_{kind}.jpg"));
            #[cfg(unix)]
            let linked = std::os::unix::fs::symlink(&cached_file, &dst).is_ok();
            #[cfg(not(unix))]
            let linked = false;
            if linked || fs::copy(&cached_file, &dst).is_ok() {
                downloaded.push(kind);
            }
        }
    }
    if downloaded.is_empty() {
        return json!({ "error": format!("Texture '{tex_id}' not found on https://infimum-lang.org/textures/") });
    }
    let tex_json_path = resolve_in_project(default_proj, "textures.json");
    let mut map = fs::read_to_string(&tex_json_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Map<String, Value>>(&s).ok())
        .unwrap_or_default();
    map.insert(slot.to_string(), Value::String(tex_id.clone()));
    if let Ok(pretty) = serde_json::to_string_pretty(&Value::Object(map)) {
        let _ = fs::write(&tex_json_path, format!("{pretty}\n"));
    }
    json!({
        "ok": true,
        "slot": slot,
        "texture_id": tex_id,
        "maps": downloaded,
        "cache_dir": cache_dir.display().to_string(),
    })
}

pub(in crate::agent) fn tool_verify_infimum_file(
    policy: &CapabilityPolicy,
    default_proj: &Path,
    args: &Value,
) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let p = resolve_in_project(default_proj, raw);
    if !is_jailed_in(&p, default_proj) {
        return json!({ "error": "Access denied outside active project sandbox" });
    }
    let bin = resolve_infimum_bin(policy);
    match Command::new(&bin)
        .args(["verify", &p.display().to_string()])
        .output()
    {
        Ok(o) => {
            let out = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            json!({
                "ok": o.status.success(),
                "output": out.chars().take(3500).collect::<String>(),
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(in crate::agent) fn tool_compile_infimum_wasm(
    policy: &CapabilityPolicy,
    default_proj: &Path,
    args: &Value,
) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let p = resolve_in_project(default_proj, raw);
    if !is_jailed_in(&p, default_proj) {
        return json!({ "error": "Access denied outside active project sandbox" });
    }
    let out_p = match args
        .get("output")
        .and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        Some(o) => resolve_in_project(default_proj, o),
        None => p.with_extension("ifw"),
    };
    if !is_jailed_in(&out_p, default_proj) {
        return json!({ "error": "Output path denied outside active project sandbox" });
    }
    if let Some(parent) = out_p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let bin = resolve_infimum_bin(policy);
    match Command::new(&bin)
        .args([
            "compile-wasm",
            &p.display().to_string(),
            "-o",
            &out_p.display().to_string(),
        ])
        .output()
    {
        Ok(o) => {
            let out = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let size_bytes = fs::metadata(&out_p).map(|m| m.len()).unwrap_or(0);
            json!({
                "ok": o.status.success(),
                "output_path": out_p.display().to_string(),
                "size_bytes": size_bytes,
                "output": out.chars().take(3500).collect::<String>(),
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(in crate::agent) fn tool_run_infimum_capsule(default_proj: &Path, args: &Value) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let p = resolve_in_project(default_proj, raw);
    if !is_jailed_in(&p, default_proj) {
        return json!({ "error": "Access denied outside active project sandbox" });
    }
    if !p.is_file() {
        return json!({ "error": format!("Capsule file not found: {}", p.display()) });
    }
    let headless = args
        .get("headless")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let web_bin = resolve_infimum_web_bin();
    if headless {
        match Command::new(&web_bin)
            .args(["--headless-check", &p.display().to_string()])
            .output()
        {
            Ok(o) => {
                let out = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                json!({
                    "ok": o.status.success(),
                    "mode": "headless",
                    "output": out.chars().take(3500).collect::<String>(),
                })
            }
            Err(e) => json!({ "error": e.to_string() }),
        }
    } else {
        match Command::new(&web_bin)
            .arg(&p)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => json!({
                "ok": true,
                "mode": "window",
                "pid": child.id(),
                "path": p.display().to_string(),
                "message": format!("Opened native infimum-web window for {}", p.display()),
            }),
            Err(e) => json!({ "error": e.to_string() }),
        }
    }
}
