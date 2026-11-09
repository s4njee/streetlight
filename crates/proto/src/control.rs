//! Control-channel messages (QUIC, postcard, u32-LE length prefixed).

use serde::{Deserialize, Serialize};

pub const ALPN: &[u8] = b"streetlight/3";
pub const CONTROL_PORT: u16 = 47800;
pub const MEDIA_PORT: u16 = 47801;
pub const WEB_PORT: u16 = 47990;
pub const MDNS_SERVICE: &str = "_streetlight._udp.local.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    /// Choose the best hardware codec shared by client and host.
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorSpace {
    /// BT.709, 8-bit, limited range.
    Sdr709,
    /// BT.2020 PQ, 10-bit.
    Hdr10,
    /// Legacy unmanaged NV12 presentation (BT.601 matrix, full range, sRGB primaries/TRC).
    Sdr601Full,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCaps {
    pub codecs: Vec<Codec>,
    pub hdr: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    #[serde(default)]
    pub cursor_metadata: bool,
    pub audio_channels: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayRequest {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// HiDPI scale for the virtual output (e.g. 2.0 on iPad).
    pub scale: f32,
    /// Create a virtual monitor (true) or mirror an existing one (false).
    pub virtual_display: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamRequest {
    pub display: DisplayRequest,
    pub codec: Codec,
    pub color: ColorSpace,
    pub bitrate_kbps: u32,
    pub fec_percent: u16,
    pub max_shard_bytes: u16,
    /// Pace video sends at this rate (Mbit/s); 0 = send each frame as one burst.
    /// Use ~2-4x the bitrate on Wi-Fi to avoid burst loss.
    pub pace_mbps: u32,
    /// Optional app id to launch (from `AppList`).
    pub app: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello { name: String, caps: ClientCaps },
    /// SPAKE2 message A (client side) for PIN pairing.
    PairStart { client_name: String, spake: Vec<u8> },
    PairConfirm { mac: Vec<u8> },
    ListApps,
    Start(StreamRequest),
    Stop,
    RequestIdr,
    SetBitrate { kbps: u32 },
    Ping { client_us: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMsg {
    Welcome { host_name: String, paired: bool, host_version: String },
    /// Host is waiting for the user to type the PIN shown on the client.
    PairAwaitingPin,
    PairSpake { spake: Vec<u8> },
    PairResult { ok: bool, mac: Vec<u8> },
    Apps(Vec<AppInfo>),
    StreamStarted(StreamInfo),
    StreamStopped { reason: String },
    Pong { client_us: u64, host_us: u64 },
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamInfo {
    pub codec: Codec,
    pub color: ColorSpace,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub media_port: u16,
    pub key: [u8; 16],
    /// Sent back by the client in a Probe packet so the host learns its media address.
    pub token: [u8; 16],
    #[serde(default)]
    pub cursor_metadata: bool,
    pub audio_channels: u8,
    pub audio_sample_rate: u32,
}

/// Client → host QUIC datagram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feedback {
    pub last_frame: u32,
    pub idr_requests: u32,
    pub frames_lost: u32,
    pub frames_recovered: u32,
    pub decode_us_avg: u32,
    pub present_us_avg: u32,
    pub rtt_us: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Gamepad state snapshot (idempotent; safe to resend).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct GamepadState {
    pub id: u8,
    /// Bitmask, see `pad_buttons`.
    pub buttons: u32,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u8,
    pub rt: u8,
}

pub mod pad_buttons {
    pub const A: u32 = 1 << 0;
    pub const B: u32 = 1 << 1;
    pub const X: u32 = 1 << 2;
    pub const Y: u32 = 1 << 3;
    pub const LB: u32 = 1 << 4;
    pub const RB: u32 = 1 << 5;
    pub const BACK: u32 = 1 << 6;
    pub const START: u32 = 1 << 7;
    pub const GUIDE: u32 = 1 << 8;
    pub const LS: u32 = 1 << 9;
    pub const RS: u32 = 1 << 10;
    pub const UP: u32 = 1 << 11;
    pub const DOWN: u32 = 1 << 12;
    pub const LEFT: u32 = 1 << 13;
    pub const RIGHT: u32 = 1 << 14;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    /// Linux evdev key code (KEY_*). Clients translate their native codes.
    Key { code: u16, down: bool },
    MouseMove { dx: i32, dy: i32 },
    /// Absolute position normalised to 0..=65535 over the streamed display.
    MouseAbs { x: u16, y: u16 },
    MouseButton { button: MouseButton, down: bool },
    /// High-resolution scroll, 120 units = one notch.
    Scroll { dx: i32, dy: i32 },
    Touch { id: u8, phase: TouchPhase, x: u16, y: u16 },
    Gamepad(GamepadState),
    GamepadDisconnected { id: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(msg).expect("serialize");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

pub fn decode<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(body)
}

pub const MAX_CONTROL_MSG: usize = 1 << 20;
