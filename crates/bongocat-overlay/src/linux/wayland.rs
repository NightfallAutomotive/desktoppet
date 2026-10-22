//! The model window on Wayland: one `wlr-layer-shell` surface.
//!
//! A layer surface is the only Wayland role that can stay above other windows
//! and be placed by the client, which is what a desktop pet needs. KDE Plasma
//! (KWin), wlroots compositors and others implement it; GNOME does not, and a
//! compositor without it reports [`WaylandError::LayerShellUnavailable`]
//! instead of a window that silently cannot do its job (ADR-0090).
//!
//! The adapter keeps its own connection rather than borrowing GPUI's, so the
//! settings UI and the model window never share an event queue. The window is
//! positioned with margins from the output's top-left corner: Wayland has no
//! global coordinates, so `OverlayWindowBounds::x`/`y` are output-relative here.
//!
//! Optional protocols degrade explicitly: without `wp_fractional_scale_v1` the
//! window renders at the integer buffer scale, and without `wp_alpha_modifier_v1`
//! the configured opacity cannot be applied (see [`LayerWindow::set_opacity`]).

use super::*;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_compositor::WlCompositor,
        wl_pointer::{self, ButtonState, WlPointer},
        wl_region::WlRegion,
        wl_registry::WlRegistry,
        wl_seat::{self, Capability, WlSeat},
        wl_surface::{self, WlSurface},
    },
};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::wp::{
    alpha_modifier::v1::client::{
        wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1,
        wp_alpha_modifier_v1::WpAlphaModifierV1,
    },
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

/// How long the compositor may take to answer a configure round trip.
const CONFIGURE_ATTEMPTS: u32 = 16;

/// The namespace the compositor sees; KWin uses it for window rules.
const LAYER_NAMESPACE: &str = "bongocat";

#[derive(Debug, thiserror::Error)]
pub(crate) enum WaylandError {
    #[error("no Wayland compositor is reachable: {0}")]
    Connect(String),
    #[error("the compositor offers no wlr-layer-shell, so it cannot host the model window")]
    LayerShellUnavailable,
    #[error("Wayland protocol error: {0}")]
    Protocol(String),
    #[error("the compositor never configured the model window")]
    NotConfigured,
    #[error("the compositor closed the model window")]
    Closed,
}

impl From<WaylandError> for OverlayError {
    fn from(error: WaylandError) -> Self {
        OverlayError::new(error.to_string())
    }
}

/// What the compositor told us about the current window since it was created.
#[derive(Debug, Default)]
struct WindowEvents {
    /// The size of the last configure, already acknowledged.
    configured: Option<(u32, u32)>,
    closed: bool,
    /// `wp_fractional_scale_v1` preferred scale, in 120ths.
    fractional_scale_120: Option<u32>,
    /// `wl_surface.preferred_buffer_scale`, the integer fallback.
    buffer_scale: Option<i32>,
}

/// The Linux input-event codes of the two buttons the model window uses.
pub(crate) const BUTTON_LEFT: u32 = 0x110;
pub(crate) const BUTTON_RIGHT: u32 = 0x111;

/// One pointer event over the model window, in surface coordinates.
///
/// Events are queued in arrival order and drained by the session each tick, so
/// a press, the motions after it and the release are seen in the order the
/// compositor sent them even when they arrive within one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PointerEvent {
    Enter { x: f64, y: f64 },
    Leave,
    Motion { x: f64, y: f64 },
    Button { button: u32, pressed: bool },
}

#[derive(Default)]
pub(crate) struct WaylandState {
    window: WindowEvents,
    pointer: Option<WlPointer>,
    cursor_shape_manager: Option<WpCursorShapeManagerV1>,
    cursor_shape: Option<WpCursorShapeDeviceV1>,
    pointer_events: Vec<PointerEvent>,
}

/// The longest pointer backlog kept between two ticks. A stalled frame loop
/// must not grow it without bound; the oldest motions are the ones dropped.
pub(crate) const MAXIMUM_POINTER_EVENTS: usize = 1024;

