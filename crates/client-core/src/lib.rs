//! Streetlight client transport, shared by every client (Linux/Pi natively, iPad via
//! the C ABI in `ffi`). Owns the QUIC control connection, pairing, the UDP media
//! receiver (decrypt + FEC + in-order reassembly + loss recovery), input and feedback.
//! Decoding and presentation are left to the platform.

mod impairment;
pub mod latency;
pub mod discovery;
pub mod ffi;
pub mod store;

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
pub use streetlight_proto::control::{
    AppInfo, ClientCaps, Codec, ColorSpace, DisplayRequest, GamepadState, InputEvent, MouseButton, StreamInfo, StreamRequest, TouchPhase,
    pad_buttons,
};
use streetlight_proto::control::{self, ClientMsg, Feedback, HostMsg, MEDIA_PORT};
use streetlight_proto::crypto::MediaCipher;
use streetlight_proto::fec::{Assembled, Reassembler};
use streetlight_proto::media::{HEADER_LEN, MediaHeader, StreamKind};
use streetlight_proto::net::{self, Identity};
use streetlight_proto::pair::{self, PairSide};
use tokio::sync::{mpsc, oneshot};

pub use streetlight_proto::control::CONTROL_PORT;

/// One complete compressed video access unit (Annex B).
pub struct VideoFrame {
    pub data: Vec<u8>,
    pub frame_number: u32,
    pub keyframe: bool,
    /// Host capture time (low 32 bits of host monotonic µs).
    pub capture_us: u32,
    /// Local monotonic µs when the last needed packet arrived.
    pub received_us: u64,
}

/// Platform side of a stream. Called from the media thread.
pub trait MediaSink: Send + 'static {
    fn configure(&mut self, _info: &StreamInfo) -> Result<()> { Ok(()) }
    fn video(&mut self, frame: VideoFrame);
    fn audio(&mut self, opus_packet: &[u8]);
    /// One missing 5 ms Opus packet. Platforms with PLC should conceal it; the
    /// default leaves a short gap without blocking later audio (including C ABI clients).
    fn audio_lost(&mut self) {}
}

