//! Linux evdev keycodes accepted from the phone.
//!
//! The phone sends physical keycodes (from `linux/input-event-codes.h`), never characters, so
//! every compositor shortcut works. The daemon refuses codes that could power off / suspend the
//! machine (a stray tap should not be able to do that) and anything outside the keyboard range.

/// Highest keyboard keycode we accept (KEY_MICMUTE = 248).
pub const MAX_KEYCODE: u16 = 248;

/// Codes that are never injected.
pub const BLOCKED_KEYCODES: &[u16] = &[
    116, // KEY_POWER
    142, // KEY_SLEEP
    143, // KEY_WAKEUP
    205, // KEY_SUSPEND
];

pub fn is_allowed_keycode(code: u16) -> bool {
    (1..=MAX_KEYCODE).contains(&code) && !BLOCKED_KEYCODES.contains(&code)
}

/// A few well-known codes, used by tests and clients.
pub mod code {
    pub const ESC: u16 = 1;
    pub const A: u16 = 30;
    pub const LEFTCTRL: u16 = 29;
    pub const LEFTSHIFT: u16 = 42;
    pub const LEFTALT: u16 = 56;
    pub const TAB: u16 = 15;
    pub const F4: u16 = 62;
    pub const LEFTMETA: u16 = 125;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges() {
        assert!(!is_allowed_keycode(0));
        assert!(is_allowed_keycode(code::ESC));
        assert!(is_allowed_keycode(code::LEFTMETA));
        assert!(is_allowed_keycode(MAX_KEYCODE));
        assert!(!is_allowed_keycode(MAX_KEYCODE + 1));
        assert!(!is_allowed_keycode(0x110)); // BTN_LEFT must go through MouseButton
        for c in BLOCKED_KEYCODES {
            assert!(!is_allowed_keycode(*c));
        }
    }
}
