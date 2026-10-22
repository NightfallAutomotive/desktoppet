//! Keyboard, mouse and pointer-motion input on Linux through evdev
//! (ADR-0090 stage L5).
//!
//! The service reads `/dev/input/event*` directly, which works the same on
//! Wayland and X11 and needs read access to those nodes, normally membership in
//! the `input` group. That access can see every key typed on the machine, so
//! nothing here logs or stores a key sequence: keys leave the worker only as
//! edges to the runtime, and diagnostics are counts.

mod decoder;
mod device;
mod keymap;
mod service;
mod session_watch;

pub use service::{DEFAULT_CURSOR_VIEWPORT, LinuxInputService};
