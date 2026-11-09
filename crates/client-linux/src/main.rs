//! streetlight-client for Linux / Raspberry Pi 5.
//!
//!   streetlight-client                 webview UI (host list, pairing, apps, settings)
//!   streetlight-client <host>          connect and stream straight away (fullscreen)
//!   streetlight-client <host> --dump f stream to a file without decoding (diagnostics)

mod audio;
#[cfg(feature = "webview")]
mod bridge;
mod decode;
mod drm;
mod session;
mod selftest;
mod settings;
mod view;
mod pointer_lock;

use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use gtk4::prelude::*;
use gtk4::{gio, glib};
use streetlight_client_core::{self as core, ClientCaps, Codec, ConnectOptions, MediaSink, VideoFrame};

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| ".".into())
        .join("streetlight-client")
}

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "linux-client".into())
}

pub fn caps() -> ClientCaps {
    ClientCaps { codecs: if drm::has_request_decoder() { vec![Codec::Hevc] } else { decode::supported_codecs() }, hdr: decode::supports_hdr(), max_width: 7680, max_height: 4320, max_fps: 240, cursor_metadata: std::env::var("SL_CLIENT_CURSOR").map(|v|v=="1").unwrap_or_else(|_|!drm::has_request_decoder()), audio_channels: 2 }
}

pub static INPUT_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[derive(Default)]
struct Args {
    host: Option<String>,
    dump: Option<String>,
    seconds: u64,
    report_json: bool,
    decode_only: bool,
    no_audio: bool,
    latency: bool,
    timed: bool,
    windowed: bool,
    selftest: bool,
    clicks: bool,
    no_input: bool,
    overrides: settings::Overrides,
}

fn parse() -> Args {
    let mut a = Args { seconds: 10, ..Default::default() };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--hdr" => a.overrides.hdr=Some(true),
            "--codec" => a.overrides.codec = Some(match it.next().as_deref() {
                Some("auto") => Codec::Auto, Some("hevc") => Codec::Hevc, Some("av1") => Codec::Av1, Some("h264") => Codec::H264,
                _ => { eprintln!("--codec expects auto, hevc, av1 or h264"); std::process::exit(2); }
            }),
            "--dump" => a.dump = it.next(),
            "--seconds" => { a.seconds = it.next().and_then(|s| s.parse().ok()).unwrap_or(10); a.timed = true; },
            "--decode-only" => a.decode_only = true,
            "--no-audio" => a.no_audio = true,
            "--latency" => a.latency = true,
            "--size" => a.overrides.size = it.next().and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))),
            "--fps" => a.overrides.fps = it.next().and_then(|s| s.parse().ok()),
            "--bitrate" => a.overrides.bitrate_kbps = it.next().and_then(|s| s.parse().ok()),
            "--report" => {
                if it.next().as_deref() != Some("json") { eprintln!("--report requires json"); std::process::exit(2); }
                a.report_json = true;
            },
            "--windowed" => a.windowed = true,
            "--selftest" => a.selftest = true,
            "--clicks" => a.clicks = true,
            "--no-input" => a.no_input = true,
            "-h" | "--help" => {
                println!("usage: streetlight-client [host] [--size WxH] [--codec auto|hevc|av1|h264] [--hdr] [--fps N] [--bitrate kbps] [--dump file | --decode-only] [--seconds N] [--latency] [--report json] [--windowed --no-input --no-audio]");
                std::process::exit(0);
            }
            h if !h.starts_with('-') => a.host = Some(h.to_string()),
            other => {
                eprintln!("unknown option {other} (see --help)");
                std::process::exit(2);
            }
        }
    }
    a
}

