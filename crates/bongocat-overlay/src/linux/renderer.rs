//! Drawing one frame off-screen.
//!
//! The renderer composites the model over a transparent target in the same
//! order as the Metal and D3D11 backends: every mask target first, then the
//! background, the drawables in render order and the key images on top. It
//! reads only the frame it is handed, so a frame can never change the product's
//! state.
//!
//! `wgpu` reports a validation failure by panicking unless an error scope
//! captures it, so every call that can fail runs inside [`GpuErrorScope`] and
//! comes back as an [`OverlayError`]; anything that still escapes a scope is
//! recorded by the device's uncaptured-error handler and reported on the next
//! frame instead of aborting the process.

use super::*;

/// How long a submitted frame may take before the readback gives up.
pub(crate) const GPU_COMPLETION_TIMEOUT: Duration = Duration::from_secs(2);

/// `copy_texture_to_buffer` requires each row of the destination to start on
/// this byte boundary (`wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`).
const ROW_ALIGNMENT: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

/// The adapter a renderer ended up on, for diagnostics and spike evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RendererAdapter {
    pub(crate) backend: String,
    pub(crate) name: String,
    pub(crate) driver: String,
}

/// Which adapter a renderer asks `wgpu` for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdapterRequest {
    pub(crate) backends: wgpu::Backends,
    pub(crate) power_preference: wgpu::PowerPreference,
    /// Ask for the software adapter (lavapipe on Mesa) instead of a GPU.
    pub(crate) force_fallback: bool,
}

impl AdapterRequest {
    /// The product's request: Vulkan first, GLES where Vulkan is missing, and
    /// the integrated GPU on a hybrid laptop, because a desktop pet should not
    /// wake the discrete one.
    pub(crate) const PRODUCT: Self = Self {
        backends: wgpu::Backends::VULKAN.union(wgpu::Backends::GL),
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback: false,
    };
}

/// Captures the validation and out-of-memory errors of the calls made while it
/// is alive. Scopes are per thread, which is why the renderer is single-owner.
pub(crate) struct GpuErrorScope {
    validation: wgpu::ErrorScopeGuard,
    out_of_memory: wgpu::ErrorScopeGuard,
}

impl GpuErrorScope {
    fn push(device: &wgpu::Device) -> Self {
        let out_of_memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        Self {
            validation,
            out_of_memory,
        }
    }

    /// Pop both scopes, innermost first, and report the first error captured.
    pub(crate) fn finish(self, what: &str) -> Result<(), OverlayError> {
        let validation = async_io::block_on(self.validation.pop());
        let out_of_memory = async_io::block_on(self.out_of_memory.pop());
        match validation.or(out_of_memory) {
            None => Ok(()),
            Some(error) => Err(OverlayError::new(format!("wgpu {what}: {error}"))),
        }
    }
}

/// The GPU device a renderer runs on, created once per renderer and kept across
/// window replacements so a hidden-then-shown model window keeps its textures.
pub(crate) struct GpuContext {
    pub(crate) instance: wgpu::Instance,
    pub(crate) adapter: wgpu::Adapter,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    info: RendererAdapter,
    uncaptured: Arc<Mutex<Option<String>>>,
}

impl GpuContext {
    pub(crate) fn instance(request: AdapterRequest) -> wgpu::Instance {
        wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: request.backends,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        })
    }

    /// Open a device on `instance`, compatible with `surface` when one is given.
    pub(crate) fn create(
        instance: wgpu::Instance,
        request: AdapterRequest,
        surface: Option<&wgpu::Surface<'_>>,
    ) -> Result<Self, OverlayError> {
        let adapter = async_io::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: request.power_preference,
            compatible_surface: surface,
            force_fallback_adapter: request.force_fallback,
        }))
        .map_err(|error| OverlayError::new(format!("no wgpu adapter is available: {error}")))?;
        let info = adapter.get_info();
        let (device, queue) = async_io::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("bongocat_overlay_device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits())
                    .using_alignment(adapter.limits()),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            }),
        )
        .map_err(|error| OverlayError::new(format!("create wgpu device: {error}")))?;
        let uncaptured = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&uncaptured);
        device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
            let mut slot = sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.get_or_insert_with(|| error.to_string());
        }));
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            info: RendererAdapter {
                backend: format!("{:?}", info.backend),
                name: info.name,
                driver: info.driver,
            },
            uncaptured,
        })
    }

    pub(crate) fn info(&self) -> &RendererAdapter {
        &self.info
    }

    fn take_uncaptured_error(&self) -> Result<(), OverlayError> {
        let error = self
            .uncaptured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match error {
            None => Ok(()),
            Some(error) => Err(OverlayError::new(format!("wgpu device error: {error}"))),
        }
    }
}

