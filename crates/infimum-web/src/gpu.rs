//! Native 1-Draw-Call `wgpu` SDF & HiDPI Unicode Renderer.

use crate::atlas::GlyphAtlas;
use crate::runtime::{ClipRect, GpuPrim, Mesh3DScene, MeshVertex3D, PointLight3D};
use bytemuck::{Pod, Zeroable};
use std::sync::Arc;
use wgpu::util::DeviceExt;
#[cfg(not(target_arch = "wasm32"))]
use winit::window::Window;

#[cfg(not(target_arch = "wasm32"))]
struct Clock(std::time::Instant);

#[cfg(not(target_arch = "wasm32"))]
impl Clock {
    fn now() -> Self {
        Self(std::time::Instant::now())
    }
    fn elapsed_secs(&self) -> f32 {
        self.0.elapsed().as_secs_f32()
    }
}

#[cfg(target_arch = "wasm32")]
struct Clock(f64);

#[cfg(target_arch = "wasm32")]
impl Clock {
    fn perf_now_sec() -> f64 {
        web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now() / 1000.0)
            .unwrap_or(0.0)
    }
    fn now() -> Self {
        Self(Self::perf_now_sec())
    }
    fn elapsed_secs(&self) -> f32 {
        (Self::perf_now_sec() - self.0).max(0.0) as f32
    }
}

const INITIAL_MAX_PRIMS: usize = 32_768;
const INITIAL_MAX_MESH_VERTS: usize = 65_536;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Mesh3DUniforms {
    view_proj: [[f32; 4]; 4],
    proj_vm: [[f32; 4]; 4],
    cam_pos: [f32; 4],
    cam_right: [f32; 4],
    cam_up: [f32; 4],
    cam_fwd: [f32; 4],
    params: [f32; 4],
    lights: [PointLight3D; 24],
}

fn mat4_mul(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];
    for c in 0..4 {
        for r in 0..4 {
            out[c][r] = a[0][r] * b[c][0]
                + a[1][r] * b[c][1]
                + a[2][r] * b[c][2]
                + a[3][r] * b[c][3];
        }
    }
    out
}

fn build_perspective_wgpu(fov_y_rad: f32, aspect: f32, z_near: f32, z_far: f32) -> [[f32; 4]; 4] {
    let f = 1.0 / (fov_y_rad * 0.5).tan();
    let nf = 1.0 / (z_near - z_far);
    [
        [f / aspect, 0.0, 0.0, 0.0],
        [0.0, f, 0.0, 0.0],
        [0.0, 0.0, z_far * nf, -1.0],
        [0.0, 0.0, z_near * z_far * nf, 0.0],
    ]
}

fn build_camera_basis(yaw: f32, pitch: f32) -> ([f32; 3], [f32; 3], [f32; 3]) {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let fwd = [sy * cp, sp, -cy * cp];
    let right = [cy, 0.0, sy];
    let up = [
        right[1] * fwd[2] - right[2] * fwd[1],
        right[2] * fwd[0] - right[0] * fwd[2],
        right[0] * fwd[1] - right[1] * fwd[0],
    ];
    (right, up, fwd)
}

fn build_view_matrix(eye: [f32; 3], right: [f32; 3], up: [f32; 3], fwd: [f32; 3]) -> [[f32; 4]; 4] {
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let tx = -dot(right, eye);
    let ty = -dot(up, eye);
    let tz = dot(fwd, eye);
    [
        [right[0], up[0], -fwd[0], 0.0],
        [right[1], up[1], -fwd[1], 0.0],
        [right[2], up[2], -fwd[2], 0.0],
        [tx, ty, tz, 1.0],
    ]
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    viewport: [f32; 2],
    legacy_ascii: u32,
    time_sec: f32,
    clips: [ClipRect; 16],
}

pub struct GpuRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    pipeline_2d_depth: wgpu::RenderPipeline,
    pipeline_3d: wgpu::RenderPipeline,
    pipeline_mesh_sky: wgpu::RenderPipeline,
    pipeline_mesh_3d: wgpu::RenderPipeline,
    mesh_uniform_buf: wgpu::Buffer,
    mesh_vm_uniform_buf: wgpu::Buffer,
    mesh_bgl: wgpu::BindGroupLayout,
    mesh_bind_group: wgpu::BindGroup,
    mesh_vm_bind_group: wgpu::BindGroup,
    mesh_textures_ready: bool,
    mesh_textures_loading: bool,
    mesh_textures_progress: Arc<std::sync::atomic::AtomicU32>,
    mesh_textures_rx: Option<std::sync::mpsc::Receiver<crate::textures3d::PreparedPbrTextures>>,
    textures_dir: Option<std::path::PathBuf>,
    splash_title: String,
    font_advances: [[i32; 176]; 3],
    mesh_vbo: wgpu::Buffer,
    mesh_vbo_cap: usize,
    mesh_vm_vbo: wgpu::Buffer,
    mesh_vm_vbo_cap: usize,
    depth_view: wgpu::TextureView,
    start_time: Clock,
    uniform_buf: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    atlas_view: wgpu::TextureView,
    atlas_sampler: wgpu::Sampler,
    bind_group: wgpu::BindGroup,
    instance_buf: wgpu::Buffer,
    instance_capacity: usize,
    image_slots: Vec<(u32, u32, Vec<u8>)>,
    image_pending_base: usize,
    pub scale_factor: f64,
}

