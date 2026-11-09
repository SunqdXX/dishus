//! Keyboard and mouse through `SendInput` (Windows).
//!
//! The phone sends Linux evdev keycodes; they are mapped to PC/AT set-1 scan codes and injected
//! with `KEYEVENTF_SCANCODE`, so the PC's own keyboard layout applies, just like a USB keyboard
//! (and like uinput on Linux). Extended keys (arrows, right Ctrl/Alt, the Insert/Home/PageUp
//! block, keypad Enter and /, the Windows keys) carry `KEYEVENTF_EXTENDEDKEY`.
//!
//! Limits of `SendInput` that no unprivileged program can get around: it does not reach windows
//! of elevated programs (UIPI), nor the secure desktop (UAC prompt, Ctrl+Alt+Del screen, lock
//! screen). Events sent while blocked are dropped and the block is logged once.
#![allow(unsafe_code)]

use std::{collections::BTreeSet, io};

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_HANGUL, VK_HANJA, VK_NUMLOCK,
    VK_PAUSE,
};

use dishus_proto::{MouseButton, keys::is_allowed_keycode};

use crate::backend::InputSink;

/// Where an evdev keycode goes on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WinKey {
    /// Set-1 scan code; `0xE0xx` = extended (`KEYEVENTF_EXTENDEDKEY`).
    Scan(u16),
    /// Keys whose scan code can't be sent on its own (Pause is `E1 1D 45`, NumLock shares 0x45
    /// with it, the Korean keys have no make/break pair): sent by virtual-key code.
    Vk(u16, bool),
}

