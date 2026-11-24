// Infimum Web — Unified 1-Draw-Call Instanced SDF + PBR 3D + HiDPI Unicode Glyph WGSL Shader
// Supports:
//   kind 0: SDF Rounded Rectangle (fill, border, soft drop shadow)
//   kind 1: HiDPI Unicode Glyph (3 styles: Regular, Bold, Mono; ASCII + Cyrillic + ₪)
//   kind 2: Anti-aliased Line Segment
//   kind 3: Anti-aliased Circle / Ring
//   kind 4: Rounded Textured Image Quad
//   kind 5: Gradient Rounded Rectangle (vertical, horizontal, diagonal, radial + shadow)
//   kind 6: 3D PBR Raymarched Sphere (glass crystal, metallic, ceramic/marble + caustics + shadow)
//   kind 7: Soft Radial Glow / Light Halo
//   kind 8: Anti-aliased Shaded 3D Triangle (packed s16 vertices in w0..w2)
//   kind 9: Hardware Z-Buffered Perspective-Textured 3D Triangle (10 procedural materials + fog)
// Plus 16 hardware clip rectangles (`clips[clip_id]`).

struct Uniforms {
    viewport: vec2<f32>,
    legacy_ascii: u32,
    time_sec: f32,
    clips: array<vec4<f32>, 16>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;
@group(0) @binding(3) var img_tex: texture_2d<f32>;

struct PrimIn {
    @location(0) w0123: vec4<i32>,
    @location(1) meta_cols_clip: vec4<u32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) px: vec2<f32>,
    @location(1) @interpolate(flat) rect: vec4<f32>,
    @location(2) @interpolate(flat) pmeta: u32,
    @location(3) @interpolate(flat) col1: vec4<f32>,
    @location(4) @interpolate(flat) col2: vec4<f32>,
    @location(5) @interpolate(flat) clip_id: u32,
    @location(6) @interpolate(flat) raw_w: vec4<i32>,
};

fn unpack_rgba(c: u32) -> vec4<f32> {
    let r = f32(c & 255u) / 255.0;
    let g = f32((c >> 8u) & 255u) / 255.0;
    let b = f32((c >> 16u) & 255u) / 255.0;
    let a = f32((c >> 24u) & 255u) / 255.0;
    return vec4<f32>(r, g, b, a);
}

fn unpack_s16_xy(packed: i32) -> vec2<f32> {
    let x = (packed << 16u) >> 16u;
    let y = packed >> 16u;
    return vec2<f32>(f32(x), f32(y));
}

@vertex
fn vs_main(@builtin(vertex_index) vid: u32, inst: PrimIn) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0)
    );
    let uv = corners[vid];
    let pmeta = inst.meta_cols_clip.x;
    let kind = pmeta & 15u;

    let f0 = f32(inst.w0123.x);
    let f1 = f32(inst.w0123.y);
    let f2 = f32(inst.w0123.z);
    let f3 = f32(inst.w0123.w);

    var bb_min = vec2<f32>(f0, f1);
    var bb_max = vec2<f32>(f0 + f2, f1 + f3);

    if (kind == 0u || kind == 5u) {
        let shadow = f32((pmeta >> 16u) & 255u);
        let pad = shadow + 2.0;
        bb_min = vec2<f32>(f0 - pad, f1 - pad);
        bb_max = vec2<f32>(f0 + f2 + pad, f1 + f3 + pad + shadow * 0.45);
    } else if (kind == 1u) {
        // Cell is 64px for 48px em-height (`64/48 = 1.3333`); expand quad so `size` = true em-height!
        let cell_sz = f2 * 1.333333;
        bb_min = vec2<f32>(f0 - f2 * 0.0625, f1 - f2 * 0.12);
        bb_max = vec2<f32>(bb_min.x + cell_sz, bb_min.y + cell_sz);
    } else if (kind == 2u) {
        let half_th = f32((pmeta >> 4u) & 255u) * 0.5 + 2.0;
        bb_min = vec2<f32>(min(f0, f2) - half_th, min(f1, f3) - half_th);
        bb_max = vec2<f32>(max(f0, f2) + half_th, max(f1, f3) + half_th);
    } else if (kind == 3u || kind == 7u) {
        let r = f2 + 2.0;
        bb_min = vec2<f32>(f0 - r, f1 - r);
        bb_max = vec2<f32>(f0 + r, f1 + r);
    } else if (kind == 6u) {
        let shadow = f32((pmeta >> 16u) & 255u);
        let pad = f2 + shadow + 4.0;
        bb_min = vec2<f32>(f0 - pad, f1 - pad);
        bb_max = vec2<f32>(f0 + pad, f1 + pad + shadow * 0.5);
    } else if (kind == 8u || kind == 9u) {
        let p0 = unpack_s16_xy(inst.w0123.x);
        let p1 = unpack_s16_xy(inst.w0123.y);
        let p2 = unpack_s16_xy(inst.w0123.z);
        bb_min = min(p0, min(p1, p2)) - vec2<f32>(2.0, 2.0);
        bb_max = max(p0, max(p1, p2)) + vec2<f32>(2.0, 2.0);
    }

    let px = mix(bb_min, bb_max, uv);
    let ndc = vec2<f32>(
        (px.x / u.viewport.x) * 2.0 - 1.0,
        1.0 - (px.y / u.viewport.y) * 2.0
    );

    var out: VsOut;
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.px = px;
    out.rect = vec4<f32>(f0, f1, f2, f3);
    out.pmeta = pmeta;
    out.col1 = unpack_rgba(inst.meta_cols_clip.y);
    out.col2 = unpack_rgba(inst.meta_cols_clip.z);
    out.clip_id = inst.meta_cols_clip.w & 255u;
    out.raw_w = inst.w0123;
    return out;
}

