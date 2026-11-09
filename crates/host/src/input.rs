//! Input injection through uinput virtual devices (works under any compositor and
//! exposes real gamepads to games / Steam Input).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use evdev::uinput::VirtualDevice;
use evdev::{AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent as Ev, InputId, KeyCode, RelativeAxisCode, UinputAbsSetup};
use streetlight_proto::control::{GamepadState, InputEvent, MouseButton, TouchPhase, pad_buttons as pb};

use crate::capture::layout::AbsMap;

/// Which desktop output the current stream shows (set by the session).
#[derive(Debug, Clone, Default)]
pub struct OutputTarget {
    /// KWin output name, e.g. "Virtual-streetlight-ipad".
    pub name: String,
    pub map: Option<AbsMap>,
    pub generation: u64,
}

pub type SharedTarget = Arc<Mutex<OutputTarget>>;

pub struct Injector {
    kbm: VirtualDevice,
    abs: VirtualDevice,
    touch: VirtualDevice,
    pads: HashMap<u8, (VirtualDevice, GamepadState)>,
    touch_slots: [bool; 10],
    target: SharedTarget,
    applied_generation: u64,
    map: Option<AbsMap>,
    last_abs: (u16, u16),
}

fn syn() -> Ev {
    Ev::new(EventType::SYNCHRONIZATION.0, 0, 0)
}
fn key(code: u16, v: i32) -> Ev {
    Ev::new(EventType::KEY.0, code, v)
}
fn rel(code: RelativeAxisCode, v: i32) -> Ev {
    Ev::new(EventType::RELATIVE.0, code.0, v)
}
fn abs(code: AbsoluteAxisCode, v: i32) -> Ev {
    Ev::new(EventType::ABSOLUTE.0, code.0, v)
}

const MOUSE_BUTTONS: [KeyCode; 5] = [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE, KeyCode::BTN_SIDE, KeyCode::BTN_EXTRA];

fn button_code(b: MouseButton) -> KeyCode {
    match b {
        MouseButton::Left => KeyCode::BTN_LEFT,
        MouseButton::Right => KeyCode::BTN_RIGHT,
        MouseButton::Middle => KeyCode::BTN_MIDDLE,
        MouseButton::Back => KeyCode::BTN_SIDE,
        MouseButton::Forward => KeyCode::BTN_EXTRA,
    }
}

fn abs_axis(code: AbsoluteAxisCode, min: i32, max: i32, flat: i32) -> UinputAbsSetup {
    UinputAbsSetup::new(code, AbsInfo::new(0, min, max, 0, flat, 0))
}