impl GpuRenderer {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(
        window: Arc<Window>,
        atlas: &GlyphAtlas,
        textures_dir: Option<&std::path::Path>,
        capsule_name: &str,
    ) -> Result<Self, String> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);
        let scale_factor = window.scale_factor();

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..Default::default()
        });

        let surface = instance
            .create_surface(window)
            .map_err(|e| format!("Failed to create wgpu surface: {e}"))?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok_or_else(|| "No compatible Vulkan/GL GPU adapter found".to_string())?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("infimum-web-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                    .using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
            },
            None,
        ))
        .map_err(|e| format!("Failed to request GPU device: {e}"))?;

        Self::init_from_surface(
            surface,
            &adapter,
            device,
            queue,
            width,
            height,
            scale_factor,
            atlas,
            textures_dir,
            capsule_name,
            false,
        )
    }

    #[cfg(target_arch = "wasm32")]
    pub async fn new_for_canvas(
        canvas: web_sys::HtmlCanvasElement,
        width: u32,
        height: u32,
        scale_factor: f64,
        atlas: &GlyphAtlas,
        capsule_name: &str,
        expect_textures: bool,
    ) -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::GL,
            ..Default::default()
        });
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(|e| format!("Failed to create WebGL2/WebGPU canvas surface: {e}"))?;
        let adapter = match instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
        {
            Some(a) => a,
            None => instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::None,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: true,
                })
                .await
                .ok_or_else(|| "No compatible WebGL2 adapter found in browser".to_string())?,
        };
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("infimum-web-wasm-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                        .using_resolution(adapter.limits()),
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                },
                None,
            )
            .await
            .map_err(|e| format!("Failed to request WebGL2/WebGPU device: {e}"))?;

        Self::init_from_surface(
            surface,
            &adapter,
            device,
            queue,
            width.max(1),
            height.max(1),
            scale_factor,
            atlas,
            None,
            capsule_name,
            expect_textures,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn init_from_surface(
        surface: wgpu::Surface<'static>,
        adapter: &wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        width: u32,
        height: u32,
        scale_factor: f64,
        atlas: &GlyphAtlas,
        textures_dir: Option<&std::path::Path>,
        capsule_name: &str,
        web_expect_textures: bool,
    ) -> Result<Self, String> {
        let _ = web_expect_textures;
        let caps = surface.get_capabilities(adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Upload HiDPI Unicode glyph atlas (R8Unorm)
        let tex_size = wgpu::Extent3d {
            width: atlas.width,
            height: atlas.height,
            depth_or_array_layers: 1,
        };
        let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("infimum-unicode-atlas"),
            size: tex_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &atlas.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atlas.width),
                rows_per_image: Some(atlas.height),
            },
            tex_size,
        );
        let atlas_view = atlas_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("infimum-atlas-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let init_uniforms = Uniforms {
            viewport: [width as f32, height as f32],
            legacy_ascii: 0,
            time_sec: 0.0,
            clips: [ClipRect::default(); 16],
        };
        let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("infimum-uniforms"),
            contents: bytemuck::bytes_of(&init_uniforms),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("infimum-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let placeholder_img = Self::create_rgba_texture_view(&device, &queue, 1, 1, &[0, 0, 0, 0]);
        let bind_group = Self::make_bind_group(
            &device,
            &bind_group_layout,
            &uniform_buf,
            &atlas_view,
            &atlas_sampler,
            &placeholder_img,
        );

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("infimum-sdf-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("infimum-pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuPrim>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Sint32x4,
                },
                wgpu::VertexAttribute {
                    offset: 16,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Uint32x4,
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("infimum-1draw-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: std::slice::from_ref(&instance_layout),
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let pipeline_2d_depth = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("infimum-2d-in-3d-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: std::slice::from_ref(&instance_layout),
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: false,
                depth_compare: wgpu::CompareFunction::Always,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let pipeline_3d = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("infimum-3d-zbuf-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[instance_layout],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main_3d"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::LessEqual,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let depth_view = Self::create_depth_view(&device, width, height);

        // --- Native Vulkan 3D Mesh + Dynamic Point Lights + Procedural Sky Pipelines ---
        let mesh_textures = crate::textures3d::create_placeholder_textures_3d(&device, &queue);
        let mesh_textures_progress = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let textures_dir_owned = textures_dir.map(|p| p.to_path_buf());
        #[cfg(not(target_arch = "wasm32"))]
        let (mesh_textures_loading, mesh_textures_rx) = if textures_dir_owned.is_some() {
            let (tx, rx) = std::sync::mpsc::channel();
            let dir_c = textures_dir_owned.clone();
            let prog_c = Arc::clone(&mesh_textures_progress);
            std::thread::spawn(move || {
                let prep = crate::textures3d::prepare_textures_3d_cpu(dir_c.as_deref(), &prog_c);
                let _ = tx.send(prep);
            });
            (true, Some(rx))
        } else {
            (false, None)
        };
        #[cfg(target_arch = "wasm32")]
        let (mesh_textures_loading, mesh_textures_rx) = (web_expect_textures, None);
        let splash_title = capsule_name
            .trim_end_matches(".ifw")
            .replace(['_', '-'], " ")
            .to_uppercase();
        let mesh_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("infimum-mesh3d-wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("mesh3d.wgsl").into()),
        });
        let mesh_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("infimum-mesh3d-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let mesh_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("infimum-mesh3d-pl"),
            bind_group_layouts: &[&mesh_bgl],
            push_constant_ranges: &[],
        });
        let zero_mesh_u = Mesh3DUniforms::zeroed();
        let mesh_uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("infimum-mesh3d-u"),
            contents: bytemuck::bytes_of(&zero_mesh_u),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let mesh_vm_uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("infimum-mesh3d-vm-u"),
            contents: bytemuck::bytes_of(&zero_mesh_u),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let mesh_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("infimum-mesh3d-bg"),
            layout: &mesh_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: mesh_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&mesh_textures.diffuse_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&mesh_textures.normal_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&mesh_textures.sampler),
                },
            ],
        });
        let mesh_vm_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("infimum-mesh3d-vm-bg"),
            layout: &mesh_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: mesh_vm_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&mesh_textures.diffuse_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&mesh_textures.normal_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&mesh_textures.sampler),
                },
            ],
        });
        let pipeline_mesh_sky = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("infimum-mesh3d-sky-pipe"),
            layout: Some(&mesh_pl),
            vertex: wgpu::VertexState {
                module: &mesh_shader,
                entry_point: Some("vs_sky"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &mesh_shader,
                entry_point: Some("fs_sky"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: false,
                depth_compare: wgpu::CompareFunction::Always,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let mesh_vbo_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<MeshVertex3D>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Sint32x3 },
                wgpu::VertexAttribute { offset: 12, shader_location: 1, format: wgpu::VertexFormat::Uint32 },
                wgpu::VertexAttribute { offset: 16, shader_location: 2, format: wgpu::VertexFormat::Sint32x2 },
                wgpu::VertexAttribute { offset: 24, shader_location: 3, format: wgpu::VertexFormat::Uint32 },
                wgpu::VertexAttribute { offset: 28, shader_location: 4, format: wgpu::VertexFormat::Uint32 },
            ],
        };
        let pipeline_mesh_3d = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("infimum-mesh3d-pipe"),
            layout: Some(&mesh_pl),
            vertex: wgpu::VertexState {
                module: &mesh_shader,
                entry_point: Some("vs_mesh"),
                buffers: &[mesh_vbo_layout],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &mesh_shader,
                entry_point: Some("fs_mesh"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::LessEqual,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let mesh_vbo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("infimum-mesh3d-vbo"),
            size: (INITIAL_MAX_MESH_VERTS * std::mem::size_of::<MeshVertex3D>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mesh_vm_vbo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("infimum-mesh3d-vm-vbo"),
            size: (4096 * std::mem::size_of::<MeshVertex3D>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("infimum-instances"),
            size: (INITIAL_MAX_PRIMS * std::mem::size_of::<GpuPrim>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            pipeline_2d_depth,
            pipeline_3d,
            pipeline_mesh_sky,
            pipeline_mesh_3d,
            mesh_uniform_buf,
            mesh_vm_uniform_buf,
            mesh_bgl,
            mesh_bind_group,
            mesh_vm_bind_group,
            mesh_textures_ready: false,
            mesh_textures_loading,
            mesh_textures_progress,
            mesh_textures_rx,
            textures_dir: textures_dir_owned,
            splash_title,
            font_advances: atlas.advances_256,
            mesh_vbo,
            mesh_vbo_cap: INITIAL_MAX_MESH_VERTS,
            mesh_vm_vbo,
            mesh_vm_vbo_cap: 4096,
            depth_view,
            start_time: Clock::now(),
            uniform_buf,
            bind_group_layout,
            atlas_view,
            atlas_sampler,
            bind_group,
            instance_buf,
            instance_capacity: INITIAL_MAX_PRIMS,
            image_slots: Vec::new(),
            image_pending_base: 0,
            scale_factor,
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub fn set_web_loading_progress(&mut self, loading: bool, pct: u32) {
        self.mesh_textures_loading = loading;
        self.mesh_textures_progress
            .store(pct.min(100), std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(target_arch = "wasm32")]
    pub fn apply_prepared_textures(&mut self, prep: crate::textures3d::PreparedPbrTextures) {
        let mesh_textures =
            crate::textures3d::upload_prepared_textures_3d(&self.device, &self.queue, prep);
        self.mesh_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("infimum-mesh3d-bg-web"),
            layout: &self.mesh_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.mesh_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&mesh_textures.diffuse_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&mesh_textures.normal_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&mesh_textures.sampler),
                },
            ],
        });
        self.mesh_vm_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("infimum-mesh3d-vm-bg-web"),
            layout: &self.mesh_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.mesh_vm_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&mesh_textures.diffuse_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&mesh_textures.normal_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&mesh_textures.sampler),
                },
            ],
        });
        self.mesh_textures_ready = true;
        self.mesh_textures_loading = false;
    }

    fn create_depth_view(device: &wgpu::Device, width: u32, height: u32) -> wgpu::TextureView {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("infimum-depth-zbuf"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    }

    fn create_rgba_texture_view(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> wgpu::TextureView {
        let size = wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        };
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("infimum-user-image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size.width * 4),
                rows_per_image: Some(size.height),
            },
            size,
        );
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    }

    fn make_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniform_buf: &wgpu::Buffer,
        atlas_view: &wgpu::TextureView,
        atlas_sampler: &wgpu::Sampler,
        img_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("infimum-bg"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(atlas_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(img_view),
                },
            ],
        })
    }

    fn rebuild_image_atlas(&mut self) {
        if self.image_slots.is_empty() {
            let dummy = [0u8; 16];
            let img_view = Self::create_rgba_texture_view(&self.device, &self.queue, 2, 2, &dummy);
            self.bind_group = Self::make_bind_group(
                &self.device,
                &self.bind_group_layout,
                &self.uniform_buf,
                &self.atlas_view,
                &self.atlas_sampler,
                &img_view,
            );
            return;
        }
        const SLOT_DIM: u32 = 512;
        let atlas_w = SLOT_DIM * 4;
        let atlas_h = SLOT_DIM * 4;
        let size = wgpu::Extent3d {
            width: atlas_w,
            height: atlas_h,
            depth_or_array_layers: 1,
        };
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("infimum-user-image-atlas-4x4"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut quad_buf = vec![0u8; (SLOT_DIM * SLOT_DIM * 4) as usize];
        for (slot_idx, (sw, sh, src)) in self.image_slots.iter().take(16).enumerate() {
            let sw_u = (*sw).max(1) as usize;
            let sh_u = (*sh).max(1) as usize;
            for dy in 0..(SLOT_DIM as usize) {
                let sy_f = (dy as f32 + 0.5) * (sh_u as f32) / (SLOT_DIM as f32) - 0.5;
                let sy0 = (sy_f.floor().max(0.0) as usize).min(sh_u - 1);
                let sy1 = (sy0 + 1).min(sh_u - 1);
                let wy = (sy_f - sy0 as f32).clamp(0.0, 1.0);
                let row0 = sy0 * sw_u;
                let row1 = sy1 * sw_u;
                for dx in 0..(SLOT_DIM as usize) {
                    let sx_f = (dx as f32 + 0.5) * (sw_u as f32) / (SLOT_DIM as f32) - 0.5;
                    let sx0 = (sx_f.floor().max(0.0) as usize).min(sw_u - 1);
                    let sx1 = (sx0 + 1).min(sw_u - 1);
                    let wx = (sx_f - sx0 as f32).clamp(0.0, 1.0);
                    let i00 = (row0 + sx0) * 4;
                    let i10 = (row0 + sx1) * 4;
                    let i01 = (row1 + sx0) * 4;
                    let i11 = (row1 + sx1) * 4;
                    let dst = (dy * (SLOT_DIM as usize) + dx) * 4;
                    for c in 0..4 {
                        let c00 = src[i00 + c] as f32;
                        let c10 = src[i10 + c] as f32;
                        let c01 = src[i01 + c] as f32;
                        let c11 = src[i11 + c] as f32;
                        let top = c00 + (c10 - c00) * wx;
                        let bot = c01 + (c11 - c01) * wx;
                        quad_buf[dst + c] = (top + (bot - top) * wy).round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
            let ox = ((slot_idx & 3) as u32) * SLOT_DIM;
            let oy = (((slot_idx >> 2) & 3) as u32) * SLOT_DIM;
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: ox, y: oy, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &quad_buf,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(SLOT_DIM * 4),
                    rows_per_image: Some(SLOT_DIM),
                },
                wgpu::Extent3d {
                    width: SLOT_DIM,
                    height: SLOT_DIM,
                    depth_or_array_layers: 1,
                },
            );
        }
        let img_view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        self.bind_group = Self::make_bind_group(
            &self.device,
            &self.bind_group_layout,
            &self.uniform_buf,
            &self.atlas_view,
            &self.atlas_sampler,
            &img_view,
        );
    }

    /// Commit currently pending composer images into the session atlas so sent messages keep their thumbnails.
    pub fn commit_pending_images(&mut self) {
        self.image_pending_base = self.image_slots.len();
    }

    pub fn image_slot_count(&self) -> usize {
        self.image_slots.len()
    }

    /// Add an attached RGBA image into the next available slot (up to 16) of the 4x4 session image atlas.
    pub fn add_image_rgba(&mut self, width: u32, height: u32, rgba: &[u8]) -> Option<usize> {
        if width == 0 || height == 0 || rgba.len() < (width as usize * height as usize * 4) {
            return None;
        }
        if self.image_slots.len() >= 16 {
            return None;
        }
        let idx = self.image_slots.len();
        self.image_slots.push((width, height, rgba.to_vec()));
        self.rebuild_image_atlas();
        Some(idx)
    }

    pub fn remove_pending_slot(&mut self, rel_idx: usize) {
        let abs_idx = self.image_pending_base + rel_idx;
        if abs_idx < self.image_slots.len() {
            self.image_slots.remove(abs_idx);
            self.rebuild_image_atlas();
        }
    }

    pub fn clear_images(&mut self) {
        self.image_pending_base = 0;
        if !self.image_slots.is_empty() {
            self.image_slots.clear();
            self.rebuild_image_atlas();
        }
    }

    /// Replace the 4x4 session image atlas with images decoded from a loaded conversation.
    #[allow(dead_code)]
    pub fn load_session_images(&mut self, slots: Vec<(u32, u32, Vec<u8>)>) {
        self.image_slots = slots.into_iter().take(16).collect();
        self.image_pending_base = self.image_slots.len();
        self.rebuild_image_atlas();
    }

    /// Backward-compatible alias for single-image upload.
    #[allow(dead_code)]
    pub fn upload_image_rgba(&mut self, width: u32, height: u32, rgba: &[u8]) {
        let _ = self.add_image_rgba(width, height, rgba);
    }

    pub fn resize(&mut self, width: u32, height: u32, scale_factor: f64) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.scale_factor = scale_factor;
        self.surface.configure(&self.device, &self.config);
        self.depth_view = Self::create_depth_view(&self.device, width, height);
    }

    pub fn logical_size(&self) -> (i32, i32) {
        let sf = self.scale_factor.max(0.5);
        (
            ((self.config.width as f64) / sf).round() as i32,
            ((self.config.height as f64) / sf).round() as i32,
        )
    }

    fn build_splash_prims(
        lw: i32,
        lh: i32,
        title: &str,
        pct: u32,
        time_sec: f32,
        font_advances: &[[i32; 176]; 3],
    ) -> Vec<GpuPrim> {
        let mut out = Vec::with_capacity(256);
        let pack = |r: u32, g: u32, b: u32, a: u32| -> u32 { r | (g << 8) | (b << 16) | (a << 24) };
        let push_rect = |out: &mut Vec<GpuPrim>, x: i32, y: i32, w: i32, h: i32, rad: u32, bw: u32, sh: u32, fill: u32, brd: u32| {
            out.push(GpuPrim {
                w0: x,
                w1: y,
                w2: w,
                w3: h,
                meta: (rad << 4) | (bw << 12) | (sh << 18),
                col1: fill,
                col2: brd,
                clip_id: 0,
            });
        };
        let cp_slot = |ch: char| -> Option<u32> {
            let cp = ch as u32;
            if (32..=126).contains(&cp) {
                Some(cp - 32)
            } else if (1040..=1071).contains(&cp) {
                Some(96 + (cp - 1040))
            } else if (1072..=1103).contains(&cp) {
                Some(128 + (cp - 1072))
            } else if cp == 1025 {
                Some(160)
            } else if cp == 1105 {
                Some(161)
            } else if cp == 8226 {
                Some(166)
            } else {
                None
            }
        };
        let glyph_adv = |ch: char, sz: i32, bold: u32| -> i32 {
            if let Some(slot) = cp_slot(ch) {
                let st = (bold as usize).min(2);
                let sl = slot as usize;
                if sl < 176 {
                    let adv256 = font_advances[st][sl];
                    if adv256 > 0 {
                        return (sz * adv256 + 128) / 256;
                    }
                }
            }
            (sz * 9) / 16
        };
        let push_text_centered = |out: &mut Vec<GpuPrim>, cx: i32, y: i32, text: &str, sz: i32, bold: u32, col: u32| {
            let total_w: i32 = text.chars().map(|ch| glyph_adv(ch, sz, bold)).sum();
            let mut x = cx - total_w / 2;
            for ch in text.chars() {
                if let Some(slot) = cp_slot(ch) {
                    if slot > 0 {
                        out.push(GpuPrim {
                            w0: x,
                            w1: y,
                            w2: sz,
                            w3: sz,
                            meta: 1 | ((slot & 255) << 4) | ((bold & 3) << 12),
                            col1: col,
                            col2: 0,
                            clip_id: 0,
                        });
                    }
                }
                x += glyph_adv(ch, sz, bold);
            }
        };

        // Deep atmospheric obsidian backdrop + subtle warm vignette card
        push_rect(&mut out, 0, 0, lw, lh, 0, 0, 0, pack(13, 15, 22, 255), 0);
        let cw = (lw - 64).clamp(340, 520);
        let ch = 220;
        let cx = lw / 2;
        let cy = lh / 2;
        let x0 = cx - cw / 2;
        let y0 = cy - ch / 2;

        // Ambient warm halo behind the card
        push_rect(&mut out, x0 - 14, y0 - 14, cw + 28, ch + 28, 22, 0, 0, pack(232, 174, 72, 24), 0);
        // Main splash card
        push_rect(&mut out, x0, y0, cw, ch, 16, 2, 20, pack(22, 25, 34, 248), pack(218, 168, 78, 220));
        // Top gold accent bar
        push_rect(&mut out, x0 + 28, y0 + 20, cw - 56, 3, 2, 0, 0, pack(242, 192, 86, 200), 0);

        // Title + subtitle
        let title_str = if title.is_empty() { "INFIMUM 3D ENGINE" } else { title };
        push_text_centered(&mut out, cx, y0 + 44, title_str, 24, 1, pack(255, 232, 176, 255));
        push_text_centered(&mut out, cx, y0 + 82, "VULKAN PBR 3D • INFIMUM WEB", 13, 0, pack(162, 174, 196, 220));

        // Progress bar track
        let bw = cw - 72;
        let bx = cx - bw / 2;
        let by = y0 + 128;
        let bh = 16;
        push_rect(&mut out, bx, by, bw, bh, 8, 1, 0, pack(14, 16, 24, 255), pack(96, 86, 68, 220));
        let p_clamped = pct.clamp(6, 100) as i32;
        let fill_w = ((bw - 4) * p_clamped / 100).max(16);
        push_rect(&mut out, bx + 2, by + 2, fill_w, bh - 4, 6, 0, 0, pack(242, 184, 64, 255), 0);
        // Animated shimmer highlight sliding across the progress bar
        let shim_w = 48.min(fill_w);
        let shim_off = ((time_sec * 180.0) as i32) % (fill_w.max(1));
        let shim_x = (bx + 2 + shim_off).min(bx + 2 + fill_w - shim_w);
        push_rect(&mut out, shim_x, by + 2, shim_w, bh - 4, 6, 0, 0, pack(255, 242, 196, 130), 0);

        // Live percentage label
        let status_lbl = format!("Загрузка 1K PBR-текстур... {}%", pct.min(100));
        push_text_centered(&mut out, cx, by + 30, &status_lbl, 14, 1, pack(224, 212, 188, 245));
        out
    }

    pub fn render(
        &mut self,
        prims: &[GpuPrim],
        clips: &[ClipRect; 16],
        legacy_ascii: bool,
        mesh3d: Option<&Mesh3DScene>,
    ) -> Result<(), wgpu::SurfaceError> {
        // Check if background 1K PBR textures finished preparing; upload to Vulkan in ~10ms!
        if let Some(rx) = &self.mesh_textures_rx {
            if let Ok(prep) = rx.try_recv() {
                let mesh_textures =
                    crate::textures3d::upload_prepared_textures_3d(&self.device, &self.queue, prep);
                self.mesh_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("infimum-mesh3d-bg"),
                    layout: &self.mesh_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: self.mesh_uniform_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&mesh_textures.diffuse_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(&mesh_textures.normal_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::Sampler(&mesh_textures.sampler),
                        },
                    ],
                });
                self.mesh_vm_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("infimum-mesh3d-vm-bg"),
                    layout: &self.mesh_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: self.mesh_vm_uniform_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&mesh_textures.diffuse_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(&mesh_textures.normal_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::Sampler(&mesh_textures.sampler),
                        },
                    ],
                });
                self.mesh_textures_ready = true;
                self.mesh_textures_loading = false;
                self.mesh_textures_rx = None;
            }
        }

        // Lazily start background procedural texture build if a 3D capsule without `textures/` renders
        if mesh3d.is_some() && !self.mesh_textures_ready && !self.mesh_textures_loading {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let (tx, rx) = std::sync::mpsc::channel();
                let dir_c = self.textures_dir.clone();
                let prog_c = Arc::clone(&self.mesh_textures_progress);
                std::thread::spawn(move || {
                    let prep = crate::textures3d::prepare_textures_3d_cpu(dir_c.as_deref(), &prog_c);
                    let _ = tx.send(prep);
                });
                self.mesh_textures_loading = true;
                self.mesh_textures_rx = Some(rx);
            }
            #[cfg(target_arch = "wasm32")]
            {
                let empty = std::collections::BTreeMap::new();
                let prep = crate::textures3d::prepare_textures_3d_from_bytes(&empty, 256);
                self.apply_prepared_textures(prep);
            }
        }

        let show_splash = self.mesh_textures_loading && !self.mesh_textures_ready && mesh3d.is_some();
        let (lw, lh) = self.logical_size();
        let time_sec = self.start_time.elapsed_secs();

        let splash_buf;
        let effective_prims: &[GpuPrim] = if show_splash {
            let pct = self
                .mesh_textures_progress
                .load(std::sync::atomic::Ordering::Relaxed);
            splash_buf = Self::build_splash_prims(
                lw.max(320),
                lh.max(240),
                &self.splash_title,
                pct,
                time_sec,
                &self.font_advances,
            );
            &splash_buf
        } else {
            prims
        };
        let effective_mesh3d = if show_splash { None } else { mesh3d };

        if effective_prims.len() > self.instance_capacity {
            self.instance_capacity = effective_prims.len().next_power_of_two();
            self.instance_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("infimum-instances"),
                size: (self.instance_capacity * std::mem::size_of::<GpuPrim>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }

        let mut effective_clips = *clips;
        if show_splash {
            effective_clips[0] = ClipRect {
                x0: 0.0,
                y0: 0.0,
                x1: lw.max(1) as f32,
                y1: lh.max(1) as f32,
            };
        }
        let uniforms = Uniforms {
            viewport: [lw.max(1) as f32, lh.max(1) as f32],
            legacy_ascii: if show_splash { 0 } else { u32::from(legacy_ascii) },
            time_sec,
            clips: effective_clips,
        };
        self.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        if !effective_prims.is_empty() {
            self.queue
                .write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(effective_prims));
        }

        // Upload 3D Mesh scene buffers if active
        if let Some(scene) = effective_mesh3d {
            if scene.world_verts.len() > self.mesh_vbo_cap {
                self.mesh_vbo_cap = scene.world_verts.len().next_power_of_two();
                self.mesh_vbo = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("infimum-mesh3d-vbo"),
                    size: (self.mesh_vbo_cap * std::mem::size_of::<MeshVertex3D>()) as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            if !scene.world_verts.is_empty() {
                self.queue.write_buffer(
                    &self.mesh_vbo,
                    0,
                    bytemuck::cast_slice(&scene.world_verts),
                );
            }
            if scene.vm_verts.len() > self.mesh_vm_vbo_cap {
                self.mesh_vm_vbo_cap = scene.vm_verts.len().next_power_of_two();
                self.mesh_vm_vbo = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("infimum-mesh3d-vm-vbo"),
                    size: (self.mesh_vm_vbo_cap * std::mem::size_of::<MeshVertex3D>()) as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            if !scene.vm_verts.is_empty() {
                self.queue.write_buffer(
                    &self.mesh_vm_vbo,
                    0,
                    bytemuck::cast_slice(&scene.vm_verts),
                );
            }
            let aspect = (self.config.width.max(1) as f32) / (self.config.height.max(1) as f32);
            let fov_rad = scene.fov_deg.to_radians();
            let proj = build_perspective_wgpu(fov_rad, aspect, 0.08, 90.0);
            let proj_vm = build_perspective_wgpu(62.0f32.to_radians(), aspect, 0.03, 12.0);
            let (right, up, fwd) = build_camera_basis(scene.cam_yaw_rad, scene.cam_pitch_rad);
            let view = build_view_matrix(scene.cam_pos_m, right, up, fwd);
            let vp = mat4_mul(proj, view);
            let mut lights = [PointLight3D::zeroed(); 24];
            let lcnt = scene.lights.len().min(24);
            lights[..lcnt].copy_from_slice(&scene.lights[..lcnt]);
            let mut mu = Mesh3DUniforms {
                view_proj: vp,
                proj_vm,
                cam_pos: [scene.cam_pos_m[0], scene.cam_pos_m[1], scene.cam_pos_m[2], time_sec],
                cam_right: [right[0], right[1], right[2], aspect],
                cam_up: [up[0], up[1], up[2], (fov_rad * 0.5).tan()],
                cam_fwd: [fwd[0], fwd[1], fwd[2], lcnt as f32],
                params: [scene.sky_mode as f32, scene.fog_mode as f32, 0.0, 0.0],
                lights,
            };
            self.queue
                .write_buffer(&self.mesh_uniform_buf, 0, bytemuck::bytes_of(&mu));
            mu.params[2] = 1.0;
            self.queue
                .write_buffer(&self.mesh_vm_uniform_buf, 0, bytemuck::bytes_of(&mu));
        }

        let has_3d = effective_prims.iter().any(|p| (p.meta & 15) == 9);
        let output = self.surface.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("infimum-frame-encoder"),
            });

        // Pass 1: Hardware Vulkan 3D Mesh + Dynamic Point Lights + Sky (if active)
        if let Some(scene) = effective_mesh3d {
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("infimum-mesh3d-world-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.05,
                                g: 0.04,
                                b: 0.07,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &self.depth_view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(1.0),
                            store: wgpu::StoreOp::Discard,
                        }),
                        stencil_ops: None,
                    }),
                    occlusion_query_set: None,
                    timestamp_writes: None,
                });
                if scene.sky_mode > 0 {
                    pass.set_pipeline(&self.pipeline_mesh_sky);
                    pass.set_bind_group(0, &self.mesh_bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
                if !scene.world_verts.is_empty() {
                    pass.set_pipeline(&self.pipeline_mesh_3d);
                    pass.set_bind_group(0, &self.mesh_bind_group, &[]);
                    pass.set_vertex_buffer(0, self.mesh_vbo.slice(..));
                    pass.draw(0..(scene.world_verts.len() as u32), 0..1);
                }
            }
            if !scene.vm_verts.is_empty() {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("infimum-mesh3d-vm-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &self.depth_view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(1.0),
                            store: wgpu::StoreOp::Discard,
                        }),
                        stencil_ops: None,
                    }),
                    occlusion_query_set: None,
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.pipeline_mesh_3d);
                pass.set_bind_group(0, &self.mesh_vm_bind_group, &[]);
                pass.set_vertex_buffer(0, self.mesh_vm_vbo.slice(..));
                pass.draw(0..(scene.vm_verts.len() as u32), 0..1);
            }
        }

        let color_load = if effective_mesh3d.is_some() {
            wgpu::LoadOp::Load
        } else {
            wgpu::LoadOp::Clear(wgpu::Color {
                r: 0.031,
                g: 0.047,
                b: 0.086,
                a: 1.0,
            })
        };

        if !has_3d {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("infimum-1draw-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: color_load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            if !effective_prims.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.instance_buf.slice(..));
                pass.draw(0..6, 0..(effective_prims.len() as u32));
            }
        } else {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("infimum-3d-zbuf-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.031,
                            g: 0.047,
                            b: 0.086,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.instance_buf.slice(..));

            let mut i = 0usize;
            while i < prims.len() {
                let is_3d = (prims[i].meta & 15) == 9;
                let start = i;
                while i < prims.len() && ((prims[i].meta & 15) == 9) == is_3d {
                    i += 1;
                }
                if is_3d {
                    pass.set_pipeline(&self.pipeline_3d);
                } else {
                    pass.set_pipeline(&self.pipeline_2d_depth);
                }
                pass.draw(0..6, (start as u32)..(i as u32));
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        Ok(())
    }
}
