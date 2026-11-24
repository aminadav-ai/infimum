//! Procedural Seamless 256x256 PBR Texture2DArray Generator (Diffuse + Normal/Roughness + Mipmaps)
//! Generates Quake-inspired weathered masonry, sandstone cliffs, timber planks, riveted iron,
//! terracotta dungeon flagstones, and fluted columns once at startup in ~30ms with zero external assets.

use wgpu::util::DeviceExt;

pub const TEX_SIZE: u32 = 256;
pub const TEX_LAYERS: u32 = 12;
pub const TEX_MIPS: u32 = 9; // 256 -> 128 -> ... -> 1 (external packs auto-upgrade to 1024x1024 / 11 mips)

pub struct MeshTextures3D {
    pub diffuse_view: wgpu::TextureView,
    pub normal_view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
}

pub struct PreparedPbrTextures {
    pub tex_size: u32,
    pub tex_mips: u32,
    pub diff_packed: Vec<u8>,
    pub norm_packed: Vec<u8>,
    pub loaded_count: u32,
}

#[inline]
fn hash_u32(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

#[inline]
fn hash2_tile(x: i32, y: i32, px: i32, py: i32, seed: u32) -> f32 {
    let tx = x.rem_euclid(px) as u32;
    let ty = y.rem_euclid(py) as u32;
    let h = hash_u32(tx.wrapping_mul(374_761_393) ^ ty.wrapping_mul(668_265_263) ^ seed);
    (h & 0x00ff_ffff) as f32 / 16_777_215.0
}

fn value_noise_tile(u: f32, v: f32, freq_x: i32, freq_y: i32, seed: u32) -> f32 {
    let x = u * freq_x as f32;
    let y = v * freq_y as f32;
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let sx = fx * fx * (3.0 - 2.0 * fx);
    let sy = fy * fy * (3.0 - 2.0 * fy);
    let a = hash2_tile(x0, y0, freq_x, freq_y, seed);
    let b = hash2_tile(x0 + 1, y0, freq_x, freq_y, seed);
    let c = hash2_tile(x0, y0 + 1, freq_x, freq_y, seed);
    let d = hash2_tile(x0 + 1, y0 + 1, freq_x, freq_y, seed);
    let ab = a + (b - a) * sx;
    let cd = c + (d - c) * sx;
    ab + (cd - ab) * sy
}

fn fbm_tile(u: f32, v: f32, base_fx: i32, base_fy: i32, octaves: u32, seed: u32) -> f32 {
    let mut sum = 0.0f32;
    let mut amp = 0.5f32;
    let mut norm = 0.0f32;
    let mut fx = base_fx;
    let mut fy = base_fy;
    for o in 0..octaves {
        sum += amp * value_noise_tile(u, v, fx, fy, seed.wrapping_add(o * 1013));
        norm += amp;
        amp *= 0.5;
        fx *= 2;
        fy *= 2;
    }
    sum / norm.max(0.001)
}

#[inline]
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[inline]
fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t)]
}

#[inline]
fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

struct TexLayerData {
    diffuse: Vec<u8>, // 256*256*4 RGBA
    normal: Vec<u8>,  // 256*256*4 (RGB = tangent normal, A = roughness)
}