#[derive(Debug, Clone)]
pub struct Welcome {
    pub host_name: String,
    /// Both sides trust each other (host knows our cert, we pinned the host's).
    pub paired: bool,
    pub host_fingerprint: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Stats {
    /// Complete video frames, including dependent frames skipped during recovery.
    pub frames: u64,
    pub last_frame: u32,
    pub frames_decoded: u64,
    pub decode_errors: u64,
    pub frames_presented: u64,
    pub cursor_positions: u64,
    pub cursor_bitmaps: u64,
    pub audio_underruns: u64,
    pub audio_output_errors: u64,
    pub frames_lost: u64,
    pub frames_recovered: u64,
    pub idr_requests: u64,
    pub keyframes: u64,
    pub audio_packets_lost: u64,
    pub test_packets_dropped: u64,
    /// Frames actually passed to the platform decoder/dump after keyframe gating.
    pub video_frames_delivered: u64,
    pub awaiting_keyframe: bool,
    /// Largest time between deliveries, including any ongoing stall (excludes startup).
    pub max_video_gap_ms: f32,
    /// Largest Lost-event to next keyframe time, including any outstanding loss.
    pub max_recovery_ms: f32,
    pub packets: u64,
    pub bad_packets: u64,
    pub fps: f32,
    pub mbps: f32,
    /// Host capture → client frame-complete, using the clock offset from pings.
    pub network_latency_ms: f32,
    pub decode_ms: f32,
    pub queue_to_present_ms: f32,
    /// Legacy platform timing (retained for C ABI compatibility).
    pub present_ms: f32,
    pub rtt_ms: f32,
}

enum Cmd {
    Pair { pin: String, reply: oneshot::Sender<Result<bool>> },
    ListApps { reply: oneshot::Sender<Result<Vec<AppInfo>>> },
    Start { req: StreamRequest, reply: oneshot::Sender<Result<StreamInfo>> },
    Stop { reply: std::sync::mpsc::Sender<Result<()>> },
    RequestIdr,
}

struct Shared {
    cursor: Mutex<streetlight_proto::cursor::Snapshot>,
    cursor_epoch: AtomicU64,
    latency: Mutex<latency::Window>,
    stats: Mutex<Stats>,
    /// host_us - local_us
    clock_offset_us: AtomicI64,
    rtt_us: AtomicU32,
    decode_us: AtomicU32,
    present_us: AtomicU32,
    stopped: Mutex<Option<String>>,
}

pub struct Client {
    rt: tokio::runtime::Runtime,
    conn: quinn::Connection,
    endpoint: quinn::Endpoint,
    cmd: mpsc::UnboundedSender<Cmd>,
    input: mpsc::UnboundedSender<InputEvent>,
    shared: Arc<Shared>,
    host_fp: [u8; 32],
    host_addr: SocketAddr,
    our_fp: [u8; 32],
    host_name: String,
    data_dir: PathBuf,
    media: Mutex<Option<MediaReceiver>>,
}

pub struct ConnectOptions {
    pub host: String,
    pub client_name: String,
    /// Where the client certificate and known hosts live.
    pub data_dir: PathBuf,
    pub caps: ClientCaps,
}

/// Monotonic µs (same clock domain as the host on Linux; only differences matter here).
pub fn now_us() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

pub fn random_pin() -> String {
    pair::random_pin()
}

impl Client {
    pub fn connect(opts: ConnectOptions) -> Result<(Self, Welcome)> {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("sl-net").build()?;
        let identity = Identity::load_or_create(&opts.data_dir, "client")?;
        let our_fp = identity.fingerprint();
        let host_addr = resolve(&opts.host)?;
        let (endpoint, conn, tx, rx, welcome_host) = rt.block_on(async {
            let mut ep = quinn::Endpoint::client(if host_addr.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? })?;
            ep.set_default_client_config(net::client_config(&identity)?);
            let conn = tokio::time::timeout(Duration::from_secs(5), ep.connect(host_addr, "streetlight")?)
                .await
                .context("connection timed out")??;
            let (mut tx, mut rx) = conn.open_bi().await?;
            net::send_msg(&mut tx, &ClientMsg::Hello { name: opts.client_name.clone(), caps: opts.caps.clone() }).await?;
            let Some(HostMsg::Welcome { host_name, paired, .. }) = net::recv_msg(&mut rx).await? else { bail!("unexpected reply") };
            anyhow::Ok((ep, conn, tx, rx, (host_name, paired)))
        })?;
        let host_fp = net::peer_fingerprint(&conn).context("host sent no certificate")?;
        let host_fp_hex = pair::fingerprint_hex(&host_fp);
        let known = store::KnownHosts::load(&opts.data_dir);
        let pinned = known.get(&host_fp_hex).is_some();
        let welcome = Welcome { host_name: welcome_host.0.clone(), paired: welcome_host.1 && pinned, host_fingerprint: host_fp_hex };

        let shared = Arc::new(Shared {
            cursor: Mutex::new(Default::default()),
            cursor_epoch: AtomicU64::new(0),
            latency: Mutex::new(latency::Window::default()),
            stats: Mutex::new(Stats::default()),
            clock_offset_us: AtomicI64::new(0),
            rtt_us: AtomicU32::new(0),
            decode_us: AtomicU32::new(0),
            present_us: AtomicU32::new(0),
            stopped: Mutex::new(None),
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        rt.spawn(control_task(tx, rx, cmd_rx, shared.clone(), host_fp, our_fp));
        rt.spawn(input_task(conn.clone(), in_rx));
        rt.spawn(feedback_task(conn.clone(), shared.clone()));
        rt.spawn(cursor_positions(conn.clone(),shared.clone()));
        rt.spawn(cursor_bitmaps(conn.clone(),shared.clone()));
        let client = Self {
            rt,
            conn,
            endpoint,
            cmd: cmd_tx,
            input: in_tx,
            shared,
            host_fp,
            host_addr,
            our_fp,
            host_name: welcome.host_name.clone(),
            data_dir: opts.data_dir,
            media: Mutex::new(None),
        };
        let _ = client.our_fp;
        Ok((client, welcome))
    }

    fn call<T>(&self, f: impl FnOnce(oneshot::Sender<Result<T>>) -> Cmd) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(f(tx)).ok().context("connection closed")?;
        rx.blocking_recv().context("connection closed")?
    }

    /// Show `pin` to the user, then call this; blocks until the host user types it (≤5 min).
    pub fn pair(&self, pin: &str) -> Result<bool> {
        let ok = self.call(|reply| Cmd::Pair { pin: pin.to_string(), reply })?;
        if ok {
            let mut k = store::KnownHosts::load(&self.data_dir);
            k.insert(pair::fingerprint_hex(&self.host_fp), self.host_name.clone(), self.host_addr.to_string());
            k.save(&self.data_dir)?;
        }
        Ok(ok)
    }

    pub fn apps(&self) -> Result<Vec<AppInfo>> {
        self.call(|reply| Cmd::ListApps { reply })
    }

