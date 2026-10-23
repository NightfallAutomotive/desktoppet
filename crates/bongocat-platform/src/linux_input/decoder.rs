//! Raw evdev events to the product's input vocabulary, one device at a time.
//!
//! The decoder holds no file descriptor, so every rule here is tested on
//! synthetic events: key and button presses and releases become edges, the
//! kernel's auto-repeat is ignored, relative pointer motion is summed per
//! report, and a `SYN_DROPPED` asks the worker to reconcile at once, because
//! the kernel discarded events and an edge may be among them.
//!
//! A touchpad reports where a finger is rather than how far it moved, so its
//! motion is the change between consecutive positions while exactly one finger
//! is down, scaled to pixels. Two- and three-finger gestures are scrolling or
//! swiping, not pointing, and move nothing; the touch and tool codes are touch
//! state, not keys.

use super::keymap::{BUTTON_END, BUTTON_FIRST, mouse_button, physical_key};
use bongocat_input::{InputControl, InputEdge};

pub(crate) const EV_SYN: u16 = 0x00;
pub(crate) const EV_KEY: u16 = 0x01;
pub(crate) const EV_REL: u16 = 0x02;
pub(crate) const EV_ABS: u16 = 0x03;
pub(crate) const ABS_X: u16 = 0x00;
pub(crate) const ABS_Y: u16 = 0x01;
const BTN_TOUCH: u16 = 0x14a;
const BTN_TOOL_FINGER: u16 = 0x145;
/// `BTN_TOOL_DOUBLETAP` to `BTN_TOOL_QUINTTAP`: more than one finger.
const BTN_TOOL_MULTI_FINGER: std::ops::RangeInclusive<u16> = 0x148..=0x14f;
/// The digitizer and tool codes (`BTN_DIGI`..`BTN_WHEEL`): touch state, not keys.
const DIGITIZER_CODES: std::ops::Range<u16> = 0x140..0x150;
pub(crate) const SYN_REPORT: u16 = 0;
pub(crate) const SYN_DROPPED: u16 = 3;
pub(crate) const REL_X: u16 = 0x00;
pub(crate) const REL_Y: u16 = 0x01;

/// One `struct input_event`, without its timestamp: the worker stamps events
/// with the product's own monotonic clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RawEvent {
    pub(crate) kind: u16,
    pub(crate) code: u16,
    pub(crate) value: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Decoded {
    Edge {
        control: InputControl,
        edge: InputEdge,
    },
    /// The summed relative motion of one report.
    Motion { dx: f64, dy: f64 },
    /// Events were dropped; the pressed state must be re-read from the device.
    Resync,
    /// A key or button the product has no name for.
    Unmapped,
}

/// Pixels per axis unit of a touchpad.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TouchpadScale {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[derive(Debug, Default)]
struct Touch {
    scale: Option<TouchpadScale>,
    touching: bool,
    single_finger: bool,
    /// Fingers beyond the first are down: a gesture, not pointing.
    multi_finger: bool,
    last: (Option<i32>, Option<i32>),
}

#[derive(Debug, Default)]
pub(crate) struct Decoder {
    pending_motion: (f64, f64),
    /// Between a `SYN_DROPPED` and the next `SYN_REPORT` every event is stale
    /// and is discarded, as the evdev protocol requires.
    dropping: bool,
    touch: Touch,
}

impl Decoder {
    /// A decoder for a touchpad, whose finger positions become motion.
    pub(crate) fn for_touchpad(scale: TouchpadScale) -> Self {
        Self {
            touch: Touch {
                scale: Some(scale),
                ..Touch::default()
            },
            ..Self::default()
        }
    }

    fn pointing(&self) -> bool {
        self.touch.touching && self.touch.single_finger && !self.touch.multi_finger
    }

    fn absolute(&mut self, axis: u16, value: i32) {
        let Some(scale) = self.touch.scale else {
            return;
        };
        let pointing = self.pointing();
        let (last, factor, pending) = if axis == ABS_X {
            (&mut self.touch.last.0, scale.x, &mut self.pending_motion.0)
        } else {
            (&mut self.touch.last.1, scale.y, &mut self.pending_motion.1)
        };
        if pointing && let Some(previous) = *last {
            *pending += f64::from(value - previous) * factor;
        }
        *last = pointing.then_some(value);
    }

