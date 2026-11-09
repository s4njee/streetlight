//! Fullscreen stream view: zero-copy video under GtkGraphicsOffload (lets the
//! compositor put the dmabuf on a hardware plane), an optional webview overlay,
//! and keyboard / mouse / scroll capture.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gtk4::prelude::*;
use gtk4::{gdk, glib};
use streetlight_client_core::{Client, InputEvent, MouseButton};

use crate::decode::DmabufFrame;

pub struct FrameMailbox {
    slot: Mutex<Option<(DmabufFrame, u64, u32)>>,
}

impl FrameMailbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { slot: Mutex::new(None) })
    }

    /// Called from the media thread. Replaces any not-yet-shown frame (never queues).
    /// Returns true if the UI needs to be woken.
    pub fn put(&self, f: DmabufFrame, received_us: u64, decode_us: u32) -> bool {
        self.slot.lock().unwrap().replace((f, received_us, decode_us)).is_none()
    }

    fn take(&self) -> Option<(DmabufFrame, u64, u32)> {
        self.slot.lock().unwrap().take()
    }
}

pub struct StreamView {
    pub root: gtk4::Overlay,
    picture: gtk4::Picture,
    stream_size: Cell<(u32, u32)>,
    client: RefCell<Option<Arc<Client>>>,
    color: gdk::ColorState,
    pressed: RefCell<HashSet<u32>>,
    input_enabled: Cell<bool>,
    pub on_escape: RefCell<Option<Box<dyn Fn()>>>,
}

impl StreamView {
    pub fn new() -> Rc<Self> {
        let picture = gtk4::Picture::new();
        picture.set_content_fit(gtk4::ContentFit::Contain);
        picture.set_can_shrink(true);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        let offload = gtk4::GraphicsOffload::new(Some(&picture));
        offload.set_black_background(true);
        let root = gtk4::Overlay::new();
        root.set_child(Some(&offload));
        root.set_focusable(true);

        // BT.709, limited range — what the host encoder signals.
        let cicp = gdk::CicpParams::new();
        cicp.set_color_primaries(1);
        cicp.set_transfer_function(1);
        cicp.set_matrix_coefficients(1);
        cicp.set_range(gdk::CicpRange::Narrow);
        let color = cicp.build_color_state().unwrap_or_else(|_| gdk::ColorState::srgb());

        let v = Rc::new(Self {
            root,
            picture,
            stream_size: Cell::new((1920, 1080)),
            client: RefCell::new(None),
            color,
            pressed: RefCell::new(HashSet::new()),
            input_enabled: Cell::new(true),
            on_escape: RefCell::new(None),
        });
        v.install_input();
        v
    }

    pub fn attach(&self, client: Option<Arc<Client>>, size: (u32, u32)) {
        self.stream_size.set(size);
        if client.is_none() {
            self.picture.set_paintable(None::<&gdk::Paintable>);
            self.release_all_keys();
        }
        *self.client.borrow_mut() = client;
        self.root.grab_focus();
    }

    pub fn streaming(&self) -> bool {
        self.client.borrow().is_some()
    }

