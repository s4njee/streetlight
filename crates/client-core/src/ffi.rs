//! C ABI for the iPad app (see clients/ios/include/StreetlightCore.h).
//!
//! Strings returned by the library are heap-allocated UTF-8 and must be released
//! with `sl_string_free`. Errors are reported through an optional `char **err`.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::PathBuf;
use std::ptr;
use std::time::Duration;

use crate::{Client, ConnectOptions, InputEvent, MediaSink, MouseButton, StreamRequest, TouchPhase, VideoFrame};
use streetlight_proto::control::{ClientCaps, Codec, GamepadState};

pub type VideoCb = extern "C" fn(ctx: *mut c_void, data: *const u8, len: usize, frame: u32, keyframe: bool, received_us: u64);
pub type AudioCb = extern "C" fn(ctx: *mut c_void, data: *const u8, len: usize);

fn cstr(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

fn out_string(s: impl Into<Vec<u8>>) -> *mut c_char {
    CString::new(s).map(CString::into_raw).unwrap_or(ptr::null_mut())
}

fn set_err(err: *mut *mut c_char, e: impl std::fmt::Display) {
    if !err.is_null() {
        unsafe { *err = out_string(e.to_string()) };
    }
}

/// # Safety
/// `s` must be null or an unreleased string returned by this library; no aliases may use it after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Connect to `host` ("addr" or "addr:port"). Returns NULL on error.
/// `welcome_json` receives {"host_name","paired","host_fingerprint"}.
/// # Safety
/// Every non-null C string must point to valid NUL-terminated bytes for the entire call.
/// Every non-null output pointer must point to writable, aligned pointer storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_connect(
    host: *const c_char,
    client_name: *const c_char,
    data_dir: *const c_char,
    caps_json: *const c_char,
    welcome_json: *mut *mut c_char,
    err: *mut *mut c_char,
) -> *mut Client {
    let caps = serde_json::from_str::<ClientCaps>(&cstr(caps_json)).unwrap_or(ClientCaps {
        codecs: vec![Codec::Hevc],
        hdr: false,
        max_width: 3840,
        max_height: 2160,
        max_fps: 120,
        cursor_metadata: false, audio_channels: 2,
    });
    let opts = ConnectOptions { host: cstr(host), client_name: cstr(client_name), data_dir: PathBuf::from(cstr(data_dir)), caps };
    match Client::connect(opts) {
        Ok((c, w)) => {
            if !welcome_json.is_null() {
                let j = serde_json::json!({ "host_name": w.host_name, "paired": w.paired, "host_fingerprint": w.host_fingerprint });
                unsafe { *welcome_json = out_string(j.to_string()) };
            }
            Box::into_raw(Box::new(c))
        }
        Err(e) => {
            set_err(err, format!("{e:#}"));
            ptr::null_mut()
        }
    }
}

/// # Safety
/// `c` must be null or an exclusively owned live handle from `sl_connect`; no concurrent calls may use it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_free(c: *mut Client) {
    if !c.is_null() {
        drop(unsafe { Box::from_raw(c) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn sl_random_pin() -> *mut c_char {
    out_string(crate::random_pin())
}

/// Blocks until the host user enters `pin` (≤5 min). Returns 1 paired, 0 rejected, -1 error.
/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
/// Every non-null C string must point to valid NUL-terminated bytes for the entire call.
/// Every non-null output pointer must point to writable, aligned pointer storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_pair(c: *const Client, pin: *const c_char, err: *mut *mut c_char) -> i32 {
    let c = unsafe { &*c };
    match c.pair(&cstr(pin)) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            set_err(err, format!("{e:#}"));
            -1
        }
    }
}

/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
/// Every non-null output pointer must point to writable, aligned pointer storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_apps_json(c: *const Client, err: *mut *mut c_char) -> *mut c_char {
    let c = unsafe { &*c };
    match c.apps() {
        Ok(a) => out_string(serde_json::json!(a.iter().map(|a| serde_json::json!({"id": a.id, "name": a.name})).collect::<Vec<_>>()).to_string()),
        Err(e) => {
            set_err(err, format!("{e:#}"));
            ptr::null_mut()
        }
    }
}

struct CSink {
    ctx: *mut c_void,
    video: VideoCb,
    audio: AudioCb,
}
unsafe impl Send for CSink {}

impl MediaSink for CSink {
    fn video(&mut self, f: VideoFrame) {
        (self.video)(self.ctx, f.data.as_ptr(), f.data.len(), f.frame_number, f.keyframe, f.received_us);
    }
    fn audio(&mut self, p: &[u8]) {
        (self.audio)(self.ctx, p.as_ptr(), p.len());
    }
}