/// The model's GPU state and the pipelines that draw it, independent of where
/// a frame goes: [`OffscreenRenderer`] reads it back, the model window presents
/// it.
pub(crate) struct Renderer {
    pub(crate) context: GpuContext,
    pub(crate) color_format: wgpu::TextureFormat,
    layouts: BindLayouts,
    pipelines: Pipelines,
    sampler: wgpu::Sampler,
    uniforms: UniformRing,
    pub(crate) model_generation: u64,
    resources: Arc<RenderResources>,
    pub(crate) model: GpuModel,
    pub(crate) corner_radius_percent: u8,
    /// The size the mask targets were created for.
    mask_size: (u32, u32),
}

impl Renderer {
    pub(crate) fn create(
        context: GpuContext,
        color_format: wgpu::TextureFormat,
        frame: &RenderFrame,
        width: u32,
        height: u32,
        corner_radius_percent: u8,
    ) -> Result<Self, OverlayError> {
        if width == 0 || height == 0 {
            return Err(OverlayError::new("renderer target has an empty dimension"));
        }
        let device = &context.device;
        let scope = GpuErrorScope::push(device);
        let layouts = BindLayouts::create(device);
        let pipelines = Pipelines::create(device, &layouts, color_format);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("bongocat_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniforms = UniformRing::new(device, &layouts);
        scope.finish("pipeline creation")?;
        let model = GpuModel::prepare(
            device,
            &context.queue,
            &layouts,
            &sampler,
            &frame.resources,
            &frame.snapshot,
            width,
            height,
        )?;
        Ok(Self {
            context,
            color_format,
            layouts,
            pipelines,
            sampler,
            uniforms,
            model_generation: frame.model_generation,
            resources: Arc::clone(&frame.resources),
            model,
            corner_radius_percent,
            mask_size: (width, height),
        })
    }

    /// Apply the newest frame: a new model generation prepares a new GPU model,
    /// anything else updates the current one in place.
    pub(crate) fn sync_frame(&mut self, frame: &RenderFrame) -> Result<bool, OverlayError> {
        if frame.model_generation != self.model_generation {
            validate_model_generation_advance(self.model_generation, frame.model_generation)?;
            let prepared = GpuModel::prepare(
                &self.context.device,
                &self.context.queue,
                &self.layouts,
                &self.sampler,
                &frame.resources,
                &frame.snapshot,
                self.mask_size.0,
                self.mask_size.1,
            )?;
            self.model = prepared;
            self.resources = Arc::clone(&frame.resources);
            self.model_generation = frame.model_generation;
            return Ok(true);
        }
        if !Arc::ptr_eq(&self.resources, &frame.resources) {
            return Err(OverlayError::new(
                "render resources changed within one model generation",
            ));
        }
        self.model
            .sync_snapshot(&self.context.queue, &frame.snapshot)?;
        Ok(false)
    }

    /// Record one frame into `view`. Mask targets follow the target size, so a
    /// resized window re-creates them here before anything draws against them.
    pub(crate) fn encode_frame(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) -> Result<(), OverlayError> {
        self.context.take_uncaptured_error()?;
        if self.mask_size != (width, height) {
            self.model
                .resize_masks(&self.context.device, &self.layouts, width, height);
            self.mask_size = (width, height);
        }
        let scale_offset = model_transform(
            self.model.bounds,
            width as f32,
            height as f32,
            self.model.mirror_horizontal,
        );
        let corner_radius =
            corner_radius_uniform(self.corner_radius_percent, width as f32, height as f32);
        let plan = self.model.plan(scale_offset, corner_radius, width, height);
        let offsets = self.uniforms.upload(
            &self.context.device,
            &self.context.queue,
            &self.layouts,
            plan.iter().map(|draw| draw.uniforms),
        );
        self.encode_mask_passes(encoder, &plan, &offsets)?;
        self.encode_main_pass(encoder, view, &plan, &offsets)
    }

    pub(crate) fn error_scope(&self) -> GpuErrorScope {
        GpuErrorScope::push(&self.context.device)
    }

    /// Render one frame into a throwaway target and run the frame smoke check
    /// on it. A window surface cannot be read back portably, so a frame that
    /// gates a model commit is checked here, at the window's size, before the
    /// same frame is presented.
    pub(crate) fn verify_frame(&mut self, width: u32, height: u32) -> Result<(), OverlayError> {
        let target = RenderTarget::new(&self.context.device, self.color_format, width, height);
        let bytes = self.render_and_read(&target)?;
        verify_frame_smoke(&bytes, target.row_pitch() as usize, width, height).map_err(|error| {
            let info = self.context.info();
            OverlayError::new(format!(
                "{error} ({} {}, {})",
                info.backend, info.name, info.driver
            ))
        })
    }

    fn render_and_read(&mut self, target: &RenderTarget) -> Result<Vec<u8>, OverlayError> {
        let scope = self.error_scope();
        let mut encoder =
            self.context
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("bongocat_readback_frame"),
                });
        self.encode_frame(&mut encoder, &target.view, target.width, target.height)?;
        let readback = target.encode_readback(&self.context.device, &mut encoder);
        let submission = self.context.queue.submit(std::iter::once(encoder.finish()));
        scope.finish("frame submission")?;
        let mut bytes = readback.read(&self.context.device, submission)?;
        if self.color_format == wgpu::TextureFormat::Rgba8Unorm {
            for pixel in bytes.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        self.context.take_uncaptured_error()?;
        Ok(bytes)
    }
}

