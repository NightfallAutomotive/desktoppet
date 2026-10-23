//! One `/dev/input/event*` node, opened read-only and never grabbed.
//!
//! The device is only observed: `EVIOCGRAB` is never issued, so every other
//! application keeps receiving the same input (ADR-0090). Two read-only ioctls
//! are used, `EVIOCGBIT` to tell keyboards and mice from everything else and
//! `EVIOCGKEY` to read which keys and buttons are down right now, which is what
//! reconciliation compares against.

use super::decoder::{
    ABS_X, ABS_Y, EV_ABS, EV_KEY, EV_REL, INPUT_EVENT_SIZE, REL_X, RawEvent, TouchpadScale,
    parse_events,
};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
};

/// The highest `KEY_*`/`BTN_*` code (`KEY_MAX`).
const KEY_MAX: usize = 0x2ff;
const KEY_BYTES: usize = KEY_MAX / 8 + 1;
/// `EV_MAX`.
const EV_BYTES: usize = 0x1f / 8 + 1;
const KEY_A: usize = 30;
const BTN_LEFT: usize = 0x110;
const BTN_TOOL_FINGER: usize = 0x145;
/// Pointer speed on a touchpad, in pixels per millimetre of finger travel: a
/// moderate unaccelerated speed, since the model only follows the motion.
const TOUCHPAD_PIXELS_PER_MM: f64 = 8.0;
/// The width assumed for a touchpad that reports no resolution.
const TOUCHPAD_FALLBACK_WIDTH_MM: f64 = 100.0;

/// `_IOC(_IOC_READ, 'E', nr, size)` from `<asm-generic/ioctl.h>`.
const fn ioc_read(nr: u32, size: usize) -> u32 {
    (2 << 30) | ((size as u32 & 0x3fff) << 16) | ((b'E' as u32) << 8) | nr
}

const fn eviocgbit(event_type: u16, size: usize) -> u32 {
    ioc_read(0x20 + event_type as u32, size)
}

const EVIOCGKEY: u32 = ioc_read(0x18, KEY_BYTES);

/// `struct input_absinfo`: six `i32` fields.
const ABSINFO_BYTES: usize = 24;

const fn eviocgabs(axis: u16) -> u32 {
    ioc_read(0x40 + axis as u32, ABSINFO_BYTES)
}

/// `(minimum, maximum, resolution)` of one absolute axis.
fn axis_info(file: &File, axis: u16) -> io::Result<(i32, i32, i32)> {
    let mut info = [0_u8; ABSINFO_BYTES];
    read_bits(file, eviocgabs(axis), &mut info)?;
    let field = |index: usize| {
        i32::from_ne_bytes([
            info[index * 4],
            info[index * 4 + 1],
            info[index * 4 + 2],
            info[index * 4 + 3],
        ])
    };
    // value, minimum, maximum, fuzz, flat, resolution (units per mm).
    Ok((field(1), field(2), field(5)))
}

/// Pixels per axis unit for a touchpad axis.
pub(crate) fn touchpad_axis_scale(minimum: i32, maximum: i32, resolution: i32) -> f64 {
    let units_per_mm = if resolution > 0 {
        f64::from(resolution)
    } else {
        (f64::from(maximum) - f64::from(minimum)).max(1.0) / TOUCHPAD_FALLBACK_WIDTH_MM
    };
    TOUCHPAD_PIXELS_PER_MM / units_per_mm
}

fn bit(bits: &[u8], index: usize) -> bool {
    bits.get(index / 8)
        .is_some_and(|byte| byte & (1 << (index % 8)) != 0)
}

/// Read one capability bitmap. `buffer` is the size the request encodes, so the
/// kernel never writes past it.
fn read_bits(file: &File, request: u32, buffer: &mut [u8]) -> io::Result<()> {
    // SAFETY: `request` is an `EVIOCG*` read request whose encoded size equals
    // `buffer.len()`, so the kernel writes at most that many bytes into the
    // live, exclusively borrowed `buffer`. The descriptor belongs to `file`,
    // which outlives the call.
    let result = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            libc::Ioctl::from(request),
            buffer.as_mut_ptr(),
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// What a device can report, as far as the product is concerned. Only
/// keyboards, mice and touchpads are opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DeviceKind {
    keyboard: bool,
    mouse: bool,
    touchpad: bool,
}

pub(crate) struct InputDevice {
    path: PathBuf,
    file: File,
    touchpad: Option<TouchpadScale>,
}

/// Why a node was not opened.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// The process may not read this node (not in the `input` group).
    PermissionDenied,
    /// Gone, not an evdev node, or not a keyboard or mouse.
    Skipped,
}

