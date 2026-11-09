//! Dev tool: fullscreen window on a given output that logs every input event it gets.
//!   cargo run --example input_probe -- streetlight-<client>
#[path="../src/pointer_lock.rs"]
#[allow(dead_code)]
mod pointer_lock;
use gtk4::prelude::*;
use gtk4::{gdk, glib};

fn main() -> glib::ExitCode {
    let target = std::env::args().nth(1).unwrap_or_default();
    let app = gtk4::Application::builder().application_id("dev.streetlight.Probe").flags(gtk4::gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_activate(move |app| {
        let w = gtk4::ApplicationWindow::builder().application(app).title("Streetlight input probe").build();
        let label = gtk4::Label::new(Some("input probe"));
        w.set_child(Some(&label));
        let t0 = std::time::Instant::now();
        let ts = move || t0.elapsed().as_millis();
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, kv, code, _| {
            println!("{} KEY down evdev={} keyval={:?}", ts(), code.saturating_sub(8), kv.name());
            glib::Propagation::Stop
        });
        keys.connect_key_released(move |_, kv, code, _| println!("{} KEY up evdev={} keyval={:?}", ts(), code.saturating_sub(8), kv.name()));
        w.add_controller(keys);
        let m = gtk4::EventControllerMotion::new();
        m.connect_enter(move |_, x, y| println!("{} ENTER {x:.0} {y:.0}", ts()));
        m.connect_motion(move |_, x, y| println!("{} MOTION {x:.0} {y:.0}", ts()));
        w.add_controller(m);
        let c = gtk4::GestureClick::new();
        c.set_button(0);
        c.connect_pressed(move |g, _, x, y| println!("{} BUTTON {} down at {x:.0} {y:.0}", ts(), g.current_button()));
        c.connect_released(move |g, _, _, _| println!("{} BUTTON {} up", ts(), g.current_button()));
        w.add_controller(c);
        let s = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        s.connect_scroll(move |_, dx, dy| {
            println!("{} SCROLL {dx:.2} {dy:.2}", ts());
            glib::Propagation::Stop
        });
        w.add_controller(s);
        let drag = gtk4::GestureDrag::new();
        drag.set_touch_only(true);
        drag.connect_drag_begin(move |_, x, y| println!("{} TOUCH begin {x:.0} {y:.0}", ts()));
        drag.connect_drag_end(move |_, dx, dy| println!("{} TOUCH end Δ {dx:.0} {dy:.0}", ts()));
        w.add_controller(drag);
        let display = gdk::Display::default().unwrap();
        let mons = display.monitors();
        let mon = (0..mons.n_items())
            .filter_map(|i| mons.item(i)?.downcast::<gdk::Monitor>().ok())
            .inspect(|m| println!("0 MONITOR-SEEN {:?} {:?}", m.connector(), m.model()))
            .find(|m| m.connector().is_some_and(|c| c.contains(target.as_str())) || m.model().is_some_and(|c| c.contains(target.as_str())));
        match &mon {
            Some(m) => {
                let g = m.geometry();
                println!("0 MONITOR {} at {},{} {}x{}", target, g.x(), g.y(), g.width(), g.height());
                w.fullscreen_on_monitor(m);
            }
            None => println!("0 MONITOR {target} not found; plain window"),
        }
        w.present();
        let lock=std::rc::Rc::new(std::cell::RefCell::new(None::<pointer_lock::PointerLock>));
        let lock_tick=lock.clone();
        let mut was_locked=false;
        glib::timeout_add_local(std::time::Duration::from_millis(4),move || {
            if let Some(lock)=lock_tick.borrow_mut().as_mut() {
                if let Err(e)=lock.dispatch() {eprintln!("POINTER_LOCK error: {e:#}");}
                if lock.active()!=was_locked { was_locked=lock.active();println!("POINTER_LOCK active={was_locked}"); }
            }
            glib::ControlFlow::Continue
        });
        let w2 = w.clone();
        let mut gate_open = false;
        let gate = std::env::var_os("SL_SELFTEST_GATE");
        let expected = mon.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            let on_target = w2.surface().and_then(|s| s.display().monitor_at_surface(&s)) == expected;
            let ready = expected.is_some() && on_target && w2.is_active() && w2.is_fullscreen();
            if std::env::var_os("SL_PROBE_POINTER_LOCK").is_some() {
                if ready && lock.borrow().is_none() {
                    if let Some(surface)=w2.surface() {
                        match pointer_lock::PointerLock::new(&surface,|dx,dy|println!("RELATIVE {dx} {dy}")) {
                            Ok(mut pointer)=>{pointer.set_enabled(true).unwrap();*lock.borrow_mut()=Some(pointer);println!("POINTER_LOCK requested");}
                            Err(e)=>eprintln!("POINTER_LOCK unavailable: {e:#}"),
                        }
                    }
                } else if !ready { lock.borrow_mut().take(); }
            }
            if let Some(path) = &gate {
                if ready {
                    let _ = std::fs::write(path, format!("ready {}", std::process::id()));
                } else if gate_open {
                    let _ = std::fs::remove_file(path);
                }
            }
            if ready != gate_open {
                println!("{} GATE {} size={}x{} active={} fullscreen={}", ts(), if ready { "open" } else { "closed" },
                    w2.width(), w2.height(), w2.is_active(), w2.is_fullscreen());
                gate_open = ready;
            }
            glib::ControlFlow::Continue
        });
        // Flush stdout promptly for the test harness.
        glib::timeout_add_local(std::time::Duration::from_millis(100), || {
            use std::io::Write;
            let _ = std::io::stdout().flush();
            glib::ControlFlow::Continue
        });
    });
    app.run_with_args::<&str>(&[])
}