/// A renderer whose frames are read back instead of presented: the model
/// cover capture and the renderer tests.
pub(crate) struct OffscreenRenderer {
    renderer: Renderer,
    target: RenderTarget,
}

impl OffscreenRenderer {
    /// Create a renderer on the product's adapter for one frame.
    pub(crate) fn create(
        frame: &RenderFrame,
        width: u32,
        height: u32,
        corner_radius_percent: u8,
    ) -> Result<Self, OverlayError> {
        Self::create_on(
            AdapterRequest::PRODUCT,
            frame,
            width,
            height,
            corner_radius_percent,
        )
    }

    pub(crate) fn create_on(
        request: AdapterRequest,
        frame: &RenderFrame,
        width: u32,
        height: u32,
        corner_radius_percent: u8,
    ) -> Result<Self, OverlayError> {
        let context = GpuContext::create(GpuContext::instance(request), request, None)?;
        let renderer = Renderer::create(
            context,
            COLOR_TARGET_FORMAT,
            frame,
            width,
            height,
            corner_radius_percent,
        )?;
        let target =
            RenderTarget::new(&renderer.context.device, COLOR_TARGET_FORMAT, width, height);
        Ok(Self { renderer, target })
    }

    #[cfg(test)]
    pub(crate) fn adapter(&self) -> &RendererAdapter {
        self.renderer.context.info()
    }

    pub(crate) fn sync_frame(&mut self, frame: &RenderFrame) -> Result<bool, OverlayError> {
        self.renderer.sync_frame(frame)
    }

    /// Draw one frame and read it back as straight-alpha RGBA.
    pub(crate) fn draw_capturing(
        &mut self,
        verify_frame: bool,
    ) -> Result<CapturedFrame, OverlayError> {
        let bytes = self.renderer.render_and_read(&self.target)?;
        let row_pitch = self.target.row_pitch() as usize;
        let (width, height) = (self.target.width, self.target.height);
        if verify_frame {
            // Name the adapter: a blank frame on one driver is a driver report,
            // not a model report, and the user's bug report should say which.
            verify_frame_smoke(&bytes, row_pitch, width, height).map_err(|error| {
                let info = self.renderer.context.info();
                OverlayError::new(format!(
                    "{error} ({} {}, {})",
                    info.backend, info.name, info.driver
                ))
            })?;
        }
        CapturedFrame::from_premultiplied_bgra(&bytes, row_pitch, width, height)
    }
}

