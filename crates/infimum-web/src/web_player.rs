//! `wasm32` WebGL2/WebGPU Browser Renderer for natively JIT-compiled `.ifw` Infimum Capsules.
//!
//! The browser's native WebAssembly JIT executes the `.ifw` game capsule and `audio_core.ifw`
//! at full machine-code speed (zero `wasmi` interpreter overhead), while this Rust/wgpu module
//! renders the exact same WGSL shaders (`shader.wgsl` + `mesh3d.wgsl`), Unicode `GlyphAtlas`,
//! and 12-layer PBR `Texture2DArray`.

use crate::atlas::GlyphAtlas;
use crate::gpu::GpuRenderer;
use crate::runtime::{ClipRect, GpuPrim, Mesh3DScene, MeshVertex3D, PointLight3D, MESH3D_MAGIC};
use std::collections::BTreeMap;
use wasm_bindgen::prelude::*;

const AUDIO_CORE_AXW: &[u8] = include_bytes!("../capsules/core/audio_core.ifw");

#[inline]
fn read_i32_le(bytes: &[u8], idx: usize) -> i32 {
    let off = idx * 4;
    if off + 4 <= bytes.len() {
        i32::from_le_bytes(bytes[off..off + 4].try_into().unwrap())
    } else {
        0
    }
}

#[wasm_bindgen]
pub struct InfimumWebPlayer {
    gpu: GpuRenderer,
    legacy_ascii_glyphs: bool,
    font_advances: Vec<i32>,
    pending_textures: BTreeMap<u32, (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)>,
}

#[wasm_bindgen]
impl InfimumWebPlayer {
    /// Embedded Z3-verified `audio_core.ifw` bytes so the browser can JIT-compile it natively.
    pub fn audio_core_axw() -> js_sys::Uint8Array {
        js_sys::Uint8Array::from(AUDIO_CORE_AXW)
    }

    /// Initialize `infimum-web` GPU pipeline on a browser `<canvas>` with Regular TTF font bytes.
    pub async fn create(
        canvas: web_sys::HtmlCanvasElement,
        font_bytes: &[u8],
        capsule_name: &str,
        expect_textures: bool,
        legacy_ascii_glyphs: bool,
    ) -> Result<InfimumWebPlayer, JsValue> {
        Self::create_with_fonts(
            canvas,
            font_bytes,
            None,
            None,
            capsule_name,
            expect_textures,
            legacy_ascii_glyphs,
        )
        .await
    }

    /// Initialize `infimum-web` GPU pipeline on a browser `<canvas>` with distinct
    /// Regular, Bold, and Mono TrueType fonts (Bold/Mono fall back to Regular when omitted).
    pub async fn create_with_fonts(
        canvas: web_sys::HtmlCanvasElement,
        regular_font_bytes: &[u8],
        bold_font_bytes: Option<Vec<u8>>,
        mono_font_bytes: Option<Vec<u8>>,
        capsule_name: &str,
        expect_textures: bool,
        legacy_ascii_glyphs: bool,
    ) -> Result<InfimumWebPlayer, JsValue> {
        console_error_panic_hook::set_once();

        let width = canvas.width().max(1);
        let height = canvas.height().max(1);
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0)
            .max(1.0);

        let atlas = GlyphAtlas::build_from_font_styles(
            regular_font_bytes,
            bold_font_bytes.as_deref(),
            mono_font_bytes.as_deref(),
        )
        .map_err(|e| JsValue::from_str(&format!("{e:?}")))?;

        let mut font_advances = Vec::with_capacity(3 * 176);
        for row in &atlas.advances_256 {
            font_advances.extend_from_slice(row);
        }

        let gpu = GpuRenderer::new_for_canvas(
            canvas,
            width,
            height,
            dpr,
            &atlas,
            capsule_name,
            expect_textures,
        )
        .await
        .map_err(|e| JsValue::from_str(&e))?;

