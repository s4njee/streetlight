//! Fullscreen stream view: zero-copy video under GtkGraphicsOffload (lets the
//! compositor put the dmabuf on a hardware plane), an optional webview overlay,
//! and keyboard / mouse / scroll capture.

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gtk4::prelude::*;
use gtk4::{gdk, glib};
use streetlight_client_core::{Client, InputEvent, MouseButton, ColorSpace};
use anyhow::Context;

use crate::decode::DmabufFrame;

pub struct FrameMailbox {
    slot: Mutex<Option<(DmabufFrame, u64, u64)>>,
}

impl FrameMailbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { slot: Mutex::new(None) })
    }

    /// Called from the media thread. Replaces any not-yet-shown frame (never queues).
    /// Returns true if the UI needs to be woken.
    pub fn put(&self, f: DmabufFrame, decoded_us: u64, capture_local_us: u64) -> bool {
        self.slot.lock().unwrap().replace((f, decoded_us, capture_local_us)).is_none()
    }

    fn take(&self) -> Option<(DmabufFrame, u64, u64)> {
        self.slot.lock().unwrap().take()
    }
}

pub struct StreamView {
    pub root: gtk4::Overlay,
    picture: gtk4::Picture,
    cursor_layer: gtk4::Fixed,
    cursor_picture: gtk4::Picture,
    cursor_shape: Cell<Option<u32>>,
    local_pointer: Cell<Option<(f64,f64)>>,
    stream_size: Cell<(u32, u32)>,
    client: RefCell<Option<Arc<Client>>>,
    color: RefCell<gdk::ColorState>,
    pressed: RefCell<HashSet<u32>>,
    input_enabled: Cell<bool>,
    pointer_lock: RefCell<Option<crate::pointer_lock::PointerLock>>,
    queued_timing: RefCell<Option<(u64, u64)>>,
    painted_timings: RefCell<VecDeque<(i64, u64, u64)>>,
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
        let cursor_layer=gtk4::Fixed::new();cursor_layer.set_can_target(false);
        cursor_layer.set_overflow(gtk4::Overflow::Hidden);
        let cursor_picture=gtk4::Picture::new();cursor_picture.set_can_target(false);
        cursor_picture.set_content_fit(gtk4::ContentFit::Fill);
        cursor_layer.put(&cursor_picture,0.0,0.0);root.add_overlay(&cursor_layer);

