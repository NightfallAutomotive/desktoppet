//! One `/dev/input/event*` node, opened read-only and never grabbed.
//!
//! The device is only observed: `EVIOCGRAB` is never issued, so every other
//! application keeps receiving the same input (ADR-0090). Two read-only ioctls
//! are used, `EVIOCGBIT` to tell keyboards and mice from everything else and
//! `EVIOCGKEY` to read which keys and buttons are down right now, which is what
//! reconciliation compares against.

use super::decoder::{EV_KEY, EV_REL, INPUT_EVENT_SIZE, REL_X, RawEvent, parse_events};
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

/// `_IOC(_IOC_READ, 'E', nr, size)` from `<asm-generic/ioctl.h>`.
const fn ioc_read(nr: u32, size: usize) -> u32 {
    (2 << 30) | ((size as u32 & 0x3fff) << 16) | ((b'E' as u32) << 8) | nr
}

const fn eviocgbit(event_type: u16, size: usize) -> u32 {
    ioc_read(0x20 + event_type as u32, size)
}

const EVIOCGKEY: u32 = ioc_read(0x18, KEY_BYTES);

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
/// keyboards and mice are opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DeviceKind {
    pub(crate) keyboard: bool,
    pub(crate) mouse: bool,
}

pub(crate) struct InputDevice {
    path: PathBuf,
    file: File,
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
        let kind = DeviceKind {
            // Every keyboard has letter keys; power buttons and media remotes
            // that only report EV_KEY do not.
            keyboard: bit(&keys, KEY_A),
            mouse: bit(&keys, BTN_LEFT) && bit(&relative, usize::from(REL_X)),
        };
        if !kind.keyboard && !kind.mouse {
            return Err(OpenError::Skipped);
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
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
    fn a_missing_node_is_skipped_rather_than_an_error() {
        assert!(matches!(
            InputDevice::open(Path::new("/dev/input/event-does-not-exist")),
            Err(OpenError::Skipped)
        ));
    }
}