    fn touch_state(&mut self, code: u16, down: bool) {
        match code {
            BTN_TOUCH => self.touch.touching = down,
            BTN_TOOL_FINGER => self.touch.single_finger = down,
            code if BTN_TOOL_MULTI_FINGER.contains(&code) => self.touch.multi_finger = down,
            _ => {}
        }
        // Any change of fingers starts a new stroke: positions from before it
        // must not become motion.
        self.touch.last = (None, None);
    }

    /// Decode one event; most events decode to nothing until their report ends.
    pub(crate) fn decode(&mut self, event: RawEvent) -> Option<Decoded> {
        if self.dropping {
            if event.kind == EV_SYN && event.code == SYN_REPORT {
                self.dropping = false;
            }
            return None;
        }
        match (event.kind, event.code) {
            (EV_SYN, SYN_DROPPED) => {
                self.dropping = true;
                self.pending_motion = (0.0, 0.0);
                self.touch.last = (None, None);
                Some(Decoded::Resync)
            }
            (EV_ABS, axis @ (ABS_X | ABS_Y)) => {
                self.absolute(axis, event.value);
                None
            }
            (EV_KEY, code) if DIGITIZER_CODES.contains(&code) => {
                if event.value != 2 {
                    self.touch_state(code, event.value != 0);
                }
                None
            }
            (EV_SYN, SYN_REPORT) => {
                let (dx, dy) = std::mem::take(&mut self.pending_motion);
                (dx != 0.0 || dy != 0.0).then_some(Decoded::Motion { dx, dy })
            }
            (EV_REL, REL_X) => {
                self.pending_motion.0 += f64::from(event.value);
                None
            }
            (EV_REL, REL_Y) => {
                self.pending_motion.1 += f64::from(event.value);
                None
            }
            (EV_KEY, code) => {
                let edge = match event.value {
                    0 => InputEdge::Up,
                    1 => InputEdge::Down,
                    // 2 is the kernel's auto-repeat: the key is already down.
                    _ => return None,
                };
                Some(
                    control_for(code)
                        .map_or(Decoded::Unmapped, |control| Decoded::Edge { control, edge }),
                )
            }
            _ => None,
        }
    }
}

/// The product control for a key or button code.
pub(crate) fn control_for(code: u16) -> Option<InputControl> {
    if (BUTTON_FIRST..BUTTON_END).contains(&code) {
        mouse_button(code).map(InputControl::Mouse)
    } else {
        physical_key(code).map(InputControl::Key)
    }
}

/// Parse `struct input_event` records (x86_64 layout: a 16-byte `timeval`,
/// then `u16 type`, `u16 code`, `i32 value`). A trailing partial record is
/// ignored; the kernel only ever returns whole ones.
pub(crate) const INPUT_EVENT_SIZE: usize = 24;

