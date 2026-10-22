//! The shader and the pipeline states the frame is drawn with.
//!
//! The colour formats follow the encoded-space contract in ADR-0063, exactly as
//! the Metal and D3D11 backends do: the model textures and the target are UNORM,
//! so nothing decodes the source or re-encodes the composited frame, alpha is
//! premultiplied for the compositor, and masks carry coverage only.
//!
//! `wgpu` bakes the cull mode into the pipeline, where Metal and D3D11 set it per
//! draw, so there is one pipeline per blend mode and cull mode pair.

use super::*;

/// The off-screen composited frame. BGRA matches what the other backends read
/// back, so the shared [`CapturedFrame::from_premultiplied_bgra`] conversion
/// applies as is. A window uses whichever of BGRA and RGBA UNORM its surface
/// offers.
pub(crate) const COLOR_TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

pub(crate) const MODEL_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

pub(crate) const MASK_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// A line-for-line port of the Metal shader in `macos/pipelines.rs`. Keep the
/// two in step: a difference here is a difference in what the user sees.
pub(crate) const SHADER_SOURCE: &str = r"
struct Uniforms {
    scale_offset: vec4<f32>,
    multiply_color: vec4<f32>,
    screen_color: vec4<f32>,
    mask_settings: vec4<f32>,
    corner_radius: vec4<f32>,
    // Model/drawable opacity in `x`. Window presentation opacity is applied
    // once to the completed surface, never per drawable.
    opacity: vec4<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(1) @binding(0) var model_texture: texture_2d<f32>;
@group(1) @binding(1) var texture_sampler: sampler;
@group(2) @binding(0) var mask_texture: texture_2d<f32>;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
};

struct RasterVertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Legacy window rounding: `corner_radius.x` is the radius as a fraction of the
// window box and `corner_radius.yz` the target size, so the arcs stay
// elliptical on a non-square window like a CSS percentage `border-radius`.
fn corner_coverage(position: vec2<f32>, corner_radius: vec4<f32>) -> f32 {
    let radius = min(corner_radius.x, 0.5);
    if (radius <= 0.0) {
        return 1.0;
    }
    let uv = position / corner_radius.yz;
    let centered = abs(uv * 2.0 - 1.0);
    let extent = 2.0 * radius;
    let delta = centered - 1.0 + extent;
    let distance = length(max(delta, vec2<f32>(0.0))) + min(max(delta.x, delta.y), 0.0) - extent;
    let scale = 0.5 * min(corner_radius.y, corner_radius.z);
    return saturate(0.5 - distance * scale);
}

@vertex
fn cubism_vertex(input: VertexInput) -> RasterVertex {
    var output: RasterVertex;
    let clip = input.position * uniforms.scale_offset.xy + uniforms.scale_offset.zw;
    output.position = vec4<f32>(clip, 0.0, 1.0);
    output.uv = vec2<f32>(input.uv.x, 1.0 - input.uv.y);
    return output;
}

@fragment
fn cubism_fragment(input: RasterVertex) -> @location(0) vec4<f32> {
    let texture_color = textureSample(model_texture, texture_sampler, input.uv);
    var color = texture_color.rgb * uniforms.multiply_color.rgb;
    color = color + uniforms.screen_color.rgb - color * uniforms.screen_color.rgb;
    // WGSL only samples in uniform control flow, so the mask is always sampled
    // and then discarded when the drawable has none. The divisor is clamped
    // because an unmasked draw carries no target size.
    let mask_uv = input.position.xy / max(uniforms.mask_settings.xy, vec2<f32>(1.0));
    var mask = textureSample(mask_texture, texture_sampler, mask_uv).a;
    if (uniforms.mask_settings.w > 0.5) {
        mask = 1.0 - mask;
    }
    if (uniforms.mask_settings.z <= 0.5) {
        mask = 1.0;
    }
    let alpha = texture_color.a * uniforms.opacity.x * mask
        * corner_coverage(input.position.xy, uniforms.corner_radius);
    return vec4<f32>(color * alpha, alpha);
}

@fragment
fn cubism_mask_fragment(input: RasterVertex) -> @location(0) vec4<f32> {
    let alpha = textureSample(model_texture, texture_sampler, input.uv).a;
    return vec4<f32>(0.0, 0.0, 0.0, alpha);
}
";

/// One draw's uniforms, in the shader's layout: six `vec4<f32>`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Uniforms {
    pub(crate) scale_offset: [f32; 4],
    pub(crate) multiply_color: [f32; 4],
    pub(crate) screen_color: [f32; 4],
    pub(crate) mask_settings: [f32; 4],
    pub(crate) corner_radius: [f32; 4],
    pub(crate) opacity: f32,
}

pub(crate) const UNIFORMS_SIZE: u64 = 96;

impl Uniforms {
    /// The uniforms of a draw with no colour adjustment and no mask: the
    /// background, a key image and every mask-source draw.
    pub(crate) const fn plain(scale_offset: [f32; 4], corner_radius: [f32; 4]) -> Self {
        Self {
            scale_offset,
            multiply_color: [1.0; 4],
            screen_color: [0.0; 4],
            mask_settings: [0.0; 4],
            corner_radius,
            opacity: 1.0,
        }
    }

