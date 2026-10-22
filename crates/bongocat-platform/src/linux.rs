//! Linux window and display helpers until ADR-0090 stages L4 and L6.
//!
//! The settings UI calls the same helpers on every platform. The ones that only
//! read a system preference have their Linux answer here already; the ones that
//! need an X11 or Wayland window protocol report that they are unavailable
//! rather than pretending to act:
//!
//! - display queries answer `None`, the documented "the platform could not
//!   say" value, so callers keep their saved or default placement;
//! - native show, hide and close report [`NativeWindowError::UnsupportedHandle`]
//!   until the window protocol work lands;
//! - window origins are already global, as on Windows, so they pass through.

use crate::{DisplayBounds, NativeWindowError};
use bongocat_config::Language;
use raw_window_handle::HasWindowHandle;

pub fn system_language() -> Language {
    sys_locale::get_locale().map_or_else(Language::default, |locale| {
        Language::from_system_locale(&locale)
    })
}

pub fn current_display_bounds() -> Option<DisplayBounds> {
    None
}

pub fn display_bounds_for_window(
    _x: f32,
    _y: f32,
    _width: f32,
    _height: f32,
) -> Option<DisplayBounds> {
    None
}

pub fn local_window_origin(_display: DisplayBounds, x: f32, y: f32) -> (f32, f32) {
    (x, y)
}

pub fn global_window_origin(_display_id: Option<u32>, x: f32, y: f32) -> (f32, f32) {
    (x, y)
}

pub fn hide_native_window(_window: &impl HasWindowHandle) -> Result<(), NativeWindowError> {
    Err(NativeWindowError::UnsupportedHandle)
}

pub fn show_native_window(_window: &impl HasWindowHandle) -> Result<(), NativeWindowError> {
    Err(NativeWindowError::UnsupportedHandle)
}

pub fn request_native_window_close(
    _window: &impl HasWindowHandle,
) -> Result<(), NativeWindowError> {
    Err(NativeWindowError::UnsupportedHandle)
}

/// Ends the process after every BongoCat-owned worker and native resource has
/// shut down, the same contract the Windows entry point keeps.
pub fn terminate_after_product_shutdown(exit_code: i32) -> ! {
    std::process::exit(exit_code)
}
