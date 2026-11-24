// Native Vulkan/wgpu 3D Mesh + PBR Texture2DArray + Normal Mapping + 24 Dynamic Point Lights Pipeline.

struct PointLight {
    pos_rad: vec4<f32>, // xyz = world meters, w = radius meters
    col_int: vec4<f32>, // rgb = color 0..1, w = intensity
};

struct Mesh3DUniforms {
    view_proj: mat4x4<f32>,
    proj_vm: mat4x4<f32>,
    cam_pos: vec4<f32>,   // xyz = world meters, w = time_sec
    cam_right: vec4<f32>, // xyz, w = aspect
    cam_up: vec4<f32>,    // xyz, w = tan_half_fov
    cam_fwd: vec4<f32>,   // xyz, w = light_count
    params: vec4<f32>,    // x = sky_mode, y = fog_mode, z = is_viewmodel, w = unused
    lights: array<PointLight, 24>,
};

@group(0) @binding(0)
var<uniform> u: Mesh3DUniforms;
@group(0) @binding(1)
var tex_diff: texture_2d_array<f32>;
@group(0) @binding(2)
var tex_norm: texture_2d_array<f32>;
@group(0) @binding(3)
var tex_samp: sampler;

fn hash21(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(127.1, 311.7))) * 43758.5453123);
}

fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let s = f * f * (3.0 - 2.0 * f);
    return mix(
        mix(hash21(i), hash21(i + vec2<f32>(1.0, 0.0)), s.x),
        mix(hash21(i + vec2<f32>(0.0, 1.0)), hash21(i + vec2<f32>(1.0, 1.0)), s.x),
        s.y
    );
}

fn fbm4(p_in: vec2<f32>) -> f32 {
    var p = p_in;
    var v = 0.0;
    var a = 0.5;
    for (var i = 0; i < 4; i = i + 1) {
        v = v + a * vnoise(p);
        p = p * 2.03 + vec2<f32>(1.7, 9.2);
        a = a * 0.5;
    }
    return v;
}

// --- Fullscreen Two-Layer Quake Storm Sky Pass ---

struct SkyVsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

@vertex
fn vs_sky(@builtin(vertex_index) vid: u32) -> SkyVsOut {
    var pts = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0)
    );
    var out: SkyVsOut;
    out.ndc = pts[vid];
    out.pos = vec4<f32>(pts[vid], 0.9999, 1.0);
    return out;
}