fn sd_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

fn sd_segment(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let denom = max(dot(ba, ba), 0.0001);
    let h = clamp(dot(pa, ba) / denom, 0.0, 1.0);
    return length(pa - ba * h);
}

fn sd_triangle(p: vec2<f32>, p0: vec2<f32>, p1: vec2<f32>, p2: vec2<f32>) -> f32 {
    let e0 = p1 - p0;
    let e1 = p2 - p1;
    let e2 = p0 - p2;
    let v0 = p - p0;
    let v1 = p - p1;
    let v2 = p - p2;
    let pq0 = v0 - e0 * clamp(dot(v0, e0) / max(dot(e0, e0), 0.0001), 0.0, 1.0);
    let pq1 = v1 - e1 * clamp(dot(v1, e1) / max(dot(e1, e1), 0.0001), 0.0, 1.0);
    let pq2 = v2 - e2 * clamp(dot(v2, e2) / max(dot(e2, e2), 0.0001), 0.0, 1.0);
    let s = sign(e0.x * e2.y - e0.y * e2.x);
    let d = min(
        min(
            vec2<f32>(dot(pq0, pq0), s * (v0.x * e0.y - v0.y * e0.x)),
            vec2<f32>(dot(pq1, pq1), s * (v1.x * e1.y - v1.y * e1.x))
        ),
        vec2<f32>(dot(pq2, pq2), s * (v2.x * e2.y - v2.y * e2.x))
    );
    return -sqrt(d.x) * sign(d.y);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let clip = u.clips[in.clip_id & 15u];
    if (in.px.x < clip.x || in.px.y < clip.y || in.px.x > clip.z || in.px.y > clip.w) {
        discard;
    }

    let kind = in.pmeta & 15u;

    if (kind == 0u) {
        // SDF Rounded Rectangle + Border + Soft Drop Shadow
        let half_sz = in.rect.zw * 0.5;
        let center = in.rect.xy + half_sz;
        let radius = min(f32((in.pmeta >> 4u) & 255u), min(half_sz.x, half_sz.y));
        let border_px = f32((in.pmeta >> 12u) & 15u);
        let shadow_px = f32((in.pmeta >> 16u) & 255u);

        let d = sd_rounded_box(in.px - center, half_sz, radius);
        let fill_a = 1.0 - smoothstep(-0.6, 0.6, d);

        var col = in.col1;
        if (border_px > 0.0 && in.col2.a > 0.0) {
            let inner = smoothstep(-border_px - 0.5, -border_px + 0.5, d);
            col = mix(in.col1, in.col2, inner);
        }
        var out_col = vec4<f32>(col.rgb, col.a * fill_a);

        if (shadow_px > 0.0 && d > 0.0) {
            let d_sh = sd_rounded_box(in.px - (center + vec2<f32>(0.0, shadow_px * 0.25)), half_sz, radius);
            let sh_a = clamp(1.0 - d_sh / shadow_px, 0.0, 1.0);
            let shadow_col = vec4<f32>(0.0, 0.0, 0.0, sh_a * sh_a * 0.32);
            out_col = mix(shadow_col, out_col, out_col.a);
        }

        if (out_col.a < 0.004) {
            discard;
        }
        return out_col;
    }

    if (kind == 1u) {
        // HiDPI Unicode Glyph from 16 x 33 grid atlas (3 styles x 11 rows, 64x64 cell / 48px em)
        var raw_idx = (in.pmeta >> 4u) & 255u;
        if (u.legacy_ascii != 0u && raw_idx >= 32u) {
            raw_idx = raw_idx - 32u;
        }
        let style = min((in.pmeta >> 12u) & 3u, 2u);
        let cell_sz = max(in.rect.z * 1.333333, 1.0);
        let origin = vec2<f32>(in.rect.x - in.rect.z * 0.0625, in.rect.y - in.rect.z * 0.12);
        let local_uv = (in.px - origin) / vec2<f32>(cell_sz, cell_sz);
        if (local_uv.x < 0.0 || local_uv.x > 1.0 || local_uv.y < 0.0 || local_uv.y > 1.0) {
            discard;
        }
        let col_i = f32(raw_idx % 16u);
        let row_i = f32(style * 11u + (raw_idx / 16u));
        let atlas_uv = vec2<f32>(
            (col_i + local_uv.x) / 16.0,
            (row_i + local_uv.y) / 33.0
        );
        let raw_alpha = textureSample(atlas_tex, atlas_smp, atlas_uv).r;
        if (raw_alpha < 0.03) {
            discard;
        }
        // Crisp gamma-corrected alpha coverage for razor-sharp UI typography
        let alpha = pow(raw_alpha, 0.72);
        return vec4<f32>(in.col1.rgb, in.col1.a * alpha);
    }

    if (kind == 2u) {
        // Anti-aliased Line Segment
        let half_th = f32((in.pmeta >> 4u) & 255u) * 0.5;
        let d = sd_segment(in.px, in.rect.xy, in.rect.zw) - half_th;
        let a = 1.0 - smoothstep(-0.6, 0.6, d);
        if (a * in.col1.a < 0.004) {
            discard;
        }
        return vec4<f32>(in.col1.rgb, in.col1.a * a);
    }

    if (kind == 3u) {
        // Anti-aliased Circle / Ring
        let dist = length(in.px - in.rect.xy);
        let r_out = in.rect.z;
        let r_in = in.rect.w;
        var a = 1.0 - smoothstep(r_out - 0.6, r_out + 0.6, dist);
        if (r_in > 0.0) {
            a = a * smoothstep(r_in - 0.6, r_in + 0.6, dist);
        }
        if (a * in.col1.a < 0.004) {
            discard;
        }
        return vec4<f32>(in.col1.rgb, in.col1.a * a);
    }

    if (kind == 4u) {
        // Rounded Textured Image Quad (pasted screenshots / icons)
        let half_sz = in.rect.zw * 0.5;
        let center = in.rect.xy + half_sz;
        let radius = min(f32((in.pmeta >> 4u) & 255u), min(half_sz.x, half_sz.y));
        let d = sd_rounded_box(in.px - center, half_sz, radius);
        let mask_a = 1.0 - smoothstep(-0.6, 0.6, d);
        let uv = (in.px - in.rect.xy) / max(in.rect.zw, vec2<f32>(1.0, 1.0));
        if (mask_a < 0.004 || uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
            discard;
        }
        let slot = (in.pmeta >> 12u) & 15u;
        let qx = f32(slot & 3u) * 0.25;
        let qy = f32((slot >> 2u) & 3u) * 0.25;
        let uv_atlas = vec2<f32>(
            qx + clamp(uv.x, 0.002, 0.998) * 0.25,
            qy + clamp(uv.y, 0.002, 0.998) * 0.25
        );
        let tex_col = textureSample(img_tex, atlas_smp, uv_atlas);
        return vec4<f32>(tex_col.rgb * in.col1.rgb, tex_col.a * in.col1.a * mask_a);
    }

    if (kind == 5u) {
        // Gradient SDF Rounded Rectangle + Soft Drop Shadow
        let half_sz = in.rect.zw * 0.5;
        let center = in.rect.xy + half_sz;
        let radius = min(f32((in.pmeta >> 4u) & 255u), min(half_sz.x, half_sz.y));
        let mode = (in.pmeta >> 12u) & 15u;
        let shadow_px = f32((in.pmeta >> 16u) & 255u);

        let d = sd_rounded_box(in.px - center, half_sz, radius);
        let fill_a = 1.0 - smoothstep(-0.6, 0.6, d);
        let uv = clamp((in.px - in.rect.xy) / max(in.rect.zw, vec2<f32>(1.0, 1.0)), vec2<f32>(0.0), vec2<f32>(1.0));

        var t = uv.y;
        if (mode == 1u) {
            t = uv.x;
        } else if (mode == 2u) {
            t = clamp((uv.x + uv.y) * 0.5, 0.0, 1.0);
        } else if (mode == 3u) {
            t = clamp(length(uv - vec2<f32>(0.5, 0.5)) * 1.4142, 0.0, 1.0);
        }
        let col = mix(in.col1, in.col2, t);
        var out_col = vec4<f32>(col.rgb, col.a * fill_a);

        if (shadow_px > 0.0 && d > 0.0) {
            let d_sh = sd_rounded_box(in.px - (center + vec2<f32>(0.0, shadow_px * 0.25)), half_sz, radius);
            let sh_a = clamp(1.0 - d_sh / shadow_px, 0.0, 1.0);
            let shadow_col = vec4<f32>(0.0, 0.0, 0.0, sh_a * sh_a * 0.34);
            out_col = mix(shadow_col, out_col, out_col.a);
        }
        if (out_col.a < 0.004) {
            discard;
        }
        return out_col;
    }

    if (kind == 6u) {
        // 3D PBR Raymarched Sphere (Glass Crystal / Metallic / Marble + Caustics + Contact Shadow)
        let center = in.rect.xy;
        let r = max(in.rect.z, 1.0);
        let light_deg = in.rect.w;
        let material = (in.pmeta >> 4u) & 15u;
        let rough_u = (in.pmeta >> 8u) & 15u;
        let shadow_px = f32((in.pmeta >> 16u) & 255u);

        let rel = (in.px - center) / r;
        let dist_sq = dot(rel, rel);
        let dist = sqrt(dist_sq);
        let edge_a = 1.0 - smoothstep(1.0 - 1.2 / r, 1.0 + 0.6 / r, dist);

        // Soft cast contact shadow below-right of sphere
        var out_col = vec4<f32>(0.0, 0.0, 0.0, 0.0);
        if (shadow_px > 0.0 && edge_a < 1.0) {
            let sh_center = center + vec2<f32>(r * 0.14, r * 0.24);
            let sh_rel = (in.px - sh_center) / vec2<f32>(r * 1.05, r * 0.85);
            let sh_d = length(sh_rel);
            let sh_a = clamp(1.0 - max(sh_d - 0.72, 0.0) / max(shadow_px / r + 0.35, 0.2), 0.0, 1.0);
            out_col = vec4<f32>(0.0, 0.0, 0.0, sh_a * sh_a * 0.42 * in.col1.a);
        }

        if (edge_a > 0.004) {
            let nz = sqrt(max(1.0 - min(dist_sq, 1.0), 0.0));
            let n = normalize(vec3<f32>(rel.x, -rel.y, nz));
            let v = vec3<f32>(0.0, 0.0, 1.0);

            // Key light direction (default top-left-front; rotatable via light_deg)
            let ang = (light_deg - 42.0) * 0.0174533;
            let l = normalize(vec3<f32>(-cos(ang) * 0.62, sin(ang) * 0.62 + 0.45, 0.72));
            let h = normalize(l + v);

            let ndl = max(dot(n, l), 0.0);
            let ndh = max(dot(n, h), 0.0);
            let ndv = max(dot(n, v), 0.0);
            let fresnel = pow(1.0 - ndv, 3.0);

            let base = in.col1.rgb;
            let rim_tint = select(mix(base, vec3<f32>(1.0, 1.0, 1.0), 0.55), in.col2.rgb, in.col2.a > 0.01);
            let rough = clamp(f32(rough_u) / 15.0, 0.08, 1.0);
            let shininess = mix(96.0, 12.0, rough);

            var rgb = base * (0.24 + 0.78 * ndl);

            if (material == 0u) {
                // Crystal / Glass Jewel: internal bottom-right refraction caustic + crisp specular + Fresnel rim
                let caustic = pow(max(dot(n, normalize(vec3<f32>(0.48, -0.58, 0.66))), 0.0), 3.0);
                let spec = pow(ndh, shininess) * 1.15;
                let spec_broad = pow(ndh, 14.0) * 0.25;
                rgb = base * (0.18 + 0.62 * ndl)
                    + mix(base, vec3<f32>(1.0, 1.0, 1.0), 0.45) * caustic * 0.58
                    + rim_tint * fresnel * 0.48
                    + vec3<f32>(1.0, 1.0, 1.0) * (spec + spec_broad);
            } else if (material == 1u) {
                // Metallic / Chrome PBR: reflective horizon band + tinted specular
                let refl_y = reflect(-v, n).y;
                let env_band = mix(0.25, 1.15, smoothstep(-0.35, 0.45, refl_y));
                let spec = pow(ndh, shininess * 0.8) * 1.3;
                rgb = base * (0.22 + 0.68 * ndl) * env_band
                    + mix(base, vec3<f32>(1.0, 1.0, 1.0), 0.65) * spec
                    + rim_tint * fresnel * 0.62;
            } else {
                // Ceramic / Marble: subtle procedural vein pattern + soft sheen
                let vein = sin((rel.x * 5.2 + rel.y * 3.7 + sin(rel.y * 6.0) * 0.7) * 3.14159) * 0.5 + 0.5;
                let marbled = mix(base * 0.84, mix(base, vec3<f32>(1.0, 1.0, 1.0), 0.28), pow(vein, 3.0));
                let spec = pow(ndh, 32.0) * 0.45;
                rgb = marbled * (0.26 + 0.76 * ndl)
                    + rim_tint * fresnel * 0.28
                    + vec3<f32>(1.0, 1.0, 1.0) * spec;
            }

            let sphere_col = vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), in.col1.a * edge_a);
            out_col = mix(out_col, sphere_col, sphere_col.a);
        }

        if (out_col.a < 0.004) {
            discard;
        }
        return out_col;
    }

    if (kind == 7u) {
        // Soft Radial Glow / Light Halo
        let dist = length(in.px - in.rect.xy) / max(in.rect.z, 1.0);
        if (dist >= 1.0) {
            discard;
        }
        let pwr = clamp(in.rect.w * 0.1, 0.8, 6.0);
        let t = pow(clamp(1.0 - dist, 0.0, 1.0), pwr);
        let outer = select(vec4<f32>(in.col1.rgb, 0.0), in.col2, in.col2.a > 0.01);
        let col = mix(outer, in.col1, t);
        if (col.a < 0.004) {
            discard;
        }
        return col;
    }

    if (kind == 8u) {
        // Anti-aliased Shaded 3D Triangle (packed s16 vertices in raw_w.xyz)
        let p0 = unpack_s16_xy(in.raw_w.x);
        let p1 = unpack_s16_xy(in.raw_w.y);
        let p2 = unpack_s16_xy(in.raw_w.z);
        let d = sd_triangle(in.px, p0, p1, p2);
        let a = 1.0 - smoothstep(-0.6, 0.6, d);
        if (a * in.col1.a < 0.004) {
            discard;
        }
        var rgb = in.col1.rgb;
        if (in.col2.a > 0.01) {
            // Smooth vertical/edge shading between col1 (p0) and col2 (p1/p2)
            let denom = max(length(p1 - p0) + length(p2 - p0), 1.0);
            let t = clamp(length(in.px - p0) / (denom * 0.5), 0.0, 1.0);
            rgb = mix(in.col1.rgb, in.col2.rgb, t);
        }
        let lit = f32(in.raw_w.w);
        if (lit > 0.0) {
            rgb = clamp(rgb * (lit / 1000.0), vec3<f32>(0.0), vec3<f32>(1.0));
        }
        return vec4<f32>(rgb, in.col1.a * a);
    }

    discard;
}