fn build_layer(layer: u32) -> TexLayerData {
    let n = (TEX_SIZE * TEX_SIZE) as usize;
    let mut diffuse = vec![0u8; n * 4];
    let mut height = vec![0.0f32; n];
    let mut rough = vec![0.75f32; n];

    for py in 0..TEX_SIZE {
        let v = py as f32 / TEX_SIZE as f32;
        for px in 0..TEX_SIZE {
            let u = px as f32 / TEX_SIZE as f32;
            let idx = (py * TEX_SIZE + px) as usize;

            let (col, h, r) = match layer {
                // 0: MAT_SOLID — Sculpted stone / monster hide / organic grit
                0 => {
                    let n1 = fbm_tile(u, v, 8, 8, 4, 11);
                    let n2 = fbm_tile(u, v, 24, 24, 3, 29);
                    let shade = 0.78 + 0.28 * n1 + 0.14 * (n2 - 0.5);
                    ([shade, shade * 0.99, shade * 0.97], n1 * 0.65 + n2 * 0.35, 0.72)
                }
                // 1: MAT_BRICK — Heavy Quake Medieval Ashlar Masonry with mortar, cracks, grime & moss
                1 => {
                    let rows = 6.0f32;
                    let cols = 4.0f32;
                    let warp_u = (fbm_tile(u, v, 8, 8, 3, 101) - 0.5) * 0.018;
                    let warp_v = (fbm_tile(u, v, 8, 8, 3, 202) - 0.5) * 0.018;
                    let bu = (u + warp_u).rem_euclid(1.0);
                    let bv = (v + warp_v).rem_euclid(1.0);
                    let row = (bv * rows).floor() as i32;
                    let shift = if row % 2 == 0 { 0.0 } else { 0.5 };
                    let bx = bu * cols + shift;
                    let by = bv * rows;
                    let cell_x = bx.floor() as i32;
                    let fx = bx.fract();
                    let fy = by.fract();

                    let edge_x = fx.min(1.0 - fx);
                    let edge_y = fy.min(1.0 - fy);
                    let bevel_x = smoothstep(0.032, 0.095, edge_x);
                    let bevel_y = smoothstep(0.048, 0.135, edge_y);
                    let block = bevel_x * bevel_y;

                    let rnd = hash2_tile(cell_x, row, cols as i32, rows as i32, 307);
                    let rnd2 = hash2_tile(cell_x, row, cols as i32, rows as i32, 911);
                    let macro_n = fbm_tile(u, v, 4, 4, 4, 409);
                    let grain = fbm_tile(u, v, 24, 24, 3, 521);
                    let pits = smoothstep(0.62, 0.85, fbm_tile(u, v, 16, 16, 3, 631));

                    // Warm medieval ochre/grey stone variation per block
                    let stone_a = [0.76, 0.68, 0.56];
                    let stone_b = [0.54, 0.50, 0.45];
                    let stone_c = [0.68, 0.52, 0.36];
                    let mut c = mix3(stone_a, stone_b, rnd);
                    c = mix3(c, stone_c, rnd2 * 0.42);
                    let detail = 0.72 + 0.36 * grain + 0.18 * (macro_n - 0.5) - 0.22 * pits;
                    c = [c[0] * detail, c[1] * detail, c[2] * detail];

                    // Moss creeping in damp seams & crevices
                    let moss_n = fbm_tile(u, v, 4, 4, 4, 733);
                    let moss = smoothstep(0.54, 0.78, moss_n) * (1.0 - block * 0.55);
                    c = mix3(c, [0.22, 0.30, 0.14], moss * 0.65);

                    // Dark gritty mortar between stones
                    let mortar = [0.16, 0.14, 0.12];
                    let final_c = mix3(mortar, c, block);
                    let h = block * (0.55 + 0.25 * rnd + 0.20 * grain - 0.22 * pits);
                    (final_c, h, lerp(0.95, 0.68, block))
                }
                // 2: MAT_PILLAR — Weathered Fluted Temple Column with rust bands & stone seams
                2 => {
                    let flutes = ((u * std::f32::consts::TAU * 8.0).cos() * 0.5 + 0.5).powf(0.75);
                    let seam_v = (v * 3.0).fract();
                    let seam = smoothstep(0.018, 0.055, seam_v.min(1.0 - seam_v));
                    let band = 1.0 - smoothstep(0.06, 0.11, (seam_v - 0.5).abs());
                    let grain = fbm_tile(u, v, 16, 16, 4, 821);
                    let rust_n = fbm_tile(u, v, 4, 8, 4, 947);
                    let streak = fbm_tile(u, v, 12, 3, 3, 1021);

                    let mut c = [0.78, 0.74, 0.68];
                    let mod_l = (0.76 + 0.22 * flutes) * (0.80 + 0.28 * grain);
                    c = [c[0] * mod_l, c[1] * mod_l, c[2] * mod_l];
                    // Rust & ochre staining near joints (just like right columns in Quake!)
                    let rust = smoothstep(0.46, 0.74, rust_n * 0.7 + streak * 0.3);
                    c = mix3(c, [0.56, 0.30, 0.16], rust * 0.68);
                    c = mix3([0.18, 0.15, 0.13], c, seam);
                    c = mix3(c, [0.42, 0.36, 0.32], band * 0.45);
                    let h = seam * (0.35 + 0.35 * flutes + 0.18 * grain + 0.15 * band);
                    (c, h, 0.62)
                }
                // 3: MAT_METAL — Riveted Gothic Iron Plates (beams, doors, armor, shotgun)
                3 => {
                    let gx = (u * 2.0).fract();
                    let gy = (v * 2.0).fract();
                    let panel = smoothstep(0.02, 0.055, gx.min(1.0 - gx))
                        * smoothstep(0.02, 0.055, gy.min(1.0 - gy));

                    // Corner rivets
                    let rx = (gx - 0.10).abs().min((gx - 0.90).abs());
                    let ry = (gy - 0.10).abs().min((gy - 0.90).abs());
                    let r_dist = (rx * rx + ry * ry).sqrt();
                    let rivet = 1.0 - smoothstep(0.024, 0.042, r_dist);

                    let brush = fbm_tile(u, v, 32, 8, 3, 1151);
                    let rust_n = fbm_tile(u, v, 6, 6, 4, 1277);
                    let rust = smoothstep(0.58, 0.80, rust_n);

                    let steel = [0.58 + 0.18 * brush, 0.60 + 0.18 * brush, 0.66 + 0.18 * brush];
                    let mut c = mix3([0.14, 0.14, 0.16], steel, panel);
                    c = mix3(c, [0.48, 0.25, 0.14], rust * 0.65);
                    c = mix3(c, [0.82, 0.80, 0.78], rivet * 0.55);
                    let h = panel * (0.45 + 0.15 * brush - 0.10 * rust) + rivet * 0.42;
                    (c, h, lerp(0.32, 0.82, rust))
                }
                // 6: MAT_FLOOR — Quake Terracotta / Dark Dungeon Brick Flagstones (like Shambler hall!)
                6 => {
                    let rows = 8.0f32;
                    let cols = 4.0f32;
                    let row = (v * rows).floor() as i32;
                    let shift = if row % 2 == 0 { 0.0 } else { 0.5 };
                    let bx = u * cols + shift;
                    let by = v * rows;
                    let cx = bx.floor() as i32;
                    let fx = bx.fract();
                    let fy = by.fract();

                    let tile = smoothstep(0.035, 0.095, fx.min(1.0 - fx))
                        * smoothstep(0.045, 0.12, fy.min(1.0 - fy));
                    let rnd = hash2_tile(cx, row, cols as i32, rows as i32, 1409);
                    let grain = fbm_tile(u, v, 20, 20, 4, 1531);
                    let grime = fbm_tile(u, v, 4, 4, 4, 1657);

                    let terra_a = [0.64, 0.42, 0.30];
                    let terra_b = [0.44, 0.32, 0.25];
                    let mut c = mix3(terra_a, terra_b, rnd);
                    let mod_l = (0.72 + 0.34 * grain) * (0.75 + 0.38 * grime);
                    c = [c[0] * mod_l, c[1] * mod_l, c[2] * mod_l];
                    c = mix3([0.12, 0.09, 0.08], c, tile);
                    let h = tile * (0.50 + 0.24 * rnd + 0.22 * grain);
                    (c, h, lerp(0.92, 0.58, tile * grime))
                }
                // 7: MAT_ROCK — Sharp Layered Canyon Sandstone & Craggy Cliff (no blobby spots!)
                7 => {
                    let strata = fbm_tile(u, v, 4, 24, 4, 1783);
                    let ledges = ((v * 14.0 + strata * 1.8).sin() * 0.5 + 0.5).powf(0.6);
                    let crag = fbm_tile(u, v, 16, 16, 4, 1907);
                    let grit = fbm_tile(u, v, 64, 64, 3, 2029);
                    let crack_n = (fbm_tile(u, v, 12, 12, 3, 2141) - 0.5).abs();
                    let crack = smoothstep(0.012, 0.045, crack_n);

                    let rock_a = [0.76, 0.54, 0.38];
                    let rock_b = [0.46, 0.34, 0.26];
                    let mut c = mix3(rock_b, rock_a, strata * 0.6 + ledges * 0.4);
                    let d = (0.70 + 0.34 * crag + 0.24 * (grit - 0.5)) * (0.48 + 0.52 * crack);
                    c = [c[0] * d, c[1] * d, c[2] * d];
                    let moss = smoothstep(0.68, 0.86, fbm_tile(u, v, 8, 8, 3, 2267)) * (1.0 - ledges * 0.5);
                    c = mix3(c, [0.24, 0.33, 0.16], moss * 0.48);
                    let h = (strata * 0.32 + ledges * 0.28 + crag * 0.25 + grit * 0.15) * (0.35 + 0.65 * crack);
                    (c, h, 0.82)
                }
                // 8: MAT_CRATE — Aged Weathered Timber Planks (vertical wood panels & plank bridge!)
                8 | 11 => {
                    let planks = 6.0f32;
                    let px_f = u * planks;
                    let plank_i = px_f.floor() as i32;
                    let fx = px_f.fract();
                    let gap = smoothstep(0.028, 0.085, fx.min(1.0 - fx));

                    let rnd = hash2_tile(plank_i, 0, planks as i32, 1, 2381);
                    let warp = fbm_tile(u, v, 6, 4, 3, 2503) * 0.25;
                    let rings = (u * 42.0 + warp * 14.0 + rnd * 6.0).sin() * 0.5 + 0.5;
                    let grain = fbm_tile(u, v, 48, 8, 4, 2621);
                    let stain = fbm_tile(u, v, 6, 6, 3, 2749);
                    let fine_wood = fbm_tile(u, v, 96, 16, 2, 2801);

                    let wood_a = if layer == 11 { [0.58, 0.38, 0.22] } else { [0.68, 0.56, 0.38] };
                    let wood_b = if layer == 11 { [0.34, 0.20, 0.11] } else { [0.38, 0.31, 0.21] };
                    let mut c = mix3(wood_b, wood_a, rnd * 0.5 + rings * 0.5);
                    let mod_l = (0.70 + 0.38 * grain + 0.18 * (fine_wood - 0.5)) * (0.74 + 0.34 * stain);
                    c = [c[0] * mod_l, c[1] * mod_l, c[2] * mod_l];
                    let lichen = smoothstep(0.66, 0.84, stain);
                    c = mix3(c, [0.34, 0.42, 0.24], lichen * 0.42);
                    c = mix3([0.07, 0.05, 0.04], c, gap);
                    let h = gap * (0.42 + 0.26 * rings + 0.24 * grain + 0.16 * fine_wood);
                    (c, h, 0.76)
                }
                // 4, 5, 9, 10: Fallback / Marble / Lava crust / Rune base
                _ => {
                    let n1 = fbm_tile(u, v, 6, 6, 4, 3001 + layer * 97);
                    let n2 = fbm_tile(u, v, 18, 18, 3, 3301 + layer * 97);
                    let shade = 0.78 + 0.28 * n1 + 0.14 * (n2 - 0.5);
                    ([shade, shade, shade], n1 * 0.6 + n2 * 0.4, 0.50)
                }
            };

            diffuse[idx * 4] = to_u8(col[0]);
            diffuse[idx * 4 + 1] = to_u8(col[1]);
            diffuse[idx * 4 + 2] = to_u8(col[2]);
            diffuse[idx * 4 + 3] = 255;
            height[idx] = h;
            rough[idx] = r;
        }
    }

    // Compute seamless Sobel tangent-space normal map from heightfield (strength tuned for crisp Quake relief)
    let strength = match layer {
        1 | 6 | 7 => 4.8,
        2 | 3 | 8 | 11 => 3.8,
        _ => 2.2,
    };
    let mut normal = vec![0u8; n * 4];
    let sz = TEX_SIZE as i32;
    let sample_h = |x: i32, y: i32| -> f32 {
        let wx = x.rem_euclid(sz) as usize;
        let wy = y.rem_euclid(sz) as usize;
        height[wy * TEX_SIZE as usize + wx]
    };

    for py in 0..sz {
        for px in 0..sz {
            let tl = sample_h(px - 1, py - 1);
            let tc = sample_h(px, py - 1);
            let tr = sample_h(px + 1, py - 1);
            let ml = sample_h(px - 1, py);
            let mr = sample_h(px + 1, py);
            let bl = sample_h(px - 1, py + 1);
            let bc = sample_h(px, py + 1);
            let br = sample_h(px + 1, py + 1);

            let dx = (tr + 2.0 * mr + br) - (tl + 2.0 * ml + bl);
            let dy = (bl + 2.0 * bc + br) - (tl + 2.0 * tc + tr);
            let nx = -dx * strength;
            let ny = -dy * strength;
            let nz = 1.0f32;
            let inv_len = 1.0 / (nx * nx + ny * ny + nz * nz).sqrt();

            let idx = (py as usize) * (TEX_SIZE as usize) + (px as usize);
            normal[idx * 4] = to_u8(nx * inv_len * 0.5 + 0.5);
            normal[idx * 4 + 1] = to_u8(ny * inv_len * 0.5 + 0.5);
            normal[idx * 4 + 2] = to_u8(nz * inv_len * 0.5 + 0.5);
            normal[idx * 4 + 3] = to_u8(rough[idx]);
        }
    }

    TexLayerData { diffuse, normal }
}