struct Globals {
    compositor: WlCompositor,
    layer_shell: ZwlrLayerShellV1,
    viewporter: Option<WpViewporter>,
    fractional_scale: Option<WpFractionalScaleManagerV1>,
    alpha_modifier: Option<WpAlphaModifierV1>,
    _seat: Option<WlSeat>,
}

/// The adapter's own Wayland connection and the globals it binds.
pub(crate) struct WaylandDisplay {
    connection: Connection,
    queue: EventQueue<WaylandState>,
    handle: QueueHandle<WaylandState>,
    state: WaylandState,
    globals: Globals,
}

impl WaylandDisplay {
    pub(crate) fn connect() -> Result<Self, WaylandError> {
        let connection = Connection::connect_to_env()
            .map_err(|error| WaylandError::Connect(error.to_string()))?;
        let (globals, queue) = registry_queue_init::<WaylandState>(&connection)
            .map_err(|error| WaylandError::Protocol(error.to_string()))?;
        let handle = queue.handle();
        let mut state = WaylandState {
            cursor_shape_manager: globals.bind(&handle, 1..=1, ()).ok(),
            ..WaylandState::default()
        };
        let globals = bind_globals(&globals, &handle)?;
        let mut queue = queue;
        // The seat announces its capabilities; the pointer is created from them.
        queue
            .roundtrip(&mut state)
            .map_err(|error| WaylandError::Protocol(error.to_string()))?;
        Ok(Self {
            connection,
            queue,
            handle,
            state,
            globals,
        })
    }

    /// Every pointer event since the last call, oldest first.
    pub(crate) fn take_pointer_events(&mut self) -> Vec<PointerEvent> {
        std::mem::take(&mut self.state.pointer_events)
    }