    pub fn start(&self, req: StreamRequest, mut sink: impl MediaSink) -> Result<StreamInfo> {
        self.stop_media();
        *self.shared.stopped.lock().unwrap() = None;
        let info = self.call(|reply| Cmd::Start { req, reply })?;
        *self.shared.cursor.lock().unwrap()=Default::default();
        self.shared.cursor_epoch.store(u64::from_le_bytes(info.token[..8].try_into().unwrap()),Ordering::Relaxed);
        if let Err(e) = sink.configure(&info) { self.stop(); return Err(e); }
        let mut media_addr = self.host_addr;
        media_addr.set_port(if info.media_port != 0 { info.media_port } else { MEDIA_PORT });
        let rx = MediaReceiver::start(media_addr, &info, Box::new(sink), self.cmd.clone(), self.shared.clone())?;
        *self.media.lock().unwrap() = Some(rx);
        Ok(info)
    }

    /// Queue a stop without waiting on the UI thread. Session teardown still
    /// calls `stop` to join media and confirm that the host released capture.
    pub fn request_stop(&self) {
        let (reply, _) = std::sync::mpsc::channel();
        let _ = self.cmd.send(Cmd::Stop { reply });
    }

    pub fn stop(&self) {
        self.stop_media();
        let (reply, result) = std::sync::mpsc::channel();
        if self.cmd.send(Cmd::Stop { reply }).is_ok() {
            match result.recv_timeout(Duration::from_secs(3)) {
                Ok(Ok(())) => {},
                _ => self.conn.close(0u32.into(), b"stop timed out"),
            }
        }
    }

    fn stop_media(&self) {
        if let Some(m) = self.media.lock().unwrap().take() {
            drop(m);
        }
    }

    pub fn send_input(&self, ev: InputEvent) {
        let _ = self.input.send(ev);
    }

    pub fn request_idr(&self) {
        let _ = self.cmd.send(Cmd::RequestIdr);
    }

    /// Platform reports how long decode / present took (rolling averages kept by caller or here).
    pub fn report_timing(&self, decode_us: u32, present_us: u32) {
        let avg = |a: &AtomicU32, v: u32| {
            let old = a.load(Ordering::Relaxed);
            a.store(if old == 0 { v } else { (old * 7 + v) / 8 }, Ordering::Relaxed);
        };
        avg(&self.shared.decode_us, decode_us);
        avg(&self.shared.present_us, present_us);
    }

    /// Record decoder work independently from the compositor queue.
    pub fn report_decode(&self, decode_us: u32) {
        let old = self.shared.decode_us.load(Ordering::Relaxed);
        self.shared.decode_us.store(if old == 0 { decode_us } else { (old * 7 + decode_us) / 8 }, Ordering::Relaxed);
        self.shared.stats.lock().unwrap().frames_decoded += 1;
        self.shared.latency.lock().unwrap().decoded(decode_us as u64);
    }

    pub fn report_audio_stats(&self, underruns: u64, errors: u64) {
        let mut stats = self.shared.stats.lock().unwrap();
        stats.audio_underruns = underruns;
        stats.audio_output_errors = errors;
    }

    pub fn report_decode_error(&self) { self.shared.stats.lock().unwrap().decode_errors += 1; }

    /// `presented_us` is the actual compositor timestamp translated to the local clock.
    pub fn report_presentation(&self, decoded_us: u64, capture_local_us: u64, presented_us: u64) {
        let queue_us = presented_us.saturating_sub(decoded_us);
        self.shared.present_us.store(queue_us.min(u32::MAX as u64) as u32, Ordering::Relaxed);
        self.shared.stats.lock().unwrap().frames_presented += 1;
        self.shared.latency.lock().unwrap().presented(queue_us, presented_us.saturating_sub(capture_local_us));
    }

    pub fn latency_report(&self) -> latency::Report { self.shared.latency.lock().unwrap().drain() }

    pub fn cursor(&self) -> streetlight_proto::cursor::Snapshot { self.shared.cursor.lock().unwrap().clone() }

    pub fn stats(&self) -> Stats {
        let mut s = self.shared.stats.lock().unwrap().clone();
        s.decode_ms = self.shared.decode_us.load(Ordering::Relaxed) as f32 / 1000.0;
        s.queue_to_present_ms = self.shared.present_us.load(Ordering::Relaxed) as f32 / 1000.0;
        s.present_ms = s.queue_to_present_ms;
        s.rtt_ms = self.shared.rtt_us.load(Ordering::Relaxed) as f32 / 1000.0;
        s
    }