fn downsample_rgba(src: &[u8], w: u32, h: u32, is_normal: bool) -> Vec<u8> {
    let nw = (w / 2).max(1);
    let nh = (h / 2).max(1);
    let mut dst = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            let sx = (x * 2).min(w - 1);
            let sy = (y * 2).min(h - 1);
            let sx1 = (sx + 1).min(w - 1);
            let sy1 = (sy + 1).min(h - 1);
            let i00 = ((sy * w + sx) * 4) as usize;
            let i10 = ((sy * w + sx1) * 4) as usize;
            let i01 = ((sy1 * w + sx) * 4) as usize;
            let i11 = ((sy1 * w + sx1) * 4) as usize;
            let di = ((y * nw + x) * 4) as usize;
            if is_normal {
                let mut nx = 0.0f32;
                let mut ny = 0.0f32;
                let mut nz = 0.0f32;
                let mut r = 0.0f32;
                for i in [i00, i10, i01, i11] {
                    nx += src[i] as f32 / 255.0 * 2.0 - 1.0;
                    ny += src[i + 1] as f32 / 255.0 * 2.0 - 1.0;
                    nz += src[i + 2] as f32 / 255.0 * 2.0 - 1.0;
                    r += src[i + 3] as f32 / 255.0;
                }
                let inv = 1.0 / (nx * nx + ny * ny + nz * nz).sqrt().max(0.001);
                dst[di] = to_u8(nx * inv * 0.5 + 0.5);
                dst[di + 1] = to_u8(ny * inv * 0.5 + 0.5);
                dst[di + 2] = to_u8(nz * inv * 0.5 + 0.5);
                dst[di + 3] = to_u8(r * 0.25);
            } else {
                for c in 0..4 {
                    let sum = src[i00 + c] as u32
                        + src[i10 + c] as u32
                        + src[i01 + c] as u32
                        + src[i11 + c] as u32;
                    dst[di + c] = (sum / 4) as u8;
                }
            }
        }
    }
    dst
}