/// evdev keycode → Windows key. Everything the phone's keyboard has, plus the common extra keys
/// of a PC keyboard. Codes not listed are dropped.
pub const KEYMAP: &[(u16, WinKey)] = {
    use WinKey::{Scan as S, Vk};
    &[
        // 1..=83 are the set-1 scan codes themselves (evdev was numbered after them), except
        // NumLock (69), which shares its scan code with Pause.
        (1, S(0x01)),                  // ESC
        (2, S(0x02)),                  // 1
        (3, S(0x03)),                  // 2
        (4, S(0x04)),                  // 3
        (5, S(0x05)),                  // 4
        (6, S(0x06)),                  // 5
        (7, S(0x07)),                  // 6
        (8, S(0x08)),                  // 7
        (9, S(0x09)),                  // 8
        (10, S(0x0A)),                 // 9
        (11, S(0x0B)),                 // 0
        (12, S(0x0C)),                 // MINUS
        (13, S(0x0D)),                 // EQUAL
        (14, S(0x0E)),                 // BACKSPACE
        (15, S(0x0F)),                 // TAB
        (16, S(0x10)),                 // Q
        (17, S(0x11)),                 // W
        (18, S(0x12)),                 // E
        (19, S(0x13)),                 // R
        (20, S(0x14)),                 // T
        (21, S(0x15)),                 // Y
        (22, S(0x16)),                 // U
        (23, S(0x17)),                 // I
        (24, S(0x18)),                 // O
        (25, S(0x19)),                 // P
        (26, S(0x1A)),                 // LEFTBRACE
        (27, S(0x1B)),                 // RIGHTBRACE
        (28, S(0x1C)),                 // ENTER
        (29, S(0x1D)),                 // LEFTCTRL
        (30, S(0x1E)),                 // A
        (31, S(0x1F)),                 // S
        (32, S(0x20)),                 // D
        (33, S(0x21)),                 // F
        (34, S(0x22)),                 // G
        (35, S(0x23)),                 // H
        (36, S(0x24)),                 // J
        (37, S(0x25)),                 // K
        (38, S(0x26)),                 // L
        (39, S(0x27)),                 // SEMICOLON
        (40, S(0x28)),                 // APOSTROPHE
        (41, S(0x29)),                 // GRAVE
        (42, S(0x2A)),                 // LEFTSHIFT
        (43, S(0x2B)),                 // BACKSLASH
        (44, S(0x2C)),                 // Z
        (45, S(0x2D)),                 // X
        (46, S(0x2E)),                 // C
        (47, S(0x2F)),                 // V
        (48, S(0x30)),                 // B
        (49, S(0x31)),                 // N
        (50, S(0x32)),                 // M
        (51, S(0x33)),                 // COMMA
        (52, S(0x34)),                 // DOT
        (53, S(0x35)),                 // SLASH
        (54, S(0x36)),                 // RIGHTSHIFT
        (55, S(0x37)),                 // KPASTERISK
        (56, S(0x38)),                 // LEFTALT
        (57, S(0x39)),                 // SPACE
        (58, S(0x3A)),                 // CAPSLOCK
        (59, S(0x3B)),                 // F1
        (60, S(0x3C)),                 // F2
        (61, S(0x3D)),                 // F3
        (62, S(0x3E)),                 // F4
        (63, S(0x3F)),                 // F5
        (64, S(0x40)),                 // F6
        (65, S(0x41)),                 // F7
        (66, S(0x42)),                 // F8
        (67, S(0x43)),                 // F9
        (68, S(0x44)),                 // F10
        (69, Vk(VK_NUMLOCK.0, true)),  // NUMLOCK
        (70, S(0x46)),                 // SCROLLLOCK
        (71, S(0x47)),                 // KP7
        (72, S(0x48)),                 // KP8
        (73, S(0x49)),                 // KP9
        (74, S(0x4A)),                 // KPMINUS
        (75, S(0x4B)),                 // KP4
        (76, S(0x4C)),                 // KP5
        (77, S(0x4D)),                 // KP6
        (78, S(0x4E)),                 // KPPLUS
        (79, S(0x4F)),                 // KP1
        (80, S(0x50)),                 // KP2
        (81, S(0x51)),                 // KP3
        (82, S(0x52)),                 // KP0
        (83, S(0x53)),                 // KPDOT
        (86, S(0x56)),                 // 102ND (the extra key left of Z on ISO keyboards)
        (87, S(0x57)),                 // F11
        (88, S(0x58)),                 // F12
        (89, S(0x73)),                 // RO
        (92, S(0x79)),                 // HENKAN
        (93, S(0x70)),                 // KATAKANAHIRAGANA
        (94, S(0x7B)),                 // MUHENKAN
        (96, S(0xE01C)),               // KPENTER
        (97, S(0xE01D)),               // RIGHTCTRL
        (98, S(0xE035)),               // KPSLASH
        (99, S(0xE037)),               // SYSRQ (Print Screen)
        (100, S(0xE038)),              // RIGHTALT (AltGr)
        (102, S(0xE047)),              // HOME
        (103, S(0xE048)),              // UP
        (104, S(0xE049)),              // PAGEUP
        (105, S(0xE04B)),              // LEFT
        (106, S(0xE04D)),              // RIGHT
        (107, S(0xE04F)),              // END
        (108, S(0xE050)),              // DOWN
        (109, S(0xE051)),              // PAGEDOWN
        (110, S(0xE052)),              // INSERT
        (111, S(0xE053)),              // DELETE
        (113, S(0xE020)),              // MUTE
        (114, S(0xE02E)),              // VOLUMEDOWN
        (115, S(0xE030)),              // VOLUMEUP
        (117, S(0x59)),                // KPEQUAL
        (119, Vk(VK_PAUSE.0, false)),  // PAUSE
        (121, S(0x7E)),                // KPCOMMA
        (122, Vk(VK_HANGUL.0, false)), // HANGEUL
        (123, Vk(VK_HANJA.0, false)),  // HANJA
        (124, S(0x7D)),                // YEN
        (125, S(0xE05B)),              // LEFTMETA (Windows key)
        (126, S(0xE05C)),              // RIGHTMETA
        (127, S(0xE05D)),              // COMPOSE (Menu key)
        (140, S(0xE021)),              // CALC
        (155, S(0xE06C)),              // MAIL
        (158, S(0xE06A)),              // BACK
        (159, S(0xE069)),              // FORWARD
        (163, S(0xE019)),              // NEXTSONG
        (164, S(0xE022)),              // PLAYPAUSE
        (165, S(0xE010)),              // PREVIOUSSONG
        (166, S(0xE024)),              // STOPCD
        (172, S(0xE032)),              // HOMEPAGE
        (173, S(0xE067)),              // REFRESH
        (183, S(0x64)),                // F13
        (184, S(0x65)),                // F14
        (185, S(0x66)),                // F15
        (186, S(0x67)),                // F16
        (187, S(0x68)),                // F17
        (188, S(0x69)),                // F18
        (189, S(0x6A)),                // F19
        (190, S(0x6B)),                // F20
        (191, S(0x6C)),                // F21
        (192, S(0x6D)),                // F22
        (193, S(0x6E)),                // F23
        (194, S(0x76)),                // F24
        (217, S(0xE065)),              // SEARCH
        (226, S(0xE06D)),              // MEDIA
    ]
};