pub(crate) fn parse_events(bytes: &[u8]) -> impl Iterator<Item = RawEvent> + '_ {
    bytes.chunks_exact(INPUT_EVENT_SIZE).map(|record| RawEvent {
        kind: u16::from_ne_bytes([record[16], record[17]]),
        code: u16::from_ne_bytes([record[18], record[19]]),
        value: i32::from_ne_bytes([record[20], record[21], record[22], record[23]]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bongocat_input::{MouseButton, PhysicalKey};

    fn key(code: u16, value: i32) -> RawEvent {
        RawEvent {
            kind: EV_KEY,
            code,
            value,
        }
    }

    fn rel(code: u16, value: i32) -> RawEvent {
        RawEvent {
            kind: EV_REL,
            code,
            value,
        }
    }

    const REPORT: RawEvent = RawEvent {
        kind: EV_SYN,
        code: SYN_REPORT,
        value: 0,
    };

    #[test]
    fn presses_and_releases_become_edges_and_repeats_are_ignored() {
        let mut decoder = Decoder::default();
        let a = InputControl::Key(PhysicalKey::from_hid_usage(0x04));
        assert_eq!(
            decoder.decode(key(30, 1)),
            Some(Decoded::Edge {
                control: a,
                edge: InputEdge::Down
            })
        );
        assert_eq!(decoder.decode(key(30, 2)), None);
        assert_eq!(
            decoder.decode(key(30, 0)),
            Some(Decoded::Edge {
                control: a,
                edge: InputEdge::Up
            })
        );
        assert_eq!(
            decoder.decode(key(0x110, 1)),
            Some(Decoded::Edge {
                control: InputControl::Mouse(MouseButton::Left),
                edge: InputEdge::Down
            })
        );
        assert_eq!(decoder.decode(key(0x2ff, 1)), Some(Decoded::Unmapped));
    }

    #[test]
    fn motion_is_summed_until_the_report_ends() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.decode(rel(REL_X, 3)), None);
        assert_eq!(decoder.decode(rel(REL_Y, -2)), None);
        assert_eq!(decoder.decode(rel(REL_X, 4)), None);
        assert_eq!(
            decoder.decode(REPORT),
            Some(Decoded::Motion { dx: 7.0, dy: -2.0 })
        );
        assert_eq!(
            decoder.decode(REPORT),
            None,
            "an empty report has no motion"
        );
    }

    #[test]
    fn a_dropped_buffer_resyncs_and_discards_the_stale_report() {
        let mut decoder = Decoder::default();
        decoder.decode(rel(REL_X, 5));
        assert_eq!(
            decoder.decode(RawEvent {
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0
            }),
            Some(Decoded::Resync)
        );
        assert_eq!(
            decoder.decode(key(30, 0)),
            None,
            "stale until the report ends"
        );
        assert_eq!(decoder.decode(REPORT), None);
        assert!(matches!(
            decoder.decode(key(30, 1)),
            Some(Decoded::Edge { .. })
        ));
    }

    fn abs(code: u16, value: i32) -> RawEvent {
        RawEvent {
            kind: EV_ABS,
            code,
            value,
        }
    }

    fn touchpad() -> Decoder {
        Decoder::for_touchpad(TouchpadScale { x: 0.5, y: 0.25 })
    }

    #[test]
    fn one_finger_on_a_touchpad_moves_by_its_position_change() {
        let mut decoder = touchpad();
        for event in [
            key(BTN_TOUCH, 1),
            key(BTN_TOOL_FINGER, 1),
            abs(ABS_X, 100),
            abs(ABS_Y, 100),
        ] {
            assert_eq!(decoder.decode(event), None);
        }
        assert_eq!(
            decoder.decode(REPORT),
            None,
            "the first position is not motion"
        );
        decoder.decode(abs(ABS_X, 140));
        decoder.decode(abs(ABS_Y, 80));
        assert_eq!(
            decoder.decode(REPORT),
            Some(Decoded::Motion { dx: 20.0, dy: -5.0 })
        );
        // Lifting the finger ends the stroke: the next touch starts fresh.
        decoder.decode(key(BTN_TOUCH, 0));
        decoder.decode(key(BTN_TOOL_FINGER, 0));
        decoder.decode(REPORT);
        decoder.decode(key(BTN_TOUCH, 1));
        decoder.decode(key(BTN_TOOL_FINGER, 1));
        decoder.decode(abs(ABS_X, 900));
        assert_eq!(decoder.decode(REPORT), None, "no jump to the new touch");
    }

    #[test]
    fn a_two_finger_gesture_moves_nothing() {
        let mut decoder = touchpad();
        decoder.decode(key(BTN_TOUCH, 1));
        decoder.decode(key(BTN_TOOL_FINGER, 1));
        decoder.decode(abs(ABS_X, 100));
        decoder.decode(REPORT);
        decoder.decode(key(BTN_TOOL_FINGER, 0));
        decoder.decode(key(0x14d, 1));
        decoder.decode(abs(ABS_X, 300));
        assert_eq!(decoder.decode(REPORT), None);
    }

    #[test]
    fn touch_codes_are_not_counted_as_unmapped_keys() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.decode(key(BTN_TOUCH, 1)), None);
        assert_eq!(decoder.decode(key(BTN_TOOL_FINGER, 1)), None);
    }

    #[test]
    fn a_mouse_ignores_absolute_axes() {
        let mut decoder = Decoder::default();
        decoder.decode(abs(ABS_X, 10));
        decoder.decode(abs(ABS_X, 50));
        assert_eq!(decoder.decode(REPORT), None);
    }

    #[test]
    fn records_parse_in_the_kernel_layout() {
        let mut bytes = vec![0_u8; INPUT_EVENT_SIZE * 2 + 5];
        bytes[16..18].copy_from_slice(&EV_KEY.to_ne_bytes());
        bytes[18..20].copy_from_slice(&30_u16.to_ne_bytes());
        bytes[20..24].copy_from_slice(&1_i32.to_ne_bytes());
        bytes[40..42].copy_from_slice(&EV_REL.to_ne_bytes());
        bytes[42..44].copy_from_slice(&REL_Y.to_ne_bytes());
        bytes[44..48].copy_from_slice(&(-9_i32).to_ne_bytes());
        assert_eq!(
            parse_events(&bytes).collect::<Vec<_>>(),
            vec![key(30, 1), rel(REL_Y, -9)]
        );
    }
}
