//! Linux input-event codes to the product's physical key vocabulary.
//!
//! Every platform reports keys in its own code space and the product compares
//! them as USB HID keyboard usages (`PhysicalKey`). This table is the Linux
//! half: it is the inverse of the kernel's own HID-to-evdev mapping
//! (`drivers/hid/hid-input.c`), restricted to the usages the product knows, so
//! a key maps to the same usage it would on Windows and macOS. A code with no
//! entry is counted as unmapped rather than guessed.

use bongocat_input::{MouseButton, PhysicalKey};

/// `(evdev KEY_* code, HID keyboard usage)`.
const KEY_USAGES: &[(u16, u16)] = &[
    // Letters.
    (30, 0x04), // A
    (48, 0x05), // B
    (46, 0x06), // C
    (32, 0x07), // D
    (18, 0x08), // E
    (33, 0x09), // F
    (34, 0x0a), // G
    (35, 0x0b), // H
    (23, 0x0c), // I
    (36, 0x0d), // J
    (37, 0x0e), // K
    (38, 0x0f), // L
    (50, 0x10), // M
    (49, 0x11), // N
    (24, 0x12), // O
    (25, 0x13), // P
    (16, 0x14), // Q
    (19, 0x15), // R
    (31, 0x16), // S
    (20, 0x17), // T
    (22, 0x18), // U
    (47, 0x19), // V
    (17, 0x1a), // W
    (45, 0x1b), // X
    (21, 0x1c), // Y
    (44, 0x1d), // Z
    // Digit row.
    (2, 0x1e),  // 1
    (3, 0x1f),  // 2
    (4, 0x20),  // 3
    (5, 0x21),  // 4
    (6, 0x22),  // 5
    (7, 0x23),  // 6
    (8, 0x24),  // 7
    (9, 0x25),  // 8
    (10, 0x26), // 9
    (11, 0x27), // 0
    // Editing and punctuation.
    (28, 0x28), // Enter
    (1, 0x29),  // Escape
    (14, 0x2a), // Backspace
    (15, 0x2b), // Tab
    (57, 0x2c), // Space
    (12, 0x2d), // Minus
    (13, 0x2e), // Equal
    (26, 0x2f), // Left brace
    (27, 0x30), // Right brace
    (43, 0x31), // Backslash
    (39, 0x33), // Semicolon
    (40, 0x34), // Apostrophe
    (41, 0x35), // Grave
    (51, 0x36), // Comma
    (52, 0x37), // Dot
    (53, 0x38), // Slash
    (58, 0x39), // Caps Lock
    // Function keys.
    (59, 0x3a),  // F1
    (60, 0x3b),  // F2
    (61, 0x3c),  // F3
    (62, 0x3d),  // F4
    (63, 0x3e),  // F5
    (64, 0x3f),  // F6
    (65, 0x40),  // F7
    (66, 0x41),  // F8
    (67, 0x42),  // F9
    (68, 0x43),  // F10
    (87, 0x44),  // F11
    (88, 0x45),  // F12
    (183, 0x68), // F13
    (184, 0x69), // F14
    (185, 0x6a), // F15
    (186, 0x6b), // F16
    (187, 0x6c), // F17
    (188, 0x6d), // F18
    (189, 0x6e), // F19
    (190, 0x6f), // F20
    (191, 0x70), // F21
    (192, 0x71), // F22
    (193, 0x72), // F23
    (194, 0x73), // F24
    // Navigation.
    (99, 0x46),  // Print Screen (SysRq)
    (70, 0x47),  // Scroll Lock
    (119, 0x48), // Pause
    (110, 0x49), // Insert
    (102, 0x4a), // Home
    (104, 0x4b), // Page Up
    (111, 0x4c), // Delete
    (107, 0x4d), // End
    (109, 0x4e), // Page Down
    (106, 0x4f), // Right
    (105, 0x50), // Left
    (108, 0x51), // Down
    (103, 0x52), // Up
    // Keypad.
    (69, 0x53),  // Num Lock
    (98, 0x54),  // Keypad /
    (55, 0x55),  // Keypad *
    (74, 0x56),  // Keypad -
    (78, 0x57),  // Keypad +
    (96, 0x58),  // Keypad Enter
    (79, 0x59),  // Keypad 1
    (80, 0x5a),  // Keypad 2
    (81, 0x5b),  // Keypad 3
    (75, 0x5c),  // Keypad 4
    (76, 0x5d),  // Keypad 5
    (77, 0x5e),  // Keypad 6
    (71, 0x5f),  // Keypad 7
    (72, 0x60),  // Keypad 8
    (73, 0x61),  // Keypad 9
    (82, 0x62),  // Keypad 0
    (83, 0x63),  // Keypad .
    (117, 0x67), // Keypad =
    (121, 0x85), // Keypad ,
    // International and layout keys.
    (86, 0x64),  // Non-US \ (102nd key)
    (127, 0x65), // Application (Compose / Menu)
    (89, 0x87),  // International 1 (Ro)
    (93, 0x88),  // International 2 (Katakana/Hiragana)
    (124, 0x89), // International 3 (Yen)
    (92, 0x8a),  // International 4 (Henkan)
    (94, 0x8b),  // International 5 (Muhenkan)
    (122, 0x90), // LANG1 (Hangeul)
    (123, 0x91), // LANG2 (Hanja)
    // Modifiers.
    (29, 0xe0),  // Left Control
    (42, 0xe1),  // Left Shift
    (56, 0xe2),  // Left Alt
    (125, 0xe3), // Left Meta
    (97, 0xe4),  // Right Control
    (54, 0xe5),  // Right Shift
    (100, 0xe6), // Right Alt
    (126, 0xe7), // Right Meta
];