// --- Hardware Z-Buffered 3D Procedural Materials (kind == 9u) ---

fn hash21(p: vec2<f32>) -> f32 {
    let q = fract(p * vec2<f32>(123.34, 456.21));
    let r = q + dot(q, q + 45.32);
    return fract(r.x * r.y);
}

fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash21(i);
    let b = hash21(i + vec2<f32>(1.0, 0.0));
    let c = hash21(i + vec2<f32>(0.0, 1.0));
    let d = hash21(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn fbm3(p: vec2<f32>) -> f32 {
    var v = 0.0;
    var amp = 0.5;
    var q = p;
    v += amp * vnoise(q); q *= 2.03; amp *= 0.5;
    v += amp * vnoise(q); q *= 2.07; amp *= 0.5;
    v += amp * vnoise(q);
    return v;
}

fn sample_material_3d(mat_id: u32, uv: vec2<f32>, base: vec3<f32>, t: f32) -> vec4<f32> {
    // Returns (rgb, emissive_factor 0..1)
    if (mat_id == 1u) {
        // Gothic brick wall / steps with dark mortar, per-brick shade variation, grain & moss
        let rows = 8.0;
        let cols = 4.0;
        let row = floor(uv.y * rows);
        let shift = (row % 2.0) * 0.5;
        let bx = uv.x * cols + shift;
        let by = uv.y * rows;
        let cell = vec2<f32>(floor(bx), row);
        let f = vec2<f32>(fract(bx), fract(by));
        let mortar = step(0.06, f.x) * step(f.x, 0.94) * step(0.10, f.y) * step(f.y, 0.90);
        let rnd = hash21(cell);
        let grain = vnoise(uv * 28.0);
        let bevel = smoothstep(0.06, 0.16, f.x) * smoothstep(0.94, 0.84, f.x)
                  * smoothstep(0.10, 0.22, f.y) * smoothstep(0.90, 0.78, f.y);
        var brick_col = base * (0.72 + 0.36 * rnd) * (0.86 + 0.24 * grain) * (0.78 + 0.26 * bevel);
        // Subtle green moss patches in crevices
        let moss = smoothstep(0.66, 0.84, fbm3(uv * 3.5 + vec2<f32>(1.7, 3.1)));
        brick_col = mix(brick_col, vec3<f32>(0.14, 0.24, 0.11), moss * 0.45);
        let mortar_col = base * 0.24;
        return vec4<f32>(mix(mortar_col, brick_col, mortar), 0.0);
    }
    if (mat_id == 2u) {
        // Weathered temple stone pillar with dark bands & rust-brown patches
        let band = smoothstep(0.03, 0.07, abs(fract(uv.y * 4.0) - 0.5));
        let weathering = smoothstep(0.46, 0.72, fbm3(vec2<f32>(uv.x * 5.0, uv.y * 7.0)));
        let grain = vnoise(uv * 32.0);
        let rust_col = vec3<f32>(0.48, 0.23, 0.14);
        var col = mix(base, rust_col, weathering * 0.65) * (0.86 + 0.22 * grain);
        col = mix(base * 0.25, col, band);
        return vec4<f32>(col, 0.0);
    }
    if (mat_id == 3u) {
        // Brushed dark ribbed techno-metal (weapon barrels, frames, crates)
        let rib = sin(uv.y * 50.0) * 0.5 + 0.5;
        let vent = step(0.18, fract(uv.x * 8.0)) * step(0.22, fract(uv.y * 12.0));
        let brush = vnoise(vec2<f32>(uv.x * 48.0, uv.y * 6.0));
        let col = base * (0.65 + 0.28 * rib + 0.18 * brush) * mix(0.62, 1.0, vent);
        return vec4<f32>(col, 0.0);
    }
    if (mat_id == 4u) {
        // Animated Slipgate portal vortex (purple-gold-white swirling rings, emissive)
        let c = uv - vec2<f32>(0.5, 0.5);
        let r = length(c) * 2.0;
        let ang = atan2(c.y, c.x);
        let swirl = sin(r * 13.0 - t * 4.2 + ang * 2.0 + sin(r * 6.0 - t * 2.0) * 1.4);
        let s = swirl * 0.5 + 0.5;
        let purple = vec3<f32>(0.36, 0.16, 0.88);
        let gold = vec3<f32>(1.0, 0.86, 0.28);
        let white = vec3<f32>(1.0, 0.98, 0.92);
        var col = mix(purple, gold, smoothstep(0.22, 0.72, s));
        col = mix(col, white, smoothstep(0.75, 0.96, s));
        return vec4<f32>(col, 1.0);
    }
    if (mat_id == 5u) {
        // Animated glowing lava / magma (emissive)
        let flow = fbm3(uv * 4.5 + vec2<f32>(t * 0.22, -t * 0.15));
        let hot = smoothstep(0.35, 0.78, flow);
        let col = mix(vec3<f32>(0.92, 0.22, 0.16), vec3<f32>(1.0, 0.62, 0.28), hot);
        return vec4<f32>(col, 1.0);
    }
    if (mat_id == 6u) {
        // Beveled stone floor / ceiling tiles
        let g = uv * 4.0;
        let cell = floor(g);
        let f = fract(g);
        let seam = step(0.05, f.x) * step(f.x, 0.95) * step(0.05, f.y) * step(f.y, 0.95);
        let rnd = hash21(cell);
        let grain = vnoise(uv * 24.0);
        let col = base * (0.76 + 0.28 * rnd) * (0.88 + 0.20 * grain);
        return vec4<f32>(mix(base * 0.25, col, seam), 0.0);
    }
    if (mat_id == 7u) {
        // Quake crimson stormy sky (emissive)
        let n1 = fbm3(uv * 3.2 + vec2<f32>(t * 0.04, t * 0.015));
        let n2 = fbm3(uv * 6.5 - vec2<f32>(t * 0.025, 0.0));
        let cloud = clamp(n1 * 0.7 + n2 * 0.45, 0.0, 1.0);
        let dark = base * 0.42;
        let bright = clamp(base * 1.45 + vec3<f32>(0.18, 0.04, 0.03), vec3<f32>(0.0), vec3<f32>(1.0));
        return vec4<f32>(mix(dark, bright, cloud), 1.0);
    }
    if (mat_id == 8u) {
        // Wooden planks / crates
        let py = uv.y * 6.0;
        let seam = step(0.07, fract(py)) * step(fract(py), 0.93);
        let grain = vnoise(vec2<f32>(uv.x * 10.0, uv.y * 42.0));
        let col = base * (0.78 + 0.30 * grain);
        return vec4<f32>(mix(base * 0.30, col, seam), 0.0);
    }
    if (mat_id == 9u) {
        // Emissive torch / lantern / energy glow
        let c = length(uv - vec2<f32>(0.5, 0.5));
        let core = smoothstep(0.65, 0.0, c);
        let col = mix(base, vec3<f32>(1.0, 0.96, 0.78), core * 0.65);
        return vec4<f32>(col, 1.0);
    }
    if (mat_id == 10u) {
        // Carved veined marble tile with beveled border and inlaid golden socket ring
        let f = fract(uv);
        let warp = fbm3(uv * 4.2 + vec2<f32>(1.3, 2.7));
        let vein = abs(sin((uv.x * 5.5 + uv.y * 3.8) + warp * 5.8));
        let vein_line = smoothstep(0.0, 0.16, vein);
        let fine = fbm3(uv * 14.0);
        let luma = dot(base, vec3<f32>(0.299, 0.587, 0.114));
        let vein_col = select(base * 1.48 + vec3<f32>(0.08, 0.07, 0.06), base * 0.72, luma > 0.45);
        var col = mix(vein_col, base, 0.35 + 0.65 * vein_line) * (0.90 + 0.16 * fine);

        // Subtle beveled tile seam
        let edge_x = min(f.x, 1.0 - f.x);
        let edge_y = min(f.y, 1.0 - f.y);
        let seam = smoothstep(0.015, 0.055, min(edge_x, edge_y));
        col = mix(base * 0.35, col, seam);

        // Carved circular socket depression + golden brass inlay ring
        let d = length(f - vec2<f32>(0.5, 0.5));
        let bowl = smoothstep(0.36, 0.10, d);
        col = col * (1.0 - 0.18 * bowl);
        let ring = smoothstep(0.024, 0.006, abs(d - 0.355));
        let brass = vec3<f32>(0.84, 0.70, 0.34) * (0.85 + 0.25 * fine);
        col = mix(col, brass, ring * 0.82);
        return vec4<f32>(col, 0.0);
    }
    if (mat_id == 11u) {
        // Burled walnut wood frame with warm satin varnish & brass accent grain
        let grain_n = fbm3(vec2<f32>(uv.x * 3.2, uv.y * 14.0));
        let rings = sin(uv.y * 26.0 + grain_n * 5.5) * 0.5 + 0.5;
        let burl = fbm3(uv * 9.0 + vec2<f32>(4.1, 1.9));
        let col = base * (0.68 + 0.34 * rings) * (0.84 + 0.24 * burl);
        return vec4<f32>(col, 0.0);
    }
    return vec4<f32>(base, 0.0);
}

struct FragOut3D {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
};

@fragment
fn fs_main_3d(in: VsOut) -> FragOut3D {
    let clip = u.clips[in.clip_id & 15u];
    if (in.px.x < clip.x || in.px.y < clip.y || in.px.x > clip.z || in.px.y > clip.w) {
        discard;
    }

    let p0 = unpack_s16_xy(in.raw_w.x);
    let p1 = unpack_s16_xy(in.raw_w.y);
    let p2 = unpack_s16_xy(in.raw_w.z);

    // Exact barycentric coordinates for crisp triangle rasterization & perspective UV
    let v0 = p1 - p0;
    let v1 = p2 - p0;
    let vp = in.px - p0;
    let area = v0.x * v1.y - v0.y * v1.x;
    if (abs(area) < 0.001) {
        discard;
    }
    let inv_area = 1.0 / area;
    let b1 = (vp.x * v1.y - vp.y * v1.x) * inv_area;
    let b2 = (v0.x * vp.y - v0.y * vp.x) * inv_area;
    let b0 = 1.0 - b1 - b2;
    if (b0 < -0.001 || b1 < -0.001 || b2 < -0.001) {
        discard;
    }

    // Unpack 10-bit vertex depths (1..1023) from raw_w.w
    let zw = bitcast<u32>(in.raw_w.w);
    let z0 = max(f32(zw & 1023u), 1.0);
    let z1 = max(f32((zw >> 10u) & 1023u), 1.0);
    let z2 = max(f32((zw >> 20u) & 1023u), 1.0);

    // Perspective-correct 1/z interpolation
    let w0 = b0 / z0;
    let w1 = b1 / z1;
    let w2 = b2 / z2;
    let inv_z = max(w0 + w1 + w2, 0.0001);
    let z_frag = 1.0 / inv_z;

    // Unpack UVs (0..255 => 0.0 .. 4.0 texture repeats per triangle)
    let u0 = f32((in.pmeta >> 16u) & 255u) / 64.0;
    let v0_uv = f32((in.pmeta >> 24u) & 255u) / 64.0;
    let c2 = u32(round(in.col2.r * 255.0))
           | (u32(round(in.col2.g * 255.0)) << 8u)
           | (u32(round(in.col2.b * 255.0)) << 16u)
           | (u32(round(in.col2.a * 255.0)) << 24u);
    let u1 = f32(c2 & 255u) / 64.0;
    let v1_uv = f32((c2 >> 8u) & 255u) / 64.0;
    let u2 = f32((c2 >> 16u) & 255u) / 64.0;
    let v2_uv = f32((c2 >> 24u) & 255u) / 64.0;

    let uv_persp = vec2<f32>(
        (w0 * u0 + w1 * u1 + w2 * u2) * z_frag,
        (w0 * v0_uv + w1 * v1_uv + w2 * v2_uv) * z_frag
    );

    let mat_id = (in.pmeta >> 4u) & 15u;
    let lit = max(f32((in.pmeta >> 8u) & 255u) / 255.0, 0.05);
    let fog_mode = (in.clip_id >> 4u) & 3u;

    // MAT_GLASS_SPHERE (12): Analytical ray-shaded 3D refractive glass jewel sphere
    if (mat_id == 12u) {
        let p = (uv_persp - vec2<f32>(0.5, 0.5)) * 2.0;
        let r2 = dot(p, p);
        if (r2 > 1.0) {
            discard;
        }
        let nz = sqrt(max(1.0 - r2, 0.0));
        let n = normalize(vec3<f32>(p.x, -p.y, nz));
        let v = vec3<f32>(0.0, 0.0, 1.0);
        let l1 = normalize(vec3<f32>(-0.42, 0.58, 0.70));
        let l2 = normalize(vec3<f32>(0.58, -0.32, 0.74));
        let h1 = normalize(l1 + v);
        let h2 = normalize(l2 + v);

        let ndl1 = max(dot(n, l1), 0.0);
        let ndl2 = max(dot(n, l2), 0.0);
        let fres = 0.06 + 0.94 * pow(1.0 - max(n.z, 0.0), 3.0);
        let caustic_dir = normalize(vec3<f32>(0.34, -0.50, 0.79));
        let caustic = pow(max(dot(n, caustic_dir), 0.0), 4.0);
        let spec1 = pow(max(dot(n, h1), 0.0), 72.0);
        let spec2 = pow(max(dot(n, h2), 0.0), 20.0);

        let base = in.col1.rgb;
        let deep = base * 0.28;
        var col = mix(deep, base * 1.08, 0.25 + 0.75 * ndl1);
        // Subsurface internal caustic glow exiting lower-right crystal
        col = col + (base * 0.75 + vec3<f32>(0.25, 0.22, 0.18)) * caustic * 0.62;
        // Cool studio rim + Schlick Fresnel reflection
        col = col + mix(base, vec3<f32>(0.92, 0.96, 1.0), 0.55) * fres * (0.48 + 0.35 * ndl2);
        // Crisp primary & soft secondary specular highlights
        col = col + vec3<f32>(1.0, 0.99, 0.96) * spec1 * 1.15;
        col = col + vec3<f32>(0.82, 0.90, 1.0) * spec2 * 0.28;
        col = col * (0.35 + 0.65 * lit);

        let aa = smoothstep(1.0, 0.93, r2);
        var out_s: FragOut3D;
        out_s.color = vec4<f32>(clamp(col, vec3<f32>(0.0), vec3<f32>(1.0)), aa * in.col1.a);
        out_s.depth = clamp((z_frag - nz * 12.0) / 1024.0, 0.0001, 0.9999);
        return out_s;
    }

    // MAT_SHADOW_CAUSTIC (13): Soft contact shadow + focused colored caustic glow on marble floor
    if (mat_id == 13u) {
        let p = (uv_persp - vec2<f32>(0.5, 0.5)) * 2.0;
        let r2 = dot(p, p);
        if (r2 > 1.0) {
            discard;
        }
        let sh = exp(-r2 * 4.2) * 0.62;
        let cp = p - vec2<f32>(0.18, 0.14);
        let ca = exp(-dot(cp, cp) * 9.5) * 0.55;
        let col = mix(vec3<f32>(0.02, 0.02, 0.03), in.col1.rgb * 1.25 + vec3<f32>(0.18), ca / max(sh + ca, 0.001));
        let alpha = clamp(sh + ca * 0.65, 0.0, 0.85) * in.col1.a * lit;
        var out_c: FragOut3D;
        out_c.color = vec4<f32>(clamp(col, vec3<f32>(0.0), vec3<f32>(1.0)), alpha);
        out_c.depth = clamp(z_frag / 1024.0, 0.0001, 0.9999);
        return out_c;
    }

    let mat_sample = sample_material_3d(mat_id, uv_persp, in.col1.rgb, u.time_sec);
    var rgb = mat_sample.rgb;
    let emissive = mat_sample.a;
    if (emissive < 0.5) {
        rgb = rgb * lit;
    }

    // Smooth atmospheric depth fog (skipped for emissive or fog_mode == 2 e.g. viewmodel gun)
    if (emissive < 0.5 && fog_mode != 2u) {
        let fog_t = clamp((z_frag - 120.0) / 560.0, 0.0, 0.86);
        let fog_col = select(vec3<f32>(0.05, 0.04, 0.06), vec3<f32>(0.24, 0.06, 0.06), fog_mode == 1u);
        rgb = mix(rgb, fog_col, fog_t);
    }

    var out: FragOut3D;
    out.color = vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), in.col1.a);
    out.depth = clamp(z_frag / 1024.0, 0.0001, 0.9999);
    return out;
}
