//! The model window on X11: one borderless ARGB window.
//!
//! This backend serves X11 sessions and, through XWayland, Wayland compositors
//! without `wlr-layer-shell` such as GNOME (ADR-0090). The window uses a
//! 32-bit TrueColor visual so the compositor blends it by its alpha, carries no
//! decorations, skips the taskbar and pager, and asks the window manager to
//! keep it above other windows with `_NET_WM_STATE_ABOVE`. Click-through is an
//! empty input shape; opacity is `_NET_WM_WINDOW_OPACITY`, which the
//! compositing manager applies to the whole window.
//!
//! X11 has global coordinates, so `OverlayWindowBounds` are root-window
//! positions here and placement uses the RandR monitor layout. Pointer events
//! carry root coordinates, which are converted to coordinates relative to the
//! window's applied origin; that keeps the shared drag rules correct even when
//! an event was generated before the server processed the last move.

use super::*;
use x11rb::{
    connection::Connection,
    properties::{WmSizeHints, WmSizeHintsSpecification},
    protocol::{
        Event,
        randr::ConnectionExt as _,
        shape::{self, ConnectionExt as _},
        xproto::{
            AtomEnum, ClientMessageEvent, ColormapAlloc, ConfigureWindowAux, ConnectionExt as _,
            CreateWindowAux, EventMask, PropMode, VisualClass, Window, WindowClass,
        },
    },
    wrapper::ConnectionExt as _,
    xcb_ffi::XCBConnection,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum X11Error {
    #[error("no X11 display is reachable: {0}")]
    Connect(String),
    #[error("the X11 screen has no 32-bit TrueColor visual, so the window cannot be transparent")]
    NoArgbVisual,
    #[error("X11 request failed: {0}")]
    Request(String),
}

impl From<X11Error> for OverlayError {
    fn from(error: X11Error) -> Self {
        OverlayError::new(error.to_string())
    }
}

fn request(error: impl std::fmt::Display) -> X11Error {
    X11Error::Request(error.to_string())
}

/// The atoms the window manager hints use, interned once per connection.
struct Atoms {
    net_wm_state: u32,
    net_wm_state_above: u32,
    net_wm_state_below: u32,
    net_wm_state_sticky: u32,
    net_wm_state_skip_taskbar: u32,
    net_wm_state_skip_pager: u32,
    net_wm_window_opacity: u32,
    net_wm_name: u32,
    utf8_string: u32,
    motif_wm_hints: u32,
}

impl Atoms {
    fn intern(connection: &XCBConnection) -> Result<Self, X11Error> {
        let intern = |name: &str| -> Result<u32, X11Error> {
            Ok(connection
                .intern_atom(false, name.as_bytes())
                .map_err(request)?
                .reply()
                .map_err(request)?
                .atom)
        };
        Ok(Self {
            net_wm_state: intern("_NET_WM_STATE")?,
            net_wm_state_above: intern("_NET_WM_STATE_ABOVE")?,
            net_wm_state_below: intern("_NET_WM_STATE_BELOW")?,
            net_wm_state_sticky: intern("_NET_WM_STATE_STICKY")?,
            net_wm_state_skip_taskbar: intern("_NET_WM_STATE_SKIP_TASKBAR")?,
            net_wm_state_skip_pager: intern("_NET_WM_STATE_SKIP_PAGER")?,
            net_wm_window_opacity: intern("_NET_WM_WINDOW_OPACITY")?,
            net_wm_name: intern("_NET_WM_NAME")?,
            utf8_string: intern("UTF8_STRING")?,
            motif_wm_hints: intern("_MOTIF_WM_HINTS")?,
        })
    }
}

/// The adapter's own X11 connection.
pub(crate) struct X11Display {
    connection: XCBConnection,
    screen: usize,
    root: Window,
    visual: u32,
    atoms: Atoms,
    pointer_events: Vec<RootPointerEvent>,
}

/// A pointer event as the server reports it, in root coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
enum RootPointerEvent {
    Enter { x: f64, y: f64 },
    Leave,
    Motion { x: f64, y: f64 },
    Button { button: u32, pressed: bool },
}

/// X11 core button numbers.
const X11_BUTTON_LEFT: u8 = 1;
const X11_BUTTON_RIGHT: u8 = 3;

impl X11Display {
    pub(crate) fn connect() -> Result<Self, X11Error> {
        let (connection, screen) =
            XCBConnection::connect(None).map_err(|error| X11Error::Connect(error.to_string()))?;
        let setup = connection.setup();
        let root_screen = &setup.roots[screen];
        let visual = root_screen
            .allowed_depths
            .iter()
            .filter(|depth| depth.depth == 32)
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.class == VisualClass::TRUE_COLOR)
            .map(|visual| visual.visual_id)
            .ok_or(X11Error::NoArgbVisual)?;
        let root = root_screen.root;
        let atoms = Atoms::intern(&connection)?;
        Ok(Self {
            connection,
            screen,
            root,
            visual,
            atoms,
            pointer_events: Vec::new(),
        })
    }

    /// The monitors in root coordinates, the primary one first.
    pub(crate) fn monitors(&self) -> Vec<OverlayScreenBounds> {
        let Ok(reply) = self
            .connection
            .randr_get_monitors(self.root, true)
            .map_err(request)
            .and_then(|cookie| cookie.reply().map_err(request))
        else {
            return Vec::new();
        };
        let mut monitors = reply.monitors;
        monitors.sort_by_key(|monitor| !monitor.primary);
        monitors
            .into_iter()
            .map(|monitor| OverlayScreenBounds {
                x: i32::from(monitor.x),
                y: i32::from(monitor.y),
                width: u32::from(monitor.width),
                height: u32::from(monitor.height),
            })
            .collect()
    }

    /// Handle whatever events have arrived, without blocking.
    pub(crate) fn dispatch(&mut self) -> Result<(), X11Error> {
        self.connection.flush().map_err(request)?;
        while let Some(event) = self.connection.poll_for_event().map_err(request)? {
            let event = match event {
                Event::EnterNotify(event) => RootPointerEvent::Enter {
                    x: f64::from(event.root_x),
                    y: f64::from(event.root_y),
                },
                Event::LeaveNotify(_) => RootPointerEvent::Leave,
                Event::MotionNotify(event) => RootPointerEvent::Motion {
                    x: f64::from(event.root_x),
                    y: f64::from(event.root_y),
                },
                Event::ButtonPress(event) | Event::ButtonRelease(event) => {
                    let button = match event.detail {
                        X11_BUTTON_LEFT => BUTTON_LEFT,
                        X11_BUTTON_RIGHT => BUTTON_RIGHT,
                        _ => continue,
                    };
                    RootPointerEvent::Button {
                        button,
                        // Press and release share one struct; the response type
                        // tells them apart.
                        pressed: event.response_type & 0x7f
                            == x11rb::protocol::xproto::BUTTON_PRESS_EVENT,
                    }
                }
                _ => continue,
            };
            if self.pointer_events.len() < MAXIMUM_POINTER_EVENTS {
                self.pointer_events.push(event);
            }
        }
        Ok(())
    }

    /// Every pointer event since the last call, in coordinates relative to
    /// `origin`, the window's applied position.
    pub(crate) fn take_pointer_events(&mut self, origin: (i32, i32)) -> Vec<PointerEvent> {
        let (origin_x, origin_y) = (f64::from(origin.0), f64::from(origin.1));
        std::mem::take(&mut self.pointer_events)
            .into_iter()
            .map(|event| match event {
                RootPointerEvent::Enter { x, y } => PointerEvent::Enter {
                    x: x - origin_x,
                    y: y - origin_y,
                },
                RootPointerEvent::Leave => PointerEvent::Leave,
                RootPointerEvent::Motion { x, y } => PointerEvent::Motion {
                    x: x - origin_x,
                    y: y - origin_y,
                },
                RootPointerEvent::Button { button, pressed } => {
                    PointerEvent::Button { button, pressed }
                }
            })
            .collect()
    }

    /// Create the window, unmapped, at `bounds` or centered on the primary
    /// monitor.
    pub(crate) fn create_window(
        &mut self,
        bounds: Option<OverlayWindowBounds>,
        default_size: (u32, u32),
        options: OverlaySessionOptions,
    ) -> Result<X11Window, X11Error> {
        let (width, height) = bounds.map_or(default_size, |bounds| (bounds.width, bounds.height));
        let (x, y) = bounds.map_or_else(
            || {
                self.monitors().first().map_or((0, 0), |monitor| {
                    (
                        monitor.x + (i64::from(monitor.width) - i64::from(width)).max(0) as i32 / 2,
                        monitor.y
                            + (i64::from(monitor.height) - i64::from(height)).max(0) as i32 / 2,
                    )
                })
            },
            |bounds| (bounds.x, bounds.y),
        );
        let connection = &self.connection;
        let colormap = connection.generate_id().map_err(request)?;
        connection
            .create_colormap(ColormapAlloc::NONE, colormap, self.root, self.visual)
            .map_err(request)?;
        let window = connection.generate_id().map_err(request)?;
        connection
            .create_window(
                32,
                window,
                self.root,
                clamp_i16(x),
                clamp_i16(y),
                clamp_u16(width),
                clamp_u16(height),
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &CreateWindowAux::new()
                    .background_pixel(0)
                    .border_pixel(0)
                    .colormap(colormap)
                    .event_mask(
                        EventMask::BUTTON_PRESS
                            | EventMask::BUTTON_RELEASE
                            | EventMask::POINTER_MOTION
                            | EventMask::ENTER_WINDOW
                            | EventMask::LEAVE_WINDOW,
                    ),
            )
            .map_err(request)?;
        let atoms = &self.atoms;
        connection
            .change_property8(
                PropMode::REPLACE,
                window,
                atoms.net_wm_name,
                atoms.utf8_string,
                b"BongoCat",
            )
            .map_err(request)?;
        connection
            .change_property8(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                b"BongoCat",
            )
            .map_err(request)?;
        // Motif hints, flags = decorations: a frameless window.
        connection
            .change_property32(
                PropMode::REPLACE,
                window,
                atoms.motif_wm_hints,
                atoms.motif_wm_hints,
                &[2, 0, 0, 0, 0],
            )
            .map_err(request)?;
        let mut size_hints = WmSizeHints::new();
        size_hints.position = Some((
            WmSizeHintsSpecification::UserSpecified,
            i32::from(clamp_i16(x)),
            i32::from(clamp_i16(y)),
        ));
        size_hints.size = Some((
            WmSizeHintsSpecification::UserSpecified,
            i32::from(clamp_u16(width)),
            i32::from(clamp_u16(height)),
        ));
        size_hints
            .set_normal_hints(connection, window)
            .map_err(request)?;
        // The initial state, set before mapping so the window manager applies
        // it when it first manages the window.
        let mut state = vec![
            atoms.net_wm_state_sticky,
            atoms.net_wm_state_skip_taskbar,
            atoms.net_wm_state_skip_pager,
        ];
        state.push(if options.always_on_top {
            atoms.net_wm_state_above
        } else {
            atoms.net_wm_state_below
        });
        connection
            .change_property32(
                PropMode::REPLACE,
                window,
                atoms.net_wm_state,
                AtomEnum::ATOM,
                &state,
            )
            .map_err(request)?;
        let mut native = X11Window {
            window,
            colormap,
            bounds: OverlayWindowBounds::new(
                x,
                y,
                u32::from(clamp_u16(width)),
                u32::from(clamp_u16(height)),
            ),
            mapped: false,
            always_on_top: options.always_on_top,
            click_through: !options.click_through,
            applied_alpha: None,
        };
        native.set_click_through(self, options.click_through)?;
        self.connection.flush().map_err(request)?;
        Ok(native)
    }

    /// Raw handles for `wgpu`, valid while this display and `window` live.
    pub(crate) fn raw_handles(
        &self,
        window: &X11Window,
    ) -> Result<(RawDisplayHandle, RawWindowHandle), OverlayError> {
        let connection = NonNull::new(self.connection.get_raw_xcb_connection())
            .ok_or_else(|| OverlayError::new("the X11 connection has no native pointer"))?;
        let window_id = std::num::NonZeroU32::new(window.window)
            .ok_or_else(|| OverlayError::new("the X11 window has no id"))?;
        let screen = i32::try_from(self.screen)
            .map_err(|_| OverlayError::new("the X11 screen number is out of range"))?;
        Ok((
            RawDisplayHandle::Xcb(XcbDisplayHandle::new(Some(connection), screen)),
            RawWindowHandle::Xcb(XcbWindowHandle::new(window_id)),
        ))
    }

    fn set_wm_state(&self, window: Window, add: bool, state: u32) -> Result<(), X11Error> {
        // EWMH: a mapped window's state changes through a client message to
        // the root window; action 1 adds the state, 0 removes it.
        let event = ClientMessageEvent::new(
            32,
            window,
            self.atoms.net_wm_state,
            [u32::from(add), state, 0, 1, 0],
        );
        self.connection
            .send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .map_err(request)?;
        Ok(())
    }
}