/// The physical key for a Linux `KEY_*` code, or `None` when unmapped.
pub(crate) fn physical_key(code: u16) -> Option<PhysicalKey> {
    KEY_USAGES
        .iter()
        .find(|(key, _)| *key == code)
        .map(|(_, usage)| PhysicalKey::from_hid_usage(*usage))
}

/// The first `BTN_*` code a pointing device reports (`BTN_MOUSE`).
pub(crate) const BUTTON_FIRST: u16 = 0x110;
/// One past the last mouse button code (`BTN_JOYSTICK`).
pub(crate) const BUTTON_END: u16 = 0x120;

/// The mouse button for a Linux `BTN_*` code in the mouse range.
pub(crate) fn mouse_button(code: u16) -> Option<MouseButton> {
    Some(match code {
        0x110 => MouseButton::Left,
        0x111 => MouseButton::Right,
        0x112 => MouseButton::Middle,
        // BTN_SIDE and BTN_BACK are both "back" on the mice that report them;
        // BTN_EXTRA and BTN_FORWARD are both "forward".
        0x113 | 0x116 => MouseButton::Back,
        0x114 | 0x115 => MouseButton::Forward,
        0x117..BUTTON_END => MouseButton::Other(u8::try_from(code - BUTTON_FIRST).ok()?),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_modifiers_map_to_their_hid_usages() {
        assert_eq!(physical_key(30), Some(PhysicalKey::from_hid_usage(0x04)));
        assert_eq!(physical_key(44), Some(PhysicalKey::from_hid_usage(0x1d)));
        assert_eq!(physical_key(11), Some(PhysicalKey::from_hid_usage(0x27)));
        assert_eq!(physical_key(57), Some(PhysicalKey::from_hid_usage(0x2c)));
        assert_eq!(physical_key(29), Some(PhysicalKey::from_hid_usage(0xe0)));
        assert_eq!(physical_key(126), Some(PhysicalKey::from_hid_usage(0xe7)));
        assert_eq!(physical_key(0), None, "KEY_RESERVED is never a key");
        assert_eq!(physical_key(0x110), None, "buttons are not keys");
    }

    #[test]
    fn every_code_and_usage_appears_once() {
        let mut codes = KEY_USAGES.iter().map(|(code, _)| *code).collect::<Vec<_>>();
        let mut usages = KEY_USAGES
            .iter()
            .map(|(_, usage)| *usage)
            .collect::<Vec<_>>();
        codes.sort_unstable();
        usages.sort_unstable();
        assert!(
            codes.windows(2).all(|pair| pair[0] != pair[1]),
            "duplicate code"
        );
        assert!(
            usages.windows(2).all(|pair| pair[0] != pair[1]),
            "duplicate usage"
        );
    }

    #[test]
    fn mouse_buttons_map_and_other_codes_do_not() {
        assert_eq!(mouse_button(0x110), Some(MouseButton::Left));
        assert_eq!(mouse_button(0x111), Some(MouseButton::Right));
        assert_eq!(mouse_button(0x112), Some(MouseButton::Middle));
        assert_eq!(mouse_button(0x113), Some(MouseButton::Back));
        assert_eq!(mouse_button(0x115), Some(MouseButton::Forward));
        assert_eq!(mouse_button(0x117), Some(MouseButton::Other(7)));
        assert_eq!(
            mouse_button(0x120),
            None,
            "joystick buttons are not mouse buttons"
        );
        assert_eq!(mouse_button(30), None);
    }
}