    /// Send queued requests and handle whatever events have arrived, without
    /// blocking: the frame loop calls this every tick.
    pub(crate) fn dispatch(&mut self) -> Result<(), WaylandError> {
        self.connection
            .flush()
            .map_err(|error| WaylandError::Protocol(error.to_string()))?;
        if let Some(guard) = self.queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(WaylandError::Protocol(error.to_string())),
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| WaylandError::Protocol(error.to_string()))?;
        if self.state.window.closed {
            return Err(WaylandError::Closed);
        }
        Ok(())
    }

    fn roundtrip(&mut self) -> Result<(), WaylandError> {
        self.queue
            .roundtrip(&mut self.state)
            .map(|_| ())
            .map_err(|error| WaylandError::Protocol(error.to_string()))
    }

    /// Wait until the compositor has configured the current window.
    fn wait_for_configure(&mut self) -> Result<(u32, u32), WaylandError> {
        for _ in 0..CONFIGURE_ATTEMPTS {
            self.roundtrip()?;
            if self.state.window.closed {
                return Err(WaylandError::Closed);
            }
            if let Some(size) = self.state.window.configured {
                return Ok(size);
            }
        }
        Err(WaylandError::NotConfigured)
    }

    /// Create and configure the model window, but do not map it: a layer
    /// surface appears when the first frame is presented into it.
    ///
    /// `bounds` is the saved placement; without one the window is centered on
    /// the output the compositor chooses.
    pub(crate) fn create_window(
        &mut self,
        bounds: Option<OverlayWindowBounds>,
        default_size: (u32, u32),
        always_on_top: bool,
        click_through: bool,
    ) -> Result<LayerWindow, WaylandError> {
        self.state.window = WindowEvents::default();
        let qh = &self.handle.clone();
        let surface = self.globals.compositor.create_surface(qh, ());
        let layer = self.globals.layer_shell.get_layer_surface(
            &surface,
            None,
            layer_for(always_on_top),
            LAYER_NAMESPACE.to_owned(),
            qh,
            (),
        );
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // Ignore other surfaces' exclusive zones, so margins are measured from
        // the output's own corner rather than from the edge of a panel.
        layer.set_exclusive_zone(-1);

        // A layer surface anchored to every edge with a zero size is configured
        // to the output's size, which is the only way a Wayland client learns
        // how large its output is in surface coordinates.
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_size(0, 0);
        surface.commit();
        let output = self.wait_for_configure()?;

        let (width, height) = bounds.map_or(default_size, |bounds| (bounds.width, bounds.height));
        let (x, y) = bounds.map_or_else(
            || {
                (
                    (i64::from(output.0) - i64::from(width)).max(0) as i32 / 2,
                    (i64::from(output.1) - i64::from(height)).max(0) as i32 / 2,
                )
            },
            |bounds| (bounds.x, bounds.y),
        );
        self.state.window.configured = None;
        layer.set_anchor(Anchor::Top | Anchor::Left);
        layer.set_size(width, height);
        layer.set_margin(y, 0, 0, x);
        surface.commit();
        let configured = self.wait_for_configure()?;
        let logical = (
            if configured.0 == 0 {
                width
            } else {
                configured.0
            },
            if configured.1 == 0 {
                height
            } else {
                configured.1
            },
        );

        let viewport = self
            .globals
            .viewporter
            .as_ref()
            .map(|viewporter| viewporter.get_viewport(&surface, qh, ()));
        let fractional_scale = self
            .globals
            .fractional_scale
            .as_ref()
            .filter(|_| viewport.is_some())
            .map(|manager| manager.get_fractional_scale(&surface, qh, ()));
        let alpha = self
            .globals
            .alpha_modifier
            .as_ref()
            .map(|modifier| modifier.get_surface(&surface, qh, ()));
        let mut window = LayerWindow {
            alpha,
            fractional_scale,
            viewport,
            layer,
            surface,
            output,
            bounds: OverlayWindowBounds::new(x, y, logical.0, logical.1),
            always_on_top,
            click_through: !click_through,
            applied_alpha: None,
        };
        window.apply_input_region(&self.globals.compositor, &self.handle, click_through);
        window.apply_viewport();
        // Learn the preferred scale before the first buffer is sized.
        self.roundtrip()?;
        Ok(window)
    }

    pub(crate) fn scale(&self) -> f64 {
        let events = &self.state.window;
        match (events.fractional_scale_120, events.buffer_scale) {
            (Some(scale), _) if scale > 0 => f64::from(scale) / 120.0,
            (_, Some(scale)) if scale > 0 => f64::from(scale),
            _ => 1.0,
        }
    }

    /// Whether the window renders through a viewport at a fractional scale,
    /// rather than at an integer `wl_surface` buffer scale.
    fn fractional(&self) -> bool {
        self.state.window.fractional_scale_120.is_some()
    }

    /// Raw handles for `wgpu`. They stay valid while this display and `window`
    /// are alive; see [`WindowSurface`] for how that is enforced.
    pub(crate) fn raw_handles(
        &self,
        window: &LayerWindow,
    ) -> Result<(RawDisplayHandle, RawWindowHandle), OverlayError> {
        let display = NonNull::new(self.connection.backend().display_ptr().cast())
            .ok_or_else(|| OverlayError::new("the Wayland display has no native pointer"))?;
        let surface = NonNull::new(window.surface.id().as_ptr().cast())
            .ok_or_else(|| OverlayError::new("the Wayland surface has no native pointer"))?;
        Ok((
            RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display)),
            RawWindowHandle::Wayland(WaylandWindowHandle::new(surface)),
        ))
    }
}

fn bind_globals(
    globals: &GlobalList,
    qh: &QueueHandle<WaylandState>,
) -> Result<Globals, WaylandError> {
    let compositor = globals
        .bind::<WlCompositor, _, _>(qh, 4..=6, ())
        .map_err(|error| WaylandError::Protocol(error.to_string()))?;
    let layer_shell = globals
        .bind::<ZwlrLayerShellV1, _, _>(qh, 2..=4, ())
        .map_err(|_| WaylandError::LayerShellUnavailable)?;
    Ok(Globals {
        compositor,
        layer_shell,
        viewporter: globals.bind(qh, 1..=1, ()).ok(),
        fractional_scale: globals.bind(qh, 1..=1, ()).ok(),
        alpha_modifier: globals.bind(qh, 1..=1, ()).ok(),
        _seat: globals.bind(qh, 5..=9, ()).ok(),
    })
}