impl Renderer {
    fn encode_mask_passes(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        plan: &[PlannedDraw],
        offsets: &[u32],
    ) -> Result<(), OverlayError> {
        for mesh in &self.model.meshes {
            let Some(mask) = &mesh.mask else {
                continue;
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("bongocat_mask_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &mask.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            for (draw, offset) in plan.iter().zip(offsets) {
                let PlannedTarget::Mask(target) = draw.target else {
                    continue;
                };
                if target != mesh.id {
                    continue;
                }
                let source = self.model.mesh(draw.source)?;
                pass.set_pipeline(self.pipelines.mask(draw.cull));
                self.bind_mesh(&mut pass, source, *offset, &self.model.empty_mask)?;
                if source.index_count > 0 {
                    pass.draw_indexed(0..source.index_count, 0, 0..1);
                }
            }
        }
        Ok(())
    }

    fn encode_main_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        plan: &[PlannedDraw],
        offsets: &[u32],
    ) -> Result<(), OverlayError> {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("bongocat_main_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
        for (draw, offset) in plan.iter().zip(offsets) {
            match draw.target {
                PlannedTarget::Mask(_) => {}
                PlannedTarget::Background => {
                    let Some(background) = &self.model.background else {
                        continue;
                    };
                    pass.set_pipeline(self.pipelines.draw(BlendMode::Normal, draw.cull));
                    self.bind_quad(
                        &mut pass,
                        &self.model.background_vertex_buffer,
                        background,
                        *offset,
                    );
                    pass.draw_indexed(0..6, 0, 0..1);
                }
                PlannedTarget::Drawable => {
                    let mesh = self.model.mesh(draw.source)?;
                    pass.set_pipeline(self.pipelines.draw(mesh.blend_mode, draw.cull));
                    let mask = mesh
                        .mask
                        .as_ref()
                        .map_or(&self.model.empty_mask, |mask| &mask.bind_group);
                    self.bind_mesh(&mut pass, mesh, *offset, mask)?;
                    if mesh.index_count > 0 {
                        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                    }
                }
                PlannedTarget::Key(asset_id) => {
                    let (Some(texture), Some(vertices)) = (
                        self.model.key_textures.get(&asset_id),
                        self.model.key_overlay_buffers.get(&asset_id),
                    ) else {
                        continue;
                    };
                    pass.set_pipeline(self.pipelines.draw(BlendMode::Normal, draw.cull));
                    self.bind_quad(&mut pass, vertices, texture, *offset);
                    pass.draw_indexed(0..6, 0, 0..1);
                }
            }
        }
        Ok(())
    }

    fn bind_mesh(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        mesh: &Mesh,
        offset: u32,
        mask: &wgpu::BindGroup,
    ) -> Result<(), OverlayError> {
        let texture = self.model.textures.get(&mesh.texture_id).ok_or_else(|| {
            OverlayError::new(format!("texture {} is unavailable", mesh.texture_id))
        })?;
        pass.set_bind_group(0, &self.uniforms.bind_group, &[offset]);
        pass.set_bind_group(1, &texture.bind_group, &[]);
        pass.set_bind_group(2, mask, &[]);
        pass.set_vertex_buffer(0, mesh.vertex_buffer.slice(..));
        pass.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
        Ok(())
    }

    fn bind_quad(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        vertices: &wgpu::Buffer,
        texture: &GpuTexture,
        offset: u32,
    ) {
        pass.set_bind_group(0, &self.uniforms.bind_group, &[offset]);
        pass.set_bind_group(1, &texture.bind_group, &[]);
        pass.set_bind_group(2, &self.model.empty_mask, &[]);
        pass.set_vertex_buffer(0, vertices.slice(..));
        pass.set_index_buffer(
            self.model.quad_index_buffer.slice(..),
            wgpu::IndexFormat::Uint16,
        );
    }
}

/// Fit the model into the target without distorting it; identical to the
/// Metal backend's transform so both place the model the same way.
pub(crate) fn model_transform(
    bounds: ModelBounds,
    width: f32,
    height: f32,
    mirror_horizontal: bool,
) -> [f32; 4] {
    let model_width = bounds.width();
    let model_height = bounds.height();
    let center = bounds.center();
    let model_aspect = model_width / model_height;
    let viewport_aspect = width / height;
    let (mut scale_x, mut scale_y) = (2.0 / model_width, 2.0 / model_height);
    if viewport_aspect > model_aspect {
        scale_x *= model_aspect / viewport_aspect;
    } else {
        scale_y *= viewport_aspect / model_aspect;
    }
    let mut offset_x = -center[0] * scale_x;
    if mirror_horizontal {
        scale_x = -scale_x;
        offset_x = -offset_x;
    }
    [scale_x, scale_y, offset_x, -center[1] * scale_y]
}

/// What one planned draw writes into.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlannedTarget {
    /// A mask source drawn into the mask target of the drawable it clips.
    Mask(DrawableId),
    Background,
    Drawable,
    Key(KeyAssetId),
}

/// One draw call in frame order, with the uniforms it reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PlannedDraw {
    pub(crate) target: PlannedTarget,
    /// The drawable whose geometry is drawn; unused for quads.
    pub(crate) source: DrawableId,
    pub(crate) cull: DrawableCullMode,
    pub(crate) uniforms: Uniforms,
}