    /// Little-endian bytes in declaration order. Encoding field by field keeps
    /// the upload safe: there is no `repr(C)` struct to reinterpret.
    pub(crate) fn to_bytes(self) -> [u8; UNIFORMS_SIZE as usize] {
        let mut bytes = [0_u8; UNIFORMS_SIZE as usize];
        let values = [
            self.scale_offset,
            self.multiply_color,
            self.screen_color,
            self.mask_settings,
            self.corner_radius,
            [self.opacity, 0.0, 0.0, 0.0],
        ];
        for (chunk, value) in bytes
            .chunks_exact_mut(4)
            .zip(values.iter().flatten().copied())
        {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
}

/// The vertex layout of `bongocat_render::Vertex`: position then UV, both `f32x2`.
pub(crate) const VERTEX_STRIDE: u64 = 16;

pub(crate) fn vertex_bytes(vertices: &[bongocat_render::Vertex]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vertices.len() * VERTEX_STRIDE as usize);
    for vertex in vertices {
        for value in [
            vertex.position[0],
            vertex.position[1],
            vertex.uv[0],
            vertex.uv[1],
        ] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

pub(crate) fn index_bytes(indices: &[u16]) -> Vec<u8> {
    indices
        .iter()
        .flat_map(|index| index.to_le_bytes())
        .collect()
}

pub(crate) struct BindLayouts {
    pub(crate) uniforms: wgpu::BindGroupLayout,
    pub(crate) texture: wgpu::BindGroupLayout,
    pub(crate) mask: wgpu::BindGroupLayout,
}

impl BindLayouts {
    pub(crate) fn create(device: &wgpu::Device) -> Self {
        let uniforms = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bongocat_uniforms_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(UNIFORMS_SIZE),
                },
                count: None,
            }],
        });
        let texture = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bongocat_texture_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let mask = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bongocat_mask_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        Self {
            uniforms,
            texture,
            mask,
        }
    }
}

const CULL_MODES: [DrawableCullMode; 3] = [
    DrawableCullMode::None,
    DrawableCullMode::Front,
    DrawableCullMode::Back,
];

const BLEND_MODES: [BlendMode; 3] = [
    BlendMode::Normal,
    BlendMode::Additive,
    BlendMode::Multiplicative,
];

pub(crate) struct Pipelines {
    /// Indexed `[blend][cull]` in the order of [`BLEND_MODES`] and [`CULL_MODES`].
    draw: [[wgpu::RenderPipeline; 3]; 3],
    /// Indexed by cull mode.
    mask: [wgpu::RenderPipeline; 3],
}

impl Pipelines {
    pub(crate) fn create(
        device: &wgpu::Device,
        layouts: &BindLayouts,
        color_format: wgpu::TextureFormat,
    ) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bongocat_cubism_shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SOURCE.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bongocat_pipeline_layout"),
            bind_group_layouts: &[
                Some(&layouts.uniforms),
                Some(&layouts.texture),
                Some(&layouts.mask),
            ],
            immediate_size: 0,
        });
        let draw = BLEND_MODES.map(|blend| {
            CULL_MODES.map(|cull| {
                create_pipeline(
                    device,
                    &layout,
                    &module,
                    "cubism_fragment",
                    color_format,
                    blend,
                    cull,
                )
            })
        });
        let mask = CULL_MODES.map(|cull| {
            create_pipeline(
                device,
                &layout,
                &module,
                "cubism_mask_fragment",
                MASK_TEXTURE_FORMAT,
                BlendMode::Normal,
                cull,
            )
        });
        Self { draw, mask }
    }

    pub(crate) fn draw(&self, mode: BlendMode, cull: DrawableCullMode) -> &wgpu::RenderPipeline {
        &self.draw[blend_index(mode)][cull_index(cull)]
    }

    pub(crate) fn mask(&self, cull: DrawableCullMode) -> &wgpu::RenderPipeline {
        &self.mask[cull_index(cull)]
    }
}

const fn blend_index(mode: BlendMode) -> usize {
    match mode {
        BlendMode::Normal => 0,
        BlendMode::Additive => 1,
        BlendMode::Multiplicative => 2,
    }
}

const fn cull_index(cull: DrawableCullMode) -> usize {
    match cull {
        DrawableCullMode::None => 0,
        DrawableCullMode::Front => 1,
        DrawableCullMode::Back => 2,
    }
}

fn create_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    fragment_entry: &str,
    format: wgpu::TextureFormat,
    mode: BlendMode,
    cull: DrawableCullMode,
) -> wgpu::RenderPipeline {
    let factors = blend_factors(mode);
    let component = |source, destination| wgpu::BlendComponent {
        src_factor: wgpu_blend_factor(source),
        dst_factor: wgpu_blend_factor(destination),
        operation: wgpu::BlendOperation::Add,
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(fragment_entry),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("cubism_vertex"),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: VERTEX_STRIDE,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
            }],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fragment_entry),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState {
                    color: component(factors.source_rgb, factors.destination_rgb),
                    alpha: component(factors.source_alpha, factors.destination_alpha),
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: wgpu_cull_mode(cull),
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

pub(crate) const fn wgpu_blend_factor(factor: BlendFactor) -> wgpu::BlendFactor {
    match factor {
        BlendFactor::Zero => wgpu::BlendFactor::Zero,
        BlendFactor::One => wgpu::BlendFactor::One,
        BlendFactor::OneMinusSourceAlpha => wgpu::BlendFactor::OneMinusSrcAlpha,
        BlendFactor::DestinationColor => wgpu::BlendFactor::Dst,
    }
}

pub(crate) const fn wgpu_cull_mode(cull: DrawableCullMode) -> Option<wgpu::Face> {
    match cull {
        DrawableCullMode::None => None,
        DrawableCullMode::Front => Some(wgpu::Face::Front),
        DrawableCullMode::Back => Some(wgpu::Face::Back),
    }
}
