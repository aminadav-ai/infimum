//! Deterministic `wasmi` capsule sandbox with per-frame fuel metering and
//! zero-copy GPU instance buffer extraction.

#[cfg(not(target_arch = "wasm32"))]
use crate::capabilities::{dispatch_host_call, E_IO_FAILED};
#[cfg(not(target_arch = "wasm32"))]
use crate::passport::{verify_capsule_passport, CapabilityPolicy, SecurityPassport};
#[cfg(not(target_arch = "wasm32"))]
use crate::browser_abi::{
    CLIPBOARD_MAX_CHARS as ABI_CLIPBOARD_MAX_CHARS, GPU_CLIPS_BYTE_OFFSET, GPU_CMD_BYTE_OFFSET,
    IFW_ABI_VERSION, INPUT_MAX_CHARS as ABI_INPUT_MAX_CHARS,
    LARGE_COPY_MAX_CHARS as ABI_LARGE_COPY_MAX_CHARS, SYS_CLIPBOARD_MAILBOX_WORD,
    SYS_FONT_METRICS_BYTE_OFFSET, SYS_FONT_METRICS_FLAG_WORD, SYS_FONT_METRICS_TABLE_WORD,
    SYS_INPUT_BYTE_OFFSET,
};
use bytemuck::{Pod, Zeroable};
#[cfg(not(target_arch = "wasm32"))]
use wasmi::{Caller, Config, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

/// Per-frame instruction fuel budget (prevents infinite-loop hangs in untrusted capsules).
#[cfg(not(target_arch = "wasm32"))]
pub const FRAME_FUEL_BUDGET: u64 = 80_000_000;

/// Load a Z3-verified core `.ifw` module from disk (`capsules/core/<file_name>`) if present
/// so edits to core Infimum modules take effect via `infimum compile-wasm` without recompiling Rust,
/// falling back seamlessly to the embedded `include_bytes!` copy.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_core_axw(file_name: &str, embedded: &[u8]) -> (Engine, Module) {
    let mut cfg = Config::default();
    cfg.consume_fuel(true);
    let engine = Engine::new(&cfg);

    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut candidates = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
                candidates.push(root.join("crates/infimum-web/capsules/core").join(file_name));
            }
        }
        candidates.push(std::path::PathBuf::from("crates/infimum-web/capsules/core").join(file_name));
        candidates.push(std::path::PathBuf::from("capsules/core").join(file_name));

        for p in candidates {
            if let Ok(bytes) = std::fs::read(&p) {
                if let Ok(module) = Module::new(&engine, &bytes) {
                    return (engine, module);
                }
            }
        }
    }
    let _ = file_name;
    let module = Module::new(&engine, embedded)
        .unwrap_or_else(|e| panic!("Embedded core module {file_name} must be valid Wasm: {e}"));
    (engine, module)
}

/// Linear memory word address used as the host <-> capsule UTF-32 clipboard mailbox (`len`, `cp0..`).
#[cfg(not(target_arch = "wasm32"))]
pub const CLIPBOARD_MAILBOX_WORD: usize = SYS_CLIPBOARD_MAILBOX_WORD as usize;
#[cfg(not(target_arch = "wasm32"))]
const CLIPBOARD_MAX_CHARS: usize = ABI_CLIPBOARD_MAX_CHARS as usize;
#[cfg(not(target_arch = "wasm32"))]
const LARGE_COPY_MAX_CHARS: usize = ABI_LARGE_COPY_MAX_CHARS as usize;

/// Linear memory word address where host writes exact TrueType advance widths (`3 * 176` words).
#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
pub const FONT_METRICS_FLAG_WORD: usize = SYS_FONT_METRICS_FLAG_WORD as usize;
#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
pub const FONT_METRICS_TABLE_WORD: usize = SYS_FONT_METRICS_TABLE_WORD as usize;

/// Single GPU primitive matching `infimum-ui` / `studio_core.ifm` 8 x i32 layout (32 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Pod, Zeroable)]
pub struct GpuPrim {
    pub w0: i32,
    pub w1: i32,
    pub w2: i32,
    pub w3: i32,
    pub meta: u32,
    pub col1: u32,
    pub col2: u32,
    pub clip_id: u32,
}

/// Hardware clip rectangle (`x0, y0, x1, y1` in logical pixels).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Pod, Zeroable)]
pub struct ClipRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FrameInput {
    pub vw: i32,
    pub vh: i32,
    pub mx: i32,
    pub my: i32,
    pub m_down: i32,
    pub m_clicked: i32,
    pub wheel_dy: i32,
    pub key_char: i32,
    pub key_special: i32,
    pub keys_held: i32,
    pub mouse_dx: i32,
    pub mouse_dy: i32,
    pub pointer_locked: i32,
}