const fn layer_for(always_on_top: bool) -> Layer {
    // Above every normal window, fullscreen ones included, like the macOS
    // floating panel; otherwise beneath them, above the wallpaper. A layer
    // surface cannot join the normal window stack in between.
    if always_on_top {
        Layer::Overlay
    } else {
        Layer::Bottom
    }
}

/// One mapped-or-mappable model window.
///
/// Fields drop in declaration order, so the protocol objects that extend the
/// surface go before the layer role and the surface itself, matching the
/// destruction order the protocols require.
pub(crate) struct LayerWindow {
    alpha: Option<WpAlphaModifierSurfaceV1>,
    fractional_scale: Option<WpFractionalScaleV1>,
    viewport: Option<WpViewport>,
    layer: ZwlrLayerSurfaceV1,
    surface: WlSurface,
    /// The output size learned at creation, in surface coordinates.
    output: (u32, u32),
    bounds: OverlayWindowBounds,
    always_on_top: bool,
    click_through: bool,
    applied_alpha: Option<u32>,
}

impl LayerWindow {
    pub(crate) fn bounds(&self) -> OverlayWindowBounds {
        self.bounds
    }

    pub(crate) fn output_bounds(&self) -> OverlayWindowBounds {
        OverlayWindowBounds::new(0, 0, self.output.0, self.output.1)
    }

    /// The buffer size for the current scale.
    pub(crate) fn buffer_size(&self, display: &WaylandDisplay) -> (u32, u32) {
        let scale = if display.fractional() {
            display.scale()
        } else {
            display.scale().round().max(1.0)
        };
        (
            (f64::from(self.bounds.width) * scale).round().max(1.0) as u32,
            (f64::from(self.bounds.height) * scale).round().max(1.0) as u32,
        )
    }

    /// Keep the surface's scale state in step with the buffer that is about to
    /// be presented: a viewport destination when fractional, else an integer
    /// buffer scale.
    pub(crate) fn sync_scale(&self, display: &WaylandDisplay) {
        if display.fractional() {
            self.apply_viewport();
        } else if self.surface.version() >= 3 {
            self.surface
                .set_buffer_scale(display.scale().round().max(1.0) as i32);
        }
    }

    fn apply_viewport(&self) {
        if let Some(viewport) = &self.viewport {
            viewport.set_destination(
                i32::try_from(self.bounds.width).unwrap_or(i32::MAX),
                i32::try_from(self.bounds.height).unwrap_or(i32::MAX),
            );
        }
    }

    /// Pass pointer events through (an empty input region) or receive them.
    /// Applied with the next presented frame, like every surface state.
    pub(crate) fn set_click_through(&mut self, display: &WaylandDisplay, click_through: bool) {
        self.apply_input_region(&display.globals.compositor, &display.handle, click_through);
    }

    fn apply_input_region(
        &mut self,
        compositor: &WlCompositor,
        qh: &QueueHandle<WaylandState>,
        click_through: bool,
    ) {
        if click_through == self.click_through {
            return;
        }
        if click_through {
            let region = compositor.create_region(qh, ());
            self.surface.set_input_region(Some(&region));
            region.destroy();
        } else {
            self.surface.set_input_region(None);
        }
        self.click_through = click_through;
    }

    pub(crate) fn set_always_on_top(&mut self, always_on_top: bool) {
        if always_on_top != self.always_on_top {
            self.layer.set_layer(layer_for(always_on_top));
            self.always_on_top = always_on_top;
        }
    }

