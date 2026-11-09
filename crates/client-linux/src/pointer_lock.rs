//! Wayland pointer constraints on GTK's own wl_surface. All use is on the GTK thread.
use anyhow::{Context, Result, ensure};
use glib::translate::ToGlibPtr;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use std::cell::Cell;
use std::rc::Rc;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1 as locked, zwp_pointer_constraints_v1 as constraints,
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1 as manager, zwp_relative_pointer_v1 as relative,
};

unsafe extern "C" {
    fn gdk_wayland_display_get_wl_display(display: *mut gdk::ffi::GdkDisplay) -> *mut libc::c_void;
    fn gdk_wayland_surface_get_wl_surface(surface: *mut gdk::ffi::GdkSurface) -> *mut libc::c_void;
}

struct State {
    active: Rc<Cell<bool>>,
    on_motion: Box<dyn Fn(i32, i32)>,
    remainder: (f64, f64),
}

pub struct PointerLock {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    surface: wl_surface::WlSurface,
    pointer: wl_pointer::WlPointer,
    seat: wl_seat::WlSeat,
    constraints: constraints::ZwpPointerConstraintsV1,
    manager: manager::ZwpRelativePointerManagerV1,
    locked: Option<locked::ZwpLockedPointerV1>,
    relative: Option<relative::ZwpRelativePointerV1>,
    // Fields drop in declaration order: release GTK last, after every guest proxy/backend.
    _surface: gdk::Surface,
}

impl PointerLock {
    pub fn new(surface: &gdk::Surface, motion: impl Fn(i32, i32) + 'static) -> Result<Self> {
        ensure!(
            surface.display().type_().name() == "GdkWaylandDisplay",
            "pointer lock requires Wayland"
        );
        let conn = unsafe {
            let display = gdk_wayland_display_get_wl_display(surface.display().to_glib_none().0);
            ensure!(!display.is_null(), "Wayland display missing");
            Connection::from_backend(wayland_backend::client::Backend::from_foreign_display(
                display.cast(),
            ))
        };
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let constraints = globals
            .bind(&qh, 1..=1, ())
            .context("compositor has no pointer-constraints protocol")?;
        let manager = globals
            .bind(&qh, 1..=1, ())
            .context("compositor has no relative-pointer protocol")?;
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 5..=9, ())
            .context("Wayland seat unavailable")?;
        let pointer = seat.get_pointer(&qh, ());
        let id = unsafe {
            let raw = gdk_wayland_surface_get_wl_surface(surface.to_glib_none().0);
            ensure!(!raw.is_null(), "Wayland surface unavailable");
            wayland_backend::client::ObjectId::from_ptr(
                wl_surface::WlSurface::interface(),
                raw.cast(),
            )?
        };
        let wl_surface = wl_surface::WlSurface::from_id(&conn, id)?;
        let mut state = State {
            active: Rc::new(Cell::new(false)),
            on_motion: Box::new(motion),
            remainder: (0.0, 0.0),
        };
        queue.roundtrip(&mut state)?;
        Ok(Self {
            _surface: surface.clone(),
            conn,
            queue,
            state,
            surface: wl_surface,
            pointer,
            seat,
            constraints,
            manager,
            locked: None,
            relative: None,
        })
    }

    pub fn active(&self) -> bool {
        self.state.active.get()
    }
    pub fn requested(&self) -> bool {
        self.locked.is_some()
    }

    pub fn set_enabled(&mut self, enabled: bool) -> Result<()> {
        if enabled == self.requested() {
            return Ok(());
        }
        if enabled {
            let qh = self.queue.handle();
            self.relative = Some(self.manager.get_relative_pointer(&self.pointer, &qh, ()));
            self.locked = Some(self.constraints.lock_pointer(
                &self.surface,
                &self.pointer,
                None,
                constraints::Lifetime::Persistent,
                &qh,
                (),
            ));
        } else {
            if let Some(lock) = self.locked.take() {
                lock.destroy();
            }
            if let Some(relative) = self.relative.take() {
                relative.destroy();
            }
            self.state.active.set(false);
            self.state.remainder = (0.0, 0.0);
        }
        self.conn.flush()?;
        Ok(())
    }

    /// GDK reads the shared Wayland connection; dispatch only our guest queue.
    pub fn dispatch(&mut self) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }
}

impl Drop for PointerLock {
    fn drop(&mut self) {
        let _ = self.set_enabled(false);
        self.pointer.release();
        self.seat.release();
        self.constraints.destroy();
        self.manager.destroy();
        let _ = self.conn.flush();
        // Never destroy GTK's borrowed wl_surface or disconnect its display.
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
macro_rules! ignore {
    ($ty:ty) => {
        impl Dispatch<$ty, ()> for State {
            fn event(
                _: &mut Self,
                _: &$ty,
                _: <$ty as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}
ignore!(wl_seat::WlSeat);
ignore!(wl_pointer::WlPointer);
ignore!(constraints::ZwpPointerConstraintsV1);
ignore!(manager::ZwpRelativePointerManagerV1);
impl Dispatch<locked::ZwpLockedPointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &locked::ZwpLockedPointerV1,
        event: locked::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            locked::Event::Locked => {
                state.active.set(true);
                tracing::info!("pointer lock active");
            }
            locked::Event::Unlocked => {
                state.active.set(false);
                state.remainder = (0.0, 0.0);
                tracing::info!("pointer lock released");
            }
            _ => {}
        }
    }
}
impl Dispatch<relative::ZwpRelativePointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &relative::ZwpRelativePointerV1,
        event: relative::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let relative::Event::RelativeMotion {
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
        {
            if !state.active.get() {
                return;
            }
            let (x, y) = quantize(&mut state.remainder, dx_unaccel, dy_unaccel);
            if x != 0 || y != 0 {
                (state.on_motion)(x, y);
            }
        }
    }
}
fn quantize(remainder: &mut (f64, f64), dx: f64, dy: f64) -> (i32, i32) {
    remainder.0 += dx;
    remainder.1 += dy;
    let out = (remainder.0.round() as i32, remainder.1.round() as i32);
    remainder.0 -= out.0 as f64;
    remainder.1 -= out.1 as f64;
    out
}
#[cfg(test)]
mod tests {
    #[test]
    fn fractional_relative_motion_is_conserved() {
        let mut remainder = (0.0, 0.0);
        let mut total = (0, 0);
        for _ in 0..100 {
            let (x, y) = super::quantize(&mut remainder, 0.3, -0.2);
            total.0 += x;
            total.1 += y;
        }
        assert_eq!(total, (30, -20));
    }
}
