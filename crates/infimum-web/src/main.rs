//! `infimum-web` — Native `.ifw` Capsule Runtime & Browser (Zero Chromium / Zero JS).

mod atlas;
mod audio;
mod browser_abi;
mod capabilities;
mod clipboard;
mod core_mem;
mod gpu;
mod passport;
mod runtime;
mod sanitizer;
mod textures3d;

use crate::atlas::GlyphAtlas;
use crate::browser_abi::{CursorKind, HostAction, KeySpecial};
use crate::clipboard::NativeClipboard;
use crate::gpu::GpuRenderer;
use crate::passport::CapabilityPolicy;
use crate::runtime::{CapsuleRuntime, FrameInput};
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{DeviceEvent, DeviceId, ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorGrabMode, CursorIcon, Icon, ImePurpose, Window, WindowId};

struct CliArgs {
    capsule_path: PathBuf,
    headless_check: bool,
    window_title: Option<String>,
    app_id: String,
    policy: CapabilityPolicy,
}

fn parse_cli() -> Result<CliArgs, String> {
    let mut capsule_path: Option<PathBuf> = None;
    let mut headless_check = false;
    let mut window_title: Option<String> = None;
    let mut app_id = "infimum-web".to_string();
    let mut policy = CapabilityPolicy::default();

    let mut args_iter = env::args().skip(1);
    while let Some(arg) = args_iter.next() {
        if arg == "--headless-check" {
            headless_check = true;
        } else if arg == "--allow-clipboard" {
            policy.allow_clipboard = true;
        } else if arg == "--allow-unsigned" {
            policy.require_passport = false;
        } else if arg == "--require-signature" {
            let spec = args_iter
                .next()
                .ok_or_else(|| "Missing <pubkey.hex|path> after --require-signature".to_string())?;
            let raw_hex = if Path::new(&spec).is_file() {
                fs::read_to_string(&spec)
                    .map_err(|e| format!("Failed to read public key '{spec}': {e}"))?
            } else {
                spec
            };
            let pk = crate::passport::decode_pubkey_hex_32(&raw_hex)?;
            policy.trusted_pubkeys.push(pk);
        } else if let Some(spec) = arg.strip_prefix("--require-signature=") {
            let raw_hex = if Path::new(spec).is_file() {
                fs::read_to_string(spec)
                    .map_err(|e| format!("Failed to read public key '{spec}': {e}"))?
            } else {
                spec.to_string()
            };
            let pk = crate::passport::decode_pubkey_hex_32(&raw_hex)?;
            policy.trusted_pubkeys.push(pk);
        } else if let Some(t) = arg.strip_prefix("--title=") {
            window_title = Some(t.to_string());
        } else if let Some(id) = arg.strip_prefix("--app-id=") {
            app_id = id.to_string();
        } else if let Some(dom) = arg.strip_prefix("--allow-net=") {
            policy
                .allowed_net_domains
                .insert(dom.trim().to_ascii_lowercase());
        } else if let Some(kv) = arg.strip_prefix("--cap-arg=") {
            if let Some((k, v)) = kv.split_once('=') {
                policy.cap_args.insert(k.trim().to_string(), v.to_string());
            }
        } else if arg == "--help" || arg == "-h" {
            println!(
                "Usage: infimum-web <capsule.ifw|capsule.wasm> [OPTIONS]\n\
                 Options:\n\
                 \t--headless-check                  Verify infimum.passport and step 1 frame without opening a window\n\
                 \t--require-signature <pubkey|file> Require valid Ed25519 signature from trusted public key\n\
                 \t--allow-net=<host[:port]>         Allow capsule network access to a specific domain or loopback port\n\
                 \t--cap-arg=<KEY=VAL>               Pass an explicit launch parameter to the capsule\n\
                 \t--allow-clipboard                 Allow capsule clipboard read/write\n\
                 \t--allow-unsigned                  Allow Wasm without an infimum.passport section"
            );
            std::process::exit(0);
        } else if capsule_path.is_none() {
            capsule_path = Some(PathBuf::from(arg));
        }
    }

    let Some(path) = capsule_path else {
        return Err("Missing <capsule.ifw> path. Run `infimum-web --help` for usage.".to_string());
    };

    Ok(CliArgs {
        capsule_path: path,
        headless_check,
        window_title,
        app_id,
        policy,
    })
}