pub const MESH3D_MAGIC: i32 = 1_028_866_894; // 0x3D53434E ("3SCN")

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct MeshVertex3D {
    pub x_mm: i32,
    pub y_mm: i32,
    pub z_mm: i32,
    pub norm_mat: u32,
    pub u_1024: i32,
    pub v_1024: i32,
    pub rgba: u32,
    pub flags_ao: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct PointLight3D {
    pub pos_rad: [f32; 4],
    pub col_int: [f32; 4],
}

pub struct Mesh3DScene {
    pub cam_pos_m: [f32; 3],
    pub cam_yaw_rad: f32,
    pub cam_pitch_rad: f32,
    pub fov_deg: f32,
    pub sky_mode: u32,
    pub fog_mode: u32,
    pub lights: Vec<PointLight3D>,
    pub world_verts: Vec<MeshVertex3D>,
    pub vm_verts: Vec<MeshVertex3D>,
}

pub struct FrameOutput {
    pub prims: Vec<GpuPrim>,
    pub clips: [ClipRect; 16],
    pub cursor_kind: i32,
    pub fuel_consumed: u64,
    pub mesh3d: Option<Mesh3DScene>,
}

#[cfg(not(target_arch = "wasm32"))]
struct HostState {
    policy: CapabilityPolicy,
}

#[cfg(not(target_arch = "wasm32"))]
type StepUiArgs = (i32, i32, i32, i32, i32, i32, i32, i32, i32);

#[cfg(not(target_arch = "wasm32"))]
pub struct CapsuleRuntime {
    pub passport: SecurityPassport,
    pub legacy_ascii_glyphs: bool,
    store: Store<HostState>,
    memory: Memory,
    step_ui: TypedFunc<StepUiArgs, i32>,
    get_cursor_kind: Option<TypedFunc<(), i32>>,
    on_paste_text: Option<TypedFunc<(i32, i32), i32>>,
    on_paste_image: Option<TypedFunc<(i32, i32), i32>>,
    get_copy_text: Option<TypedFunc<(), i32>>,
    pop_host_action_fn: Option<TypedFunc<(), i32>>,
    copy_mailbox_word: usize,
    copy_max_chars: usize,
    paste_mailbox_word: usize,
    cmd_byte_offset: usize,
    clips_byte_offset: usize,
    font_metrics_byte_offset: usize,
    input_byte_offset: usize,
    mesh3d_byte_offset: Option<usize>,
}

#[cfg(not(target_arch = "wasm32"))]
impl CapsuleRuntime {
    /// Verify `infimum.passport` BEFORE instantiating Wasm, then boot `init_app`.
    pub fn load(
        wasm_bytes: &[u8],
        policy: CapabilityPolicy,
        init_w: i32,
        init_h: i32,
    ) -> Result<Self, String> {
        let passport = verify_capsule_passport(wasm_bytes, &policy)
            .map_err(|e| e.to_string())?;

        let mut cfg = Config::default();
        cfg.consume_fuel(true);
        let engine = Engine::new(&cfg);

        let module = Module::new(&engine, wasm_bytes)
            .map_err(|e| format!("Failed to compile Wasm module: {e}"))?;

        let mut store = Store::new(&engine, HostState { policy });
        store
            .set_fuel(FRAME_FUEL_BUDGET)
            .map_err(|e| format!("Failed to set Wasm fuel: {e}"))?;

        let mut linker = Linker::<HostState>::new(&engine);
        linker
            .func_wrap(
                "env",
                "host_call",
                |mut caller: Caller<'_, HostState>, op: i64, a0: i64, a1: i64, a2: i64| -> i64 {
                    let Some(ext) = caller.get_export("memory") else {
                        return E_IO_FAILED;
                    };
                    let Some(mem) = ext.into_memory() else {
                        return E_IO_FAILED;
                    };
                    let (mem_bytes, host_state) = mem.data_and_store_mut(&mut caller);
                    dispatch_host_call(&host_state.policy, mem_bytes, op, a0, a1, a2)
                },
            )
            .map_err(|e| format!("Failed to register env.host_call: {e}"))?;

        let instance: Instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| format!("Failed to instantiate Wasm module: {e}"))?
            .start(&mut store)
            .map_err(|e| format!("Failed to start Wasm module: {e}"))?;

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| "Capsule does not export linear `memory`".to_string())?;

        // Detect Unicode atlas vs legacy raw ASCII codepoints (`studio_core` exports `set_studio_telemetry`).
        let has_studio_unicode = instance
            .get_func(&store, "set_studio_telemetry")
            .is_some()
            || instance.get_func(&store, "ifw_unicode_atlas").is_some();
        let legacy_ascii_glyphs = !has_studio_unicode;

        let cmd_byte_offset = instance
            .get_typed_func::<(), i32>(&store, "get_cmd_byte_offset")
            .ok()
            .and_then(|f| f.call(&mut store, ()).ok())
            .map(|v| v.max(0) as usize)
            .unwrap_or(GPU_CMD_BYTE_OFFSET as usize);

        let clips_byte_offset = instance
            .get_typed_func::<(), i32>(&store, "get_clips_byte_offset")
            .ok()
            .and_then(|f| f.call(&mut store, ()).ok())
            .map(|v| v.max(0) as usize)
            .unwrap_or(GPU_CLIPS_BYTE_OFFSET as usize);

        if let Ok(abi_fn) = instance.get_typed_func::<(), i32>(&store, "ifw_abi_version") {
            let v = abi_fn
                .call(&mut store, ())
                .map_err(|e| format!("Capsule `ifw_abi_version` trapped: {e}"))?;
            if v != IFW_ABI_VERSION as i32 {
                return Err(format!(
                    "Capsule ABI version mismatch: host expects {IFW_ABI_VERSION}, capsule exports {v}"
                ));
            }
        }

        let font_metrics_byte_offset = instance
            .get_typed_func::<(), i32>(&store, "get_font_metrics_byte_offset")
            .ok()
            .and_then(|f| f.call(&mut store, ()).ok())
            .map(|v| v.max(0) as usize)
            .unwrap_or(SYS_FONT_METRICS_BYTE_OFFSET as usize);

        let input_byte_offset = instance
            .get_typed_func::<(), i32>(&store, "get_input_byte_offset")
            .ok()
            .and_then(|f| f.call(&mut store, ()).ok())
            .map(|v| v.max(0) as usize)
            .unwrap_or(SYS_INPUT_BYTE_OFFSET as usize);

        let mesh3d_byte_offset = instance
            .get_typed_func::<(), i32>(&store, "get_mesh3d_byte_offset")
            .ok()
            .and_then(|f| f.call(&mut store, ()).ok())
            .filter(|&v| v > 0)
            .map(|v| v as usize);

        if let Ok(init_app) = instance.get_typed_func::<(i32, i32, i32), i32>(&store, "init_app") {
            let _ = init_app
                .call(&mut store, (2026, init_w, init_h))
                .map_err(|e| format!("Capsule `init_app` trapped: {e}"))?;
        } else if let Ok(init_app) = instance.get_typed_func::<i32, i32>(&store, "init_app") {
            let _ = init_app
                .call(&mut store, 2026)
                .map_err(|e| format!("Capsule `init_app` trapped: {e}"))?;
        } else if let Ok(init_app) = instance.get_typed_func::<(), i32>(&store, "init_app") {
            let _ = init_app
                .call(&mut store, ())
                .map_err(|e| format!("Capsule `init_app` trapped: {e}"))?;
        }

        let step_ui = instance
            .get_typed_func::<(i32, i32, i32, i32, i32, i32, i32, i32, i32), i32>(&store, "step_ui")
            .map_err(|e| format!("Capsule missing `step_ui` export: {e}"))?;

        let get_cursor_kind = instance
            .get_typed_func::<(), i32>(&store, "get_cursor_kind")
            .ok();
        let on_paste_text = instance
            .get_typed_func::<(i32, i32), i32>(&store, "ifw_on_paste_text")
            .ok();
        let on_paste_image = instance
            .get_typed_func::<(i32, i32), i32>(&store, "ifw_on_paste_image")
            .ok();
        let get_copy_text = instance
            .get_typed_func::<(), i32>(&store, "ifw_get_copy_text")
            .ok();
        let pop_host_action_fn = instance
            .get_typed_func::<(), i32>(&store, "ifw_pop_host_action")
            .ok();
        let (copy_mailbox_word, copy_max_chars) = instance
            .get_typed_func::<(), i32>(&store, "ifw_get_copy_mailbox_word")
            .ok()
            .and_then(|f| f.call(&mut store, ()).ok())
            .filter(|&w| w > 0)
            .map(|w| (w as usize, LARGE_COPY_MAX_CHARS))
            .unwrap_or((CLIPBOARD_MAILBOX_WORD, CLIPBOARD_MAX_CHARS));

        let paste_mailbox_word = instance
            .get_typed_func::<(), i32>(&store, "ifw_get_paste_mailbox_word")
            .ok()
            .and_then(|f| f.call(&mut store, ()).ok())
            .filter(|&w| w > 0)
            .map(|w| w as usize)
            .unwrap_or(CLIPBOARD_MAILBOX_WORD);

        Ok(Self {
            passport,
            legacy_ascii_glyphs,
            store,
            memory,
            step_ui,
            get_cursor_kind,
            on_paste_text,
            on_paste_image,
            get_copy_text,
            pop_host_action_fn,
            copy_mailbox_word,
            copy_max_chars,
            paste_mailbox_word,
            cmd_byte_offset,
            clips_byte_offset,
            font_metrics_byte_offset,
            input_byte_offset,
            mesh3d_byte_offset,
        })
    }

    /// Upload exact TrueType per-glyph advance widths (`1/256` of em) into capsule linear memory.
    pub fn upload_font_metrics(&mut self, advances_256: &[[i32; 176]; 3]) {
        let mem = self.memory.data_mut(&mut self.store);
        let flag_off = self.font_metrics_byte_offset;
        if flag_off + 4 > mem.len() {
            return;
        }
        mem[flag_off..flag_off + 4].copy_from_slice(&1i32.to_le_bytes());
        let table_base = flag_off + 4;
        for (style, row) in advances_256.iter().enumerate() {
            for (slot, &adv) in row.iter().enumerate() {
                let off = table_base + (style * 176 + slot) * 4;
                if off + 4 <= mem.len() {
                    mem[off..off + 4].copy_from_slice(&adv.to_le_bytes());
                }
            }
        }
    }

    /// Pop a pending host clipboard action requested by the capsule UI (`1=copy, 2=paste, 3=cut`).
    pub fn pop_host_action(&mut self) -> i32 {
        let Some(hook) = self.pop_host_action_fn else {
            return 0;
        };
        let _ = self.store.set_fuel(FRAME_FUEL_BUDGET);
        hook.call(&mut self.store, ()).unwrap_or(0)
    }

    /// Write pasted UTF-8 text as UTF-32 words into the capsule clipboard mailbox (chunked up to 8192 chars) and notify capsule.
    pub fn paste_text(&mut self, text: &str) {
        let Some(hook) = self.on_paste_text else {
            return;
        };
        let all_chars: Vec<u32> = text
            .chars()
            .filter(|&c| c != '\r')
            .take(ABI_INPUT_MAX_CHARS as usize)
            .map(|c| c as u32)
            .collect();
        if all_chars.is_empty() {
            return;
        }
        let base_byte = self.paste_mailbox_word * 4;
        for chunk in all_chars.chunks(CLIPBOARD_MAX_CHARS) {
            let needed_bytes = (chunk.len() + 1) * 4;
            let mem_mut = self.memory.data_mut(&mut self.store);
            if base_byte + needed_bytes > mem_mut.len() {
                return;
            }
            mem_mut[base_byte..base_byte + 4].copy_from_slice(&(chunk.len() as i32).to_le_bytes());
            for (i, &cp) in chunk.iter().enumerate() {
                let off = base_byte + (i + 1) * 4;
                mem_mut[off..off + 4].copy_from_slice(&(cp as i32).to_le_bytes());
            }
            let _ = self.store.set_fuel(FRAME_FUEL_BUDGET);
            let _ = hook.call(&mut self.store, (self.paste_mailbox_word as i32, chunk.len() as i32));
        }
    }

    /// Notify capsule that a pasted RGBA image of size `(w, h)` was uploaded to GPU binding 3.
    pub fn paste_image(&mut self, width: u32, height: u32) {
        if let Some(hook) = self.on_paste_image {
            let _ = self.store.set_fuel(FRAME_FUEL_BUDGET);
            let _ = hook.call(&mut self.store, (width as i32, height as i32));
        }
    }

    /// Ask capsule to write selected text into the clipboard mailbox and return it as UTF-8.
    pub fn copy_text(&mut self) -> Option<String> {
        let hook = self.get_copy_text?;
        let _ = self.store.set_fuel(FRAME_FUEL_BUDGET);
        let len = hook.call(&mut self.store, ()).ok()?.max(0) as usize;
        if len == 0 {
            return None;
        }
        let capped = len.min(self.copy_max_chars);
        let base_byte = self.copy_mailbox_word * 4;
        let mem = self.memory.data(&self.store);
        if base_byte + (capped + 1) * 4 > mem.len() {
            return None;
        }
        let mut out = String::with_capacity(capped);
        for i in 0..capped {
            let off = base_byte + (i + 1) * 4;
            let cp = u32::from_le_bytes(mem[off..off + 4].try_into().ok()?);
            if let Some(ch) = char::from_u32(cp) {
                out.push(ch);
            }
        }
        Some(out)
    }

    /// Execute one deterministic UI frame under fuel metering and extract GPU primitives.
    pub fn step_frame(&mut self, inp: FrameInput) -> Result<FrameOutput, String> {
        self.store
            .set_fuel(FRAME_FUEL_BUDGET)
            .map_err(|e| format!("Failed to replenish fuel: {e}"))?;

        // Write real-time input state (`std/input.ifm` at `input_byte_offset`) before stepping the frame.
        {
            let base_off = self.input_byte_offset;
            let mem = self.memory.data_mut(&mut self.store);
            if base_off + 16 <= mem.len() {
                mem[base_off..base_off + 4]
                    .copy_from_slice(&inp.keys_held.to_le_bytes());
                mem[base_off + 4..base_off + 8]
                    .copy_from_slice(&inp.mouse_dx.to_le_bytes());
                mem[base_off + 8..base_off + 12]
                    .copy_from_slice(&inp.mouse_dy.to_le_bytes());
                mem[base_off + 12..base_off + 16]
                    .copy_from_slice(&inp.pointer_locked.to_le_bytes());
            }
        }

        let prim_count = self
            .step_ui
            .call(
                &mut self.store,
                (
                    inp.vw,
                    inp.vh,
                    inp.mx,
                    inp.my,
                    inp.m_down,
                    inp.m_clicked,
                    inp.wheel_dy,
                    inp.key_char,
                    inp.key_special,
                ),
            )
            .map_err(|e| format!("Capsule `step_ui` trapped (or exceeded fuel): {e}"))?;

        let cursor_kind = self
            .get_cursor_kind
            .as_ref()
            .and_then(|f| f.call(&mut self.store, ()).ok())
            .unwrap_or(0);

        let fuel_left = self.store.get_fuel().unwrap_or(0);
        let fuel_consumed = FRAME_FUEL_BUDGET.saturating_sub(fuel_left);

        let mem_bytes = self.memory.data(&self.store);

        let mut clips = [ClipRect {
            x0: 0.0,
            y0: 0.0,
            x1: inp.vw as f32,
            y1: inp.vh as f32,
        }; 16];

        if self.clips_byte_offset + 16 * 16 <= mem_bytes.len() {
            for (i, clip) in clips.iter_mut().enumerate() {
                let off = self.clips_byte_offset + i * 16;
                let x0 = i32::from_le_bytes(mem_bytes[off..off + 4].try_into().unwrap());
                let y0 = i32::from_le_bytes(mem_bytes[off + 4..off + 8].try_into().unwrap());
                let x1 = i32::from_le_bytes(mem_bytes[off + 8..off + 12].try_into().unwrap());
                let y1 = i32::from_le_bytes(mem_bytes[off + 12..off + 16].try_into().unwrap());
                *clip = ClipRect {
                    x0: x0 as f32,
                    y0: y0 as f32,
                    x1: x1 as f32,
                    y1: y1 as f32,
                };
            }
        }

        let count = (prim_count.max(0) as usize).min(180_000);
        let mut prims = Vec::with_capacity(count);
        let max_bytes = count.saturating_mul(32);
        if self.cmd_byte_offset + max_bytes <= mem_bytes.len() {
            for i in 0..count {
                let off = self.cmd_byte_offset + i * 32;
                let w0 = i32::from_le_bytes(mem_bytes[off..off + 4].try_into().unwrap());
                let w1 = i32::from_le_bytes(mem_bytes[off + 4..off + 8].try_into().unwrap());
                let w2 = i32::from_le_bytes(mem_bytes[off + 8..off + 12].try_into().unwrap());
                let w3 = i32::from_le_bytes(mem_bytes[off + 12..off + 16].try_into().unwrap());
                let meta = u32::from_le_bytes(mem_bytes[off + 16..off + 20].try_into().unwrap());
                let col1 = u32::from_le_bytes(mem_bytes[off + 20..off + 24].try_into().unwrap());
                let col2 = u32::from_le_bytes(mem_bytes[off + 24..off + 28].try_into().unwrap());
                let clip_id = u32::from_le_bytes(mem_bytes[off + 28..off + 32].try_into().unwrap());
                prims.push(GpuPrim {
                    w0,
                    w1,
                    w2,
                    w3,
                    meta,
                    col1,
                    col2,
                    clip_id,
                });
            }
        }

        let mesh3d = extract_mesh3d_scene(mem_bytes, self.mesh3d_byte_offset);

        Ok(FrameOutput {
            prims,
            clips,
            cursor_kind,
            fuel_consumed,
            mesh3d,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_i32_word(mem: &[u8], word: usize) -> i32 {
    let off = word * 4;
    if off + 4 <= mem.len() {
        i32::from_le_bytes(mem[off..off + 4].try_into().unwrap())
    } else {
        0
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn extract_mesh3d_verts(mem: &[u8], base_word: usize, count: usize) -> Vec<MeshVertex3D> {
    let max_fit = mem.len().saturating_sub(base_word * 4) / 32;
    let n = count.min(max_fit);
    if n == 0 {
        return Vec::new();
    }
    let start = base_word * 4;
    let slice = &mem[start..start + n * 32];
    bytemuck::cast_slice::<u8, MeshVertex3D>(slice).to_vec()
}

#[cfg(not(target_arch = "wasm32"))]
fn extract_mesh3d_scene(mem: &[u8], mesh3d_byte_offset: Option<usize>) -> Option<Mesh3DScene> {
    let base = mesh3d_byte_offset? / 4;
    let (hdr_word, lights_word, vbo_word, vm_vbo_word) =
        (base, base + 100, base + 1000, base + 961_000);
    if read_i32_word(mem, hdr_word) != MESH3D_MAGIC {
        return None;
    }
    let cam_x = read_i32_word(mem, hdr_word + 1) as f32 * 0.001;
    let cam_y = read_i32_word(mem, hdr_word + 2) as f32 * 0.001;
    let cam_z = read_i32_word(mem, hdr_word + 3) as f32 * 0.001;
    let yaw_i = read_i32_word(mem, hdr_word + 4);
    let pitch_i = read_i32_word(mem, hdr_word + 5).clamp(-240, 240);
    let fov_deg = read_i32_word(mem, hdr_word + 6).clamp(45, 110) as f32;
    let sky_mode = read_i32_word(mem, hdr_word + 7).max(0) as u32;
    let fog_mode = read_i32_word(mem, hdr_word + 8).clamp(0, 2) as u32;
    let vert_count = (read_i32_word(mem, hdr_word + 9).max(0) as usize).min(120_000);
    let light_count = (read_i32_word(mem, hdr_word + 10).max(0) as usize).min(24);
    let vm_vert_count = (read_i32_word(mem, hdr_word + 11).max(0) as usize).min(6_000);

    let mut lights = Vec::with_capacity(light_count);
    for i in 0..light_count {
        let b = lights_word + i * 8;
        let lx = read_i32_word(mem, b) as f32 * 0.001;
        let ly = read_i32_word(mem, b + 1) as f32 * 0.001;
        let lz = read_i32_word(mem, b + 2) as f32 * 0.001;
        let rad = (read_i32_word(mem, b + 3).max(500) as f32) * 0.001;
        let r = (read_i32_word(mem, b + 4).clamp(0, 255) as f32) / 255.0;
        let g = (read_i32_word(mem, b + 5).clamp(0, 255) as f32) / 255.0;
        let bl = (read_i32_word(mem, b + 6).clamp(0, 255) as f32) / 255.0;
        let inten = (read_i32_word(mem, b + 7).clamp(0, 512) as f32) / 255.0;
        lights.push(PointLight3D {
            pos_rad: [lx, ly, lz, rad],
            col_int: [r, g, bl, inten],
        });
    }

    let world_verts = extract_mesh3d_verts(mem, vbo_word, vert_count);
    let vm_verts = extract_mesh3d_verts(mem, vm_vbo_word, vm_vert_count);

    Some(Mesh3DScene {
        cam_pos_m: [cam_x, cam_y, cam_z],
        cam_yaw_rad: (yaw_i as f32) * std::f32::consts::TAU / 4096.0,
        cam_pitch_rad: (pitch_i as f32) * std::f32::consts::FRAC_PI_2 / 256.0,
        fov_deg,
        sky_mode,
        fog_mode,
        lights,
        world_verts,
        vm_verts,
    })
}

#[cfg(test)]
#[path = "../../../apps/infimum-code/daemon/src/studio_abi.rs"]
mod studio_abi;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn studio_capsule_path() -> &'static Path {
        Path::new("../../apps/infimum-code/capsule/infimum_code.ifw")
    }

    fn studio_test_policy() -> CapabilityPolicy {
        let mut policy = CapabilityPolicy {
            allow_clipboard: true,
            ..Default::default()
        };
        policy.allowed_net_domains.insert("127.0.0.1".to_string());
        policy
            .allowed_net_domains
            .insert("infimum-lang.org".to_string());
        policy
    }

    #[test]
    fn boots_and_steps_studio_core_capsule_if_present() {
        let p = studio_capsule_path();
        assert!(p.exists(), "Expected precompiled Infimum Code capsule at {}", p.display());
        let bytes = std::fs::read(p).unwrap();
        let mut rt = CapsuleRuntime::load(&bytes, studio_test_policy(), 1280, 720).unwrap();
        assert!(!rt.legacy_ascii_glyphs);
        let frame = rt
            .step_frame(FrameInput {
                vw: 1280,
                vh: 720,
                mx: 400,
                my: 240,
                ..Default::default()
            })
            .unwrap();
        assert!(frame.prims.len() > 20, "Expected GPU primitives, got {}", frame.prims.len());
        assert!(frame.fuel_consumed > 0 && frame.fuel_consumed < FRAME_FUEL_BUDGET);
    }

    #[test]
    fn studio_capsule_selects_and_copies_chat_message_substring() {
        let p = studio_capsule_path();
        assert!(p.exists(), "Expected precompiled Infimum Code capsule at {}", p.display());
        let bytes = std::fs::read(p).unwrap();
        let mut rt = CapsuleRuntime::load(&bytes, studio_test_policy(), 1280, 720).unwrap();
        assert_eq!(rt.copy_mailbox_word, studio_abi::STUDIO_MEM_COPY_MAILBOX_WORD);
        assert_eq!(rt.copy_max_chars, LARGE_COPY_MAX_CHARS);

        // Scan vertically to locate the welcome assistant reply text (cursor_kind == 2 = I-beam)
        let mut text_y = None;
        for y in (70..260).step_by(8) {
            let f = rt
                .step_frame(FrameInput {
                    vw: 1280,
                    vh: 720,
                    mx: 520,
                    my: y,
                    ..Default::default()
                })
                .unwrap();
            if f.cursor_kind == 2 {
                text_y = Some(y);
                break;
            }
        }
        let ty = text_y.expect("Expected I-beam hover cursor over chat reply text");

        // Mouse press at x=460, drag to x=660, release -> copy_text() must return selected fragment
        rt.step_frame(FrameInput {
            vw: 1280,
            vh: 720,
            mx: 460,
            my: ty,
            m_down: 1,
            m_clicked: 1,
            ..Default::default()
        })
        .unwrap();
        rt.step_frame(FrameInput {
            vw: 1280,
            vh: 720,
            mx: 660,
            my: ty,
            m_down: 1,
            ..Default::default()
        })
        .unwrap();
        rt.step_frame(FrameInput {
            vw: 1280,
            vh: 720,
            mx: 660,
            my: ty,
            m_down: 0,
            ..Default::default()
        })
        .unwrap();

        let copied = rt
            .copy_text()
            .expect("Expected non-empty copied substring from chat reply");
        assert!(
            copied.chars().count() >= 4,
            "Expected meaningful selected substring, got {:?}",
            copied
        );
        assert!(
            copied.contains("Infimum") || copied.contains("Добро") || copied.contains("пожаловать") || copied.contains("Studio"),
            "Unexpected copied text: {:?}",
            copied
        );
    }

    #[test]
    fn studio_capsule_composer_scroll_image_and_chevron() {
        let p = studio_capsule_path();
        assert!(p.exists(), "Expected precompiled Infimum Code capsule at {}", p.display());
        let bytes = std::fs::read(p).unwrap();
        let mut rt = CapsuleRuntime::load(&bytes, studio_test_policy(), 1280, 720).unwrap();

        // 1. Paste a large 30-line (>3000 char) text into composer
        let big_text: String = (1..=30)
            .map(|i| format!("Строка номер {i}: проверка прокрутки большого текста в нативной Infimum Code для длинного промпта.\n"))
            .collect();
        assert!(big_text.chars().count() > 2500);
        rt.paste_text(&big_text);

        // Step 1 frame so composer computes layout and auto-follows cursor to bottom
        let f1 = rt
            .step_frame(FrameInput {
                vw: 1280,
                vh: 720,
                mx: 640,
                my: 600,
                ..Default::default()
            })
            .unwrap();
        let read_w = |rt: &CapsuleRuntime, word: usize| -> i32 {
            let mem = rt.memory.data(&rt.store);
            i32::from_le_bytes(mem[word * 4..word * 4 + 4].try_into().unwrap())
        };
        let input_len = read_w(&rt, studio_abi::STUDIO_MEM_INPUT_BUF_WORD);
        assert!(input_len > 2500, "Expected >2500 pasted chars, got {input_len}");
        let scroll_bottom = read_w(&rt, studio_abi::STUDIO_MEM_IN_SCROLL_Y_WORD);
        assert!(scroll_bottom > 100, "Expected composer to auto-scroll to bottom, got {scroll_bottom}");

        // Scroll UP inside composer using mouse wheel
        rt.step_frame(FrameInput {
            vw: 1280,
            vh: 720,
            mx: 640,
            my: 600,
            wheel_dy: -120,
            ..Default::default()
        })
        .unwrap();
        let scroll_after_up = read_w(&rt, studio_abi::STUDIO_MEM_IN_SCROLL_Y_WORD);
        assert!(
            scroll_after_up < scroll_bottom,
            "Expected wheel up inside composer to decrease scroll_y ({scroll_after_up} < {scroll_bottom})"
        );

        // Ensure chevron / vector icon line primitives (kind == 2) exist in composer toolbar
        assert!(
            f1.prims.iter().any(|pr| (pr.meta & 15) == 2 && pr.w1 > 650),
            "Expected vector chevron / image icon line primitives in composer toolbar"
        );

        // 2. Attach screenshot image -> verify kind == 4 preview renders inside bottom composer card
        rt.paste_image(1280, 720);
        let f_img = rt
            .step_frame(FrameInput {
                vw: 1280,
                vh: 720,
                mx: 640,
                my: 600,
                ..Default::default()
            })
            .unwrap();
        let img_prim = f_img
            .prims
            .iter()
            .find(|pr| (pr.meta & 15) == 4)
            .expect("Expected kind==4 image preview primitive inside composer");
        assert!(
            img_prim.w1 > 300 && img_prim.w1 < 700,
            "Expected image preview inside bottom composer card (300 < y < 700), got y={}",
            img_prim.w1
        );
    }

    #[test]
    fn studio_capsule_renders_markdown_table_grid_and_selection() {
        let p = studio_capsule_path();
        assert!(p.exists(), "Expected precompiled Infimum Code capsule at {}", p.display());
        let bytes = std::fs::read(p).unwrap();
        let mut rt = CapsuleRuntime::load(&bytes, studio_test_policy(), 1280, 720).unwrap();

        let table_md = "Ключевые моменты:\n\n\
            | Под-язык | Назначение |\n\
            |---------|------------|\n\
            | Telos | Формальная верификация через Z3 SMT: предусловия (`req`), постусловия (`ens`) |\n\
            | Flux | Система эффектов: `-[Mem]->`, `mem_load`/`mem_store` |\n\
            | Strata | Прямая компиляция в Wasm (`.ifw`) |";

        // Overwrite message 0 (assistant welcome message) in msg_pool and invalidate cached height
        {
            let mem = rt.memory.data_mut(&mut rt.store);
            let meta0 = studio_abi::STUDIO_MEM_MSG_META_WORD * 4;
            let text_ptr = i32::from_le_bytes(mem[meta0 + 16..meta0 + 20].try_into().unwrap()) as usize;
            let chars: Vec<u32> = table_md.chars().map(|c| c as u32).collect();
            let base = text_ptr * 4;
            mem[base..base + 4].copy_from_slice(&(chars.len() as i32).to_le_bytes());
            for (idx, cp) in chars.iter().enumerate() {
                let off = base + (1 + idx) * 4;
                mem[off..off + 4].copy_from_slice(&(*cp as i32).to_le_bytes());
            }
            // Invalidate cached message height (msg_meta[5])
            mem[meta0 + 20..meta0 + 24].copy_from_slice(&0i32.to_le_bytes());
        }

        let frame = rt
            .step_frame(FrameInput {
                vw: 1280,
                vh: 720,
                mx: 640,
                my: 200,
                ..Default::default()
            })
            .unwrap();

        // 1. No raw pipe '|' glyph should be emitted in the Feed clip (clip_id == 1)
        let pipe_atlas_idx = ('|' as u32) - (' ' as u32);
        let feed_pipes = frame
            .prims
            .iter()
            .filter(|pr| pr.clip_id == 1 && (pr.meta & 15) == 1 && ((pr.meta >> 4) & 255) == pipe_atlas_idx)
            .count();
        assert_eq!(
            feed_pipes, 0,
            "Markdown table delimiters '|' must not render as raw text glyphs"
        );

        // 2. Expect vertical column dividers (1px wide, >=24px tall) and horizontal row dividers in Feed clip
        let v_dividers = frame
            .prims
            .iter()
            .filter(|pr| pr.clip_id == 1 && (pr.meta & 15) == 0 && pr.w2 == 1 && pr.w3 >= 24)
            .count();
        let h_dividers = frame
            .prims
            .iter()
            .filter(|pr| pr.clip_id == 1 && (pr.meta & 15) == 0 && pr.w2 >= 200 && (pr.w3 == 1 || pr.w3 == 2))
            .count();
        assert!(
            v_dividers >= 12,
            "Expected >=12 vertical border/divider segments across 4 table rows (3 per row), got {v_dividers}"
        );
        assert!(
            h_dividers >= 5,
            "Expected >=5 horizontal table border segments (top + 4 row bottoms), got {h_dividers}"
        );

        // 3. Drag-select across the table header row and verify copy_text() extracts table text
        let header_rect = frame
            .prims
            .iter()
            .filter(|pr| pr.clip_id == 1 && (pr.meta & 15) == 0 && pr.w2 >= 200 && pr.w3 >= 24)
            .min_by_key(|pr| pr.w1)
            .expect("Expected table header row background rect");
        let header_y = header_rect.w1 + header_rect.w3 / 2;
        let drag_x0 = header_rect.w0 + 8;
        let drag_x1 = header_rect.w0 + header_rect.w2 - 8;

        rt.step_frame(FrameInput {
            vw: 1280,
            vh: 720,
            mx: drag_x0,
            my: header_y,
            m_down: 1,
            m_clicked: 1,
            ..Default::default()
        })
        .unwrap();
        rt.step_frame(FrameInput {
            vw: 1280,
            vh: 720,
            mx: drag_x1,
            my: header_y,
            m_down: 1,
            ..Default::default()
        })
        .unwrap();
        rt.step_frame(FrameInput {
            vw: 1280,
            vh: 720,
            mx: drag_x1,
            my: header_y,
            m_down: 0,
            ..Default::default()
        })
        .unwrap();

        let copied = rt
            .copy_text()
            .expect("Expected non-empty copied substring from rendered table");
        assert!(
            copied.contains("Под-язык") || copied.contains("Назначение"),
            "Expected copied text from table header, got {:?}",
            copied
        );
    }
}