impl Injector {
    pub fn new(target: SharedTarget) -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for c in 1..=248u16 {
            keys.insert(KeyCode::new(c));
        }
        for b in MOUSE_BUTTONS {
            keys.insert(b);
        }
        let mut rels = AttributeSet::<RelativeAxisCode>::new();
        for r in [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ] {
            rels.insert(r);
        }
        let kbm = VirtualDevice::builder()?
            .name("Streetlight keyboard/mouse")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5354, 0x0001, 1))
            .with_keys(&keys)?
            .with_relative_axes(&rels)?
            .build()?;

        let mut abs_keys = AttributeSet::<KeyCode>::new();
        for b in MOUSE_BUTTONS {
            abs_keys.insert(b);
        }
        let abs_dev = VirtualDevice::builder()?
            .name("Streetlight absolute pointer")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5354, 0x0002, 1))
            .with_keys(&abs_keys)?
            .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_X, 0, 65535, 0))?
            .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_Y, 0, 65535, 0))?
            .build()?;

        let mut tkeys = AttributeSet::<KeyCode>::new();
        tkeys.insert(KeyCode::BTN_TOUCH);
        let mut props = AttributeSet::<evdev::PropType>::new();
        props.insert(evdev::PropType::DIRECT);
        let touch = VirtualDevice::builder()?
            .name("Streetlight touchscreen")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5354, 0x0003, 1))
            .with_properties(&props)?
            .with_keys(&tkeys)?
            .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_X, 0, 65535, 0))?
            .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_Y, 0, 65535, 0))?
            .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_MT_SLOT, 0, 9, 0))?
            .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 0, 65535, 0))?
            .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_MT_POSITION_X, 0, 65535, 0))?
            .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0, 65535, 0))?
            .build()?;

        let mut kbm = kbm;
        // Relative motion must reach games 1:1: turn off KWin pointer acceleration for our
        // virtual mouse (the client side already applied the user's own acceleration).
        if let Some(sys) = sysname(&mut kbm) {
            std::thread::spawn(move || {
                for _ in 0..20 {
                    if kwin_set(&sys, "pointerAccelerationProfileFlat", "b", "true") {
                        kwin_set(&sys, "pointerAcceleration", "d", "0");
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                tracing::warn!("couldn't disable pointer acceleration on {sys}");
            });
        }
        Ok(Self { kbm, abs: abs_dev, touch, pads: HashMap::new(), touch_slots: [false; 10], target, applied_generation: u64::MAX, map: None, last_abs: (u16::MAX, u16::MAX) })
    }

    /// Pick up a new stream target: rescale absolute pointer coordinates into the whole
    /// desktop (KWin maps absolute pointers across all outputs) and pin the touchscreen
    /// to the streamed output.
    fn sync_target(&mut self) {
        let t = self.target.lock().unwrap().clone();
        if t.generation == self.applied_generation {
            return;
        }
        self.applied_generation = t.generation;
        self.map = t.map;
        tracing::info!("input mapped to output '{}' ({:?})", t.name, t.map);
        if t.name.is_empty() {
            return;
        }
        if let Some(sys) = sysname(&mut self.touch)
            && !kwin_set(&sys, "outputName", "s", &t.name)
        {
            tracing::warn!("couldn't map touchscreen {sys} to {}", t.name);
        }
    }

    pub fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        if matches!(ev, InputEvent::MouseAbs { .. } | InputEvent::Touch { .. }) {
            self.sync_target();
        }
        match *ev {
            InputEvent::Key { code, down } => self.kbm.emit(&[key(code, down as i32), syn()])?,
            InputEvent::MouseMove { dx, dy } => self.kbm.emit(&[rel(RelativeAxisCode::REL_X, dx), rel(RelativeAxisCode::REL_Y, dy), syn()])?,
            InputEvent::MouseAbs { x, y } => {
                let (mut x, y) = self.map.map_or((x, y), |m| m.map(x, y));
                // evdev drops an ABS value equal to the previous one, so a repeat (e.g. after
                // relative motion moved the cursor away) would be ignored. Nudge by one unit.
                if (x, y) == self.last_abs {
                    x ^= 1;
                }
                self.last_abs = (x, y);
                self.abs.emit(&[abs(AbsoluteAxisCode::ABS_X, x as i32), abs(AbsoluteAxisCode::ABS_Y, y as i32), syn()])?
            }
            InputEvent::MouseButton { button, down } => self.kbm.emit(&[key(button_code(button).0, down as i32), syn()])?,
            InputEvent::Scroll { dx, dy } => {
                let mut evs = Vec::with_capacity(5);
                if dy != 0 {
                    evs.push(rel(RelativeAxisCode::REL_WHEEL_HI_RES, dy));
                    if dy.abs() >= 120 {
                        evs.push(rel(RelativeAxisCode::REL_WHEEL, dy / 120));
                    }
                }
                if dx != 0 {
                    evs.push(rel(RelativeAxisCode::REL_HWHEEL_HI_RES, dx));
                    if dx.abs() >= 120 {
                        evs.push(rel(RelativeAxisCode::REL_HWHEEL, dx / 120));
                    }
                }
                evs.push(syn());
                self.kbm.emit(&evs)?
            }
            InputEvent::Touch { id, phase, x, y } => self.touch(id, phase, x, y)?,
            InputEvent::Gamepad(state) => self.gamepad(state)?,
            InputEvent::GamepadDisconnected { id } => {
                self.pads.remove(&id);
            }
            InputEvent::Text(ref t) => tracing::debug!("text input not supported yet: {t:?}"),
        }
        Ok(())
    }

    fn touch(&mut self, id: u8, phase: TouchPhase, x: u16, y: u16) -> Result<()> {
        let slot = (id as usize).min(9);
        let mut evs = vec![abs(AbsoluteAxisCode::ABS_MT_SLOT, slot as i32)];
        match phase {
            TouchPhase::Down => {
                self.touch_slots[slot] = true;
                evs.push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, id as i32));
                evs.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x as i32));
                evs.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y as i32));
            }
            TouchPhase::Move => {
                evs.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x as i32));
                evs.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y as i32));
            }
            TouchPhase::Up => {
                self.touch_slots[slot] = false;
                evs.push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1));
            }
        }
        let any = self.touch_slots.iter().any(|s| *s);
        evs.push(key(KeyCode::BTN_TOUCH.0, any as i32));
        if phase != TouchPhase::Up {
            evs.push(abs(AbsoluteAxisCode::ABS_X, x as i32));
            evs.push(abs(AbsoluteAxisCode::ABS_Y, y as i32));
        }
        evs.push(syn());
        self.touch.emit(&evs)?;
        Ok(())
    }

    fn gamepad(&mut self, s: GamepadState) -> Result<()> {
        if !self.pads.contains_key(&s.id) {
            let dev = new_pad(s.id)?;
            tracing::info!("gamepad {} connected", s.id);
            self.pads.insert(s.id, (dev, GamepadState::default()));
        }
        let (dev, prev) = self.pads.get_mut(&s.id).unwrap();
        let mut evs = Vec::with_capacity(24);
        const MAP: [(u32, KeyCode); 11] = [
            (pb::A, KeyCode::BTN_SOUTH),
            (pb::B, KeyCode::BTN_EAST),
            (pb::X, KeyCode::BTN_NORTH),
            (pb::Y, KeyCode::BTN_WEST),
            (pb::LB, KeyCode::BTN_TL),
            (pb::RB, KeyCode::BTN_TR),
            (pb::BACK, KeyCode::BTN_SELECT),
            (pb::START, KeyCode::BTN_START),
            (pb::GUIDE, KeyCode::BTN_MODE),
            (pb::LS, KeyCode::BTN_THUMBL),
            (pb::RS, KeyCode::BTN_THUMBR),
        ];
        let changed = s.buttons ^ prev.buttons;
        for (bit, code) in MAP {
            if changed & bit != 0 {
                evs.push(key(code.0, (s.buttons & bit != 0) as i32));
            }
        }
        let hat = |neg: u32, pos: u32| -> i32 { (s.buttons & pos != 0) as i32 - (s.buttons & neg != 0) as i32 };
        let axes = [
            (AbsoluteAxisCode::ABS_X, s.lx as i32, prev.lx as i32),
            (AbsoluteAxisCode::ABS_Y, -(s.ly as i32).max(-32767), -(prev.ly as i32).max(-32767)),
            (AbsoluteAxisCode::ABS_RX, s.rx as i32, prev.rx as i32),
            (AbsoluteAxisCode::ABS_RY, -(s.ry as i32).max(-32767), -(prev.ry as i32).max(-32767)),
            (AbsoluteAxisCode::ABS_Z, s.lt as i32, prev.lt as i32),
            (AbsoluteAxisCode::ABS_RZ, s.rt as i32, prev.rt as i32),
            (AbsoluteAxisCode::ABS_HAT0X, hat(pb::LEFT, pb::RIGHT), i32::MIN),
            (AbsoluteAxisCode::ABS_HAT0Y, hat(pb::UP, pb::DOWN), i32::MIN),
        ];
        for (code, v, old) in axes {
            if v != old {
                evs.push(abs(code, v));
            }
        }
        evs.push(syn());
        dev.emit(&evs)?;
        *prev = s;
        Ok(())
    }
}