pub(crate) struct GpuTexture {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub(crate) struct MaskTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

pub(crate) struct Mesh {
    pub(crate) id: DrawableId,
    pub(crate) render_order: i32,
    vertex_buffer: wgpu::Buffer,
    /// Byte length of the snapshot's own vertex array; zero when the drawable
    /// has no vertices and the buffer holds a placeholder.
    vertex_bytes: usize,
    index_buffer: wgpu::Buffer,
    indices: Vec<u16>,
    index_count: u32,
    texture_id: TextureId,
    pub(crate) opacity: f32,
    pub(crate) blend_mode: BlendMode,
    multiply_color: [f32; 4],
    screen_color: [f32; 4],
    pub(crate) masks: Vec<DrawableId>,
    pub(crate) visible: bool,
    pub(crate) double_sided: bool,
    inverted_mask: bool,
    mask: Option<MaskTarget>,
}

pub(crate) struct GpuModel {
    textures: BTreeMap<TextureId, GpuTexture>,
    key_textures: BTreeMap<KeyAssetId, GpuTexture>,
    key_overlay_buffers: BTreeMap<KeyAssetId, wgpu::Buffer>,
    background: Option<GpuTexture>,
    background_vertex_buffer: wgpu::Buffer,
    quad_index_buffer: wgpu::Buffer,
    pub(crate) meshes: Vec<Mesh>,
    empty_mask: wgpu::BindGroup,
    bounds: ModelBounds,
    model_opacity: f32,
    mirror_horizontal: bool,
    active_keys: Vec<KeyOverlay>,
}

impl GpuModel {
    #[expect(
        clippy::too_many_arguments,
        reason = "the device objects and the frame are separate owners"
    )]
    pub(crate) fn prepare(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layouts: &BindLayouts,
        sampler: &wgpu::Sampler,
        resources: &RenderResources,
        snapshot: &RenderSnapshot,
        width: u32,
        height: u32,
    ) -> Result<Self, OverlayError> {
        validate_render_snapshot(resources, snapshot)
            .map_err(|error| OverlayError::new(error.to_string()))?;
        let scope = GpuErrorScope::push(device);
        let upload = |asset: &TextureAsset| load_texture(device, queue, layouts, sampler, asset);
        let textures = resources
            .textures
            .iter()
            .map(|asset| upload(asset).map(|texture| (asset.id, texture)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let key_textures = resources
            .key_assets
            .iter()
            .map(|asset| {
                upload(&TextureAsset {
                    id: TextureId::new(asset.id.index()),
                    path: asset.path.clone(),
                    width: asset.width,
                    height: asset.height,
                })
                .map(|texture| (asset.id, texture))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let background = resources
            .background
            .as_ref()
            .map(|asset| {
                upload(&TextureAsset {
                    id: TextureId::new(usize::MAX),
                    path: asset.path.clone(),
                    width: asset.width,
                    height: asset.height,
                })
            })
            .transpose()?;
        let canvas_bounds = ModelBounds::from_canvas(snapshot.canvas);
        let key_overlay_buffers = resources
            .key_assets
            .iter()
            .map(|asset| {
                let vertices = quad_vertices(legacy_key_overlay_bounds(
                    canvas_bounds,
                    asset.width,
                    asset.height,
                ));
                (
                    asset.id,
                    buffer_with(device, "key_overlay", &vertex_bytes(&vertices), VERTEX),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut meshes = snapshot
            .drawables
            .iter()
            .map(|drawable| {
                // A zero-length buffer cannot be bound, and a drawable the Core
                // reports without triangles has nothing to upload, so both arrays
                // fall back to one placeholder element exactly like the Metal
                // backend. `vertex_bytes` keeps the snapshot's own length so a
                // changed vertex count is still caught by `sync_snapshot`.
                let vertices = if drawable.vertices.is_empty() {
                    vertex_bytes(&[bongocat_render::Vertex {
                        position: [0.0, 0.0],
                        uv: [0.0, 0.0],
                    }])
                } else {
                    vertex_bytes(&drawable.vertices)
                };
                let indices = if drawable.indices.is_empty() {
                    index_bytes(&[0])
                } else {
                    index_bytes(&drawable.indices)
                };
                let index_count = u32::try_from(drawable.indices.len()).map_err(|_| {
                    OverlayError::new(format!("drawable {} has too many indices", drawable.id))
                })?;
                Ok(Mesh {
                    id: drawable.id,
                    render_order: drawable.render_order,
                    vertex_buffer: buffer_with(device, "drawable_vertices", &vertices, VERTEX),
                    vertex_bytes: std::mem::size_of_val(drawable.vertices.as_slice()),
                    index_buffer: buffer_with(device, "drawable_indices", &indices, INDEX),
                    indices: drawable.indices.clone(),
                    index_count,
                    texture_id: drawable.texture_id,
                    opacity: drawable.opacity,
                    blend_mode: drawable.blend_mode,
                    multiply_color: drawable.multiply_color,
                    screen_color: drawable.screen_color,
                    masks: drawable.masks.clone(),
                    visible: drawable.visible,
                    double_sided: drawable.double_sided,
                    inverted_mask: drawable.inverted_mask,
                    mask: (!drawable.masks.is_empty())
                        .then(|| create_mask_target(device, layouts, width, height)),
                })
            })
            .collect::<Result<Vec<_>, OverlayError>>()?;
        meshes.sort_by_key(|mesh| (mesh.render_order, mesh.id));
        let background_vertices = vertex_bytes(&quad_vertices(canvas_bounds));
        let model = Self {
            textures,
            key_textures,
            key_overlay_buffers,
            background,
            background_vertex_buffer: buffer_with(
                device,
                "background_vertices",
                &background_vertices,
                VERTEX,
            ),
            quad_index_buffer: buffer_with(
                device,
                "quad_indices",
                &index_bytes(&[0, 1, 2, 0, 2, 3]),
                INDEX,
            ),
            meshes,
            empty_mask: create_empty_mask(device, queue, layouts),
            bounds: snapshot.bounds,
            model_opacity: snapshot.model_opacity,
            mirror_horizontal: snapshot.mirror_horizontal,
            active_keys: snapshot.active_keys.clone(),
        };
        scope.finish("model preparation")?;
        Ok(model)
    }

    /// Update the prepared model from a newer snapshot of the same generation.
    /// The rules are the Metal backend's: topology, indices and vertex counts
    /// are immutable within a generation, everything else may change.
    pub(crate) fn sync_snapshot(
        &mut self,
        queue: &wgpu::Queue,
        snapshot: &RenderSnapshot,
    ) -> Result<(), OverlayError> {
        if !snapshot.model_opacity.is_finite() || !(0.0..=1.0).contains(&snapshot.model_opacity) {
            return Err(OverlayError::new("model opacity is outside [0, 1]"));
        }
        if snapshot.drawables.len() != self.meshes.len() {
            return Err(OverlayError::new(format!(
                "drawable count changed from {} to {}",
                self.meshes.len(),
                snapshot.drawables.len()
            )));
        }
        for drawable in &snapshot.drawables {
            let mesh = self
                .meshes
                .iter_mut()
                .find(|mesh| mesh.id == drawable.id)
                .ok_or_else(|| {
                    OverlayError::new(format!("drawable source {} is unavailable", drawable.id))
                })?;
            if mesh.mask.is_some() != !drawable.masks.is_empty() {
                return Err(OverlayError::new(format!(
                    "drawable {} changed clipping topology",
                    drawable.id
                )));
            }
            if mesh.indices != drawable.indices {
                return Err(OverlayError::new(format!(
                    "drawable {} changed immutable triangle indices",
                    drawable.id
                )));
            }
            if mesh.vertex_bytes != std::mem::size_of_val(drawable.vertices.as_slice()) {
                return Err(OverlayError::new(format!(
                    "drawable {} changed vertex buffer size",
                    drawable.id
                )));
            }
            if drawable.dynamic_flags.vertex_positions_changed && !drawable.vertices.is_empty() {
                queue.write_buffer(&mesh.vertex_buffer, 0, &vertex_bytes(&drawable.vertices));
            }
            mesh.render_order = drawable.render_order;
            mesh.texture_id = drawable.texture_id;
            mesh.opacity = drawable.opacity;
            mesh.blend_mode = drawable.blend_mode;
            mesh.multiply_color = drawable.multiply_color;
            mesh.screen_color = drawable.screen_color;
            mesh.masks.clone_from(&drawable.masks);
            mesh.visible = drawable.visible;
            mesh.double_sided = drawable.double_sided;
            mesh.inverted_mask = drawable.inverted_mask;
        }
        self.meshes.sort_by_key(|mesh| (mesh.render_order, mesh.id));
        self.bounds = snapshot.bounds;
        self.active_keys.clone_from(&snapshot.active_keys);
        self.model_opacity = snapshot.model_opacity;
        self.mirror_horizontal = snapshot.mirror_horizontal;
        Ok(())
    }

    pub(crate) fn drawable_count(&self) -> usize {
        self.meshes.len()
    }

    pub(crate) fn masked_drawable_count(&self) -> usize {
        self.meshes
            .iter()
            .filter(|mesh| mesh.mask.is_some())
            .count()
    }

    pub(crate) fn texture_count(&self) -> usize {
        self.textures.len()
    }

    /// Re-create every mask target at a new target size. Textures and buffers
    /// are size-independent and stay, so a resize never reloads model assets.
    pub(crate) fn resize_masks(
        &mut self,
        device: &wgpu::Device,
        layouts: &BindLayouts,
        width: u32,
        height: u32,
    ) {
        for mesh in &mut self.meshes {
            if mesh.mask.is_some() {
                mesh.mask = Some(create_mask_target(device, layouts, width, height));
            }
        }
    }

    fn mesh(&self, id: DrawableId) -> Result<&Mesh, OverlayError> {
        self.meshes
            .iter()
            .find(|mesh| mesh.id == id)
            .ok_or_else(|| OverlayError::new(format!("mask source {id} is unavailable")))
    }

    /// Every draw of one frame, in the order the backends composite it. Kept
    /// free of GPU calls so the order and the uniforms can be tested alone.
    pub(crate) fn plan(
        &self,
        scale_offset: [f32; 4],
        corner_radius: [f32; 4],
        width: u32,
        height: u32,
    ) -> Vec<PlannedDraw> {
        let mut draws = Vec::new();
        for mesh in &self.meshes {
            if mesh.mask.is_none() {
                continue;
            }
            for source_id in &mesh.masks {
                let Some(source) = self.meshes.iter().find(|source| source.id == *source_id) else {
                    // Reported by `mesh` when the pass is encoded.
                    draws.push(PlannedDraw {
                        target: PlannedTarget::Mask(mesh.id),
                        source: *source_id,
                        cull: DrawableCullMode::None,
                        uniforms: Uniforms::plain(scale_offset, [0.0; 4]),
                    });
                    continue;
                };
                draws.push(PlannedDraw {
                    target: PlannedTarget::Mask(mesh.id),
                    source: source.id,
                    cull: drawable_cull_mode(source.double_sided, self.mirror_horizontal),
                    uniforms: Uniforms::plain(scale_offset, [0.0; 4]),
                });
            }
        }
        if self.background.is_some() {
            draws.push(PlannedDraw {
                target: PlannedTarget::Background,
                source: DrawableId::new(0),
                cull: DrawableCullMode::None,
                uniforms: Uniforms::plain(scale_offset, corner_radius),
            });
        }
        for mesh in &self.meshes {
            if !mesh.visible || mesh.opacity <= 0.0 {
                continue;
            }
            draws.push(PlannedDraw {
                target: PlannedTarget::Drawable,
                source: mesh.id,
                cull: drawable_cull_mode(mesh.double_sided, self.mirror_horizontal),
                uniforms: Uniforms {
                    scale_offset,
                    multiply_color: mesh.multiply_color,
                    screen_color: mesh.screen_color,
                    mask_settings: [
                        width as f32,
                        height as f32,
                        f32::from(mesh.mask.is_some()),
                        f32::from(mesh.inverted_mask),
                    ],
                    corner_radius,
                    opacity: mesh.opacity * self.model_opacity,
                },
            });
        }
        // Key images are the topmost layer so pressed-key imagery stays visible
        // above both the background and the Live2D drawables.
        for overlay in &self.active_keys {
            draws.push(PlannedDraw {
                target: PlannedTarget::Key(overlay.asset_id),
                source: DrawableId::new(0),
                cull: DrawableCullMode::None,
                uniforms: Uniforms::plain(scale_offset, corner_radius),
            });
        }
        draws
    }
}

const VERTEX: wgpu::BufferUsages = wgpu::BufferUsages::VERTEX.union(wgpu::BufferUsages::COPY_DST);
const INDEX: wgpu::BufferUsages = wgpu::BufferUsages::INDEX;

/// A buffer holding `contents`, padded to the 4-byte copy alignment.
fn buffer_with(
    device: &wgpu::Device,
    label: &str,
    contents: &[u8],
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    use wgpu::util::DeviceExt;
    let mut padded = contents.to_vec();
    padded.resize(contents.len().next_multiple_of(4).max(4), 0);
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: &padded,
        usage,
    })
}

fn load_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layouts: &BindLayouts,
    sampler: &wgpu::Sampler,
    asset: &TextureAsset,
) -> Result<GpuTexture, OverlayError> {
    let image = ImageReader::open(&asset.path)
        .map_err(|error| OverlayError::new(format!("open {}: {error}", asset.path.display())))?
        .decode()
        .map_err(|error| OverlayError::new(format!("decode {}: {error}", asset.path.display())))?
        .into_rgba8();
    if image.width() != asset.width || image.height() != asset.height {
        return Err(OverlayError::new(format!(
            "texture dimensions changed for {}",
            asset.path.display()
        )));
    }
    let size = wgpu::Extent3d {
        width: asset.width,
        height: asset.height,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("model_texture"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: MODEL_TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        image.as_raw(),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(asset.width * 4),
            rows_per_image: Some(asset.height),
        },
        size,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("model_texture_bind_group"),
        layout: &layouts.texture,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    });
    Ok(GpuTexture {
        _texture: texture,
        bind_group,
    })
}

fn mask_bind_group(
    device: &wgpu::Device,
    layouts: &BindLayouts,
    view: &wgpu::TextureView,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("mask_bind_group"),
        layout: &layouts.mask,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(view),
        }],
    })
}