/// Connects, pairing in the terminal if needed.
pub fn connect_cli(host: &str) -> Result<Arc<core::Client>> {
    let (client, welcome) = core::Client::connect(ConnectOptions { host: host.into(), client_name: hostname(), data_dir: data_dir(), caps: caps() })?;
    println!("connected to '{}' (paired: {})", welcome.host_name, welcome.paired);
    if !welcome.paired {
        let pin = core::random_pin();
        println!("\n  Pairing PIN: {pin}\n  Enter it in the host web UI (http://127.0.0.1:47990) or run `streetlight-host pair {pin}` on the host.\n");
        if !client.pair(&pin)? {
            bail!("pairing failed");
        }
        println!("paired!");
    }
    Ok(Arc::new(client))
}

fn resolve_host(h: Option<String>) -> Result<String> {
    if let Some(h) = h {
        return Ok(h);
    }
    let found = core::discovery::browse(Duration::from_secs(2));
    match found.first() {
        Some(h) => {
            println!("using discovered host '{}' at {}", h.name, h.addr);
            Ok(h.addr.clone())
        }
        None => bail!("no host found on the LAN; pass an address"),
    }
}

struct DumpSink {
    file: Option<std::fs::File>,
    decoder: Option<decode::Decoder>,
    decode_enabled: bool,
    client: std::sync::Weak<core::Client>,
}
impl MediaSink for DumpSink {
    fn configure(&mut self, info: &core::StreamInfo) -> Result<()> {
        if self.decode_enabled { let decoder=decode::Decoder::new(info.codec)?; tracing::info!("decode diagnostic: {}",decoder.description);self.decoder=Some(decoder); }
        Ok(())
    }
    fn video(&mut self, f: VideoFrame) {
        if let Some(file) = &mut self.file { let _ = file.write_all(&f.data); }
        if let Some(decoder) = &mut self.decoder {
            let started = core::now_us();
            match decoder.decode(&f.data) {
                Ok(Some(_)) => { if let Some(c) = self.client.upgrade() { c.report_decode((core::now_us() - started) as u32); } },
                Ok(None) => {},
                Err(e) => { tracing::error!("decode: {e:#}"); if let Some(c) = self.client.upgrade() { c.report_decode_error(); c.request_idr(); } },
            }
        }
    }
    fn audio(&mut self, _: &[u8]) {}
}

fn dump(args: Args) -> Result<()> {
    let client = connect_cli(&resolve_host(args.host)?)?;
    let req = settings::Settings::load().request(None, &args.overrides, None);
    let decoder = None;
    let sink = DumpSink { file: args.dump.as_ref().map(std::fs::File::create).transpose()?, decoder, decode_enabled: args.decode_only, client: Arc::downgrade(&client) };
    let info = client.start(req, sink)?;
    println!("stream: {}x{}@{} {:?} {:?}", info.width, info.height, info.fps, info.codec, info.color);
    for second in 1..=args.seconds {
        std::thread::sleep(Duration::from_secs(1));
        if args.report_json { println!("{}", serde_json::to_string(&client.stats())?); }
        else { print_stats(&client); }
        if args.latency && second.is_multiple_of(5) { println!("LATENCY {}", serde_json::to_string(&client.latency_report())?); }
        if let Some(r) = client.stopped_reason() {
            println!("stopped: {r}");
            break;
        }
    }
    client.stop();
    let stats = client.stats();
    if args.decode_only && (stats.frames_decoded == 0 || stats.decode_errors > 0) { bail!("decode check failed: {} frames, {} errors", stats.frames_decoded, stats.decode_errors); }
    Ok(())
}