    /// Main thread: show the newest decoded frame.
    pub fn present(&self, mailbox: &FrameMailbox) {
        let Some((f, received_us, decode_us)) = mailbox.take() else { return };
        let display = self.picture.display();
        let mut b = gdk::DmabufTextureBuilder::new()
            .set_display(&display)
            .set_width(f.width)
            .set_height(f.height)
            .set_fourcc(f.fourcc)
            .set_modifier(f.modifier)
            .set_premultiplied(true)
            .set_color_state(Some(&self.color))
            .set_n_planes(f.planes.len() as u32);
        for (i, p) in f.planes.iter().enumerate() {
            b = unsafe { b.set_fd(i as u32, p.fd) }.set_offset(i as u32, p.offset).set_stride(i as u32, p.stride);
        }
        // GTK keeps the dmabuf until it's done scanning out / sampling, then we free the frame.
        match unsafe { b.build_with_release_func(move || drop(f)) } {
            Ok(tex) => {
                self.picture.set_paintable(Some(&tex));
                if let Some(c) = self.client.borrow().as_ref() {
                    let present_us = streetlight_client_core::now_us().saturating_sub(received_us) as u32;
                    c.report_timing(decode_us, present_us);
                }
            }
            Err(e) => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| tracing::error!("dmabuf texture import failed: {e}"));
            }
        }
    }

    pub fn set_input_enabled(&self, on: bool) {
        self.input_enabled.set(on);
    }

    fn send(&self, ev: InputEvent) {
        if !self.input_enabled.get() {
            return;
        }
        if let Some(c) = self.client.borrow().as_ref() {
            c.send_input(ev);
        }
    }

    fn release_all_keys(&self) {
        let keys: Vec<u32> = self.pressed.borrow_mut().drain().collect();
        for k in keys {
            self.send(InputEvent::Key { code: k as u16, down: false });
        }
    }

    /// Widget coords → 0..65535 over the letterboxed video rectangle.
    fn map_abs(&self, x: f64, y: f64) -> Option<(u16, u16)> {
        let (sw, sh) = self.stream_size.get();
        let (ww, wh) = (self.picture.width() as f64, self.picture.height() as f64);
        if ww <= 0.0 || wh <= 0.0 {
            return None;
        }
        let scale = (ww / sw as f64).min(wh / sh as f64);
        let (vw, vh) = (sw as f64 * scale, sh as f64 * scale);
        let (ox, oy) = ((ww - vw) / 2.0, (wh - vh) / 2.0);
        let nx = ((x - ox) / vw).clamp(0.0, 1.0);
        let ny = ((y - oy) / vh).clamp(0.0, 1.0);
        Some(((nx * 65535.0) as u16, (ny * 65535.0) as u16))
    }

    fn install_input(self: &Rc<Self>) {
        // Keyboard: GDK hardware keycodes on Linux are evdev codes + 8.
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let v = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, keyval, keycode, state| {
            let Some(v) = v.upgrade() else { return glib::Propagation::Proceed };
            if !v.streaming() {
                return glib::Propagation::Proceed;
            }
            // Ctrl+Alt+Shift+Q leaves the stream.
            let combo = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SHIFT_MASK;
            if state.contains(combo) && keyval.to_lower() == gdk::Key::q {
                v.release_all_keys();
                if let Some(cb) = v.on_escape.borrow().as_ref() {
                    cb();
                }
                return glib::Propagation::Stop;
            }
            let code = keycode.saturating_sub(8);
            if code > 0 && v.pressed.borrow_mut().insert(code) {
                v.send(InputEvent::Key { code: code as u16, down: true });
            }
            glib::Propagation::Stop
        });
        let v = Rc::downgrade(self);
        keys.connect_key_released(move |_, _, keycode, _| {
            let Some(v) = v.upgrade() else { return };
            let code = keycode.saturating_sub(8);
            if v.pressed.borrow_mut().remove(&code) {
                v.send(InputEvent::Key { code: code as u16, down: false });
            }
        });
        self.root.add_controller(keys);
        let focus = gtk4::EventControllerFocus::new();
        let v = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            if let Some(v) = v.upgrade() {
                v.release_all_keys();
            }
        });
        self.root.add_controller(focus);

        let motion = gtk4::EventControllerMotion::new();
        let v = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            let Some(v) = v.upgrade() else { return };
            if let Some((x, y)) = v.map_abs(x, y) {
                v.send(InputEvent::MouseAbs { x, y });
            }
        });
        self.picture.add_controller(motion);

        let click = gtk4::GestureClick::new();
        click.set_button(0);
        let v = Rc::downgrade(self);
        let map_btn = |b: u32| match b {
            1 => Some(MouseButton::Left),
            2 => Some(MouseButton::Middle),
            3 => Some(MouseButton::Right),
            8 => Some(MouseButton::Back),
            9 => Some(MouseButton::Forward),
            _ => None,
        };
        click.connect_pressed(move |g, _, x, y| {
            let Some(v) = v.upgrade() else { return };
            v.root.grab_focus();
            if let Some((x, y)) = v.map_abs(x, y) {
                v.send(InputEvent::MouseAbs { x, y });
            }
            if let Some(b) = map_btn(g.current_button()) {
                v.send(InputEvent::MouseButton { button: b, down: true });
            }
        });
        let v = Rc::downgrade(self);
        click.connect_released(move |g, _, _, _| {
            let Some(v) = v.upgrade() else { return };
            if let Some(b) = map_btn(g.current_button()) {
                v.send(InputEvent::MouseButton { button: b, down: false });
            }
        });
        self.picture.add_controller(click);

        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        let v = Rc::downgrade(self);
        scroll.connect_scroll(move |_, dx, dy| {
            if let Some(v) = v.upgrade() {
                v.send(InputEvent::Scroll { dx: (dx * 120.0) as i32, dy: (-dy * 120.0) as i32 });
            }
            glib::Propagation::Stop
        });
        self.picture.add_controller(scroll);
    }
}