fn decode_image_rgba(path: &std::path::Path) -> Option<(u32, u32, Vec<u8>)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    if ext == "jpg" || ext == "jpeg" {
        let mut dec = jpeg_decoder::Decoder::new(reader);
        let pixels = dec.decode().ok()?;
        let info = dec.info()?;
        let w = info.width as u32;
        let h = info.height as u32;
        let n = (w * h) as usize;
        let mut rgba = Vec::with_capacity(n * 4);
        match info.pixel_format {
            jpeg_decoder::PixelFormat::RGB24 => {
                for chunk in pixels.as_chunks::<3>().0 {
                    rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
                }
            }
            jpeg_decoder::PixelFormat::L8 => {
                for &g in &pixels {
                    rgba.extend_from_slice(&[g, g, g, 255]);
                }
            }
            jpeg_decoder::PixelFormat::CMYK32 => {
                for chunk in pixels.as_chunks::<4>().0 {
                    let c = chunk[0] as u32;
                    let m = chunk[1] as u32;
                    let y = chunk[2] as u32;
                    let k = chunk[3] as u32;
                    rgba.extend_from_slice(&[
                        ((c * k) / 255) as u8,
                        ((m * k) / 255) as u8,
                        ((y * k) / 255) as u8,
                        255,
                    ]);
                }
            }
            _ => return None,
        }
        return Some((w, h, rgba));
    }
    if ext == "png" {
        let mut dec = png::Decoder::new(reader);
        dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut pr = dec.read_info().ok()?;
        let mut buf = vec![0u8; pr.output_buffer_size()];
        let info = pr.next_frame(&mut buf).ok()?;
        let data = &buf[..info.buffer_size()];
        let mut rgba = Vec::with_capacity((info.width * info.height * 4) as usize);
        match info.color_type {
            png::ColorType::Rgba => rgba.extend_from_slice(data),
            png::ColorType::Rgb => {
                for chunk in data.as_chunks::<3>().0 {
                    rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
                }
            }
            png::ColorType::Grayscale => {
                for &g in data {
                    rgba.extend_from_slice(&[g, g, g, 255]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for chunk in data.as_chunks::<2>().0 {
                    rgba.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
                }
            }
            _ => return None,
        }
        return Some((info.width, info.height, rgba));
    }
    None
}

fn resize_rgba_bilinear(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    if sw == dw && sh == dh {
        return src.to_vec();
    }
    let mut dst = vec![0u8; (dw * dh * 4) as usize];
    for y in 0..dh {
        let fy = (y as f32 + 0.5) * (sh as f32 / dh as f32) - 0.5;
        let y0 = (fy.floor() as i32).clamp(0, sh as i32 - 1) as u32;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = (fy - fy.floor()).clamp(0.0, 1.0);
        for x in 0..dw {
            let fx = (x as f32 + 0.5) * (sw as f32 / dw as f32) - 0.5;
            let x0 = (fx.floor() as i32).clamp(0, sw as i32 - 1) as u32;
            let x1 = (x0 + 1).min(sw - 1);
            let wx = (fx - fx.floor()).clamp(0.0, 1.0);
            let i00 = ((y0 * sw + x0) * 4) as usize;
            let i10 = ((y0 * sw + x1) * 4) as usize;
            let i01 = ((y1 * sw + x0) * 4) as usize;
            let i11 = ((y1 * sw + x1) * 4) as usize;
            let di = ((y * dw + x) * 4) as usize;
            for c in 0..4 {
                let top = src[i00 + c] as f32 * (1.0 - wx) + src[i10 + c] as f32 * wx;
                let bot = src[i01 + c] as f32 * (1.0 - wx) + src[i11 + c] as f32 * wx;
                dst[di + c] = (top * (1.0 - wy) + bot * wy).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    dst
}

#[cfg(not(target_arch = "wasm32"))]
fn find_layer_file(dir: &std::path::Path, layer: u32, suffix: &str) -> Option<std::path::PathBuf> {
    let prefix = format!("{layer}_");
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            if stem.starts_with(&prefix) && stem.ends_with(suffix) {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(target_arch = "wasm32")]
fn pack_mip_chain_single(
    tex_size: u32,
    tex_mips: u32,
    layers_mip0: &[Vec<u8>],
    is_normal: bool,
) -> Vec<u8> {
    let mut packed = Vec::new();
    for mip0 in layers_mip0 {
        let mut cur = mip0.clone();
        let mut w = tex_size;
        let mut h = tex_size;
        for mip in 0..tex_mips {
            packed.extend_from_slice(&cur);
            if mip + 1 < tex_mips {
                cur = downsample_rgba(&cur, w, h, is_normal);
                w = (w / 2).max(1);
                h = (h / 2).max(1);
            }
        }
    }
    packed
}

#[cfg(not(target_arch = "wasm32"))]
fn pack_mip_chain_parallel(
    tex_size: u32,
    tex_mips: u32,
    layers_mip0: &[Vec<u8>],
    is_normal: bool,
) -> Vec<u8> {
    let layer_chunks: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = layers_mip0
            .iter()
            .map(|mip0| {
                s.spawn(move || {
                    let mut out = Vec::with_capacity(mip0.len() * 4 / 3 + 1024);
                    let mut cur = mip0.clone();
                    let mut w = tex_size;
                    let mut h = tex_size;
                    for mip in 0..tex_mips {
                        out.extend_from_slice(&cur);
                        if mip + 1 < tex_mips {
                            cur = downsample_rgba(&cur, w, h, is_normal);
                            w = (w / 2).max(1);
                            h = (h / 2).max(1);
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let total_bytes: usize = layer_chunks.iter().map(|c| c.len()).sum();
    let mut packed = Vec::with_capacity(total_bytes);
    for chunk in layer_chunks {
        packed.extend_from_slice(&chunk);
    }
    packed
}

fn upload_packed_texture_array(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    tex_size: u32,
    tex_mips: u32,
    packed: &[u8],
) -> wgpu::TextureView {
    // WebGL2 / wgpu-hal GLES quirk: square 2D texture arrays whose layer count is a
    // multiple of 6 (like 12) are misclassified as GL_TEXTURE_CUBE_MAP_ARRAY (0x9009),
    // which is unsupported in WebGL2. Padding with 1 unused layer (13) forces a true
    // GL_TEXTURE_2D_ARRAY target on WebGL2 while shaders clamp to layers 0..11.
    #[cfg(target_arch = "wasm32")]
    let (layers, padded_buf): (u32, Vec<u8>) = {
        let per_layer = packed.len() / (TEX_LAYERS as usize).max(1);
        let mut buf = Vec::with_capacity(packed.len() + per_layer);
        buf.extend_from_slice(packed);
        buf.resize(packed.len() + per_layer, 0);
        (TEX_LAYERS + 1, buf)
    };
    #[cfg(target_arch = "wasm32")]
    let upload_data: &[u8] = &padded_buf;

    #[cfg(not(target_arch = "wasm32"))]
    let (layers, upload_data): (u32, &[u8]) = (TEX_LAYERS, packed);

    let tex = device.create_texture_with_data(
        queue,
        &wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: tex_size,
                height: tex_size,
                depth_or_array_layers: layers,
            },
            mip_level_count: tex_mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        },
        wgpu::util::TextureDataOrder::LayerMajor,
        upload_data,
    );

    tex.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    })
}

/// Instant (< 0.2 ms) 1x1x12 placeholder texture array so the window opens immediately
/// while 1K photo-PBR textures decode & build mipmaps on a background thread.
pub fn create_placeholder_textures_3d(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) -> MeshTextures3D {
    let mut diff_1x1 = Vec::with_capacity((TEX_LAYERS * 4) as usize);
    let mut norm_1x1 = Vec::with_capacity((TEX_LAYERS * 4) as usize);
    for _ in 0..TEX_LAYERS {
        diff_1x1.extend_from_slice(&[180, 180, 180, 255]);
        norm_1x1.extend_from_slice(&[128, 128, 255, 200]);
    }
    let diffuse_view =
        upload_packed_texture_array(device, queue, "infimum-mesh3d-diff-ph", 1, 1, &diff_1x1);
    let normal_view =
        upload_packed_texture_array(device, queue, "infimum-mesh3d-norm-ph", 1, 1, &norm_1x1);
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("infimum-mesh3d-sampler"),
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Linear,
        #[cfg(not(target_arch = "wasm32"))]
        anisotropy_clamp: 16,
        #[cfg(target_arch = "wasm32")]
        anisotropy_clamp: 1,
        ..Default::default()
    });
    MeshTextures3D {
        diffuse_view,
        normal_view,
        sampler,
    }
}

/// Pure CPU background loader: decodes external 1K JPEG/PNG PBR textures (or builds
/// procedural fallback layers) and generates full mipmap chains across all CPU cores,
/// reporting live 0..100 progress for the instant splash screen.
/// Decode raw PNG or JPEG bytes into RGBA8 (used on wasm32 where files come from `fetch()`).
#[cfg(target_arch = "wasm32")]
pub fn decode_image_bytes(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if bytes.len() >= 8 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().ok()?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).ok()?;
        let data = &buf[..info.buffer_size()];
        let mut rgba = Vec::with_capacity((info.width * info.height * 4) as usize);
        match info.color_type {
            png::ColorType::Rgba => rgba.extend_from_slice(data),
            png::ColorType::Rgb => {
                for chunk in data.as_chunks::<3>().0 {
                    rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
                }
            }
            png::ColorType::Grayscale => {
                for &g in data {
                    rgba.extend_from_slice(&[g, g, g, 255]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for chunk in data.as_chunks::<2>().0 {
                    rgba.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
                }
            }
            _ => return None,
        }
        return Some((info.width, info.height, rgba));
    }
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let pixels = dec.decode().ok()?;
    let info = dec.info()?;
    let w = info.width as u32;
    let h = info.height as u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            for chunk in pixels.chunks_exact(3) {
                rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
        }
        jpeg_decoder::PixelFormat::L8 => {
            for &g in &pixels {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        _ => return None,
    }
    Some((w, h, rgba))
}

/// Build PBR Texture2DArray on CPU from optional fetched layer bytes (single-threaded, wasm32-safe).
#[cfg(target_arch = "wasm32")]
pub fn prepare_textures_3d_from_bytes(
    custom: &std::collections::BTreeMap<u32, (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)>,
    max_size: u32,
) -> PreparedPbrTextures {
    let target_size: u32 = if custom.is_empty() {
        TEX_SIZE.min(max_size)
    } else {
        1024u32.min(max_size)
    };
    let target_mips: u32 = target_size.trailing_zeros() + 1;
    let mut diffs = Vec::with_capacity(TEX_LAYERS as usize);
    let mut norms = Vec::with_capacity(TEX_LAYERS as usize);
    let mut loaded_count = 0u32;

    for layer in 0..TEX_LAYERS {
        let pair = if let Some((diff_b, norm_b, rough_b)) = custom.get(&layer) {
            if let Some((dw, dh, d_rgba)) = decode_image_bytes(diff_b) {
                loaded_count += 1;
                let diff_resized = resize_rgba_bilinear(&d_rgba, dw, dh, target_size, target_size);
                let mut norm_resized = norm_b
                    .as_deref()
                    .and_then(decode_image_bytes)
                    .map(|(nw, nh, n_rgba)| {
                        resize_rgba_bilinear(&n_rgba, nw, nh, target_size, target_size)
                    })
                    .unwrap_or_else(|| {
                        let mut flat = vec![0u8; (target_size * target_size * 4) as usize];
                        for px in flat.chunks_exact_mut(4) {
                            px[0] = 128;
                            px[1] = 128;
                            px[2] = 255;
                            px[3] = 190;
                        }
                        flat
                    });
                if let Some((rw, rh, r_rgba)) = rough_b.as_deref().and_then(decode_image_bytes) {
                    let rough_resized =
                        resize_rgba_bilinear(&r_rgba, rw, rh, target_size, target_size);
                    for (n_px, r_px) in
                        norm_resized.chunks_exact_mut(4).zip(rough_resized.chunks_exact(4))
                    {
                        n_px[3] = r_px[0];
                    }
                }
                (diff_resized, norm_resized)
            } else {
                let data = build_layer(layer);
                (
                    resize_rgba_bilinear(&data.diffuse, TEX_SIZE, TEX_SIZE, target_size, target_size),
                    resize_rgba_bilinear(&data.normal, TEX_SIZE, TEX_SIZE, target_size, target_size),
                )
            }
        } else {
            let data = build_layer(layer);
            (
                resize_rgba_bilinear(&data.diffuse, TEX_SIZE, TEX_SIZE, target_size, target_size),
                resize_rgba_bilinear(&data.normal, TEX_SIZE, TEX_SIZE, target_size, target_size),
            )
        };
        diffs.push(pair.0);
        norms.push(pair.1);
    }

    let diff_packed = pack_mip_chain_single(target_size, target_mips, &diffs, false);
    let norm_packed = pack_mip_chain_single(target_size, target_mips, &norms, true);
    PreparedPbrTextures {
        tex_size: target_size,
        tex_mips: target_mips,
        diff_packed,
        norm_packed,
        loaded_count,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn load_textures_json_map(textures_path: Option<&std::path::Path>) -> std::collections::HashMap<u32, String> {
    let mut out = std::collections::HashMap::new();
    let Some(p) = textures_path else { return out; };
    let json_path = if p.is_file() {
        p.to_path_buf()
    } else {
        p.parent().map(|par| par.join("textures.json")).unwrap_or_else(|| p.join("textures.json"))
    };
    let Ok(raw) = std::fs::read_to_string(&json_path) else { return out; };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else { return out; };
    let Some(obj) = val.as_object() else { return out; };
    for (k, v) in obj {
        let Ok(slot) = k.parse::<u32>() else { continue; };
        if slot >= TEX_LAYERS { continue; }
        let Some(id_raw) = v.as_str() else { continue; };
        let id = id_raw.trim().to_ascii_lowercase();
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            out.insert(slot, id);
        }
    }
    out
}

#[cfg(not(target_arch = "wasm32"))]
fn ensure_cached_hub_texture(tex_id: &str, kind: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = std::path::PathBuf::from(&home)
        .join(".cache")
        .join("infimum-web")
        .join("textures")
        .join(tex_id);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{kind}.jpg"));
    if path.is_file() && std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 0 {
        return Some(path);
    }
    let url = format!("https://infimum-lang.org/textures/{tex_id}/{kind}.jpg");
    let tmp = dir.join(format!("{kind}.jpg.tmp"));
    let ok = std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "20", "-o", &tmp.display().to_string(), &url])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok && std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) > 0 {
        let _ = std::fs::rename(&tmp, &path);
        return Some(path);
    }
    let _ = std::fs::remove_file(&tmp);
    None
}

#[cfg(not(target_arch = "wasm32"))]
pub fn prepare_textures_3d_cpu(
    textures_dir: Option<&std::path::Path>,
    progress: &std::sync::atomic::AtomicU32,
) -> PreparedPbrTextures {
    use std::sync::atomic::{AtomicU32, Ordering};
    progress.store(5, Ordering::Relaxed);

    let local_dir = textures_dir.filter(|p| p.is_dir());
    let hub_map = load_textures_json_map(textures_dir);
    let has_external_pack = !hub_map.is_empty()
        || local_dir
            .map(|d| (0..TEX_LAYERS).any(|l| find_layer_file(d, l, "_diff").is_some()))
            .unwrap_or(false);
    let (target_size, target_mips) = if has_external_pack {
        (1024u32, 11u32)
    } else {
        (TEX_SIZE, TEX_MIPS)
    };

    let loaded_count = AtomicU32::new(0);
    let done_layers = AtomicU32::new(0);
    let results: Vec<(Vec<u8>, Vec<u8>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..TEX_LAYERS)
            .map(|layer| {
                let lc = &loaded_count;
                let dl = &done_layers;
                let hub_id = hub_map.get(&layer).cloned();
                s.spawn(move || {
                    let resolve_map = |suffix: &str, kind: &str| -> Option<std::path::PathBuf> {
                        if let Some(p) = local_dir.and_then(|d| find_layer_file(d, layer, suffix)) {
                            return Some(p);
                        }
                        hub_id.as_deref().and_then(|id| ensure_cached_hub_texture(id, kind))
                    };
                    let ext_diff = resolve_map("_diff", "diff").and_then(|p| decode_image_rgba(&p));
                    let pair = if let Some((dw, dh, d_rgba)) = ext_diff {
                        lc.fetch_add(1, Ordering::Relaxed);
                        let diff_resized =
                            resize_rgba_bilinear(&d_rgba, dw, dh, target_size, target_size);
                        let mut norm_resized = resolve_map("_norm", "norm")
                            .and_then(|p| decode_image_rgba(&p))
                            .map(|(nw, nh, n_rgba)| {
                                resize_rgba_bilinear(&n_rgba, nw, nh, target_size, target_size)
                            })
                            .unwrap_or_else(|| {
                                let mut flat = vec![0u8; (target_size * target_size * 4) as usize];
                                for px in flat.as_chunks_mut::<4>().0 {
                                    px[0] = 128;
                                    px[1] = 128;
                                    px[2] = 255;
                                    px[3] = 190;
                                }
                                flat
                            });
                        if let Some((rw, rh, r_rgba)) = resolve_map("_rough", "rough")
                            .and_then(|p| decode_image_rgba(&p))
                        {
                            let rough_resized =
                                resize_rgba_bilinear(&r_rgba, rw, rh, target_size, target_size);
                            for (n_px, r_px) in
                                norm_resized.as_chunks_mut::<4>().0.iter_mut().zip(rough_resized.as_chunks::<4>().0)
                            {
                                n_px[3] = r_px[0];
                            }
                        }
                        (diff_resized, norm_resized)
                    } else {
                        let data = build_layer(layer);
                        (
                            resize_rgba_bilinear(
                                &data.diffuse,
                                TEX_SIZE,
                                TEX_SIZE,
                                target_size,
                                target_size,
                            ),
                            resize_rgba_bilinear(
                                &data.normal,
                                TEX_SIZE,
                                TEX_SIZE,
                                target_size,
                                target_size,
                            ),
                        )
                    };
                    let d = dl.fetch_add(1, Ordering::Relaxed) + 1;
                    // Layers decode phase accounts for 5..70%
                    let pct = 5 + (d * 65) / TEX_LAYERS;
                    progress.store(pct.min(72), Ordering::Relaxed);
                    pair
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut diffs = Vec::with_capacity(TEX_LAYERS as usize);
    let mut norms = Vec::with_capacity(TEX_LAYERS as usize);
    for (d, n) in results {
        diffs.push(d);
        norms.push(n);
    }

    let diff_packed = pack_mip_chain_parallel(target_size, target_mips, &diffs, false);
    progress.store(86, Ordering::Relaxed);
    let norm_packed = pack_mip_chain_parallel(target_size, target_mips, &norms, true);
    progress.store(100, Ordering::Relaxed);

    PreparedPbrTextures {
        tex_size: target_size,
        tex_mips: target_mips,
        diff_packed,
        norm_packed,
        loaded_count: loaded_count.load(Ordering::Relaxed),
    }
}

pub fn upload_prepared_textures_3d(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    prep: PreparedPbrTextures,
) -> MeshTextures3D {
    if prep.loaded_count > 0 {
        eprintln!(
            "infimum-web PBR | Loaded {} external 1K photo-PBR materials ({}x{}, {} mips)",
            prep.loaded_count, prep.tex_size, prep.tex_size, prep.tex_mips
        );
    }
    let diffuse_view = upload_packed_texture_array(
        device,
        queue,
        "infimum-mesh3d-diffuse-arr",
        prep.tex_size,
        prep.tex_mips,
        &prep.diff_packed,
    );
    let normal_view = upload_packed_texture_array(
        device,
        queue,
        "infimum-mesh3d-normal-arr",
        prep.tex_size,
        prep.tex_mips,
        &prep.norm_packed,
    );
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("infimum-mesh3d-sampler"),
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Linear,
        #[cfg(not(target_arch = "wasm32"))]
        anisotropy_clamp: 16,
        #[cfg(target_arch = "wasm32")]
        anisotropy_clamp: 1,
        ..Default::default()
    });
    MeshTextures3D {
        diffuse_view,
        normal_view,
        sampler,
    }
}
