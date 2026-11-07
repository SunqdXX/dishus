//! Virtual keyboard + mouse through `/dev/uinput`. Works under every compositor (and X11, and
//! the TTY) because events enter the kernel input stack like a real USB device would.
//!
//! Two separate devices are created so libinput classifies them correctly (a combined device
//! with keys and relative axes is sometimes treated as a mouse only).

use evdev::{
    AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode, RelativeAxisCode,
    uinput::VirtualDevice,
};

use dishus_proto::{MouseButton, keys::is_allowed_keycode};

use crate::backend::InputSink;

pub struct UinputSink {
    kbd: VirtualDevice,
    mouse: VirtualDevice,
    /// Remainders for converting hi-res scroll to classic wheel clicks.
    wheel_acc: (i32, i32),
}

impl UinputSink {
    pub fn new() -> std::io::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=dishus_proto::keys::MAX_KEYCODE {
            if is_allowed_keycode(code) {
                keys.insert(KeyCode::new(code));
            }
        }
        let kbd = VirtualDevice::builder()?
            .name("DiShus Virtual Keyboard")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x1d5b, 0x0001, 1))
            .with_keys(&keys)?
            .build()?;

        let mut buttons = AttributeSet::<KeyCode>::new();
        buttons.insert(KeyCode::BTN_LEFT);
        buttons.insert(KeyCode::BTN_RIGHT);
        buttons.insert(KeyCode::BTN_MIDDLE);
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        axes.insert(RelativeAxisCode::REL_X);
        axes.insert(RelativeAxisCode::REL_Y);
        axes.insert(RelativeAxisCode::REL_WHEEL);
        axes.insert(RelativeAxisCode::REL_HWHEEL);
        axes.insert(RelativeAxisCode::REL_WHEEL_HI_RES);
        axes.insert(RelativeAxisCode::REL_HWHEEL_HI_RES);
        let mouse = VirtualDevice::builder()?
            .name("DiShus Virtual Pointer")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x1d5b, 0x0002, 1))
            .with_keys(&buttons)?
            .with_relative_axes(&axes)?
            .build()?;
        Ok(Self {
            kbd,
            mouse,
            wheel_acc: (0, 0),
        })
    }
}

fn ev(t: EventType, code: u16, value: i32) -> InputEvent {
    InputEvent::new(t.0, code, value)
}

impl InputSink for UinputSink {
    fn key(&mut self, code: u16, down: bool) -> std::io::Result<()> {
        // Validation already happened in the protocol layer; this is defence in depth.
        if !is_allowed_keycode(code) {
            return Ok(());
        }
        self.kbd.emit(&[ev(EventType::KEY, code, i32::from(down))])
    }

    fn button(&mut self, b: MouseButton, down: bool) -> std::io::Result<()> {
        let code = match b {
            MouseButton::Left => KeyCode::BTN_LEFT,
            MouseButton::Right => KeyCode::BTN_RIGHT,
            MouseButton::Middle => KeyCode::BTN_MIDDLE,
        };
        self.mouse
            .emit(&[ev(EventType::KEY, code.0, i32::from(down))])
    }

    fn motion(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        let mut evs = Vec::with_capacity(2);
        if dx != 0 {
            evs.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_X.0, dx));
        }
        if dy != 0 {
            evs.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_Y.0, dy));
        }
        if evs.is_empty() {
            Ok(())
        } else {
            self.mouse.emit(&evs)
        }
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        // evdev: positive REL_WHEEL scrolls *up*; our protocol uses positive dy = down.
        let (vy, vx) = (-dy, dx);
        let mut evs = Vec::with_capacity(4);
        if vy != 0 {
            evs.push(ev(
                EventType::RELATIVE,
                RelativeAxisCode::REL_WHEEL_HI_RES.0,
                vy,
            ));
            self.wheel_acc.1 += vy;
            let clicks = self.wheel_acc.1 / 120;
            if clicks != 0 {
                self.wheel_acc.1 -= clicks * 120;
                evs.push(ev(
                    EventType::RELATIVE,
                    RelativeAxisCode::REL_WHEEL.0,
                    clicks,
                ));
            }
        }
        if vx != 0 {
            evs.push(ev(
                EventType::RELATIVE,
                RelativeAxisCode::REL_HWHEEL_HI_RES.0,
                vx,
            ));
            self.wheel_acc.0 += vx;
            let clicks = self.wheel_acc.0 / 120;
            if clicks != 0 {
                self.wheel_acc.0 -= clicks * 120;
                evs.push(ev(
                    EventType::RELATIVE,
                    RelativeAxisCode::REL_HWHEEL.0,
                    clicks,
                ));
            }
        }
        if evs.is_empty() {
            Ok(())
        } else {
            self.mouse.emit(&evs)
        }
    }
}