@fragment
fn fs_sky(in: SkyVsOut) -> @location(0) vec4<f32> {
    let dir = normalize(
        u.cam_fwd.xyz
        + in.ndc.x * u.cam_right.w * u.cam_up.w * u.cam_right.xyz
        + in.ndc.y * u.cam_up.w * u.cam_up.xyz
    );
    let up = clamp(dir.y, 0.0, 1.0);
    let t = u.cam_pos.w;
    let uv = dir.xz / (abs(dir.y) + 0.22);

    // Classic 2-layer scrolling Quake sky (blood-crimson lower layer + brooding dark-violet storm canopy)
    let layer1 = fbm4(uv * 1.15 + vec2<f32>(t * 0.045, t * 0.025));
    let layer2 = fbm4(uv * 2.35 - vec2<f32>(t * 0.085, -t * 0.04) + vec2<f32>(layer1 * 0.65, 0.0));

    // Mode 2: Golden African Savanna Sky (warm azure zenith, golden horizon, soft drifting clouds)
    if (u.params.x > 1.5) {
        let down = clamp(-dir.y, 0.0, 1.0);
        let horizon = vec3<f32>(0.98, 0.80, 0.56);
        let zenith = vec3<f32>(0.26, 0.56, 0.86);
        let desert_mist = vec3<f32>(0.82, 0.66, 0.44);
        var scol = mix(horizon, zenith, smoothstep(0.0, 0.65, up));
        scol = mix(scol, desert_mist, smoothstep(0.0, 0.45, down));
        let clouds = smoothstep(0.46, 0.80, layer1 * 0.65 + layer2 * 0.35) * smoothstep(0.02, 0.35, up);
        scol = mix(scol, vec3<f32>(1.0, 0.96, 0.88), clouds * 0.62);
        let sun_d = normalize(vec3<f32>(0.48, 0.42, -0.76));
        let sd = max(dot(dir, sun_d), 0.0);
        scol = scol + vec3<f32>(1.0, 0.88, 0.62) * pow(sd, 16.0) * 0.55;
        return vec4<f32>(clamp(scol, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    }

    let horizon = vec3<f32>(0.96, 0.34, 0.30);
    let crimson = vec3<f32>(0.76, 0.20, 0.28);
    let plum = vec3<f32>(0.36, 0.14, 0.28);
    let storm_black = vec3<f32>(0.07, 0.04, 0.08);

    var col = mix(horizon, crimson, smoothstep(0.0, 0.48, up));
    let wisps = smoothstep(0.32, 0.78, layer1) * smoothstep(0.02, 0.42, up);
    col = mix(col, plum, wisps * 0.72);
    let canopy = smoothstep(0.42, 0.80, layer2) * smoothstep(0.08, 0.58, up);
    col = mix(col, storm_black, canopy * 0.85);

    let sun_dir = normalize(vec3<f32>(0.48, 0.56, 0.66));
    let sun_dot = max(dot(dir, sun_dir), 0.0);
    col = col + vec3<f32>(1.0, 0.66, 0.42) * pow(sun_dot, 10.0) * 0.42;
    return vec4<f32>(clamp(col, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

// --- 3D Mesh World & Viewmodel Pass ---

struct MeshVsIn {
    @location(0) pos_mm: vec3<i32>,
    @location(1) norm_mat: u32,
    @location(2) uv_1024: vec2<i32>,
    @location(3) rgba: u32,
    @location(4) flags_ao: u32,
};

struct MeshVsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) world_pos: vec3<f32>,
    @location(1) world_n: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) col: vec4<f32>,
    @location(4) @interpolate(flat) mat_flags: u32,
    @location(5) ao: f32,
};

fn unpack_rgba8(c: u32) -> vec4<f32> {
    return vec4<f32>(
        f32(c & 255u) / 255.0,
        f32((c >> 8u) & 255u) / 255.0,
        f32((c >> 16u) & 255u) / 255.0,
        f32((c >> 24u) & 255u) / 255.0
    );
}

@vertex
fn vs_mesh(in: MeshVsIn) -> MeshVsOut {
    let pos_m = vec3<f32>(in.pos_mm) * 0.001;
    let nx = f32(in.norm_mat & 255u) - 128.0;
    let ny = f32((in.norm_mat >> 8u) & 255u) - 128.0;
    let nz = f32((in.norm_mat >> 16u) & 255u) - 128.0;
    let n_local = normalize(vec3<f32>(nx, ny, nz) + vec3<f32>(0.0, 0.0001, 0.0));
    let mat_id = (in.norm_mat >> 24u) & 15u;
    let emissive_bit = (in.flags_ao >> 8u) & 1u;

    var out: MeshVsOut;
    if (u.params.z > 0.5) {
        out.world_pos = u.cam_pos.xyz
            + u.cam_right.xyz * pos_m.x
            + u.cam_up.xyz * pos_m.y
            - u.cam_fwd.xyz * pos_m.z;
        out.world_n = normalize(
            u.cam_right.xyz * n_local.x
            + u.cam_up.xyz * n_local.y
            - u.cam_fwd.xyz * n_local.z
        );
        out.clip_pos = u.proj_vm * vec4<f32>(pos_m, 1.0);
    } else {
        out.world_pos = pos_m;
        out.world_n = n_local;
        out.clip_pos = u.view_proj * vec4<f32>(pos_m, 1.0);
    }
    out.uv = vec2<f32>(in.uv_1024) * (1.0 / 1024.0);
    out.col = unpack_rgba8(in.rgba);
    out.mat_flags = mat_id | (emissive_bit << 8u);
    out.ao = max(f32(in.flags_ao & 255u) / 255.0, 0.15);
    return out;
}

// Screen-space cotangent frame TBN (Mikkt-free, works at ANY distance including 1cm close-ups & viewmodels!)
fn perturb_normal(n_geo: vec3<f32>, world_pos: vec3<f32>, uv: vec2<f32>, map_n: vec3<f32>) -> vec3<f32> {
    let dp1 = dpdx(world_pos) * 1024.0;
    let dp2 = dpdy(world_pos) * 1024.0;
    let duv1 = dpdx(uv) * 1024.0;
    let duv2 = dpdy(uv) * 1024.0;

    let dp2perp = cross(dp2, n_geo);
    let dp1perp = cross(n_geo, dp1);
    let t = dp2perp * duv1.x + dp1perp * duv2.x;
    let b = dp2perp * duv1.y + dp1perp * duv2.y;
    let invmax = max(dot(t, t), dot(b, b));
    if (invmax < 1e-18) {
        return n_geo;
    }
    let scale = inverseSqrt(invmax);
    return normalize(t * (map_n.x * scale) + b * (map_n.y * scale) + n_geo * map_n.z);
}

@fragment
fn fs_mesh(in: MeshVsOut) -> @location(0) vec4<f32> {
    let mat_id = in.mat_flags & 15u;
    let force_emissive = (in.mat_flags >> 8u) & 1u;
    let t = u.cam_pos.w;

    // Sample 512x512 mipmapped PBR texture array with crisp negative LOD bias (-0.85)
    // so angled walls & floors stay razor-sharp (zero blurry mip transitions!).
    let layer = i32(min(mat_id, 11u));
    let tex_albedo = textureSampleBias(tex_diff, tex_samp, in.uv, layer, -0.85).rgb;
    let tex_nr = textureSampleBias(tex_norm, tex_samp, in.uv, layer, -0.85);
    let map_n = tex_nr.rgb * 2.0 - vec3<f32>(1.0);
    let roughness = clamp(tex_nr.a, 0.18, 0.96);

    // Animated emissive materials (Portal, Lava, Glow)
    if (mat_id == 4u) {
        let c = fract(in.uv) - vec2<f32>(0.5, 0.5);
        let r = length(c) * 2.0;
        let ang = atan2(c.y, c.x);
        let swirl = sin(r * 14.0 - t * 4.4 + ang * 2.0 + sin(r * 6.0 - t * 2.1) * 1.5) * 0.5 + 0.5;
        var col = mix(vec3<f32>(0.34, 0.14, 0.92), vec3<f32>(1.0, 0.84, 0.26), smoothstep(0.20, 0.72, swirl));
        col = mix(col, vec3<f32>(1.0, 0.98, 0.92), smoothstep(0.76, 0.96, swirl));
        return vec4<f32>(clamp(col, vec3<f32>(0.0), vec3<f32>(1.0)), in.col.a);
    }
    if (mat_id == 5u) {
        let flow = fbm4(in.uv * 3.2 + vec2<f32>(t * 0.18, -t * 0.11));
        let hot = smoothstep(0.28, 0.78, flow);
        let crust = mix(vec3<f32>(0.24, 0.06, 0.04), vec3<f32>(0.96, 0.24, 0.08), hot);
        let core = mix(crust, vec3<f32>(1.0, 0.78, 0.28), smoothstep(0.62, 0.92, flow));
        return vec4<f32>(core, in.col.a);
    }
    if (mat_id == 9u || force_emissive == 1u) {
        return vec4<f32>(clamp(in.col.rgb * 1.18, vec3<f32>(0.0), vec3<f32>(1.0)), in.col.a);
    }

    // Blend rich 512x512 procedural albedo with per-surface tint from capsule
    var albedo: vec3<f32>;
    if (mat_id == 0u) {
        // Monsters / sculpted solid parts preserve strong capsule colors + subtle organic texture
        albedo = in.col.rgb * (0.76 + 0.36 * tex_albedo);
    } else {
        // Architecture / wood / stone / metal keep rich multi-tone moss/rust/mortar from 512x512 texture
        albedo = tex_albedo * mix(vec3<f32>(1.18), in.col.rgb * 1.42, 0.42);
    }

    // Infinite-resolution close-up micro-detail grain & micro-bump (stays razor-sharp even 10cm from walls & monsters!)
    let cam_dist = length(u.cam_pos.xyz - in.world_pos);
    let close_w = 1.0 - smoothstep(1.5, 9.0, cam_dist);
    let d_uv = in.uv * 16.0;
    let d_n0 = vnoise(d_uv) * 0.62 + vnoise(d_uv * 2.7 + vec2<f32>(5.2, 1.3)) * 0.38;
    let d_nx = (vnoise(d_uv + vec2<f32>(0.08, 0.0)) - d_n0) * 2.2;
    let d_ny = (vnoise(d_uv + vec2<f32>(0.0, 0.08)) - d_n0) * 2.2;
    albedo = albedo * mix(1.0, 0.82 + 0.36 * d_n0, close_w * 0.85);
    let detail_map_n = normalize(map_n + vec3<f32>(d_nx, d_ny, 0.0) * (close_w * 0.65));

    let n_geo = normalize(in.world_n);
    let n = perturb_normal(n_geo, in.world_pos, in.uv, detail_map_n);
    let view_dir = normalize(u.cam_pos.xyz - in.world_pos);

    // Per-vertex ambient occlusion supplied by capsule (no hardcoded y == 0 floor bias so multi-deck 3D worlds work cleanly)
    let contact_ao = in.ao;

    // Warm directional sunlight + hemisphere ambient (brighter sunlit savanna when sky_mode == 2)
    let is_day = u.params.x > 1.5;
    let sun_dir = normalize(vec3<f32>(0.48, 0.72, 0.50));
    let ndl_sun = max(dot(n, sun_dir), 0.0);
    let hemi_lo = select(vec3<f32>(0.15, 0.12, 0.18), vec3<f32>(0.36, 0.30, 0.22), is_day);
    let hemi_hi = select(vec3<f32>(0.32, 0.24, 0.22), vec3<f32>(0.54, 0.58, 0.64), is_day);
    let sun_col = select(vec3<f32>(0.96, 0.78, 0.60) * 0.76, vec3<f32>(1.0, 0.92, 0.76) * 0.92, is_day);
    let hemi = mix(hemi_lo, hemi_hi, n.y * 0.5 + 0.5);
    var lit_col = albedo * (hemi + sun_col * ndl_sun) * contact_ao;

    // Subtle close-range player fill light so normal maps & micro-relief pop even on shadowed walls & approaching monsters!
    if (u.params.z < 0.5 && cam_dist < 6.0) {
        let p_fall = clamp(1.0 - cam_dist / 6.0, 0.0, 1.0);
        let p_ndl = max(dot(n, view_dir), 0.0);
        lit_col = lit_col + albedo * vec3<f32>(0.86, 0.76, 0.66) * (p_ndl * 0.85 + 0.15) * (p_fall * p_fall * 0.34);
    }

    // Roughness-driven Blinn-Phong specular highlight from sun
    let spec_pow = mix(64.0, 8.0, roughness);
    let spec_str = mix(0.46, 0.06, roughness);
    let half_sun = normalize(sun_dir + view_dir);
    let spec_sun = pow(max(dot(n, half_sun), 0.0), spec_pow) * spec_str;
    lit_col = lit_col + vec3<f32>(1.0, 0.88, 0.76) * spec_sun * ndl_sun;

    // Accumulate up to 24 dynamic 3D point lights with normal-mapped diffuse + specular!
    let lcnt = u32(u.cam_fwd.w + 0.5);
    for (var i = 0u; i < 24u; i = i + 1u) {
        if (i >= lcnt) {
            break;
        }
        let lp = u.lights[i].pos_rad.xyz;
        let rad = max(u.lights[i].pos_rad.w, 0.5);
        let lvec = lp - in.world_pos;
        let dist = length(lvec);
        if (dist < rad) {
            let ldir = lvec / max(dist, 0.001);
            let ndl = max(dot(n, ldir), 0.0);
            let falloff = clamp(1.0 - dist / rad, 0.0, 1.0);
            let atten = falloff * falloff * u.lights[i].col_int.w;
            let lcol = u.lights[i].col_int.rgb;
            lit_col = lit_col + albedo * lcol * (ndl * 0.88 + 0.12) * atten;
            let h_l = normalize(ldir + view_dir);
            let sp_l = pow(max(dot(n, h_l), 0.0), spec_pow) * spec_str * 1.15;
            lit_col = lit_col + lcol * sp_l * atten;
        }
    }

    // True 3D radial distance fog (skipped for viewmodel; 0 = horizon haze, 1 = dark fog, 2 = crimson fog)
    if (u.params.z < 0.5) {
        let fog_dist = length(u.cam_pos.xyz - in.world_pos);
        if (u.params.y > 1.5) {
            let fog_f = clamp((fog_dist - 10.0) / 38.0, 0.0, 0.80);
            lit_col = mix(lit_col, vec3<f32>(0.26, 0.14, 0.15), fog_f);
        } else if (u.params.y > 0.5) {
            let fog_f = clamp((fog_dist - 10.0) / 38.0, 0.0, 0.80);
            lit_col = mix(lit_col, vec3<f32>(0.05, 0.07, 0.10), fog_f);
        } else if (u.params.x > 1.5) {
            let fog_f = clamp((fog_dist - 18.0) / 48.0, 0.0, 0.45);
            lit_col = mix(lit_col, vec3<f32>(0.92, 0.78, 0.58), fog_f);
        } else {
            let fog_f = clamp((fog_dist - 18.0) / 48.0, 0.0, 0.55);
            lit_col = mix(lit_col, vec3<f32>(0.12, 0.10, 0.14), fog_f);
        }
    }

    // Subtle filmic contrast / tone curve so darks stay rich and highlights pop
    let tone = lit_col / (lit_col + vec3<f32>(0.85)) * 1.48;
    return vec4<f32>(clamp(tone, vec3<f32>(0.0), vec3<f32>(1.0)), in.col.a);
}