/// Scan codes of the ACPI power keys; never in [`KEYMAP`], whatever the phone sends.
const POWER_SCANCODES: [u16; 3] = [0xE05E, 0xE05F, 0xE063];

pub fn lookup(code: u16) -> Option<WinKey> {
    KEYMAP
        .binary_search_by_key(&code, |(c, _)| *c)
        .ok()
        .map(|i| KEYMAP[i].1)
}

pub struct SendInputSink {
    /// The last SendInput was refused (secure desktop); logged once until it works again.
    blocked: bool,
    /// Keycodes without a Windows key, logged once each.
    unmapped: BTreeSet<u16>,
}

impl SendInputSink {
    pub fn new() -> Self {
        Self {
            blocked: false,
            unmapped: BTreeSet::new(),
        }
    }

    fn send(&mut self, inputs: &[INPUT]) -> io::Result<()> {
        // SAFETY: a slice of fully initialised INPUT structs and their size.
        let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if n as usize == inputs.len() {
            if self.blocked {
                self.blocked = false;
                tracing::info!("input: SendInput works again");
            }
        } else if !self.blocked {
            // Lock screen, UAC prompt or Ctrl+Alt+Del screen (the secure desktop): nothing an
            // unprivileged program can type into. Logged once per blocked period, not per event.
            self.blocked = true;
            tracing::warn!(
                "input: SendInput blocked ({}); the secure desktop (lock screen, UAC prompt) \
                 is probably showing. Events are dropped until it closes.",
                io::Error::last_os_error()
            );
        }
        Ok(())
    }
}

impl Default for SendInputSink {
    fn default() -> Self {
        Self::new()
    }
}