/// Start streaming. `request_json` is a `StreamRequest`. Callbacks run on the media
/// thread; the data pointers are only valid during the call. Returns StreamInfo JSON.
/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
/// Every non-null C string must point to valid NUL-terminated bytes for the entire call.
/// Every non-null output pointer must point to writable, aligned pointer storage.
/// Callbacks and their context must remain valid until `sl_stop` or `sl_free` finishes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_start(
    c: *const Client,
    request_json: *const c_char,
    video: VideoCb,
    audio: AudioCb,
    ctx: *mut c_void,
    err: *mut *mut c_char,
) -> *mut c_char {
    let c = unsafe { &*c };
    let req: StreamRequest = match serde_json::from_str(&cstr(request_json)) {
        Ok(r) => r,
        Err(e) => {
            set_err(err, format!("bad request: {e}"));
            return ptr::null_mut();
        }
    };
    match c.start(req, CSink { ctx, video, audio }) {
        Ok(info) => out_string(
            serde_json::json!({
                "width": info.width, "height": info.height, "fps": info.fps,
                "codec": format!("{:?}", info.codec), "audio_channels": info.audio_channels,
                "audio_sample_rate": info.audio_sample_rate,
            })
            .to_string(),
        ),
        Err(e) => {
            set_err(err, format!("{e:#}"));
            ptr::null_mut()
        }
    }
}

/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_stop(c: *const Client) {
    unsafe { &*c }.stop();
}

/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_request_idr(c: *const Client) {
    unsafe { &*c }.request_idr();
}

/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_report_timing(c: *const Client, decode_us: u32, present_us: u32) {
    unsafe { &*c }.report_timing(decode_us, present_us);
}

/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_stats_json(c: *const Client) -> *mut c_char {
    out_string(serde_json::to_string(&unsafe { &*c }.stats()).unwrap_or_default())
}

/// NULL while the stream is healthy, otherwise the reason it ended.
/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_stopped_reason(c: *const Client) -> *mut c_char {
    match unsafe { &*c }.stopped_reason() {
        Some(r) => out_string(r),
        None => ptr::null_mut(),
    }
}

/// # Safety
/// Every non-null C string must point to valid NUL-terminated bytes for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_known_hosts_json(data_dir: *const c_char) -> *mut c_char {
    let k = crate::store::KnownHosts::load(&PathBuf::from(cstr(data_dir)));
    out_string(serde_json::to_string(&k.hosts).unwrap_or_default())
}

/// # Safety
/// Every non-null C string must point to valid NUL-terminated bytes for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_forget_host(data_dir: *const c_char, fingerprint: *const c_char) {
    let dir = PathBuf::from(cstr(data_dir));
    let mut k = crate::store::KnownHosts::load(&dir);
    k.remove(&cstr(fingerprint));
    let _ = k.save(&dir);
}

#[unsafe(no_mangle)]
pub extern "C" fn sl_discover_json(timeout_ms: u32) -> *mut c_char {
    out_string(serde_json::to_string(&crate::discovery::browse(Duration::from_millis(timeout_ms as u64))).unwrap_or_default())
}

// ---- input (fast paths; called at touch / pointer rate) --------------------------------

/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_input_key(c: *const Client, evdev_code: u16, down: bool) {
    unsafe { &*c }.send_input(InputEvent::Key { code: evdev_code, down });
}

/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_input_mouse_move(c: *const Client, dx: i32, dy: i32) {
    unsafe { &*c }.send_input(InputEvent::MouseMove { dx, dy });
}

/// x, y normalised to 0..=65535 over the streamed display.
/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_input_mouse_abs(c: *const Client, x: u16, y: u16) {
    unsafe { &*c }.send_input(InputEvent::MouseAbs { x, y });
}

/// button: 0 left, 1 right, 2 middle, 3 back, 4 forward.
/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_input_mouse_button(c: *const Client, button: u8, down: bool) {
    let button = match button {
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        _ => MouseButton::Left,
    };
    unsafe { &*c }.send_input(InputEvent::MouseButton { button, down });
}

/// 120 units per wheel notch; positive dy scrolls up.
/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_input_scroll(c: *const Client, dx: i32, dy: i32) {
    unsafe { &*c }.send_input(InputEvent::Scroll { dx, dy });
}

/// phase: 0 down, 1 move, 2 up.
/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_input_touch(c: *const Client, id: u8, phase: u8, x: u16, y: u16) {
    let phase = match phase {
        0 => TouchPhase::Down,
        1 => TouchPhase::Move,
        _ => TouchPhase::Up,
    };
    unsafe { &*c }.send_input(InputEvent::Touch { id, phase, x, y });
}

/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn sl_input_gamepad(c: *const Client, id: u8, buttons: u32, lx: i16, ly: i16, rx: i16, ry: i16, lt: u8, rt: u8) {
    unsafe { &*c }.send_input(InputEvent::Gamepad(GamepadState { id, buttons, lx, ly, rx, ry, lt, rt }));
}

/// # Safety
/// `c` must be a non-null live handle returned by `sl_connect`, valid for the entire call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sl_input_gamepad_disconnected(c: *const Client, id: u8) {
    unsafe { &*c }.send_input(InputEvent::GamepadDisconnected { id });
}