    /// Apply the window opacity through the compositor, once for the whole
    /// surface. A compositor without `wp_alpha_modifier_v1` shows the window
    /// opaque; reporting that through capability diagnostics is ADR-0090 L6.
    pub(crate) fn set_opacity(&mut self, alpha: f64) {
        let Some(modifier) = &self.alpha else {
            return;
        };
        let multiplier = (alpha.clamp(0.0, 1.0) * f64::from(u32::MAX)).round() as u32;
        if self.applied_alpha != Some(multiplier) {
            modifier.set_multiplier(multiplier);
            self.applied_alpha = Some(multiplier);
        }
    }

    /// Move and resize the window. A size change takes effect with the
    /// compositor's next configure, which the dispatch loop acknowledges.
    pub(crate) fn set_bounds(&mut self, bounds: OverlayWindowBounds) {
        if bounds == self.bounds {
            return;
        }
        if (bounds.width, bounds.height) != (self.bounds.width, self.bounds.height) {
            self.layer.set_size(bounds.width, bounds.height);
        }
        self.layer.set_margin(bounds.y, 0, 0, bounds.x);
        self.bounds = bounds;
        self.apply_viewport();
    }
}

impl Drop for LayerWindow {
    fn drop(&mut self) {
        if let Some(alpha) = self.alpha.take() {
            alpha.destroy();
        }
        if let Some(fractional) = self.fractional_scale.take() {
            fractional.destroy();
        }
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        self.layer.destroy();
        self.surface.destroy();
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event {
            state.window.buffer_scale = Some(factor);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                state.window.configured = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => state.window.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.window.fractional_scale_120 = Some(scale);
        }
    }
}

impl Dispatch<WlSeat, ()> for WaylandState {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        else {
            return;
        };
        let has_pointer = capabilities.contains(Capability::Pointer);
        if has_pointer && state.pointer.is_none() {
            let pointer = seat.get_pointer(qh, ());
            state.cursor_shape = state
                .cursor_shape_manager
                .as_ref()
                .map(|manager| manager.get_pointer(&pointer, qh, ()));
            state.pointer = Some(pointer);
        } else if !has_pointer && let Some(pointer) = state.pointer.take() {
            if let Some(shape) = state.cursor_shape.take() {
                shape.destroy();
            }
            pointer.release();
        }
    }
}

impl WaylandState {
    fn push_pointer_event(&mut self, event: PointerEvent) {
        if self.pointer_events.len() >= MAXIMUM_POINTER_EVENTS {
            let Some(index) = self
                .pointer_events
                .iter()
                .position(|queued| matches!(queued, PointerEvent::Motion { .. }))
            else {
                return;
            };
            self.pointer_events.remove(index);
        }
        self.pointer_events.push(event);
    }
}

impl Dispatch<WlPointer, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                // An unset cursor is undefined on Wayland; ask for the normal
                // arrow while the pointer is over the cat.
                if let Some(shape) = &state.cursor_shape {
                    shape.set_shape(serial, Shape::Default);
                }
                state.push_pointer_event(PointerEvent::Enter {
                    x: surface_x,
                    y: surface_y,
                });
            }
            wl_pointer::Event::Leave { .. } => state.push_pointer_event(PointerEvent::Leave),
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.push_pointer_event(PointerEvent::Motion {
                x: surface_x,
                y: surface_y,
            }),
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(button_state),
                ..
            } => state.push_pointer_event(PointerEvent::Button {
                button,
                pressed: button_state == ButtonState::Pressed,
            }),
            _ => {}
        }
    }
}

delegate_noop!(WaylandState: ignore WlCompositor);
delegate_noop!(WaylandState: ignore WpCursorShapeManagerV1);
delegate_noop!(WaylandState: ignore WpCursorShapeDeviceV1);
delegate_noop!(WaylandState: ignore WlRegion);
delegate_noop!(WaylandState: ignore ZwlrLayerShellV1);
delegate_noop!(WaylandState: ignore WpViewporter);
delegate_noop!(WaylandState: ignore WpViewport);
delegate_noop!(WaylandState: ignore WpFractionalScaleManagerV1);
delegate_noop!(WaylandState: ignore WpAlphaModifierV1);
delegate_noop!(WaylandState: ignore WpAlphaModifierSurfaceV1);