        let v = Rc::new(Self {
            root,
            picture, cursor_layer,cursor_picture,cursor_shape:Cell::new(None),local_pointer:Cell::new(None),
            stream_size: Cell::new((1920, 1080)),
            client: RefCell::new(None),
            color: RefCell::new(gdk::ColorState::srgb()),
            pressed: RefCell::new(HashSet::new()),
            input_enabled: Cell::new(true),
            pointer_lock: RefCell::new(None),
            queued_timing: RefCell::new(None),
            painted_timings: RefCell::new(VecDeque::new()),
            on_escape: RefCell::new(None),
        });
        v.install_input();
        v.install_presentation_timing();
        let weak=Rc::downgrade(&v);
        glib::timeout_add_local(std::time::Duration::from_millis(4),move || {
            let Some(v)=weak.upgrade() else { return glib::ControlFlow::Break };
            v.update_cursor();
            if let Some(lock)=v.pointer_lock.borrow_mut().as_mut()
                && let Err(e)=lock.dispatch() { tracing::warn!("pointer lock: {e:#}"); let _=lock.set_enabled(false); }
            glib::ControlFlow::Continue
        });
        v
    }

    pub fn set_stream_color(&self, color: ColorSpace) -> anyhow::Result<()> {
        if color == ColorSpace::Sdr601Full {
            // Unmanaged legacy NV12 uses this matrix/range at the compositor. Its
            // resulting RGB values use sRGB; GTK 4.18 cannot express the YUV CICP.
            *self.color.borrow_mut() = gdk::ColorState::srgb();
            tracing::info!("presentation: legacy BT.601/full NV12 compatibility");
            return Ok(());
        }
        let cicp = gdk::CicpParams::new();
        let hdr = color == ColorSpace::Hdr10;
        cicp.set_color_primaries(if hdr { 9 } else { 1 });
        cicp.set_transfer_function(if hdr { 16 } else { 1 });
        cicp.set_matrix_coefficients(if hdr { 9 } else { 1 });
        cicp.set_range(gdk::CicpRange::Narrow);
        *self.color.borrow_mut() = cicp.build_color_state().context("GTK cannot represent the stream's YUV color state; use the legacy compatibility path on GTK <4.20")?;
        Ok(())
    }

    pub fn attach(&self, client: Option<Arc<Client>>, size: (u32, u32)) {
        self.stream_size.set(size);
        if client.is_none() {
            self.pointer_lock.borrow_mut().take();
            self.cursor_picture.set_visible(false);self.cursor_shape.set(None);self.local_pointer.set(None);self.root.set_cursor_from_name(None);
            self.queued_timing.borrow_mut().take();
            self.painted_timings.borrow_mut().clear();
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
        let Some((f, decoded_us, capture_local_us)) = mailbox.take() else { return };
        if self.client.borrow().is_none() { return; }
        let display = self.picture.display();
        let mut b = gdk::DmabufTextureBuilder::new()
            .set_display(&display)
            .set_width(f.width)
            .set_height(f.height)
            .set_fourcc(f.fourcc)
            .set_modifier(f.modifier)
            .set_premultiplied(true)
            .set_color_state(Some(&self.color.borrow()))
            .set_n_planes(f.planes.len() as u32);
        for (i, p) in f.planes.iter().enumerate() {
            b = unsafe { b.set_fd(i as u32, p.fd) }.set_offset(i as u32, p.offset).set_stride(i as u32, p.stride);
        }
        // GTK keeps the dmabuf until it's done scanning out / sampling, then we free the frame.
        match unsafe { b.build_with_release_func(move || drop(f)) } {
            Ok(tex) => {
                self.picture.set_paintable(Some(&tex));
                *self.queued_timing.borrow_mut() = Some((decoded_us, capture_local_us));
            }
            Err(e) => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| tracing::error!("dmabuf texture import failed: {e}"));
            }
        }
    }

    fn install_presentation_timing(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.picture.connect_realize(move |picture| {
            let (Some(view), Some(clock)) = (weak.upgrade(), picture.frame_clock()) else { return };
            let weak = Rc::downgrade(&view);
            clock.connect_after_paint(move |clock| {
                let Some(view) = weak.upgrade() else { return };
                if let Some((decoded, captured)) = view.queued_timing.borrow_mut().take() {
                    let mut pending = view.painted_timings.borrow_mut();
                    if pending.len() == 120 { pending.pop_front(); }
                    pending.push_back((clock.frame_counter(), decoded, captured));
                }
            });
        });
        let weak = Rc::downgrade(self);
        self.picture.add_tick_callback(move |_, clock| {
            let Some(view) = weak.upgrade() else { return glib::ControlFlow::Break };
            let mut pending = view.painted_timings.borrow_mut();
            while let Some(&(counter, decoded, captured)) = pending.front() {
                let Some(timing) = clock.timings(counter) else { pending.pop_front(); continue };
                if !timing.is_complete() { break; }
                pending.pop_front();
                let presented = timing.presentation_time();
                // Zero means the compositor did not provide actual presentation feedback.
                // Never substitute the predicted time or texture creation time.
                if presented > 0 {
                    let age = glib::monotonic_time().saturating_sub(presented).max(0) as u64;
                    let local = streetlight_client_core::now_us().saturating_sub(age);
                    if let Some(client) = view.client.borrow().as_ref() { client.report_presentation(decoded, captured, local); }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    fn update_cursor(&self) {
        let Some(client)=self.client.borrow().as_ref().cloned() else {return};
        let cursor=client.cursor();let p=cursor.position;
        let Some(bitmap)=cursor.bitmap else {self.cursor_picture.set_visible(false);return};
        if !p.visible || p.shape_id!=bitmap.id || p.source_width==0 || p.source_height==0 {
            self.cursor_picture.set_visible(false);return;
        }
        if self.cursor_shape.get()!=Some(bitmap.id) {
            let bytes=glib::Bytes::from_owned(bitmap.rgba.clone());
            let texture=gdk::MemoryTexture::new(bitmap.width as i32,bitmap.height as i32,
                gdk::MemoryFormat::R8g8b8a8Premultiplied,&bytes,bitmap.width as usize*4);
            self.cursor_picture.set_paintable(Some(&texture));self.cursor_shape.set(Some(bitmap.id));
            tracing::info!("cursor overlay: shape {} {}x{} on {}x{} source",bitmap.id,bitmap.width,bitmap.height,p.source_width,p.source_height);
        }
        let (ww,wh)=(self.picture.width() as f64,self.picture.height() as f64);
        let scale=(ww/p.source_width as f64).min(wh/p.source_height as f64);
        let ox=(ww-p.source_width as f64*scale)/2.0;let oy=(wh-p.source_height as f64*scale)/2.0;
        self.cursor_picture.set_size_request((bitmap.width as f64*scale).round() as i32,(bitmap.height as f64*scale).round() as i32);
        let (x,y)=self.local_pointer.get().map(|(x,y)|(x-bitmap.hotspot_x as f64*scale,y-bitmap.hotspot_y as f64*scale))
            .unwrap_or((ox+(p.x-bitmap.hotspot_x) as f64*scale,oy+(p.y-bitmap.hotspot_y) as f64*scale));
        self.cursor_layer.move_(&self.cursor_picture,x,y);
        if self.input_enabled.get() { self.root.set_cursor_from_name(Some("none")); }
        self.cursor_picture.set_visible(true);
    }

    pub fn toggle_pointer_lock(self: &Rc<Self>) {
        if !self.input_enabled.get() || !self.streaming() { return; }
        let mut pointer=self.pointer_lock.borrow_mut();
        if pointer.is_none() {
            let Some(surface)=self.root.native().and_then(|n| n.surface()) else { return };
            let weak=Rc::downgrade(self);
            match crate::pointer_lock::PointerLock::new(&surface,move |dx,dy| {
                if let Some(v)=weak.upgrade() { v.send(InputEvent::MouseMove { dx,dy }); }
            }) {
                Ok(lock)=>*pointer=Some(lock),
                Err(e)=>{tracing::warn!("relative pointer unavailable; using absolute input: {e:#}");return;}
            }
        }
        let lock=pointer.as_mut().unwrap();
        let enable=!lock.requested();
        if let Err(e)=lock.set_enabled(enable) { tracing::warn!("pointer lock: {e:#}"); }
        self.root.set_cursor_from_name(if enable { Some("none") } else { None });
    }

    pub fn set_input_enabled(&self, on: bool) {
        self.input_enabled.set(on);
        if !on { self.pointer_lock.borrow_mut().take(); self.root.set_cursor_from_name(None); }
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
            if state.contains(combo) && keyval.to_lower() == gdk::Key::m {
                v.release_all_keys(); v.toggle_pointer_lock(); return glib::Propagation::Stop;
            }
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
                v.pointer_lock.borrow_mut().take();
                v.root.set_cursor_from_name(None);
            }
        });
        self.root.add_controller(focus);

        let motion = gtk4::EventControllerMotion::new();
        let v = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            let Some(v) = v.upgrade() else { return };
            if v.pointer_lock.borrow().as_ref().is_some_and(|p| p.active()) { v.local_pointer.set(None);return; }
            if v.input_enabled.get() && v.streaming() {v.local_pointer.set(Some((x,y)));}
            if !v.pointer_lock.borrow().as_ref().is_some_and(|p| p.active())
                && let Some((x, y)) = v.map_abs(x, y) {
                v.send(InputEvent::MouseAbs { x, y });
            }
        });
        let v=Rc::downgrade(self);
        motion.connect_leave(move |_|{if let Some(v)=v.upgrade(){v.local_pointer.set(None);}});
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
            if !v.pointer_lock.borrow().as_ref().is_some_and(|p| p.active())
                && let Some((x, y)) = v.map_abs(x, y) {
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