    /// Some(reason) once the host ended the stream or the connection dropped.
    pub fn stopped_reason(&self) -> Option<String> {
        if let Some(r) = self.conn.close_reason() {
            return Some(format!("connection lost: {r}"));
        }
        self.shared.stopped.lock().unwrap().clone()
    }

    /// Host-clock µs (low 32 bits) → local µs, for latency estimates.
    pub fn host_to_local_us(&self, host_us32: u32) -> u64 {
        host_to_local(&self.shared, host_us32)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let active = self.media.lock().unwrap().is_some();
        if active { self.stop(); } else { self.stop_media(); }
        self.conn.close(0u32.into(), b"bye");
        // Keep Quinn's driver alive until it flushes the close packet. Shutting
        // down the runtime immediately leaves the host waiting for idle timeout.
        let _ = self.rt.block_on(async {
            tokio::time::timeout(Duration::from_millis(500), self.endpoint.wait_idle()).await
        });
        let rt = std::mem::replace(&mut self.rt, tokio::runtime::Builder::new_current_thread().build().unwrap());
        rt.shutdown_timeout(Duration::from_millis(200));
    }
}

fn host_to_local(shared: &Shared, host_us32: u32) -> u64 {
    let off = shared.clock_offset_us.load(Ordering::Relaxed);
    let local_now = now_us() as i64;
    let host_now = local_now + off;
    // Reconstruct the full host timestamp nearest to host_now.
    let delta = (host_us32.wrapping_sub(host_now as u32)) as i32 as i64;
    (host_now + delta - off).max(0) as u64
}

pub fn resolve(host: &str) -> Result<SocketAddr> {
    let with_port = if host.parse::<SocketAddr>().is_ok() || host.rsplit_once(':').is_some_and(|(h, p)| !h.ends_with(':') && p.parse::<u16>().is_ok() && !h.contains(':')) {
        host.to_string()
    } else if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{CONTROL_PORT}")
    } else {
        format!("{host}:{CONTROL_PORT}")
    };
    with_port.to_socket_addrs()?.next().with_context(|| format!("can't resolve {host}"))
}

async fn control_task(
    mut tx: quinn::SendStream,
    mut rx: quinn::RecvStream,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    shared: Arc<Shared>,
    host_fp: [u8; 32],
    our_fp: [u8; 32],
) {
    let mut ping = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return };
                if let Err(e) = handle_cmd(cmd, &mut tx, &mut rx, &shared, &host_fp, &our_fp).await {
                    tracing::warn!("control: {e:#}");
                }
            }
            msg = net::recv_msg::<HostMsg>(&mut rx) => match msg {
                Ok(Some(m)) => unsolicited(m, &shared),
                _ => {
                    *shared.stopped.lock().unwrap() = Some("control stream closed".into());
                    return;
                }
            },
            _ = ping.tick() => {
                let _ = net::send_msg(&mut tx, &ClientMsg::Ping { client_us: now_us() }).await;
            }
        }
    }
}

fn unsolicited(m: HostMsg, shared: &Shared) {
    match m {
        HostMsg::Pong { client_us, host_us } => {
            let now = now_us();
            let rtt = now.saturating_sub(client_us);
            let mid = client_us + rtt / 2;
            let offset = host_us as i64 - mid as i64;
            let old = shared.rtt_us.load(Ordering::Relaxed);
            // Keep the offset from the lowest-RTT sample seen recently.
            if old == 0 || rtt as u32 <= old.saturating_mul(3) / 2 {
                shared.clock_offset_us.store(offset, Ordering::Relaxed);
            }
            shared.rtt_us.store(if old == 0 { rtt as u32 } else { (old * 3 + rtt as u32) / 4 }, Ordering::Relaxed);
        }
        HostMsg::StreamStopped { reason } => *shared.stopped.lock().unwrap() = Some(reason),
        HostMsg::Error(e) => tracing::warn!("host error: {e}"),
        other => tracing::debug!("unsolicited {other:?}"),
    }
}

/// Read until a non-Pong message arrives (Pongs are processed on the way).
async fn next_reply(rx: &mut quinn::RecvStream, shared: &Shared) -> Result<HostMsg> {
    loop {
        match net::recv_msg::<HostMsg>(rx).await?.context("connection closed")? {
            m @ HostMsg::Pong { .. } => unsolicited(m, shared),
            m => return Ok(m),
        }
    }
}