fn create_mask_target(
    device: &wgpu::Device,
    layouts: &BindLayouts,
    width: u32,
    height: u32,
) -> MaskTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("mask_target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: MASK_TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = mask_bind_group(device, layouts, &view);
    MaskTarget {
        _texture: texture,
        view,
        bind_group,
    }
}

/// The 1x1 transparent mask bound to every unmasked draw, as on Metal.
fn create_empty_mask(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layouts: &BindLayouts,
) -> wgpu::BindGroup {
    let size = wgpu::Extent3d {
        width: 1,
        height: 1,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("empty_mask"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: MASK_TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &[0, 0, 0, 0],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4),
            rows_per_image: Some(1),
        },
        size,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    mask_bind_group(device, layouts, &view)
}

/// One frame's uniforms, each in its own aligned slot of one buffer that the
/// draws select with a dynamic offset. The buffer grows when a frame needs
/// more slots than the last one and is otherwise reused.
struct UniformRing {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    stride: u64,
    slots: u64,
}

impl UniformRing {
    const INITIAL_SLOTS: u64 = 256;

    fn new(device: &wgpu::Device, layouts: &BindLayouts) -> Self {
        let alignment = u64::from(device.limits().min_uniform_buffer_offset_alignment);
        let stride = UNIFORMS_SIZE.next_multiple_of(alignment);
        Self::with_slots(device, layouts, stride, Self::INITIAL_SLOTS)
    }