pub fn print_stats(c: &core::Client) {
    let s = c.stats();
    println!(
        "fps {:5.1} | {:6.1} Mbit/s | frames {} lost {} recovered {} IDR {} keyframes {} | net {:.2} ms decode {:.2} ms queue→present {:.2} ms | rtt {:.2} ms",
        s.fps, s.mbps, s.frames, s.frames_lost, s.frames_recovered, s.idr_requests, s.keyframes, s.network_latency_ms, s.decode_ms, s.queue_to_present_ms, s.rtt_ms
    );
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = parse();
    INPUT_ENABLED.store(!args.no_input, std::sync::atomic::Ordering::Relaxed);
    if args.no_audio { audio::DISABLED.store(true, std::sync::atomic::Ordering::Relaxed); }
    if args.dump.is_some() || args.decode_only {
        return dump(args);
    }
    if args.selftest {
        return selftest::run(connect_cli(&resolve_host(args.host.clone())?)?, args.clicks);
    }
    let app = gtk4::Application::builder().application_id("dev.streetlight.Client").flags(gio::ApplicationFlags::NON_UNIQUE).build();
    let args = Rc::new(RefCell::new(Some(args)));
    app.connect_activate(move |app| {
        let a = args.borrow_mut().take().unwrap_or_default();
        if let Err(e) = activate(app, a) {
            eprintln!("{e:#}");
            app.quit();
        }
    });
    #[cfg(feature = "webview")]
    app.connect_shutdown(|_| bridge::shutdown());
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn activate(app: &gtk4::Application, args: Args) -> Result<()> {
    let window = gtk4::ApplicationWindow::builder().application(app).title("Streetlight").default_width(1280).default_height(720).build();
    let view = view::StreamView::new();
    window.set_child(Some(&view.root));
    if !args.windowed {
        window.fullscreen();
    }
    view.set_input_enabled(!args.no_input);

    #[cfg(feature = "webview")]
    if args.host.is_none() {
        bridge::install(&window, view.clone());
        window.present();
        return Ok(());
    }

    // Direct mode: connect + stream immediately; Ctrl+Alt+Shift+Q quits.
    let host = resolve_host(args.host)?;
    let client = connect_cli(&host)?;
    let mailbox = view::FrameMailbox::new();
    let req = settings::Settings::load().request(None, &args.overrides, native_mode(&window));
    let v = view.clone();
    let mb = mailbox.clone();
    let sess = session::start(&client, req, mailbox, move || v.present(&mb))?;
    println!("stream: {}x{}@{} {:?} {:?}", sess.info.width, sess.info.height, sess.info.fps, sess.info.codec, sess.info.color);
    view.set_stream_color(sess.info.color)?;
    view.attach(Some(client.clone()), (sess.info.width, sess.info.height));
    let sess = Rc::new(RefCell::new(Some(sess)));
    {
        let (app, client, sess) = (app.clone(), client.clone(), sess.clone());
        *view.on_escape.borrow_mut() = Some(Box::new(move || {
            sess.borrow_mut().take();
            client.stop();
            app.quit();
        }));
    }
    let (c, app2) = (client.clone(), app.clone());
    let mut seconds = 0u64;
    let duration = args.timed.then_some(args.seconds);
    glib::timeout_add_seconds_local(1, move || {
        seconds += 1;
        if args.report_json { println!("{}", serde_json::to_string(&c.stats()).unwrap()); }
        else { print_stats(&c); }
        if args.latency && seconds.is_multiple_of(5) { println!("LATENCY {}", serde_json::to_string(&c.latency_report()).unwrap()); }
        if duration.is_some_and(|limit| seconds >= limit) { c.stop(); app2.quit(); return glib::ControlFlow::Break; }
        if let Some(r) = c.stopped_reason() {
            println!("stream ended: {r}");
            app2.quit();
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
    let closing = client.clone();
    window.connect_close_request(move |_| {
        closing.stop();
        glib::Propagation::Proceed
    });
    window.present();
    Ok(())
}

/// The monitor's native mode (pixels, Hz) — used for "match this display" streaming.
pub fn native_mode(window: &impl IsA<gtk4::Widget>) -> Option<(u32, u32, u32)> {
    let display = gtk4::prelude::WidgetExt::display(window);
    let monitors = display.monitors();
    let m = monitors.item(0)?.downcast::<gtk4::gdk::Monitor>().ok()?;
    let g = m.geometry();
    let scale = m.scale();
    Some(((g.width() as f64 * scale) as u32, (g.height() as f64 * scale) as u32, (m.refresh_rate() as u32).div_ceil(1000)))
}