async fn handle_cmd(cmd: Cmd, tx: &mut quinn::SendStream, rx: &mut quinn::RecvStream, shared: &Shared, host_fp: &[u8; 32], our_fp: &[u8; 32]) -> Result<()> {
    match cmd {
        Cmd::Pair { pin, reply } => {
            let r = async {
                let (side, spake_a) = PairSide::client(&pin);
                let name = std::env::var("HOSTNAME").unwrap_or_else(|_| "streetlight-client".into());
                net::send_msg(tx, &ClientMsg::PairStart { client_name: name, spake: spake_a }).await?;
                loop {
                    match next_reply(rx, shared).await? {
                        HostMsg::PairAwaitingPin => continue,
                        HostMsg::PairSpake { spake } => {
                            let key = side.finish(&spake).context("SPAKE2 failed")?;
                            net::send_msg(tx, &ClientMsg::PairConfirm { mac: key.mac(pair::CLIENT_ID, our_fp, host_fp) }).await?;
                            let HostMsg::PairResult { ok, mac } = next_reply(rx, shared).await? else { bail!("expected PairResult") };
                            return Ok(ok && key.verify(pair::HOST_ID, our_fp, host_fp, &mac));
                        }
                        HostMsg::PairResult { ok: false, .. } => return Ok(false),
                        HostMsg::Error(e) => bail!("{e}"),
                        other => bail!("unexpected {other:?}"),
                    }
                }
            }
            .await;
            let _ = reply.send(r);
        }
        Cmd::ListApps { reply } => {
            let r = async {
                net::send_msg(tx, &ClientMsg::ListApps).await?;
                match next_reply(rx, shared).await? {
                    HostMsg::Apps(a) => Ok(a),
                    HostMsg::Error(e) => bail!("{e}"),
                    other => bail!("unexpected {other:?}"),
                }
            }
            .await;
            let _ = reply.send(r);
        }
        Cmd::Start { req, reply } => {
            let r = async {
                net::send_msg(tx, &ClientMsg::Start(req)).await?;
                loop {
                    match next_reply(rx, shared).await? {
                        HostMsg::StreamStarted(i) => return Ok(i),
                        HostMsg::StreamStopped { .. } => continue,
                        HostMsg::Error(e) => bail!("{e}"),
                        other => bail!("unexpected {other:?}"),
                    }
                }
            }
            .await;
            let _ = reply.send(r);
        }
        Cmd::Stop { reply } => {
            let result = tokio::time::timeout(Duration::from_secs(2), async {
                net::send_msg(tx, &ClientMsg::Stop).await?;
                match next_reply(rx, shared).await? {
                    HostMsg::StreamStopped { .. } => Ok(()),
                    HostMsg::Error(e) => bail!("{e}"),
                    other => bail!("unexpected stop reply {other:?}"),
                }
            }).await.context("stop acknowledgement timed out").and_then(|r| r);
            let _ = reply.send(result);
        },
        Cmd::RequestIdr => {
            net::send_msg(tx, &ClientMsg::RequestIdr).await?;
            shared.stats.lock().unwrap().idr_requests += 1;
        },
    }
    Ok(())
}

async fn input_task(conn: quinn::Connection, mut rx: mpsc::UnboundedReceiver<InputEvent>) {
    let Ok(mut s) = conn.open_uni().await else { return };
    let _ = s.set_priority(10);
    let mut pending: Option<InputEvent> = None;
    loop {
        let ev = match pending.take() {
            Some(e) => e,
            None => match rx.recv().await {
                Some(e) => e,
                None => return,
            },
        };
        // Coalesce queued relative motion so a slow link never builds an input backlog.
        let ev = match ev {
            InputEvent::MouseMove { mut dx, mut dy } => {
                while let Ok(next) = rx.try_recv() {
                    match next {
                        InputEvent::MouseMove { dx: x, dy: y } => {
                            dx += x;
                            dy += y;
                        }
                        other => {
                            pending = Some(other);
                            break;
                        }
                    }
                }
                InputEvent::MouseMove { dx, dy }
            }
            e => e,
        };
        if net::send_msg(&mut s, &ev).await.is_err() {
            return;
        }
    }
}

