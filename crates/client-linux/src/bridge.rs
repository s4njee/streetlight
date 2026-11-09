//! WebKitGTK webview UI and its JS ↔ native bridge (same protocol as the iPad app):
//!   JS → native: webkit.messageHandlers.streetlight.postMessage('{"id","cmd","args"}')
//!   native → JS: streetlight.__resolve(id, ok, value) / streetlight.__event(name, data)

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{gdk, glib};
use serde_json::{Value, json};
use streetlight_client_core::{self as core, Client, ConnectOptions};
use webkit6::prelude::*;

use crate::session::{self, Session};
use crate::settings::{Overrides, Settings};
use crate::view::{FrameMailbox, StreamView};

const INDEX_HTML: &str = include_str!("../../../web/client/index.html");

struct Ui {
    webview: webkit6::WebView,
    view: Rc<StreamView>,
    window: gtk4::ApplicationWindow,
    client: Option<Arc<Client>>,
    session: Option<Session>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

pub fn install(window: &gtk4::ApplicationWindow, view: Rc<StreamView>) {
    let ucm = webkit6::UserContentManager::new();
    ucm.register_script_message_handler("streetlight", None);
    let webview = webkit6::WebView::builder().user_content_manager(&ucm).build();
    webview.set_background_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    webview.set_hexpand(true);
    webview.set_vexpand(true);
    if let Some(s) = WebViewExt::settings(&webview) {
        s.set_enable_developer_extras(std::env::var_os("SL_DEVTOOLS").is_some());
        s.set_enable_back_forward_navigation_gestures(false);
    }
    ucm.connect_script_message_received(Some("streetlight"), |_, value| {
        let raw = value.to_str();
        match serde_json::from_str::<Value>(&raw) {
            Ok(msg) => handle(msg),
            Err(e) => tracing::warn!("bad bridge message: {e}"),
        }
    });
    // Dev hook: run a script once the UI has loaded (used for automated UI tests).
    if let Ok(script) = std::env::var("SL_UI_EVAL") {
        webview.connect_load_changed(move |wv, ev| {
            if ev == webkit6::LoadEvent::Finished {
                wv.evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |_| {});
            }
        });
    }
    webview.load_html(INDEX_HTML, Some("streetlight://app/"));
    view.root.add_overlay(&webview);

    let v2 = view.clone();
    *view.on_escape.borrow_mut() = Some(Box::new(move || {
        let _ = &v2;
        end_stream(Some("returned to menu".into()));
    }));
    UI.with(|u| *u.borrow_mut() = Some(Ui { webview, view, window: window.clone(), client: None, session: None }));

    // Watch for the host ending the stream / dropping the connection.
    glib::timeout_add_local(Duration::from_millis(500), || {
        let reason = UI.with(|u| {
            let u = u.borrow();
            let u = u.as_ref()?;
            u.session.as_ref()?;
            u.client.as_ref()?.stopped_reason()
        });
        if let Some(r) = reason {
            end_stream(Some(r));
        }
        glib::ControlFlow::Continue
    });
}

fn js(script: String) {
    UI.with(|u| {
        if let Some(u) = u.borrow().as_ref() {
            u.webview.evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |_| {});
        }
    });
}

fn resolve(id: u64, r: Result<Value, String>) {
    let (ok, v) = match r {
        Ok(v) => (true, v),
        Err(e) => (false, Value::String(e)),
    };
    js(format!("window.streetlight.__resolve({id}, {ok}, {v})"));
}

fn event(name: &str, data: Value) {
    js(format!("window.streetlight.__event({}, {data})", Value::String(name.into())));
}

/// Run `f` on a worker thread and resolve the JS promise on the main thread.
fn background(id: u64, f: impl FnOnce() -> Result<Value, String> + Send + 'static) {
    std::thread::spawn(move || {
        let r = f();
        glib::idle_add_once(move || resolve(id, r));
    });
}

fn client() -> Option<Arc<Client>> {
    UI.with(|u| u.borrow().as_ref().and_then(|u| u.client.clone()))
}

