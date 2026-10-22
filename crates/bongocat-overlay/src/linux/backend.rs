//! Which window system the model window lives on.
//!
//! Wayland with `wlr-layer-shell` is preferred: it is the only Wayland role that
//! can stay above other windows. A Wayland compositor without it (GNOME) and an
//! X11 session both get the X11 backend, through XWayland in the first case.
//! `BONGOCAT_LINUX_WINDOW=wayland|x11` forces one backend, for testing a
//! backend on a session that would choose the other.

use super::*;

/// The environment variable that forces a backend.
pub(crate) const WINDOW_BACKEND_VARIABLE: &str = "BONGOCAT_LINUX_WINDOW";

pub(crate) enum NativeDisplay {
    Wayland(Box<WaylandDisplay>),
    X11(Box<X11Display>),
}

pub(crate) enum NativeWindow {
    Wayland(Box<LayerWindow>),
    X11(X11Window),
}

impl NativeDisplay {
    pub(crate) fn connect() -> Result<Self, OverlayError> {
        match std::env::var(WINDOW_BACKEND_VARIABLE).as_deref() {
            Ok("wayland") => return Ok(Self::Wayland(Box::new(WaylandDisplay::connect()?))),
            Ok("x11") => return Ok(Self::X11(Box::new(X11Display::connect()?))),
            Ok(other) => {
                return Err(OverlayError::new(format!(
                    "{WINDOW_BACKEND_VARIABLE} must be wayland or x11, not {other:?}"
                )));
            }
            Err(_) => {}
        }
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let x11 = std::env::var_os("DISPLAY").is_some();
        if wayland {
            match WaylandDisplay::connect() {
                Ok(display) => return Ok(Self::Wayland(Box::new(display))),
                Err(WaylandError::LayerShellUnavailable) if x11 => {}
                Err(error) => return Err(error.into()),
            }
        }
        if x11 {
            return Ok(Self::X11(Box::new(X11Display::connect()?)));
        }
        Err(OverlayError::new(
            "neither a Wayland nor an X11 display is available for the model window",
        ))
    }

    pub(crate) fn dispatch(&mut self) -> Result<(), OverlayError> {
        match self {
            Self::Wayland(display) => display.dispatch()?,
            Self::X11(display) => display.dispatch()?,
        }
        Ok(())
    }

    /// Every pointer event since the last call, in window coordinates.
    pub(crate) fn take_pointer_events(
        &mut self,
        window: Option<&NativeWindow>,
    ) -> Vec<PointerEvent> {
        match (self, window) {
            (Self::Wayland(display), _) => display.take_pointer_events(),
            (Self::X11(display), Some(NativeWindow::X11(window))) => {
                let bounds = window.bounds();
                display.take_pointer_events((bounds.x, bounds.y))
            }
            (Self::X11(display), _) => {
                drop(display.take_pointer_events((0, 0)));
                Vec::new()
            }
        }
    }

    /// Whether a move takes effect with the next presented frame (Wayland) or
    /// as soon as it is requested (X11). A drag is measured from the origin
    /// the window system already has.
    pub(crate) const fn moves_with_frames(&self) -> bool {
        matches!(self, Self::Wayland(_))
    }

    pub(crate) fn create_window(
        &mut self,
        bounds: Option<OverlayWindowBounds>,
        default_size: (u32, u32),
        options: OverlaySessionOptions,
    ) -> Result<NativeWindow, OverlayError> {
        Ok(match self {
            Self::Wayland(display) => NativeWindow::Wayland(Box::new(display.create_window(
                bounds,
                default_size,
                options.always_on_top,
                options.click_through,
            )?)),
            Self::X11(display) => {
                NativeWindow::X11(display.create_window(bounds, default_size, options)?)
            }
        })
    }

    pub(crate) fn raw_handles(
        &self,
        window: &NativeWindow,
    ) -> Result<(RawDisplayHandle, RawWindowHandle), OverlayError> {
        match (self, window) {
            (Self::Wayland(display), NativeWindow::Wayland(window)) => display.raw_handles(window),
            (Self::X11(display), NativeWindow::X11(window)) => display.raw_handles(window),
            _ => Err(OverlayError::new(
                "the model window belongs to another display",
            )),
        }
    }

    /// The screens the placement constraint keeps the window on.
    pub(crate) fn screens(&self, window: &NativeWindow) -> Vec<OverlayScreenBounds> {
        match (self, window) {
            (Self::Wayland(_), NativeWindow::Wayland(window)) => {
                let output = window.output_bounds();
                vec![OverlayScreenBounds {
                    x: output.x,
                    y: output.y,
                    width: output.width,
                    height: output.height,
                }]
            }
            (Self::X11(display), NativeWindow::X11(_)) => display.monitors(),
            _ => Vec::new(),
        }
    }
}

impl NativeWindow {
    pub(crate) fn bounds(&self) -> OverlayWindowBounds {
        match self {
            Self::Wayland(window) => window.bounds(),
            Self::X11(window) => window.bounds(),
        }
    }

    pub(crate) fn buffer_size(&self, display: &NativeDisplay) -> (u32, u32) {
        match (self, display) {
            (Self::Wayland(window), NativeDisplay::Wayland(display)) => window.buffer_size(display),
            (Self::Wayland(window), _) => (window.bounds().width, window.bounds().height),
            (Self::X11(window), _) => window.buffer_size(),
        }
    }

    pub(crate) fn sync_scale(&self, display: &NativeDisplay) {
        if let (Self::Wayland(window), NativeDisplay::Wayland(display)) = (self, display) {
            window.sync_scale(display);
        }
    }

    /// Apply the per-tick presentation settings.
    pub(crate) fn apply(
        &mut self,
        display: &NativeDisplay,
        alpha: f64,
        click_through: bool,
        always_on_top: bool,
    ) -> Result<(), OverlayError> {
        match (self, display) {
            (Self::Wayland(window), NativeDisplay::Wayland(display)) => {
                window.set_opacity(alpha);
                window.set_click_through(display, click_through);
                window.set_always_on_top(always_on_top);
            }
            (Self::X11(window), NativeDisplay::X11(display)) => {
                window.set_opacity(display, alpha)?;
                window.set_click_through(display, click_through)?;
                window.set_always_on_top(display, always_on_top)?;
            }
            _ => {
                return Err(OverlayError::new(
                    "the model window belongs to another display",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn set_bounds(
        &mut self,
        display: &NativeDisplay,
        bounds: OverlayWindowBounds,
    ) -> Result<(), OverlayError> {
        match (self, display) {
            (Self::Wayland(window), _) => window.set_bounds(bounds),
            (Self::X11(window), NativeDisplay::X11(display)) => {
                window.set_bounds(display, bounds)?
            }
            (Self::X11(_), _) => {
                return Err(OverlayError::new(
                    "the model window belongs to another display",
                ));
            }
        }
        Ok(())
    }

    /// After a frame was presented: an X11 window is mapped then, so it never
    /// appears empty. A Wayland surface maps by itself with its first buffer.
    pub(crate) fn after_present(&mut self, display: &NativeDisplay) -> Result<(), OverlayError> {
        if let (Self::X11(window), NativeDisplay::X11(display)) = (self, display) {
            window.show(display)?;
        }
        Ok(())
    }

    /// Release the window. A Wayland window releases itself when dropped; an
    /// X11 window needs the connection it was created on.
    pub(crate) fn close(self, display: &NativeDisplay) {
        if let (Self::X11(window), NativeDisplay::X11(display)) = (&self, display) {
            window.close(display);
        }
    }
}