fn clamp_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn clamp_u16(value: u32) -> u16 {
    value.clamp(1, u32::from(u16::MAX)) as u16
}

pub(crate) struct X11Window {
    window: Window,
    colormap: u32,
    bounds: OverlayWindowBounds,
    mapped: bool,
    always_on_top: bool,
    click_through: bool,
    applied_alpha: Option<u32>,
}

impl X11Window {
    pub(crate) fn bounds(&self) -> OverlayWindowBounds {
        self.bounds
    }

    pub(crate) fn buffer_size(&self) -> (u32, u32) {
        (self.bounds.width, self.bounds.height)
    }

    /// Map the window after its first frame, so it never appears empty.
    pub(crate) fn show(&mut self, display: &X11Display) -> Result<(), X11Error> {
        if !self.mapped {
            display
                .connection
                .map_window(self.window)
                .map_err(request)?;
            // Window managers may ignore the initial position of a new window;
            // asking again after mapping places it where the user left it.
            display
                .connection
                .configure_window(
                    self.window,
                    &ConfigureWindowAux::new().x(self.bounds.x).y(self.bounds.y),
                )
                .map_err(request)?;
            self.mapped = true;
        }
        Ok(())
    }

    pub(crate) fn set_click_through(
        &mut self,
        display: &X11Display,
        click_through: bool,
    ) -> Result<(), X11Error> {
        if click_through == self.click_through {
            return Ok(());
        }
        if click_through {
            display
                .connection
                .shape_rectangles(
                    shape::SO::SET,
                    shape::SK::INPUT,
                    x11rb::protocol::xproto::ClipOrdering::UNSORTED,
                    self.window,
                    0,
                    0,
                    &[],
                )
                .map_err(request)?;
        } else {
            display
                .connection
                .shape_mask(
                    shape::SO::SET,
                    shape::SK::INPUT,
                    self.window,
                    0,
                    0,
                    x11rb::NONE,
                )
                .map_err(request)?;
        }
        self.click_through = click_through;
        Ok(())
    }