    fn with_slots(device: &wgpu::Device, layouts: &BindLayouts, stride: u64, slots: u64) -> Self {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniform_ring"),
            size: stride * slots,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uniform_ring_bind_group"),
            layout: &layouts.uniforms,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &buffer,
                    offset: 0,
                    size: wgpu::BufferSize::new(UNIFORMS_SIZE),
                }),
            }],
        });
        Self {
            buffer,
            bind_group,
            stride,
            slots,
        }
    }

    /// Write every draw's uniforms and return each draw's dynamic offset.
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layouts: &BindLayouts,
        uniforms: impl ExactSizeIterator<Item = Uniforms>,
    ) -> Vec<u32> {
        let count = uniforms.len() as u64;
        if count > self.slots {
            *self = Self::with_slots(device, layouts, self.stride, count.next_power_of_two());
        }
        let mut bytes = vec![0_u8; (self.stride * count.max(1)) as usize];
        let mut offsets = Vec::with_capacity(count as usize);
        for (index, value) in uniforms.enumerate() {
            let start = index * self.stride as usize;
            bytes[start..start + UNIFORMS_SIZE as usize].copy_from_slice(&value.to_bytes());
            offsets.push(u32::try_from(start).unwrap_or(u32::MAX));
        }
        queue.write_buffer(&self.buffer, 0, &bytes);
        offsets
    }
}