fn find_capsule_icon_path(capsule_dir: Option<&Path>, capsule_stem: &str) -> Option<PathBuf> {
    let dir = capsule_dir?;
    for name in [
        format!("{capsule_stem}.png"),
        "icon.png".to_string(),
        "icon-256.png".to_string(),
    ] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(fs::canonicalize(&p).unwrap_or(p));
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn sanitize_app_id_stem(stem: &str) -> String {
    let mut out = String::with_capacity(stem.len());
    let mut prev_dash = false;
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "capsule".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(target_os = "linux")]
fn pretty_capsule_title(stem: &str) -> String {
    let words: Vec<String> = stem
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() {
        "Infimum Capsule".to_string()
    } else {
        words.join(" ")
    }
}

/// Wayland compositors (including GNOME Shell) ignore `winit`'s `with_window_icon` and
/// resolve taskbar/dock icons strictly via `xdg_toplevel.app_id` -> `<app_id>.desktop`.
/// Register a hidden per-capsule `.desktop` entry pointing to the capsule's `icon.png`.
#[cfg(target_os = "linux")]
fn ensure_wayland_desktop_icon(
    app_id: &str,
    display_name: &str,
    icon_path: &Path,
    capsule_path: &Path,
) {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let apps_dir = PathBuf::from(home).join(".local/share/applications");
    let desktop_path = apps_dir.join(format!("{app_id}.desktop"));
    let abs_icon = fs::canonicalize(icon_path).unwrap_or_else(|_| icon_path.to_path_buf());
    let abs_capsule = fs::canonicalize(capsule_path).unwrap_or_else(|_| capsule_path.to_path_buf());
    let content = format!(
        "[Desktop Entry]\n\
         Version=1.0\n\
         Type=Application\n\
         Name={display_name}\n\
         Exec=infimum-web {}\n\
         Icon={}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         StartupWMClass={app_id}\n",
        abs_capsule.display(),
        abs_icon.display()
    );
    if fs::read_to_string(&desktop_path).ok().as_deref() == Some(content.as_str()) {
        return;
    }
    if fs::create_dir_all(&apps_dir).is_ok() {
        let _ = fs::write(&desktop_path, content);
    }
}

fn load_window_icon(capsule_dir: Option<&Path>, capsule_stem: &str) -> Option<Icon> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = find_capsule_icon_path(capsule_dir, capsule_stem) {
        candidates.push(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            candidates.push(root.join("icons/icon-256.png"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".local/share/icons/hicolor/256x256/apps/infimum-code.png"),
        );
    }

    for p in candidates {
        let Ok(file) = fs::File::open(&p) else {
            continue;
        };
        let mut decoder = png::Decoder::new(file);
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let Ok(mut reader) = decoder.read_info() else {
            continue;
        };
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let Ok(info) = reader.next_frame(&mut buf) else {
            continue;
        };
        let data = &buf[..info.buffer_size()];
        let rgba: Vec<u8> = match info.color_type {
            png::ColorType::Rgba => data.to_vec(),
            png::ColorType::Rgb => {
                let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
                for c in data.as_chunks::<3>().0 {
                    out.extend_from_slice(&[c[0], c[1], c[2], 255]);
                }
                out
            }
            _ => continue,
        };
        if let Ok(icon) = Icon::from_rgba(rgba, info.width, info.height) {
            return Some(icon);
        }
    }
    None
}

struct InfimumWebApp {
    capsule_name: String,
    capsule_dir: Option<PathBuf>,
    textures_dir: Option<PathBuf>,
    window_title: Option<String>,
    app_id: String,
    runtime: CapsuleRuntime,
    atlas: GlyphAtlas,
    clipboard: NativeClipboard,
    modifiers: ModifiersState,
    // Drop `gpu` (Vulkan/Wayland surface) strictly BEFORE `window` to prevent Wayland close segfault.
    gpu: Option<GpuRenderer>,
    window: Option<Arc<Window>>,
    mx: i32,
    my: i32,
    m_down: i32,
    m_clicked: i32,
    wheel_dy: i32,
    scroll_vel: f32,
    keys_held: i32,
    window_focused: bool,
    is_3d_app: bool,
    pointer_locked: bool,
    raw_mdx: f64,
    raw_mdy: f64,
    key_queue: VecDeque<(i32, i32)>,
}

fn attach_image_to_session(
    gpu: &mut GpuRenderer,
    runtime: &mut CapsuleRuntime,
    img: clipboard::PastedImage,
) {
    if !capabilities::has_browser_pending_images() {
        gpu.commit_pending_images();
    }
    if gpu.image_slot_count() < 16 {
        let w = img.width;
        let h = img.height;
        let rgba = img.rgba.clone();
        if capabilities::push_browser_pending_image(img) {
            let _ = gpu.add_image_rgba(w, h, &rgba);
            runtime.paste_image(w, h);
        }
    }
}

impl InfimumWebApp {
    fn sync_ime_area(win: &Window) {
        let sf = win.scale_factor();
        let sz = win.inner_size();
        let lw = (sz.width as f64 / sf).max(320.0);
        let lh = (sz.height as f64 / sf).max(240.0);
        win.set_ime_cursor_area(
            LogicalPosition::new((lw * 0.25).max(260.0), (lh - 110.0).max(60.0)),
            LogicalSize::new((lw * 0.55).max(300.0), 80.0),
        );
    }

    fn attach_pasted_image(&mut self, img: clipboard::PastedImage) {
        if let Some(gpu) = &mut self.gpu {
            attach_image_to_session(gpu, &mut self.runtime, img);
        } else {
            self.runtime.paste_image(img.width, img.height);
        }
        if let Some(win) = &self.window {
            win.request_redraw();
        }
    }

    fn apply_pointer_lock(&mut self, lock: bool) {
        if let Some(win) = &self.window {
            if lock {
                win.set_ime_allowed(false);
                let ok = win
                    .set_cursor_grab(CursorGrabMode::Locked)
                    .or_else(|_| win.set_cursor_grab(CursorGrabMode::Confined))
                    .is_ok();
                if ok {
                    win.set_cursor_visible(false);
                    self.pointer_locked = true;
                    self.raw_mdx = 0.0;
                    self.raw_mdy = 0.0;
                } else {
                    win.set_ime_allowed(true);
                }
            } else {
                let _ = win.set_cursor_grab(CursorGrabMode::None);
                win.set_cursor_visible(true);
                if !self.is_3d_app {
                    win.set_ime_allowed(true);
                    Self::sync_ime_area(win);
                }
                self.pointer_locked = false;
                self.raw_mdx = 0.0;
                self.raw_mdy = 0.0;
            }
        }
    }
}

impl ApplicationHandler for InfimumWebApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let title = match &self.window_title {
            Some(t) if !t.is_empty() => t.clone(),
            _ => format!("Infimum Web — {}", self.capsule_name),
        };
        let mut attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(1280.0, 820.0));
        #[cfg(target_os = "linux")]
        {
            use winit::platform::wayland::WindowAttributesExtWayland;
            use winit::platform::x11::WindowAttributesExtX11;
            attrs = WindowAttributesExtWayland::with_name(
                attrs,
                self.app_id.clone(),
                self.app_id.clone(),
            );
            attrs = WindowAttributesExtX11::with_name(
                attrs,
                self.app_id.clone(),
                self.app_id.clone(),
            );
        }
        let stem = self.capsule_name.trim_end_matches(".ifw");
        if let Some(icon) = load_window_icon(self.capsule_dir.as_deref(), stem) {
            attrs = attrs.with_window_icon(Some(icon));
        }

        let win = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("Failed to create native Wayland/X11 window"),
        );
        // Enable Wayland text-input-v3 / IBus IME so voice dictation & dead keys reach the capsule.
        win.set_ime_allowed(true);
        win.set_ime_purpose(ImePurpose::Normal);
        Self::sync_ime_area(&win);

        let renderer = GpuRenderer::new(
            Arc::clone(&win),
            &self.atlas,
            self.textures_dir.as_deref(),
            &self.capsule_name,
        )
        .expect("Failed to initialize wgpu 1-Draw-Call renderer");
        self.window = Some(win);
        self.gpu = Some(renderer);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // Explicitly destroy wgpu surface & device before releasing the Wayland window.
                self.gpu = None;
                self.window = None;
                event_loop.exit();
            }
            WindowEvent::Focused(focused) => {
                self.window_focused = focused;
                if focused {
                    if let Some(win) = &self.window {
                        if !self.pointer_locked && !self.is_3d_app {
                            win.set_ime_allowed(true);
                            Self::sync_ime_area(win);
                        } else {
                            win.set_ime_allowed(false);
                        }
                    }
                } else {
                    self.keys_held = 0;
                    if self.pointer_locked {
                        self.apply_pointer_lock(false);
                    }
                }
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                if !text.is_empty() {
                    self.runtime.paste_text(&text);
                    if let Some(win) = &self.window {
                        win.request_redraw();
                    }
                }
            }
            WindowEvent::Resized(new_size) => {
                if let (Some(gpu), Some(win)) = (&mut self.gpu, &self.window) {
                    gpu.resize(new_size.width, new_size.height, win.scale_factor());
                    Self::sync_ime_area(win);
                    win.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let (Some(gpu), Some(win)) = (&mut self.gpu, &self.window) {
                    let s = win.inner_size();
                    gpu.resize(s.width, s.height, scale_factor);
                    Self::sync_ime_area(win);
                    win.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let sf = self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.0);
                self.mx = (position.x / sf).round() as i32;
                self.my = (position.y / sf).round() as i32;
            }
            WindowEvent::MouseInput { state, button, .. } => match button {
                MouseButton::Left => {
                    if state == ElementState::Pressed {
                        self.m_down = 1;
                        self.m_clicked = 1;
                        self.scroll_vel = 0.0;
                    } else {
                        self.m_down = 0;
                    }
                }
                MouseButton::Right if state == ElementState::Pressed => {
                    self.scroll_vel = 0.0;
                    self.key_queue.push_back((0, KeySpecial::RightClick as i32));
                }
                _ => {}
            },
            WindowEvent::MouseWheel { delta, .. } => {
                match delta {
                    MouseScrollDelta::LineDelta(_, y) => {
                        let impulse = -y * 42.0;
                        if impulse * self.scroll_vel < 0.0 {
                            self.scroll_vel = 0.0;
                        }
                        self.scroll_vel = (self.scroll_vel + impulse).clamp(-360.0, 360.0);
                    }
                    MouseScrollDelta::PixelDelta(pos) => {
                        let dy = -(pos.y as f32);
                        let direct = (dy * 1.4).round() as i32;
                        self.wheel_dy = self.wheel_dy.saturating_add(direct);
                        let impulse = dy * 0.35;
                        if impulse * self.scroll_vel < 0.0 {
                            self.scroll_vel = 0.0;
                        }
                        self.scroll_vel = (self.scroll_vel + impulse).clamp(-360.0, 360.0);
                    }
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::DroppedFile(path) => {
                if clipboard::is_supported_image_ext(&path) {
                    if let Some(img) = clipboard::load_image_file(&path) {
                        self.attach_pasted_image(img);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // Update layout-independent physical held-key bitmask (`std/input.ifm` `InputMem.keys_held`),
                // with Cyrillic/Latin logical fallback for Wayland/XKB/IME edge cases.
                let mut key_bit = match event.physical_key {
                    PhysicalKey::Code(KeyCode::KeyW) => 1,
                    PhysicalKey::Code(KeyCode::KeyA) => 2,
                    PhysicalKey::Code(KeyCode::KeyS) => 4,
                    PhysicalKey::Code(KeyCode::KeyD) => 8,
                    PhysicalKey::Code(KeyCode::ArrowUp) => 16,
                    PhysicalKey::Code(KeyCode::ArrowDown) => 32,
                    PhysicalKey::Code(KeyCode::ArrowLeft) => 64,
                    PhysicalKey::Code(KeyCode::ArrowRight) => 128,
                    PhysicalKey::Code(KeyCode::KeyQ) => 256,
                    PhysicalKey::Code(KeyCode::KeyE) => 512,
                    PhysicalKey::Code(KeyCode::Space) => 1024,
                    PhysicalKey::Code(KeyCode::ShiftLeft)
                    | PhysicalKey::Code(KeyCode::ShiftRight) => 2048,
                    PhysicalKey::Code(KeyCode::KeyR) => 4096,
                    PhysicalKey::Code(KeyCode::KeyZ) => 8192,
                    PhysicalKey::Code(KeyCode::KeyY) => 16384,
                    PhysicalKey::Code(KeyCode::KeyF) => 32768,
                    PhysicalKey::Code(KeyCode::ControlLeft)
                    | PhysicalKey::Code(KeyCode::ControlRight) => 65536,
                    PhysicalKey::Code(KeyCode::AltLeft)
                    | PhysicalKey::Code(KeyCode::AltRight) => 131072,
                    PhysicalKey::Code(KeyCode::Digit1) => 262144,
                    PhysicalKey::Code(KeyCode::Digit2) => 524288,
                    PhysicalKey::Code(KeyCode::Digit3) => 1048576,
                    PhysicalKey::Code(KeyCode::Digit4) => 2097152,
                    PhysicalKey::Code(KeyCode::Digit5) => 4194304,
                    PhysicalKey::Code(KeyCode::Digit6) => 8388608,
                    PhysicalKey::Code(KeyCode::Digit7) => 16777216,
                    PhysicalKey::Code(KeyCode::Digit8) => 33554432,
                    PhysicalKey::Code(KeyCode::Digit9) => 67108864,
                    _ => 0,
                };
                if key_bit == 0 {
                    if let Key::Character(ref s) = event.logical_key {
                        key_bit = match s.as_str() {
                            "w" | "W" | "ц" | "Ц" => 1,
                            "a" | "A" | "ф" | "Ф" => 2,
                            "s" | "S" | "ы" | "Ы" => 4,
                            "d" | "D" | "в" | "В" => 8,
                            "q" | "Q" | "й" | "Й" => 256,
                            "e" | "E" | "у" | "У" => 512,
                            "r" | "R" | "к" | "К" => 4096,
                            "z" | "Z" | "я" | "Я" => 8192,
                            "y" | "Y" | "н" | "Н" => 16384,
                            "f" | "F" | "а" | "А" => 32768,
                            "1" => 262144,
                            "2" => 524288,
                            "3" => 1048576,
                            "4" => 2097152,
                            "5" => 4194304,
                            "6" => 8388608,
                            "7" => 16777216,
                            "8" => 33554432,
                            "9" => 67108864,
                            _ => 0,
                        };
                    }
                }
                if key_bit != 0 {
                    if event.state == ElementState::Pressed {
                        self.keys_held |= key_bit;
                    } else {
                        self.keys_held &= !key_bit;
                    }
                }
                if event.state != ElementState::Pressed {
                    return;
                }
                let ctrl = self.modifiers.control_key() || self.modifiers.super_key();
                let shift = self.modifiers.shift_key() || (self.keys_held & 2048) != 0;
                let is_insert = matches!(event.logical_key, Key::Named(NamedKey::Insert))
                    || event.physical_key == PhysicalKey::Code(KeyCode::Insert);

                // Layout-independent Shift+Insert (used by Wayland voice dictation via /dev/uinput) and NamedKey::Paste
                if (shift && is_insert) || matches!(event.logical_key, Key::Named(NamedKey::Paste)) {
                    self.key_queue.push_back((0, KeySpecial::CtrlV as i32));
                    if let Some(txt) = self.clipboard.read_text() {
                        self.runtime.paste_text(&txt);
                    } else if let Some(img) = self.clipboard.read_image() {
                        self.attach_pasted_image(img);
                    }
                    return;
                }

                // Standard CUA Shift+Delete (cut)
                if shift && matches!(event.logical_key, Key::Named(NamedKey::Delete)) {
                    if let Some(sel) = self.runtime.copy_text() {
                        let _ = self.clipboard.write_text(&sel);
                    }
                    self.key_queue.push_back((0, KeySpecial::CtrlX as i32));
                    return;
                }

                if ctrl {
                    if is_insert {
                        self.key_queue.push_back((0, KeySpecial::CtrlC as i32));
                        if let Some(sel) = self.runtime.copy_text() {
                            let _ = self.clipboard.write_text(&sel);
                        }
                        return;
                    }
                    let phys = event.physical_key;
                    let logical_lower = match &event.logical_key {
                        Key::Character(ch_str) => Some(ch_str.to_ascii_lowercase()),
                        _ => None,
                    };
                    let l = logical_lower.as_deref().unwrap_or("");
                    if phys == PhysicalKey::Code(KeyCode::KeyA) || l == "a" || l == "ф" {
                        self.key_queue.push_back((0, KeySpecial::CtrlA as i32));
                    } else if phys == PhysicalKey::Code(KeyCode::KeyC) || l == "c" || l == "с" {
                        self.key_queue.push_back((0, KeySpecial::CtrlC as i32));
                        if let Some(sel) = self.runtime.copy_text() {
                            let _ = self.clipboard.write_text(&sel);
                        }
                    } else if phys == PhysicalKey::Code(KeyCode::KeyV) || l == "v" || l == "м" {
                        self.key_queue.push_back((0, KeySpecial::CtrlV as i32));
                        if let Some(img) = self.clipboard.read_image() {
                            self.attach_pasted_image(img);
                        } else if let Some(txt) = self.clipboard.read_text() {
                            self.runtime.paste_text(&txt);
                        }
                    } else if phys == PhysicalKey::Code(KeyCode::KeyX) || l == "x" || l == "ч" {
                        if let Some(sel) = self.runtime.copy_text() {
                            let _ = self.clipboard.write_text(&sel);
                        }
                        self.key_queue.push_back((0, KeySpecial::CtrlX as i32));
                    } else if matches!(event.logical_key, Key::Named(NamedKey::Home))
                        || phys == PhysicalKey::Code(KeyCode::Home)
                    {
                        self.key_queue.push_back((
                            0,
                            if shift {
                                KeySpecial::CtrlShiftHome as i32
                            } else {
                                KeySpecial::CtrlHome as i32
                            },
                        ));
                    } else if matches!(event.logical_key, Key::Named(NamedKey::End))
                        || phys == PhysicalKey::Code(KeyCode::End)
                    {
                        self.key_queue.push_back((
                            0,
                            if shift {
                                KeySpecial::CtrlShiftEnd as i32
                            } else {
                                KeySpecial::CtrlEnd as i32
                            },
                        ));
                    }
                    return;
                }

                let sp = match event.logical_key {
                    Key::Named(NamedKey::Backspace) => KeySpecial::Backspace as i32,
                    Key::Named(NamedKey::Enter) => {
                        if shift {
                            KeySpecial::ShiftEnter as i32
                        } else {
                            KeySpecial::Enter as i32
                        }
                    }
                    Key::Named(NamedKey::Escape) => KeySpecial::Escape as i32,
                    Key::Named(NamedKey::Tab) => KeySpecial::Tab as i32,
                    Key::Named(NamedKey::ArrowLeft) => {
                        if shift {
                            KeySpecial::ShiftLeft as i32
                        } else {
                            KeySpecial::ArrowLeft as i32
                        }
                    }
                    Key::Named(NamedKey::ArrowRight) => {
                        if shift {
                            KeySpecial::ShiftRight as i32
                        } else {
                            KeySpecial::ArrowRight as i32
                        }
                    }
                    Key::Named(NamedKey::ArrowUp) => {
                        if shift {
                            KeySpecial::ShiftUp as i32
                        } else {
                            KeySpecial::ArrowUp as i32
                        }
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        if shift {
                            KeySpecial::ShiftDown as i32
                        } else {
                            KeySpecial::ArrowDown as i32
                        }
                    }
                    Key::Named(NamedKey::Home) => {
                        if shift {
                            KeySpecial::ShiftHome as i32
                        } else {
                            KeySpecial::Home as i32
                        }
                    }
                    Key::Named(NamedKey::End) => {
                        if shift {
                            KeySpecial::ShiftEnd as i32
                        } else {
                            KeySpecial::End as i32
                        }
                    }
                    Key::Named(NamedKey::Delete) => KeySpecial::Delete as i32,
                    _ => match event.physical_key {
                        PhysicalKey::Code(KeyCode::Home) => {
                            if shift {
                                KeySpecial::ShiftHome as i32
                            } else {
                                KeySpecial::Home as i32
                            }
                        }
                        PhysicalKey::Code(KeyCode::End) => {
                            if shift {
                                KeySpecial::ShiftEnd as i32
                            } else {
                                KeySpecial::End as i32
                            }
                        }
                        PhysicalKey::Code(KeyCode::ArrowLeft) => {
                            if shift {
                                KeySpecial::ShiftLeft as i32
                            } else {
                                KeySpecial::ArrowLeft as i32
                            }
                        }
                        PhysicalKey::Code(KeyCode::ArrowRight) => {
                            if shift {
                                KeySpecial::ShiftRight as i32
                            } else {
                                KeySpecial::ArrowRight as i32
                            }
                        }
                        PhysicalKey::Code(KeyCode::ArrowUp) => {
                            if shift {
                                KeySpecial::ShiftUp as i32
                            } else {
                                KeySpecial::ArrowUp as i32
                            }
                        }
                        PhysicalKey::Code(KeyCode::ArrowDown) => {
                            if shift {
                                KeySpecial::ShiftDown as i32
                            } else {
                                KeySpecial::ArrowDown as i32
                            }
                        }
                        _ => KeySpecial::None as i32,
                    },
                };
                if sp != 0 {
                    self.key_queue.push_back((0, sp));
                    return;
                }

                // Capture printable text from event.text OR Key::Character (synthetic voice/IME tools).
                let text_owned: Option<String> = event
                    .text
                    .as_ref()
                    .map(|s| s.to_string())
                    .or_else(|| match &event.logical_key {
                        Key::Character(s) => Some(s.to_string()),
                        Key::Named(NamedKey::Space) => Some(" ".to_string()),
                        _ => None,
                    });
                if let Some(txt) = text_owned {
                    let clean: String = txt
                        .chars()
                        .filter(|c| !c.is_control() || *c == '\n')
                        .collect();
                    let mut chars = clean.chars();
                    if let Some(first) = chars.next() {
                        if chars.next().is_some() {
                            // Multi-char burst (dictation / IME commit via keyboard event)
                            self.runtime.paste_text(&clean);
                        } else {
                            self.key_queue.push_back((first as i32, 0));
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(gpu), Some(win)) = (&mut self.gpu, &self.window) {
                    // Coalesce rapid printable keypresses (e.g. ydotool/wtype voice dictation) into a single paste burst.
                    if self.key_queue.len() >= 2
                        && self.key_queue.front().map(|&(c, s)| c >= 32 && s == 0).unwrap_or(false)
                    {
                        let mut burst = String::new();
                        while let Some(&(c, s)) = self.key_queue.front() {
                            if c >= 32 && s == 0 {
                                if let Some(ch) = char::from_u32(c as u32) {
                                    burst.push(ch);
                                }
                                self.key_queue.pop_front();
                            } else {
                                break;
                            }
                        }
                        if !burst.is_empty() {
                            self.runtime.paste_text(&burst);
                        }
                    }

                    if self.scroll_vel.abs() >= 0.6 {
                        self.wheel_dy = self.wheel_dy.saturating_add(self.scroll_vel.round() as i32);
                        self.scroll_vel *= 0.86;
                        if self.scroll_vel.abs() < 0.6 {
                            self.scroll_vel = 0.0;
                        }
                    }

                    let (key_char, key_special) = self.key_queue.pop_front().unwrap_or((0, 0));
                    if key_special == KeySpecial::Escape as i32 && self.pointer_locked {
                        let _ = win.set_cursor_grab(CursorGrabMode::None);
                        win.set_cursor_visible(true);
                        if !self.is_3d_app {
                            win.set_ime_allowed(true);
                            Self::sync_ime_area(win);
                        }
                        self.pointer_locked = false;
                        self.raw_mdx = 0.0;
                        self.raw_mdy = 0.0;
                    }
                    let mdx = self.raw_mdx.trunc() as i32;
                    let mdy = self.raw_mdy.trunc() as i32;
                    self.raw_mdx -= mdx as f64;
                    self.raw_mdy -= mdy as f64;
                    let (vw, vh) = gpu.logical_size();
                    let inp = FrameInput {
                        vw: vw.max(320),
                        vh: vh.max(240),
                        mx: self.mx,
                        my: self.my,
                        m_down: self.m_down,
                        m_clicked: self.m_clicked,
                        wheel_dy: self.wheel_dy,
                        key_char,
                        key_special,
                        keys_held: self.keys_held,
                        mouse_dx: mdx,
                        mouse_dy: mdy,
                        pointer_locked: if self.pointer_locked { 1 } else { 0 },
                    };
                    self.m_clicked = 0;
                    self.wheel_dy = 0;

                    match self.runtime.step_frame(inp) {
                        Ok(frame) => {
                            // Execute host action requested by capsule UI (typed HostAction ABI)
                            match HostAction::try_from(self.runtime.pop_host_action()) {
                                Ok(HostAction::Copy) => {
                                    if let Some(sel) = self.runtime.copy_text() {
                                        let _ = self.clipboard.write_text(&sel);
                                    }
                                }
                                Ok(HostAction::Paste) => {
                                    if let Some(img) = self.clipboard.read_image() {
                                        attach_image_to_session(gpu, &mut self.runtime, img);
                                    } else if let Some(txt) = self.clipboard.read_text() {
                                        self.runtime.paste_text(&txt);
                                    }
                                }
                                Ok(HostAction::Cut) => {
                                    if let Some(sel) = self.runtime.copy_text() {
                                        let _ = self.clipboard.write_text(&sel);
                                    }
                                    self.key_queue.push_back((0, KeySpecial::CtrlX as i32));
                                }
                                Ok(HostAction::PickImage) => {
                                    for path in clipboard::pick_image_files_dialog() {
                                        if let Some(img) = clipboard::load_image_file(&path) {
                                            attach_image_to_session(gpu, &mut self.runtime, img);
                                        }
                                    }
                                }
                                Ok(HostAction::ClearImage) => {
                                    capabilities::clear_browser_pending_images();
                                    gpu.clear_images();
                                }
                                Ok(HostAction::RemoveImage0) => {
                                    capabilities::remove_browser_pending_image(0);
                                    gpu.remove_pending_slot(0);
                                }
                                Ok(HostAction::RemoveImage1) => {
                                    capabilities::remove_browser_pending_image(1);
                                    gpu.remove_pending_slot(1);
                                }
                                Ok(HostAction::RemoveImage2) => {
                                    capabilities::remove_browser_pending_image(2);
                                    gpu.remove_pending_slot(2);
                                }
                                Ok(HostAction::RemoveImage3) => {
                                    capabilities::remove_browser_pending_image(3);
                                    gpu.remove_pending_slot(3);
                                }
                                Ok(HostAction::RemoveImage4) => {
                                    capabilities::remove_browser_pending_image(4);
                                    gpu.remove_pending_slot(4);
                                }
                                Ok(HostAction::RemoveImage5) => {
                                    capabilities::remove_browser_pending_image(5);
                                    gpu.remove_pending_slot(5);
                                }
                                Ok(HostAction::None) | Err(_) => {}
                            }
                            for cmd in capabilities::drain_gpu_image_cmds() {
                                match cmd {
                                    capabilities::GpuImageCmd::Clear => gpu.clear_images(),
                                    capabilities::GpuImageCmd::Add(w, h, rgba) => {
                                        let _ = gpu.add_image_rgba(w, h, &rgba);
                                    }
                                }
                            }
                            if frame.mesh3d.is_some() && !self.is_3d_app {
                                // 3D games never need Wayland/IBus IME; keeping IME off guarantees
                                // layout-independent hardware PhysicalKey scancodes (RU/HE/EN/any)!
                                self.is_3d_app = true;
                                win.set_ime_allowed(false);
                            }
                            if let Some(lock) = capabilities::take_pointer_lock_request() {
                                if lock != self.pointer_locked {
                                    if lock {
                                        win.set_ime_allowed(false);
                                        let ok = win
                                            .set_cursor_grab(CursorGrabMode::Locked)
                                            .or_else(|_| win.set_cursor_grab(CursorGrabMode::Confined))
                                            .is_ok();
                                        if ok {
                                            win.set_cursor_visible(false);
                                            self.pointer_locked = true;
                                            self.raw_mdx = 0.0;
                                            self.raw_mdy = 0.0;
                                        } else if !self.is_3d_app {
                                            win.set_ime_allowed(true);
                                        }
                                    } else {
                                        let _ = win.set_cursor_grab(CursorGrabMode::None);
                                        win.set_cursor_visible(true);
                                        if !self.is_3d_app {
                                            win.set_ime_allowed(true);
                                            Self::sync_ime_area(win);
                                        }
                                        self.pointer_locked = false;
                                        self.raw_mdx = 0.0;
                                        self.raw_mdy = 0.0;
                                    }
                                }
                            }
                            let icon = match CursorKind::try_from(frame.cursor_kind) {
                                Ok(CursorKind::Pointer) => CursorIcon::Pointer,
                                Ok(CursorKind::Text) => CursorIcon::Text,
                                Ok(CursorKind::EwResize) => CursorIcon::EwResize,
                                Ok(CursorKind::Default) | Err(_) => CursorIcon::Default,
                            };
                            win.set_cursor(icon);

                            if let Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) =
                                gpu.render(
                                    &frame.prims,
                                    &frame.clips,
                                    self.runtime.legacy_ascii_glyphs,
                                    frame.mesh3d.as_ref(),
                                )
                            {
                                let s = win.inner_size();
                                gpu.resize(s.width, s.height, win.scale_factor());
                            }
                        }
                        Err(err) => {
                            eprintln!("Capsule runtime error: {err}");
                        }
                    }
                    win.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        match event {
            DeviceEvent::MouseMotion { delta: (dx, dy) } => {
                if self.pointer_locked {
                    self.raw_mdx += dx;
                    self.raw_mdy += dy;
                }
            }
            DeviceEvent::Key(raw_key) => {
                // Layout- & IME-independent hardware scancode tracking (fixes Cyrillic/any layout on Wayland/X11!)
                if self.window_focused || raw_key.state == ElementState::Released {
                    let bit = match raw_key.physical_key {
                        PhysicalKey::Code(KeyCode::KeyW) => 1,
                        PhysicalKey::Code(KeyCode::KeyA) => 2,
                        PhysicalKey::Code(KeyCode::KeyS) => 4,
                        PhysicalKey::Code(KeyCode::KeyD) => 8,
                        PhysicalKey::Code(KeyCode::ArrowUp) => 16,
                        PhysicalKey::Code(KeyCode::ArrowDown) => 32,
                        PhysicalKey::Code(KeyCode::ArrowLeft) => 64,
                        PhysicalKey::Code(KeyCode::ArrowRight) => 128,
                        PhysicalKey::Code(KeyCode::KeyQ) => 256,
                        PhysicalKey::Code(KeyCode::KeyE) => 512,
                        PhysicalKey::Code(KeyCode::Space) => 1024,
                        PhysicalKey::Code(KeyCode::ShiftLeft)
                        | PhysicalKey::Code(KeyCode::ShiftRight) => 2048,
                        PhysicalKey::Code(KeyCode::KeyR) => 4096,
                        _ => 0,
                    };
                    if bit != 0 {
                        if raw_key.state == ElementState::Pressed {
                            self.keys_held |= bit;
                        } else {
                            self.keys_held &= !bit;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(win) = &self.window {
            win.request_redraw();
        }
    }
}

fn read_rss_mb() -> f64 {
    if let Ok(st) = fs::read_to_string("/proc/self/status") {
        for line in st.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
                if let Ok(kb) = digits.parse::<u64>() {
                    return kb as f64 / 1024.0;
                }
            }
        }
    }
    0.0
}

fn main() {
    let args = parse_cli().unwrap_or_else(|e| {
        eprintln!("CLI error: {e}");
        std::process::exit(1);
    });
    audio::set_headless(args.headless_check);

    let wasm_bytes = fs::read(&args.capsule_path).unwrap_or_else(|e| {
        eprintln!("Failed to read capsule '{}': {e}", args.capsule_path.display());
        std::process::exit(1);
    });

    let mut runtime = CapsuleRuntime::load(&wasm_bytes, args.policy, 1280, 820).unwrap_or_else(|e| {
        eprintln!("Gatekeeper / Runtime blocked capsule: {e}");
        std::process::exit(2);
    });

    let capsule_name = Path::new(&args.capsule_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("capsule.ifw")
        .to_string();

    println!(
        "infimum-web M1 | Capsule: {} | Passport: entry='{}' #{} nodes={} effects={:?} | LegacyASCII={} | RSS={:.2} MB",
        capsule_name,
        runtime.passport.display_entry(),
        runtime.passport.short_hash,
        runtime.passport.total_reachable_nodes,
        runtime.passport.effect_names(),
        runtime.legacy_ascii_glyphs,
        read_rss_mb()
    );

    let atlas = GlyphAtlas::build_system_atlas();
    runtime.upload_font_metrics(&atlas.advances_256);

    if args.headless_check {
        let frame = runtime
            .step_frame(FrameInput {
                vw: 1280,
                vh: 820,
                mx: 400,
                my: 240,
                ..Default::default()
            })
            .unwrap_or_else(|e| {
                eprintln!("Headless frame failed: {e}");
                std::process::exit(3);
            });
        println!(
            "HEADLESS_OK | Prims={} | Fuel={} | Atlas={}x{} | RSS={:.2} MB",
            frame.prims.len(),
            frame.fuel_consumed,
            atlas.width,
            atlas.height,
            read_rss_mb()
        );
        return;
    }

    let event_loop = EventLoop::new().expect("Failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let abs_capsule_path =
        fs::canonicalize(&args.capsule_path).unwrap_or_else(|_| args.capsule_path.clone());
    let capsule_dir = abs_capsule_path.parent().map(|p| p.to_path_buf());
    let textures_dir = capsule_dir.as_ref().and_then(|p| {
        let dir = p.join("textures");
        if dir.is_dir() {
            Some(dir)
        } else {
            let manifest = p.join("textures.json");
            if manifest.is_file() {
                Some(manifest)
            } else {
                None
            }
        }
    });

    #[allow(unused_mut)]
    let mut app_id = args.app_id;
    #[cfg(target_arch = "x86_64")]
    let _ = ();
    #[cfg(target_os = "linux")]
    if app_id == "infimum-web" {
        let stem = capsule_name.trim_end_matches(".ifw");
        if let Some(icon_path) = find_capsule_icon_path(capsule_dir.as_deref(), stem) {
            app_id = format!("infimum-ifw-{}", sanitize_app_id_stem(stem));
            let display_name = args
                .window_title
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|t| t.to_string())
                .unwrap_or_else(|| pretty_capsule_title(stem));
            ensure_wayland_desktop_icon(&app_id, &display_name, &icon_path, &abs_capsule_path);
        }
    }

    let mut app = InfimumWebApp {
        capsule_name,
        capsule_dir,
        textures_dir,
        window_title: args.window_title,
        app_id,
        runtime,
        atlas,
        clipboard: NativeClipboard::new(),
        modifiers: ModifiersState::empty(),
        window: None,
        gpu: None,
        mx: 0,
        my: 0,
        m_down: 0,
        m_clicked: 0,
        wheel_dy: 0,
        scroll_vel: 0.0,
        keys_held: 0,
        window_focused: true,
        is_3d_app: false,
        pointer_locked: false,
        raw_mdx: 0.0,
        raw_mdy: 0.0,
        key_queue: VecDeque::new(),
    };

    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("Event loop error: {e}");
        std::process::exit(1);
    }
}
