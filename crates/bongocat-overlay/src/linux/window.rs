//! Presenting the renderer's frames into the model window.

use super::*;

/// The model window and the `wgpu` surface that presents into it.
///
/// `surface` is declared before `window` so it drops first: the `wgpu` surface
/// holds raw pointers to the window and to the display connection (a
/// `wl_surface` and `wl_display`, or an X11 window and `xcb_connection_t`), and
/// must never outlive either. The display itself is owned by the session, which
/// declares it after this window for the same reason.
pub(crate) struct ModelWindow {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pub(crate) window: NativeWindow,
}

/// Open a layer window and the `wgpu` surface on it, before any device exists:
/// the device is chosen to be compatible with this surface.
pub(crate) fn open_window(
    display: &mut NativeDisplay,
    instance: &wgpu::Instance,
    bounds: Option<OverlayWindowBounds>,
    default_size: (u32, u32),
    options: OverlaySessionOptions,
) -> Result<(NativeWindow, wgpu::Surface<'static>), OverlayError> {
    let window = display.create_window(bounds, default_size, options)?;
    let (raw_display_handle, raw_window_handle) = display.raw_handles(&window)?;
    // SAFETY: both handles point at live objects of `display`'s connection: the
    // connection lives as long as `display`, and the window as long as `window`
    // (an X11 window until `NativeWindow::close`). The surface returned here is only ever stored in a
    // `ModelWindow` together with `window`, declared before it so it drops
    // first, and the session owning that `ModelWindow` declares `display` after
    // it. Neither object is therefore destroyed while the surface exists.
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(raw_display_handle),
            raw_window_handle,
        })
    }
    .map_err(|error| OverlayError::new(format!("create the window GPU surface: {error}")))?;
    Ok((window, surface))
}

/// The colour format the window renders in: UNORM BGRA, else UNORM RGBA. An
/// sRGB format would re-encode the composited frame (ADR-0063).
pub(crate) fn window_color_format(
    surface: &wgpu::Surface<'_>,
    adapter: &wgpu::Adapter,
) -> Result<wgpu::TextureFormat, OverlayError> {
    let capabilities = surface.get_capabilities(adapter);
    [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Rgba8Unorm,
    ]
    .into_iter()
    .find(|format| capabilities.formats.contains(format))
    .ok_or_else(|| {
        OverlayError::new(format!(
            "the window surface offers no UNORM colour format: {:?}",
            capabilities.formats
        ))
    })
}

impl ModelWindow {
    pub(crate) fn new(
        window: NativeWindow,
        surface: wgpu::Surface<'static>,
        renderer: &Renderer,
        display: &NativeDisplay,
    ) -> Result<Self, OverlayError> {
        let capabilities = surface.get_capabilities(&renderer.context.adapter);
        // A transparent desktop pet needs the compositor to blend the frame by
        // its alpha. Premultiplied is what the renderer writes; `Inherit` on
        // Wayland is the compositor's default, which is premultiplied too.
        let alpha_mode = [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::Inherit,
        ]
        .into_iter()
        .find(|mode| capabilities.alpha_modes.contains(mode))
        .ok_or_else(|| {
            OverlayError::new(format!(
                "the window surface cannot be transparent: {:?}",
                capabilities.alpha_modes
            ))
        })?;
        // The frame loop paces itself, so presenting must not block on the
        // compositor's refresh: on Wayland a FIFO swapchain can wait forever
        // for a frame callback while the surface is not shown.
        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|mode| capabilities.present_modes.contains(mode))
            .unwrap_or(wgpu::PresentMode::Fifo);
        let (width, height) = window.buffer_size(display);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: renderer.color_format,
            width,
            height,
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: Vec::new(),
        };
        let scope = renderer.error_scope();
        surface.configure(&renderer.context.device, &config);
        scope.finish("surface configuration")?;
        Ok(Self {
            surface,
            config,
            window,
        })
    }

    fn reconfigure(&mut self, renderer: &Renderer, width: u32, height: u32) {
        self.config.width = width;
        self.config.height = height;
        self.surface
            .configure(&renderer.context.device, &self.config);
    }

    /// Draw the renderer's current frame and present it, which also maps the
    /// window the first time.
    ///
    /// `verify` runs the shared frame smoke check on the same frame first, as
    /// the other backends do for a frame that gates a model commit.
    pub(crate) fn present(
        &mut self,
        renderer: &mut Renderer,
        display: &NativeDisplay,
        verify: bool,
    ) -> Result<(), OverlayError> {
        let (width, height) = self.window.buffer_size(display);
        if (width, height) != (self.config.width, self.config.height) {
            self.reconfigure(renderer, width, height);
        }
        self.window.sync_scale(display);
        if verify {
            renderer.verify_frame(width, height)?;
        }
        let (texture, suboptimal) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => (texture, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => (texture, true),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Err(OverlayError::temporary_presentation_unavailable(
                    "the window surface has no frame available",
                ));
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.reconfigure(renderer, width, height);
                return Err(OverlayError::temporary_presentation_unavailable(
                    "the window surface was outdated and is reconfigured",
                ));
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                return Err(OverlayError::new("the window GPU surface was lost"));
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(OverlayError::new(
                    "acquiring the window surface failed validation",
                ));
            }
        };
        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let scope = renderer.error_scope();
        let mut encoder =
            renderer
                .context
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("bongocat_window_frame"),
                });
        renderer.encode_frame(&mut encoder, &view, width, height)?;
        renderer
            .context
            .queue
            .submit(std::iter::once(encoder.finish()));
        scope.finish("window frame submission")?;
        texture.present();
        if suboptimal {
            self.reconfigure(renderer, width, height);
        }
        self.window.after_present(display)
    }

    /// Release the GPU surface, then the window it presents into.
    pub(crate) fn close(self, display: &NativeDisplay) {
        let Self {
            surface, window, ..
        } = self;
        drop(surface);
        window.close(display);
    }
}