struct RenderTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl RenderTarget {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame_target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            width,
            height,
        }
    }

    fn row_pitch(&self) -> u32 {
        (self.width * 4).next_multiple_of(ROW_ALIGNMENT)
    }

    fn encode_readback(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Readback {
        let row_pitch = self.row_pitch();
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame_readback"),
            size: u64::from(row_pitch) * u64::from(self.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_pitch),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        Readback { buffer }
    }
}

struct Readback {
    buffer: wgpu::Buffer,
}

impl Readback {
    /// Wait for the submission, then copy the padded BGRA rows out.
    fn read(
        self,
        device: &wgpu::Device,
        submission: wgpu::SubmissionIndex,
    ) -> Result<Vec<u8>, OverlayError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(GPU_COMPLETION_TIMEOUT),
            })
            .map_err(|error| OverlayError::new(format!("wait for the GPU frame: {error}")))?;
        receiver
            .recv_timeout(GPU_COMPLETION_TIMEOUT)
            .map_err(|_| OverlayError::new("the GPU frame readback never completed"))?
            .map_err(|error| OverlayError::new(format!("map the GPU frame: {error}")))?;
        let bytes = self.buffer.slice(..).get_mapped_range().to_vec();
        self.buffer.unmap();
        Ok(bytes)
    }
}

/// The same 17x17 grid the other backends sample, read from the BGRA rows.
pub(crate) fn verify_frame_smoke(
    bytes: &[u8],
    row_pitch: usize,
    width: u32,
    height: u32,
) -> Result<(), OverlayError> {
    let grid = FRAME_SMOKE_GRID_DIMENSION;
    let mut pixels = Vec::with_capacity((grid * grid) as usize);
    for y in 0..grid {
        for x in 0..grid {
            let column = (u64::from(width).saturating_sub(1) * x / (grid - 1)) as usize;
            let row = (u64::from(height).saturating_sub(1) * y / (grid - 1)) as usize;
            let start = row * row_pitch + column * 4;
            let pixel: [u8; 4] = bytes
                .get(start..start + 4)
                .and_then(|slice| slice.try_into().ok())
                .ok_or_else(|| OverlayError::new("frame readback is shorter than the target"))?;
            pixels.push(pixel);
        }
    }
    validate_frame_smoke(pixels).map_err(|error| OverlayError::new(format!("wgpu {error}")))?;
    Ok(())
}