fn handle(msg: Value) {
    let id = msg["id"].as_u64().unwrap_or(0);
    let args = msg["args"].clone();
    let cmd = msg["cmd"].as_str().unwrap_or("").to_string();
    let err = |e: anyhow::Error| format!("{e:#}");
    match cmd.as_str() {
        "discover" => background(id, || Ok(json!(core::discovery::browse(Duration::from_millis(1500))))),
        "knownHosts" => resolve(id, Ok(json!(core::store::KnownHosts::load(&crate::data_dir()).hosts))),
        "forgetHost" => {
            let mut k = core::store::KnownHosts::load(&crate::data_dir());
            k.remove(args["fingerprint"].as_str().unwrap_or(""));
            resolve(id, k.save(&crate::data_dir()).map(|_| Value::Null).map_err(|e| e.to_string()));
        }
        "connect" => {
            let host = args["host"].as_str().unwrap_or("").to_string();
            std::thread::spawn(move || {
                let r = Client::connect(ConnectOptions { host, client_name: crate::hostname(), data_dir: crate::data_dir(), caps: crate::caps() });
                glib::idle_add_once(move || match r {
                    Ok((c, w)) => {
                        UI.with(|u| u.borrow_mut().as_mut().unwrap().client = Some(Arc::new(c)));
                        resolve(id, Ok(json!({ "host_name": w.host_name, "paired": w.paired, "host_fingerprint": w.host_fingerprint })));
                    }
                    Err(e) => resolve(id, Err(err(e))),
                });
            });
        }
        "disconnect" => {
            end_stream(None);
            UI.with(|u| u.borrow_mut().as_mut().unwrap().client = None);
            resolve(id, Ok(Value::Null));
        }
        "pair" => {
            let Some(c) = client() else { return resolve(id, Err("not connected".into())) };
            let pin = core::random_pin();
            event("pairPin", json!({ "pin": pin }));
            background(id, move || c.pair(&pin).map(|ok| json!({ "ok": ok })).map_err(err));
        }
        "apps" => {
            let Some(c) = client() else { return resolve(id, Err("not connected".into())) };
            background(id, move || c.apps().map(|a| json!(a.iter().map(|a| json!({"id": a.id, "name": a.name})).collect::<Vec<_>>())).map_err(err));
        }
        "getSettings" => resolve(id, Ok(json!(Settings::load()))),
        "setSettings" => {
            let r = serde_json::from_value::<Settings>(args).map_err(|e| e.to_string()).and_then(|s| s.save().map_err(|e| e.to_string()));
            resolve(id, r.map(|_| Value::Null));
        }
        "start" => resolve(id, start_stream(args["app"].as_str().map(String::from))),
        "stop" => {
            end_stream(None);
            resolve(id, Ok(Value::Null));
        }
        "stats" => resolve(id, Ok(client().map(|c| json!(c.stats())).unwrap_or(Value::Null))),
        "quit" => UI.with(|u| {
            if let Some(u) = u.borrow().as_ref() {
                u.window.close();
            }
        }),
        other => resolve(id, Err(format!("unknown command {other}"))),
    }
}

fn start_stream(app: Option<String>) -> Result<Value, String> {
    let (client, view, window, webview) = UI.with(|u| {
        let u = u.borrow();
        let u = u.as_ref().unwrap();
        (u.client.clone(), u.view.clone(), u.window.clone(), u.webview.clone())
    });
    let client = client.ok_or("not connected")?;
    let req = Settings::load().request(app, &Overrides::default(), crate::native_mode(&window));
    let mailbox = FrameMailbox::new();
    let (v, mb) = (view.clone(), mailbox.clone());
    let sess = session::start(&client, req, mailbox, move || v.present(&mb)).map_err(|e| format!("{e:#}"))?;
    let info = json!({ "width": sess.info.width, "height": sess.info.height, "fps": sess.info.fps, "codec": format!("{:?}", sess.info.codec) });
    view.attach(Some(client), (sess.info.width, sess.info.height));
    // Hide the webview entirely so the video surface can be scanned out directly.
    webview.set_visible(false);
    UI.with(|u| u.borrow_mut().as_mut().unwrap().session = Some(sess));
    Ok(info)
}

fn end_stream(reason: Option<String>) {
    let had = UI.with(|u| {
        let mut u = u.borrow_mut();
        let u = u.as_mut().unwrap();
        let had = u.session.take().is_some();
        if had {
            if let Some(c) = &u.client {
                c.stop();
            }
            u.view.attach(None, (0, 0));
            u.webview.set_visible(true);
            u.webview.grab_focus();
        }
        had
    });
    if had {
        event("streamEnded", json!({ "reason": reason }));
    }
}