fn key_input(k: WinKey, down: bool) -> INPUT {
    let up = if down {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    let ki = match k {
        WinKey::Scan(sc) => KEYBDINPUT {
            wVk: VIRTUAL_KEY(0),
            wScan: sc & 0xFF,
            dwFlags: KEYEVENTF_SCANCODE
                | up
                | if sc & 0xFF00 == 0xE000 {
                    KEYEVENTF_EXTENDEDKEY
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
            time: 0,
            dwExtraInfo: 0,
        },
        WinKey::Vk(vk, ext) => KEYBDINPUT {
            wVk: VIRTUAL_KEY(vk),
            wScan: 0,
            dwFlags: up
                | if ext {
                    KEYEVENTF_EXTENDEDKEY
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
            time: 0,
            dwExtraInfo: 0,
        },
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                // Signed wheel delta, carried in a DWORD.
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

impl InputSink for SendInputSink {
    fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
        // Validation already happened in the protocol layer; this is defence in depth.
        if !is_allowed_keycode(code) {
            return Ok(());
        }
        match lookup(code) {
            Some(WinKey::Scan(sc)) if POWER_SCANCODES.contains(&sc) => Ok(()),
            Some(k) => self.send(&[key_input(k, down)]),
            None => {
                if self.unmapped.insert(code) {
                    tracing::debug!("input: keycode {code} has no Windows equivalent; ignored");
                }
                Ok(())
            }
        }
    }

    fn button(&mut self, b: MouseButton, down: bool) -> io::Result<()> {
        let f = match (b, down) {
            (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
            (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
            (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        };
        self.send(&[mouse_input(0, 0, 0, f)])
    }

    fn motion(&mut self, dx: i32, dy: i32) -> io::Result<()> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        self.send(&[mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)])
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> io::Result<()> {
        // Windows takes high-resolution deltas directly (120 per notch). Positive WHEEL scrolls
        // *up*, our protocol uses positive dy = down; positive HWHEEL scrolls right, as ours.
        let mut evs = Vec::with_capacity(2);
        if dy != 0 {
            evs.push(mouse_input(0, 0, -dy, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            evs.push(mouse_input(0, 0, dx, MOUSEEVENTF_HWHEEL));
        }
        if evs.is_empty() {
            return Ok(());
        }
        self.send(&evs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dishus_proto::keys::{BLOCKED_KEYCODES, MAX_KEYCODE};

    #[test]
    fn keymap_is_sorted_unique_and_allowed() {
        for w in KEYMAP.windows(2) {
            assert!(w[0].0 < w[1].0, "KEYMAP not sorted at {}", w[1].0);
        }
        let mut seen = BTreeSet::new();
        for (code, k) in KEYMAP {
            assert!(
                is_allowed_keycode(*code),
                "{code} is not an allowed keycode"
            );
            assert!(seen.insert(*k), "two keycodes map to {k:?}");
            if let WinKey::Scan(sc) = k {
                assert!(sc & 0xFF != 0, "{code}: empty scan code");
                assert!(matches!(sc >> 8, 0 | 0xE0), "{code}: bad prefix {sc:#x}");
                assert!(!POWER_SCANCODES.contains(sc), "{code} maps to a power key");
            }
        }
    }

    #[test]
    fn blocked_keys_are_not_mapped() {
        for c in BLOCKED_KEYCODES {
            assert_eq!(lookup(*c), None, "blocked keycode {c} is mapped");
        }
        assert_eq!(lookup(0), None);
        assert_eq!(lookup(MAX_KEYCODE + 1), None);
    }

    #[test]
    fn main_block_is_identity() {
        // Letters, digits, punctuation, modifiers, F1..F10 and the keypad: evdev == set 1.
        for code in 1..=83u16 {
            if code == 69 {
                continue; // NumLock
            }
            assert_eq!(lookup(code), Some(WinKey::Scan(code)), "keycode {code}");
        }
        assert_eq!(lookup(87), Some(WinKey::Scan(0x57))); // F11
        assert_eq!(lookup(88), Some(WinKey::Scan(0x58))); // F12
    }

    #[test]
    fn extended_keys_are_extended() {
        // arrows, right Ctrl/Alt, Insert/Delete/Home/End/PageUp/PageDown, keypad Enter and /,
        // both Windows keys, Menu, Print Screen
        let ext = [
            96, 97, 98, 99, 100, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 125, 126, 127,
        ];
        for code in ext {
            match lookup(code) {
                Some(WinKey::Scan(sc)) => assert_eq!(sc >> 8, 0xE0, "keycode {code}"),
                other => panic!("keycode {code}: {other:?}"),
            }
        }
        assert_eq!(lookup(103), Some(WinKey::Scan(0xE048))); // UP
        assert_eq!(lookup(125), Some(WinKey::Scan(0xE05B))); // LEFTMETA
        // Left-hand modifiers are not extended.
        for code in [29, 42, 54, 56] {
            assert!(matches!(lookup(code), Some(WinKey::Scan(sc)) if sc >> 8 == 0));
        }
    }

    #[test]
    fn key_flags() {
        let i = key_input(WinKey::Scan(0xE048), false);
        // SAFETY: we built a keyboard input.
        let ki = unsafe { i.Anonymous.ki };
        assert_eq!(ki.wScan, 0x48);
        assert_eq!(
            ki.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY
        );
        let i = key_input(WinKey::Scan(0x1E), true);
        // SAFETY: as above.
        let ki = unsafe { i.Anonymous.ki };
        assert_eq!((ki.wScan, ki.dwFlags), (0x1E, KEYEVENTF_SCANCODE));
        let i = key_input(WinKey::Vk(VK_PAUSE.0, false), true);
        // SAFETY: as above.
        let ki = unsafe { i.Anonymous.ki };
        assert_eq!((ki.wVk, ki.wScan), (VK_PAUSE, 0));
    }
}