async fn cursor_positions(conn:quinn::Connection,shared:Arc<Shared>) {
    while let Ok(bytes)=conn.read_datagram().await {
        let Ok(streetlight_proto::cursor::Datagram::Cursor(position))=control::decode(&bytes) else {continue};
        if position.epoch!=shared.cursor_epoch.load(Ordering::Relaxed) {continue;}
        let mut cursor=shared.cursor.lock().unwrap();
        if cursor.position.epoch==position.epoch && position.serial.wrapping_sub(cursor.position.serial) as i32 <=0 {continue;}
        cursor.position=position;
        shared.stats.lock().unwrap().cursor_positions+=1;
    }
}
async fn cursor_bitmaps(conn:quinn::Connection,shared:Arc<Shared>) {
    while let Ok(mut stream)=conn.accept_uni().await {
        while let Ok(Some(bitmap))=net::recv_msg::<streetlight_proto::cursor::Bitmap>(&mut stream).await {
            if bitmap.valid() && bitmap.epoch==shared.cursor_epoch.load(Ordering::Relaxed) {
                shared.cursor.lock().unwrap().bitmap=Some(Arc::new(bitmap));
                shared.stats.lock().unwrap().cursor_bitmaps+=1;
            }
        }
    }
}

async fn feedback_task(conn: quinn::Connection, shared: Arc<Shared>) {
    let mut t = tokio::time::interval(Duration::from_millis(500));
    loop {
        t.tick().await;
        let s = shared.stats.lock().unwrap().clone();
        let fb = Feedback {
            last_frame: s.last_frame,
            idr_requests: s.idr_requests as u32,
            frames_lost: s.frames_lost as u32,
            frames_recovered: s.frames_recovered as u32,
            decode_us_avg: shared.decode_us.load(Ordering::Relaxed),
            present_us_avg: shared.present_us.load(Ordering::Relaxed),
            rtt_us: shared.rtt_us.load(Ordering::Relaxed),
        };
        let body = postcard_body(&fb);
        if conn.send_datagram(body.into()).is_err() && conn.close_reason().is_some() {
            return;
        }
    }
}

fn postcard_body<T: serde::Serialize>(v: &T) -> Vec<u8> {
    control::encode(v)[4..].to_vec()
}