impl InputDevice {
    pub(crate) fn open(path: &Path) -> Result<Self, OpenError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| match error.kind() {
                io::ErrorKind::PermissionDenied => OpenError::PermissionDenied,
                _ => OpenError::Skipped,
            })?;
        let mut types = [0_u8; EV_BYTES];
        read_bits(&file, eviocgbit(0, EV_BYTES), &mut types).map_err(|_| OpenError::Skipped)?;
        let mut keys = [0_u8; KEY_BYTES];
        if bit(&types, usize::from(EV_KEY)) {
            read_bits(&file, eviocgbit(EV_KEY, KEY_BYTES), &mut keys)
                .map_err(|_| OpenError::Skipped)?;
        }
        let mut relative = [0_u8; 1];
        if bit(&types, usize::from(EV_REL)) {
            read_bits(&file, eviocgbit(EV_REL, 1), &mut relative)
                .map_err(|_| OpenError::Skipped)?;
        }
        let mut absolute = [0_u8; 8];
        if bit(&types, usize::from(EV_ABS)) {
            read_bits(&file, eviocgbit(EV_ABS, 8), &mut absolute)
                .map_err(|_| OpenError::Skipped)?;
        }
        let kind = DeviceKind {
            // Every keyboard has letter keys; power buttons and media remotes
            // that only report EV_KEY do not.
            keyboard: bit(&keys, KEY_A),
            mouse: bit(&keys, BTN_LEFT) && bit(&relative, usize::from(REL_X)),
            // A touchpad reports finger positions, not motion; a tablet or a
            // touchscreen has no finger tool and is not a pointer to follow.
            touchpad: bit(&keys, BTN_TOOL_FINGER)
                && bit(&absolute, usize::from(ABS_X))
                && bit(&absolute, usize::from(ABS_Y)),
        };
        if !kind.keyboard && !kind.mouse && !kind.touchpad {
            return Err(OpenError::Skipped);
        }
        let touchpad = if kind.touchpad {
            let (x_min, x_max, x_resolution) =
                axis_info(&file, ABS_X).map_err(|_| OpenError::Skipped)?;
            let (y_min, y_max, y_resolution) =
                axis_info(&file, ABS_Y).map_err(|_| OpenError::Skipped)?;
            Some(TouchpadScale {
                x: touchpad_axis_scale(x_min, x_max, x_resolution),
                y: touchpad_axis_scale(y_min, y_max, y_resolution),
            })
        } else {
            None
        };
        Ok(Self {
            path: path.to_path_buf(),
            file,
            touchpad,
        })
    }

    /// The motion scale when this device is a touchpad.
    pub(crate) fn touchpad(&self) -> Option<TouchpadScale> {
        self.touchpad
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Every event currently queued. `Err` means the device is gone.
    pub(crate) fn read_events(&mut self, events: &mut Vec<RawEvent>) -> io::Result<()> {
        let mut buffer = [0_u8; INPUT_EVENT_SIZE * 64];
        loop {
            match self.file.read(&mut buffer) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(read) => events.extend(parse_events(&buffer[..read])),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Every `KEY_*`/`BTN_*` code that is down right now.
    pub(crate) fn pressed_codes(&self) -> io::Result<Vec<u16>> {
        let mut keys = [0_u8; KEY_BYTES];
        read_bits(&self.file, EVIOCGKEY, &mut keys)?;
        Ok((0..=KEY_MAX)
            .filter(|code| bit(&keys, *code))
            .filter_map(|code| u16::try_from(code).ok())
            .collect())
    }
}

/// Wait until one of `fds` is readable or `timeout_ms` passes. Returns the
/// indices of the readable descriptors.
pub(crate) fn poll_readable(fds: &[RawFd], timeout_ms: i32) -> io::Result<Vec<usize>> {
    let mut polled = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: *fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect::<Vec<_>>();
    let count = libc::nfds_t::try_from(polled.len())
        .map_err(|_| io::Error::other("too many input devices to poll"))?;
    // SAFETY: `polled` is a live, exclusively borrowed array of `count`
    // initialized `pollfd` records, which is exactly what `poll` reads and
    // writes. Every descriptor belongs to a device the caller keeps open for
    // the duration of the call.
    let result = unsafe { libc::poll(polled.as_mut_ptr(), count, timeout_ms) };
    if result < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(Vec::new())
        } else {
            Err(error)
        };
    }
    Ok(polled
        .iter()
        .enumerate()
        .filter(|(_, fd)| fd.revents != 0)
        .map(|(index, _)| index)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_requests_match_the_kernel_encoding() {
        // EVIOCGBIT(0, 4) and EVIOCGKEY(96) as <linux/input.h> computes them.
        assert_eq!(eviocgbit(0, 4), 0x8004_4520);
        assert_eq!(eviocgbit(EV_KEY, KEY_BYTES), 0x8060_4521);
        assert_eq!(EVIOCGKEY, 0x8060_4518);
        // EVIOCGABS(ABS_Y) reads a 24-byte input_absinfo.
        assert_eq!(eviocgabs(ABS_Y), 0x8018_4541);
    }

    #[test]
    fn bits_are_read_little_endian_within_each_byte() {
        let bits = [0b0000_0010, 0b1000_0000];
        assert!(bit(&bits, 1));
        assert!(bit(&bits, 15));
        assert!(!bit(&bits, 0));
        assert!(!bit(&bits, 64), "past the end is clear");
    }

    #[test]
    fn touchpad_motion_scales_by_the_reported_resolution() {
        // 32 units per mm moves 8 px per mm: a quarter pixel per unit.
        assert!((touchpad_axis_scale(0, 3200, 32) - 0.25).abs() < 1e-9);
        // No resolution: the axis is assumed to be 100 mm wide.
        assert!((touchpad_axis_scale(0, 1000, 0) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn a_missing_node_is_skipped_rather_than_an_error() {
        assert!(matches!(
            InputDevice::open(Path::new("/dev/input/event-does-not-exist")),
            Err(OpenError::Skipped)
        ));
    }
}