        Ok(InfimumWebPlayer {
            gpu,
            legacy_ascii_glyphs,
            font_advances,
            pending_textures: BTreeMap::new(),
        })
    }

    /// TrueType advance widths (`3 * 176` i32s) to upload into `.ifw` linear memory
    /// at the capsule's exported `get_font_metrics_byte_offset()` table.
    pub fn font_advances_i32(&self) -> js_sys::Int32Array {
        js_sys::Int32Array::from(self.font_advances.as_slice())
    }

    pub fn logical_width(&self) -> i32 {
        self.gpu.logical_size().0 as i32
    }

    pub fn logical_height(&self) -> i32 {
        self.gpu.logical_size().1 as i32
    }

    pub fn set_loading_progress(&mut self, pct: u32) {
        self.gpu.set_web_loading_progress(true, pct);
    }

    pub fn add_texture_file(&mut self, layer: u32, kind: &str, bytes: &[u8]) {
        let entry = self
            .pending_textures
            .entry(layer)
            .or_insert_with(|| (Vec::new(), None, None));
        match kind {
            "diff" => entry.0 = bytes.to_vec(),
            "norm" => entry.1 = Some(bytes.to_vec()),
            "rough" => entry.2 = Some(bytes.to_vec()),
            _ => {}
        }
    }

    pub fn finalize_textures(&mut self) {
        let map = std::mem::take(&mut self.pending_textures);
        let prep = crate::textures3d::prepare_textures_3d_from_bytes(&map, 512);
        self.gpu.apply_prepared_textures(prep);
    }

    pub fn resize(&mut self, width: u32, height: u32, scale_factor: f64) {
        self.gpu
            .resize(width.max(1), height.max(1), scale_factor.max(1.0));
    }

    pub fn add_image_rgba(&mut self, width: u32, height: u32, rgba: &[u8]) -> i32 {
        self.gpu
            .add_image_rgba(width, height, rgba)
            .map(|i| i as i32)
            .unwrap_or(-1)
    }

    pub fn remove_pending_slot(&mut self, rel_idx: u32) {
        self.gpu.remove_pending_slot(rel_idx as usize);
    }

    pub fn clear_images(&mut self) {
        self.gpu.clear_images();
    }

    /// Render one frame from exact `.ifw` linear-memory byte slices (`clips`, `prims`, and optional `mesh3d`).
    pub fn render_frame(
        &mut self,
        clips_bytes: &[u8],
        prims_bytes: &[u8],
        mesh_hdr_bytes: &[u8],
        mesh_lights_bytes: &[u8],
        world_verts_bytes: &[u8],
        vm_verts_bytes: &[u8],
    ) -> Result<(), JsValue> {
        let (lw, lh) = self.gpu.logical_size();
        let mut clips = [ClipRect {
            x0: 0.0,
            y0: 0.0,
            x1: lw as f32,
            y1: lh as f32,
        }; 16];
        if clips_bytes.len() >= 16 * 16 {
            for (i, clip) in clips.iter_mut().enumerate() {
                let base = i * 4;
                *clip = ClipRect {
                    x0: read_i32_le(clips_bytes, base) as f32,
                    y0: read_i32_le(clips_bytes, base + 1) as f32,
                    x1: read_i32_le(clips_bytes, base + 2) as f32,
                    y1: read_i32_le(clips_bytes, base + 3) as f32,
                };
            }
        }

        let prim_count = (prims_bytes.len() / 32).min(180_000);
        let prims: Vec<GpuPrim> = prims_bytes[..prim_count * 32]
            .chunks_exact(32)
            .map(bytemuck::pod_read_unaligned::<GpuPrim>)
            .collect();

        let mesh3d = if mesh_hdr_bytes.len() >= 48 && read_i32_le(mesh_hdr_bytes, 0) == MESH3D_MAGIC
        {
            let cam_x = read_i32_le(mesh_hdr_bytes, 1) as f32 * 0.001;
            let cam_y = read_i32_le(mesh_hdr_bytes, 2) as f32 * 0.001;
            let cam_z = read_i32_le(mesh_hdr_bytes, 3) as f32 * 0.001;
            let yaw_i = read_i32_le(mesh_hdr_bytes, 4);
            let pitch_i = read_i32_le(mesh_hdr_bytes, 5).clamp(-240, 240);
            let fov_deg = read_i32_le(mesh_hdr_bytes, 6).clamp(45, 110) as f32;
            let sky_mode = read_i32_le(mesh_hdr_bytes, 7).max(0) as u32;
            let fog_mode = read_i32_le(mesh_hdr_bytes, 8).clamp(0, 2) as u32;
            let light_count = (read_i32_le(mesh_hdr_bytes, 10).max(0) as usize)
                .min(24)
                .min(mesh_lights_bytes.len() / 32);

            let mut lights = Vec::with_capacity(light_count);
            for i in 0..light_count {
                let b = i * 8;
                let lx = read_i32_le(mesh_lights_bytes, b) as f32 * 0.001;
                let ly = read_i32_le(mesh_lights_bytes, b + 1) as f32 * 0.001;
                let lz = read_i32_le(mesh_lights_bytes, b + 2) as f32 * 0.001;
                let rad = (read_i32_le(mesh_lights_bytes, b + 3).max(500) as f32) * 0.001;
                let r = (read_i32_le(mesh_lights_bytes, b + 4).clamp(0, 255) as f32) / 255.0;
                let g = (read_i32_le(mesh_lights_bytes, b + 5).clamp(0, 255) as f32) / 255.0;
                let bl = (read_i32_le(mesh_lights_bytes, b + 6).clamp(0, 255) as f32) / 255.0;
                let inten = (read_i32_le(mesh_lights_bytes, b + 7).clamp(0, 512) as f32) / 255.0;
                lights.push(PointLight3D {
                    pos_rad: [lx, ly, lz, rad],
                    col_int: [r, g, bl, inten],
                });
            }

            let wn = (world_verts_bytes.len() / 32).min(120_000);
            let world_verts: Vec<MeshVertex3D> = world_verts_bytes[..wn * 32]
                .chunks_exact(32)
                .map(bytemuck::pod_read_unaligned::<MeshVertex3D>)
                .collect();
            let vn = (vm_verts_bytes.len() / 32).min(6_000);
            let vm_verts: Vec<MeshVertex3D> = vm_verts_bytes[..vn * 32]
                .chunks_exact(32)
                .map(bytemuck::pod_read_unaligned::<MeshVertex3D>)
                .collect();

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
        } else {
            None
        };

        self.gpu
            .render(&prims, &clips, self.legacy_ascii_glyphs, mesh3d.as_ref())
            .map_err(|e| JsValue::from_str(&format!("{e:?}")))
    }
}