    pub(crate) fn set_always_on_top(
        &mut self,
        display: &X11Display,
        always_on_top: bool,
    ) -> Result<(), X11Error> {
        if always_on_top == self.always_on_top {
            return Ok(());
        }
        let atoms = &display.atoms;
        display.set_wm_state(self.window, always_on_top, atoms.net_wm_state_above)?;
        display.set_wm_state(self.window, !always_on_top, atoms.net_wm_state_below)?;
        self.always_on_top = always_on_top;
        Ok(())
    }

    pub(crate) fn set_opacity(&mut self, display: &X11Display, alpha: f64) -> Result<(), X11Error> {
        let opacity = (alpha.clamp(0.0, 1.0) * f64::from(u32::MAX)).round() as u32;
        if self.applied_alpha == Some(opacity) {
            return Ok(());
        }
        display
            .connection
            .change_property32(
                PropMode::REPLACE,
                self.window,
                display.atoms.net_wm_window_opacity,
                AtomEnum::CARDINAL,
                &[opacity],
            )
            .map_err(request)?;
        self.applied_alpha = Some(opacity);
        Ok(())
    }

    pub(crate) fn set_bounds(
        &mut self,
        display: &X11Display,
        bounds: OverlayWindowBounds,
    ) -> Result<(), X11Error> {
        if bounds == self.bounds {
            return Ok(());
        }
        display
            .connection
            .configure_window(
                self.window,
                &ConfigureWindowAux::new()
                    .x(bounds.x)
                    .y(bounds.y)
                    .width(u32::from(clamp_u16(bounds.width)))
                    .height(u32::from(clamp_u16(bounds.height))),
            )
            .map_err(request)?;
        self.bounds = bounds;
        Ok(())
    }

    pub(crate) fn close(&self, display: &X11Display) {
        let _ = display.connection.destroy_window(self.window);
        let _ = display.connection.free_colormap(self.colormap);
        let _ = display.connection.flush();
    }
}