fn new_pad(id: u8) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for k in [
        KeyCode::BTN_SOUTH,
        KeyCode::BTN_EAST,
        KeyCode::BTN_NORTH,
        KeyCode::BTN_WEST,
        KeyCode::BTN_TL,
        KeyCode::BTN_TR,
        KeyCode::BTN_SELECT,
        KeyCode::BTN_START,
        KeyCode::BTN_MODE,
        KeyCode::BTN_THUMBL,
        KeyCode::BTN_THUMBR,
    ] {
        keys.insert(k);
    }
    let name = format!("Streetlight gamepad {id}");
    // Xbox 360 IDs so SDL / Steam / games pick a standard mapping.
    Ok(VirtualDevice::builder()?
        .name(&name)
        .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x110))
        .with_keys(&keys)?
        .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_X, -32768, 32767, 128))?
        .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_Y, -32768, 32767, 128))?
        .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_RX, -32768, 32767, 128))?
        .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_RY, -32768, 32767, 128))?
        .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_Z, 0, 255, 0))?
        .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_RZ, 0, 255, 0))?
        .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_HAT0X, -1, 1, 0))?
        .with_absolute_axis(&abs_axis(AbsoluteAxisCode::ABS_HAT0Y, -1, 1, 0))?
        .build()?)
}

/// "eventN" for a uinput device we created.
fn sysname(dev: &mut VirtualDevice) -> Option<String> {
    let p = dev.enumerate_dev_nodes_blocking().ok()?.find_map(|p| p.ok())?;
    p.file_name()?.to_str().map(String::from)
}

/// Set an org.kde.KWin.InputDevice property (KDE-only; harmless no-op elsewhere).
fn kwin_set(sys: &str, prop: &str, sig: &str, value: &str) -> bool {
    std::process::Command::new("busctl")
        .args(["--user", "set-property", "org.kde.KWin", &format!("/org/kde/KWin/InputDevice/{sys}"), "org.kde.KWin.InputDevice", prop, sig, value])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