struct MediaReceiver {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for MediaReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl MediaReceiver {
    fn start(
        host: SocketAddr,
        info: &StreamInfo,
        mut sink: Box<dyn MediaSink>,
        cmd: mpsc::UnboundedSender<Cmd>,
        shared: Arc<Shared>,
    ) -> Result<Self> {
        let mut impairment = impairment::Impairment::from_env()?;
        *shared.stats.lock().unwrap() = Stats::default();
        *shared.latency.lock().unwrap() = latency::Window::default();
        shared.decode_us.store(0, Ordering::Relaxed);
        shared.present_us.store(0, Ordering::Relaxed);
        let sock = UdpSocket::bind(if host.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        })?;
        let _ = socket2::SockRef::from(&sock).set_recv_buffer_size(16 << 20);
        sock.set_read_timeout(Some(Duration::from_millis(1)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let (key, token) = (info.key, info.token);
        let stop2 = stop.clone();
        let thread = thread::Builder::new()
            .name("sl-media".into())
            .spawn(move || {
                let mut video = Reassembler::new(StreamKind::Video, MediaCipher::new(&key));
                let mut audio = Reassembler::new(StreamKind::Audio, MediaCipher::new(&key));
                let mut probe = [0u8; HEADER_LEN + 16];
                MediaHeader {
                    stream: StreamKind::Probe,
                    shard_index: 0,
                    frame_number: 0,
                    data_shards: 1,
                    parity_shards: 0,
                    frame_bytes: 16,
                    capture_us: 0,
                    shard_bytes: 16,
                    flags: 0,
                }
                .write(&mut probe);
                probe[HEADER_LEN..].copy_from_slice(&token);
                let mut last_probe = Instant::now() - Duration::from_secs(10);
                let mut buf = vec![0u8; 65536];
                let mut video_out = Vec::new();
                let mut audio_out = Vec::new();
                let mut packets = Vec::new();
                let mut last_video = None::<Instant>;
                let mut loss_since = None::<Instant>;
                let mut keyframes = 0u64;
                let mut delivered = 0u64;
                let mut last_frame = 0;
                let mut test_dropped = 0u64;
                let (mut max_gap, mut max_recovery) = (0f32, 0f32);
                // Loss handling: after a lost frame, wait for a keyframe (re-requesting periodically).
                let mut awaiting_key = true;
                let mut last_idr_req = Instant::now() - Duration::from_secs(10);
                let (mut win_frames, mut win_bytes, mut win_start) = (0u32, 0u64, Instant::now());
                let mut lat_avg = 0f32;
                while !stop2.load(Ordering::Relaxed) {
                    if last_probe.elapsed() > Duration::from_millis(500) {
                        let _ = sock.send_to(&probe, host);
                        last_probe = Instant::now();
                    }
                    if awaiting_key && last_idr_req.elapsed() > Duration::from_millis(250) {
                        let _ = cmd.send(Cmd::RequestIdr);
                        last_idr_req = Instant::now();
                    }
                    match sock.recv_from(&mut buf) {
                        Ok((n, from)) if from == host => {
                            win_bytes += n as u64;
                            if matches!(buf[..n].get(1), Some(0 | 1))
                                && impairment.push(&buf[..n], &mut packets)
                            {
                                test_dropped += 1;
                            }
                        }
                        Ok(_) => {}
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(e) => {
                            tracing::warn!("media receive: {e}");
                            break;
                        }
                    }
                    impairment.poll(&mut packets);
                    for mut packet in packets.drain(..) {
                        match packet.get(1).copied() {
                            Some(0) => video.push(&mut packet, &mut video_out),
                            Some(1) => audio.push(&mut packet, &mut audio_out),
                            _ => {}
                        }
                    }
                    video.poll(&mut video_out);
                    audio.poll(&mut audio_out);
                    for ev in video_out.drain(..) {
                        match ev {
                            Assembled::Frame {
                                data,
                                frame_number,
                                keyframe,
                                capture_us,
                                ..
                            } => {
                                if awaiting_key && !keyframe {
                                    continue;
                                }
                                if keyframe {
                                    keyframes += 1;
                                    if let Some(since) = loss_since.take() {
                                        let ms = since.elapsed().as_secs_f32() * 1000.0;
                                        max_recovery = max_recovery.max(ms);
                                        tracing::info!(
                                            frame_number,
                                            recovery_ms = ms,
                                            "video recovered at keyframe"
                                        );
                                    }
                                }
                                let now = Instant::now();
                                if let Some(last) = last_video.replace(now) {
                                    max_gap = max_gap
                                        .max(now.duration_since(last).as_secs_f32() * 1000.0);
                                }
                                last_frame = frame_number;
                                awaiting_key = false;
                                win_frames += 1;
                                delivered += 1;
                                let received_us = now_us();
                                let lat = received_us
                                    .saturating_sub(host_to_local(&shared, capture_us))
                                    as f32
                                    / 1000.0;
                                if lat < 1000.0 && shared.rtt_us.load(Ordering::Relaxed) > 0 {
                                    shared.latency.lock().unwrap().received((lat * 1000.0) as u64);
                                    lat_avg = if lat_avg == 0.0 {
                                        lat
                                    } else {
                                        lat_avg * 0.9 + lat * 0.1
                                    };
                                }
                                sink.video(VideoFrame {
                                    data,
                                    frame_number,
                                    keyframe,
                                    capture_us,
                                    received_us,
                                });
                            }
                            Assembled::Lost { first, last } => {
                                tracing::info!(first, last, "lost video frames");
                                loss_since.get_or_insert_with(Instant::now);
                                awaiting_key = true;
                                last_idr_req = Instant::now() - Duration::from_secs(10);
                            }
                        }
                    }
                    for ev in audio_out.drain(..) {
                        match ev {
                            Assembled::Frame { data, .. } => sink.audio(&data),
                            Assembled::Lost { first, last } => {
                                // Bound concealment after a long outage to avoid a playback backlog.
                                for _ in 0..last.wrapping_sub(first).saturating_add(1).min(100) {
                                    sink.audio_lost();
                                }
                            }
                        }
                    }
                    // Update totals on every iteration, including when the socket is idle.
                    {
                        let mut s = shared.stats.lock().unwrap();
                        s.last_frame = last_frame;
                        s.frames = video.stats.frames;
                        s.frames_lost = video.stats.frames_lost;
                        s.frames_recovered = video.stats.frames_recovered;
                        s.keyframes = keyframes;
                        s.audio_packets_lost = audio.stats.frames_lost;
                        s.test_packets_dropped = test_dropped;
                        s.video_frames_delivered = delivered;
                        s.awaiting_keyframe = awaiting_key;
                        s.max_video_gap_ms = last_video.map_or(max_gap, |t| max_gap.max(t.elapsed().as_secs_f32() * 1000.0));
                        s.max_recovery_ms = loss_since.map_or(max_recovery, |t| max_recovery.max(t.elapsed().as_secs_f32() * 1000.0));
                        s.packets = video.stats.packets + audio.stats.packets;
                        s.bad_packets = video.stats.bad_packets + audio.stats.bad_packets;
                    }
                    if win_start.elapsed() >= Duration::from_secs(1) {
                        let secs = win_start.elapsed().as_secs_f32();
                        let mut s = shared.stats.lock().unwrap();
                        let vs = video.stats;
                        s.frames = vs.frames;
                        s.frames_lost = vs.frames_lost;
                        s.frames_recovered = vs.frames_recovered;
                        s.packets = vs.packets + audio.stats.packets;
                        s.bad_packets = vs.bad_packets + audio.stats.bad_packets;
                        s.fps = win_frames as f32 / secs;
                        s.mbps = win_bytes as f32 * 8.0 / secs / 1e6;
                        s.network_latency_ms = lat_avg;
                        (win_frames, win_bytes, win_start) = (0, 0, Instant::now());
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

#[cfg(test)]
mod media_tests {
    use super::*;
    use streetlight_proto::{
        fec::{FecConfig, Packetizer},
        media::flags,
    };

    struct Sink(Arc<Mutex<Vec<(bool, Vec<u8>)>>>);
    impl MediaSink for Sink {
        fn audio_lost(&mut self) { self.audio(&[]); }
        fn video(&mut self, f: VideoFrame) {
            self.0.lock().unwrap().push((true, f.data));
        }
        fn audio(&mut self, p: &[u8]) {
            self.0.lock().unwrap().push((false, p.to_vec()));
        }
    }

    #[test]
    fn udp_loss_requests_idr_skips_dependent_frames_and_conceals_audio() {
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let info = StreamInfo {
            codec: Codec::Hevc,
            color: ColorSpace::Sdr709,
            width: 1920,
            height: 1080,
            fps: 60,
            media_port: host.local_addr().unwrap().port(),
            key: [7; 16],
            token: [8; 16],
            cursor_metadata: false, audio_channels: 2,
            audio_sample_rate: 48_000,
        };
        let shared = Arc::new(Shared {
            cursor: Mutex::new(Default::default()),
            cursor_epoch: AtomicU64::new(0),
            latency: Mutex::new(latency::Window::default()),
            stats: Mutex::new(Stats::default()),
            clock_offset_us: AtomicI64::new(0),
            rtt_us: AtomicU32::new(0),
            decode_us: AtomicU32::new(0),
            present_us: AtomicU32::new(0),
            stopped: Mutex::new(None),
        });
        let (tx, mut cmds) = mpsc::unbounded_channel();
        let received = Arc::new(Mutex::new(Vec::new()));
        let receiver = MediaReceiver::start(
            host.local_addr().unwrap(),
            &info,
            Box::new(Sink(received.clone())),
            tx,
            shared.clone(),
        )
        .unwrap();
        let (_, peer) = host.recv_from(&mut [0; 1024]).unwrap();
        let mut p = Packetizer::new(
            FecConfig {
                percent: 0,
                ..Default::default()
            },
            MediaCipher::new(&info.key),
        );
        let mut send = |kind, n, key, bytes: &[u8]| {
            for packet in p
                .packetize(kind, n, 0, if key { flags::KEYFRAME } else { 0 }, bytes)
                .packets()
            {
                host.send_to(packet, peer).unwrap();
            }
        };
        send(StreamKind::Video, 0, true, b"key0");
        send(StreamKind::Audio, 0, false, b"audio0");
        send(StreamKind::Video, 2, false, b"dependent2");
        send(StreamKind::Audio, 2, false, b"audio2");
        let deadline = Instant::now() + Duration::from_secs(2);
        while shared.stats.lock().unwrap().frames_lost == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(shared.stats.lock().unwrap().frames_lost, 1);
        // Startup request plus a prompt loss-triggered request.
        let mut requests = 0;
        while Instant::now() < deadline && requests < 2 {
            if let Ok(Cmd::RequestIdr) = cmds.try_recv() {
                requests += 1;
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(requests, 2);
        send(StreamKind::Video, 3, true, b"key3");
        send(StreamKind::Video, 4, false, b"dependent4");
        while received.lock().unwrap().len() < 6 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        drop(receiver);
        let events = received.lock().unwrap();
        let video: Vec<_> = events
            .iter()
            .filter(|(v, _)| *v)
            .map(|(_, b)| b.as_slice())
            .collect();
        let audio: Vec<_> = events
            .iter()
            .filter(|(v, _)| !*v)
            .map(|(_, b)| b.as_slice())
            .collect();
        assert_eq!(video, vec![b"key0".as_slice(), b"key3", b"dependent4"]);
        assert_eq!(audio, vec![b"audio0".as_slice(), b"", b"audio2"]);
        assert_eq!(shared.stats.lock().unwrap().audio_packets_lost, 1);
    }
}
