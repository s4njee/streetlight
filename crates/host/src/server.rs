//! QUIC control server: pairing, session lifecycle, input and feedback.

use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use streetlight_proto::control::{self, AppInfo, ClientMsg, Codec, ColorSpace, Feedback, HostMsg, InputEvent, StreamInfo, StreamRequest};
use streetlight_proto::fec::FecConfig;
use streetlight_proto::net::{self, Identity};
use streetlight_proto::pair::{self, PairSide};
use tokio::sync::{oneshot, watch};

use crate::capture::virtual_output::{CursorMode, Source};
use crate::config::{PairedClient, Settings, Trust};
use crate::encode::EncoderConfig;
use crate::media::{MediaMsg, MediaServer, SessionKeys};
use crate::video::{VideoConfig, VideoPipeline};
use crate::{adaptive, audio, input};

pub struct PendingPair {
    pub client_name: String,
    pub fingerprint: String,
    pin_tx: Option<oneshot::Sender<String>>,
    owner: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Status {
    pub streaming: bool,
    pub client: Option<String>,
    /// Exact virtual-output name (without KWin’s Virtual- prefix), for diagnostics.
    pub output_name: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Option<String>,
    pub bitrate_kbps: u32,
    pub last_feedback: Option<FeedbackView>,
    pub app_id: Option<String>,
    pub app_pid: Option<u32>,
    pub app_running: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FeedbackView {
    pub last_frame: u32,
    pub idr_requests: u32,
    pub frames_lost: u32,
    pub frames_recovered: u32,
    pub decode_us_avg: u32,
    pub present_us_avg: u32,
    pub rtt_us: u32,
}

pub struct AppState {
    pub identity: Identity,
    pub settings: Mutex<Settings>,
    pub trust: Mutex<Trust>,
    pub pending: Mutex<Option<PendingPair>>,
    pub status: Mutex<Status>,
    pub media: MediaServer,
    session: Mutex<Option<Session>>,
    session_gate: Mutex<()>,
    pair_attempts: Mutex<HashMap<IpAddr, Instant>>,
    session_owner: Mutex<u64>,
    pub status_tx: watch::Sender<u64>,
    pub output_target: input::SharedTarget,
}

struct Session {
    app: Option<crate::app::RunningApp>,
    video: VideoPipeline,
    _audio: Option<audio::AudioCapture>,
    adaptive: adaptive::Controller,
    adapt_enabled: bool,
    cursor_metadata: bool,
    cursor_epoch: u64,
}

impl AppState {
    pub fn new(identity: Identity, settings: Settings, media: MediaServer) -> Arc<Self> {
        Self::with_trust(identity, settings, media, Trust::load())
    }

    fn with_trust(identity: Identity, settings: Settings, media: MediaServer, trust: Trust) -> Arc<Self> {
        Arc::new(Self {
            identity,
            settings: Mutex::new(settings),
            trust: Mutex::new(trust),
            pending: Mutex::new(None),
            status: Mutex::new(Status::default()),
            media,
            session: Mutex::new(None),
            session_gate: Mutex::new(()),
            pair_attempts: Mutex::new(HashMap::new()),
            session_owner: Mutex::new(0),
            status_tx: watch::channel(0).0,
            output_target: Default::default(),
        })
    }

    pub fn notify(&self) {
        self.status_tx.send_modify(|v| *v += 1);
    }

    /// Called by the web UI with the PIN the user typed.
    pub fn submit_pin(&self, pin: String) -> bool {
        let mut p = self.pending.lock().unwrap();
        match p.as_mut().and_then(|p| p.pin_tx.take()) {
            Some(tx) => tx.send(pin).is_ok(),
            None => false,
        }
    }

    fn stop_session(&self, owner: Option<u64>) {
        let _gate = self.session_gate.lock().unwrap();
        self.stop_session_locked(owner);
    }

    fn stop_session_locked(&self, owner: Option<u64>) {
        let mut o = self.session_owner.lock().unwrap();
        if owner.is_some_and(|id| id != *o) {
            return;
        }
        *o = 0;
        let s = self.session.lock().unwrap().take();
        if s.is_some() {
            self.media.end();
            drop(s);
            *self.status.lock().unwrap() = Status::default();
            self.notify();
            tracing::info!("session stopped");
        }
    }
}

pub async fn run(state: Arc<AppState>, port: u16) -> Result<()> {
    let cfg = net::server_config(&state.identity)?;
    let endpoint = match quinn::Endpoint::server(cfg.clone(), (std::net::Ipv6Addr::UNSPECIFIED, port).into()) {
        Ok(e) => e,
        Err(_) => quinn::Endpoint::server(cfg, (std::net::Ipv4Addr::UNSPECIFIED, port).into()).context("bind QUIC endpoint")?,
    };
    tracing::info!("control server on UDP {port}, host fingerprint {}", pair::fingerprint_hex(&state.identity.fingerprint()));
    let mut next_id = 1u64;
    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        let id = next_id;
        next_id += 1;
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = handle(state.clone(), conn, id).await {
                        tracing::info!("{remote}: {e:#}");
                    }
                    state.stop_session(Some(id));
                }
                Err(e) => tracing::debug!("{remote}: handshake failed: {e}"),
            }
        });
    }
    Ok(())
}

async fn handle(state: Arc<AppState>, conn: quinn::Connection, conn_id: u64) -> Result<()> {
    let remote = conn.remote_address();
    let fp = net::peer_fingerprint(&conn).context("client presented no certificate")?;
    let fp_hex = pair::fingerprint_hex(&fp);
    let (mut tx, mut rx) = conn.accept_bi().await?;

    let Some(ClientMsg::Hello { name, caps }) = net::recv_msg(&mut rx).await? else { bail!("expected Hello") };
    let mut paired = state.trust.lock().unwrap().is_trusted(&fp_hex);
    tracing::info!("{remote}: '{name}' connected (paired: {paired}) caps {caps:?}");
    let host_name = state.settings.lock().unwrap().host_name.clone();
    net::send_msg(&mut tx, &HostMsg::Welcome { host_name, paired, host_version: env!("CARGO_PKG_VERSION").into() }).await?;

    // Input + feedback run for the lifetime of the connection once paired.
    let mut aux_started = false;

    loop {
        if paired && !aux_started {
            aux_started = true;
            tokio::spawn(input_task(conn.clone(), state.clone(), conn_id));
            tokio::spawn(feedback_task(conn.clone(), state.clone(), conn_id));
            tokio::spawn(cursor_task(conn.clone(),state.clone(),conn_id));
        }
        let Some(msg) = net::recv_msg::<ClientMsg>(&mut rx).await? else { return Ok(()) };
        match msg {
            ClientMsg::Ping { client_us } => {
                net::send_msg(&mut tx, &HostMsg::Pong { client_us, host_us: streetlight_proto::now_us() }).await?;
            }
            ClientMsg::PairStart { client_name, spake } if !paired => {
                paired = pair_flow(&state, &conn, &mut tx, &mut rx, PairRequest { owner: conn_id, client_name, spake, fingerprint: &fp, fingerprint_hex: &fp_hex }).await?;
            }
            _ if !paired => {
                net::send_msg(&mut tx, &HostMsg::Error("not paired".into())).await?;
            }
            ClientMsg::ListApps => {
                let apps = state.settings.lock().unwrap().apps.iter().map(|a| AppInfo { id: a.id.clone(), name: a.name.clone() }).collect();
                net::send_msg(&mut tx, &HostMsg::Apps(apps)).await?;
            }
            ClientMsg::Start(req) => {
                let st = state.clone();
                let client = name.clone();
                let fingerprint = fp_hex.clone();
                let caps = caps.clone();
                let r = tokio::task::spawn_blocking(move || start_session(&st, req, conn_id, client, &fingerprint, &caps)).await?;
                match r {
                    Ok(info) => net::send_msg(&mut tx, &HostMsg::StreamStarted(info)).await?,
                    Err(e) => {
                        tracing::error!("start failed: {e:#}");
                        net::send_msg(&mut tx, &HostMsg::Error(format!("{e:#}"))).await?
                    }
                }
            }
            ClientMsg::Stop => {
                let st = state.clone();
                tokio::task::spawn_blocking(move || st.stop_session(Some(conn_id))).await?;
                net::send_msg(&mut tx, &HostMsg::StreamStopped { reason: "client request".into() }).await?;
            }
            ClientMsg::RequestIdr => {
                let owner = state.session_owner.lock().unwrap();
                if *owner != conn_id { continue; }
                if let Some(s) = state.session.lock().unwrap().as_ref() {
                    state.media.record_idr_request();
                    s.video.request_idr();
                }
            }
            ClientMsg::SetBitrate { kbps } => {
                let owner = state.session_owner.lock().unwrap();
                if *owner == conn_id && let Some(s) = state.session.lock().unwrap().as_mut() {
                    let target = s.adaptive.set(kbps, Instant::now());
                    s.video.set_bitrate(target);
                }
            },
            ClientMsg::Hello { .. } | ClientMsg::PairStart { .. } | ClientMsg::PairConfirm { .. } => {}
        }
    }
}

fn reserve_pair(
    pending: &mut Option<PendingPair>, recent: &mut HashMap<IpAddr, Instant>, now: Instant,
    ip: IpAddr, owner: u64, client_name: String, fingerprint: String,
) -> Result<oneshot::Receiver<String>, &'static str> {
    if pending.is_some() { return Err("pairing in progress"); }
    recent.retain(|_, last| now.saturating_duration_since(*last) < Duration::from_secs(5));
    if recent.contains_key(&ip) { return Err("pairing rate limited; retry in 5 seconds"); }
    recent.insert(ip, now);
    let (pin_tx, pin_rx) = oneshot::channel();
    *pending = Some(PendingPair { client_name, fingerprint, pin_tx: Some(pin_tx), owner });
    Ok(pin_rx)
}

struct PendingGuard<'a> { state: &'a AppState, owner: u64 }
impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        let mut pending = self.state.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|p| p.owner == self.owner) {
            *pending = None;
            self.state.notify();
        }
    }
}

struct PairRequest<'a> {
    owner: u64,
    client_name: String,
    spake: Vec<u8>,
    fingerprint: &'a [u8; 32],
    fingerprint_hex: &'a str,
}

async fn pair_flow(
    state: &AppState,
    conn: &quinn::Connection,
    tx: &mut quinn::SendStream,
    rx: &mut quinn::RecvStream,
    request: PairRequest<'_>,
) -> Result<bool> {
    let PairRequest { owner, client_name, spake: spake_a, fingerprint: fp, fingerprint_hex: fp_hex } = request;
    let reservation = {
        let mut pending = state.pending.lock().unwrap();
        let mut recent = state.pair_attempts.lock().unwrap();
        reserve_pair(&mut pending, &mut recent, Instant::now(), conn.remote_address().ip().to_canonical(),
            owner, client_name.clone(), fp_hex.into())
    };
    let pin_rx = match reservation {
        Ok(rx) => rx,
        Err(reason) => { net::send_msg(tx, &HostMsg::Error(reason.into())).await?; return Ok(false); }
    };
    let _pending_guard = PendingGuard { state, owner };
    state.notify();
    tracing::warn!("pairing request from '{client_name}': enter the PIN shown on the client at http://127.0.0.1:{}/ or run `streetlight-host pair <PIN>`", control::WEB_PORT);
    net::send_msg(tx, &HostMsg::PairAwaitingPin).await?;

    let pin = tokio::select! {
        pin = tokio::time::timeout(Duration::from_secs(300), pin_rx) => pin,
        _ = conn.closed() => return Ok(false),
    };
    let Ok(Ok(pin)) = pin else {
        net::send_msg(tx, &HostMsg::PairResult { ok: false, mac: vec![] }).await?;
        return Ok(false);
    };
    let (side, spake_b) = PairSide::host(&pin);
    net::send_msg(tx, &HostMsg::PairSpake { spake: spake_b }).await?;
    let key = side.finish(&spake_a);
    let Some(ClientMsg::PairConfirm { mac }) = tokio::time::timeout(Duration::from_secs(10), net::recv_msg(rx)).await.context("pair confirmation timed out")?? else { bail!("expected PairConfirm") };
    let host_fp = state.identity.fingerprint();
    let ok = key.as_ref().is_some_and(|k| k.verify(pair::CLIENT_ID, fp, &host_fp, &mac));
    let host_mac = match (&key, ok) {
        (Some(k), true) => k.mac(pair::HOST_ID, fp, &host_fp),
        _ => vec![],
    };
    if ok {
        let mut t = state.trust.lock().unwrap();
        t.clients.retain(|c| c.fingerprint != fp_hex);
        t.clients.push(PairedClient {
            name: client_name.clone(),
            fingerprint: fp_hex.into(),
            paired_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        });
        t.save()?;
        tracing::info!("paired with '{client_name}'");
    } else {
        tracing::warn!("pairing with '{client_name}' failed (wrong PIN?)");
    }
    net::send_msg(tx, &HostMsg::PairResult { ok, mac: host_mac }).await?;
    Ok(ok)
}

fn negotiate_codec(requested: Codec, client: &[Codec], host: &[Codec]) -> Result<Codec> {
    let candidates = if requested == Codec::Auto { vec![Codec::Av1, Codec::Hevc, Codec::H264] } else { vec![requested] };
    candidates.into_iter().find(|codec| client.contains(codec) && host.contains(codec))
        .context("no shared hardware codec (requested codec must be supported by both peers)")
}

fn start_session(state: &AppState, req: StreamRequest, owner: u64, client: String, fingerprint: &str, caps: &control::ClientCaps) -> Result<StreamInfo> {
    let _gate = state.session_gate.lock().unwrap();
    let active_owner = *state.session_owner.lock().unwrap();
    anyhow::ensure!(active_owner == 0 || active_owner == owner, "host busy");
    let settings = state.settings.lock().unwrap().clone();
    let mut req = req;
    anyhow::ensure!(req.color != ColorSpace::Hdr10 || caps.hdr, "client does not support HDR");
    if req.color==ColorSpace::Hdr10 {
        settings.hdr_metadata.as_ref().context("HDR requires configured mastering metadata")?.validate()?;
    }
    let supported = crate::encode::Encoder::supported_codecs(&settings.render_node, req.color == ColorSpace::Hdr10)?;
    req.codec = negotiate_codec(req.codec, &caps.codecs, &supported)?;
    anyhow::ensure!(req.app.as_ref().is_none_or(|id| settings.apps.iter().any(|a| &a.id == id)), "unknown app");
    let d = &req.display;
    anyhow::ensure!(d.width >= 64 && d.height >= 64 && d.width <= 8192 && d.height <= 8192, "bad size");
    let (w, h) = (d.width & !1, d.height & !1);
    let fps = d.fps.clamp(1, 240);
    let bitrate = req.bitrate_kbps.clamp(1000, settings.max_bitrate_kbps.max(1000));
    let ten_bit = req.color == ColorSpace::Hdr10;

    state.stop_session_locked(Some(owner));
    let key = streetlight_proto::crypto::MediaCipher::random_key();
    let mut token = [0u8; 16];
    rand::fill(&mut token);
    let fec = FecConfig {
        max_shard_bytes: req.max_shard_bytes.clamp(256, 1400) & !1,
        percent: req.fec_percent.min(100),
        min_parity: if req.fec_percent > 0 { 2 } else { 0 },
    };
    state.media.begin(SessionKeys { key, token, fec, pace_mbps: req.pace_mbps });
    struct StartupMedia<'a>(&'a MediaServer,bool);
    impl Drop for StartupMedia<'_> {fn drop(&mut self){if !self.1 {self.0.end();}}}
    let mut startup_media=StartupMedia(&state.media,false);

    let scale = if d.scale > 0.5 { d.scale as f64 } else { 1.0 };
    let display_name: String = client.chars().filter(|c| c.is_ascii_alphanumeric()).take(40).collect();
    let display_name = if display_name.is_empty() { "client" } else { &display_name };
    let vname = format!("streetlight-{}-{display_name}", &fingerprint[..12]);
    let kwin_output = if d.virtual_display { format!("Virtual-{vname}") } else { String::new() };
    let source = if d.virtual_display {
        Source::Virtual {
            name: vname.clone(),
            width: (w as f64 / scale).round() as u32,
            height: (h as f64 / scale).round() as u32,
            scale,
        }
    } else {
        Source::Output { name: None }
    };
    let cursor_metadata = settings.embed_cursor && caps.cursor_metadata;
    let cfg = VideoConfig {
        source,
        cursor: if cursor_metadata { CursorMode::Metadata } else if settings.embed_cursor { CursorMode::Embedded } else { CursorMode::Hidden },
        encoder: EncoderConfig {
            codec: req.codec,
            width: w,
            height: h,
            fps,
            bitrate_kbps: bitrate,
            ten_bit,
            hdr_metadata: if ten_bit {settings.hdr_metadata.clone()} else {None},
            color: req.color,
            render_node: settings.render_node.clone(),
            rc_mode: settings.rc_mode.clone(),
        },
        force_idr: Default::default(),
    };
    let media = state.media.clone();
    let idr = cfg.force_idr.clone();
    let video = VideoPipeline::start(cfg, move |f| {
        let ok = media.send(MediaMsg::Video { data: f.data, keyframe: f.keyframe, capture_us: f.capture_us as u32 });
        if !ok {
            // The client can't see this gap, so restart the reference chain ourselves.
            tracing::debug!("media queue full, dropped frame; forcing IDR");
            idr.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })?;

    let media = state.media.clone();
    let audio = match audio::AudioCapture::start(move |data, ts| {
        media.send(MediaMsg::Audio { data, capture_us: ts as u32 });
    }) {
        Ok(a) => Some(a),
        Err(e) => {
            tracing::warn!("audio disabled: {e:#}");
            None
        }
    };

    let app = req.app.as_ref().and_then(|id| settings.apps.iter().find(|a| &a.id == id));
    let app = match app {
        Some(app) => crate::app::RunningApp::launch(app, &kwin_output, w, h)?,
        None => None,
    };
    let app_id = app.as_ref().map(|a| a.id.clone());
    let app_pid = app.as_ref().map(|a| a.pid);

    // Absolute input needs the new output's place in the desktop layout; it appears asynchronously.
    {
        let target = state.output_target.clone();
        let gen_ = { let mut t = target.lock().unwrap(); t.generation += 1; t.name = kwin_output.clone(); t.map = None; t.generation };
        // Where the output should sit: its logical size at the client's scale
        let logical = ((w as f64 / scale).round() as i32, (h as f64 / scale).round() as i32);
        std::thread::spawn(move || {
            if kwin_output.is_empty() {
                return;
            }
            let map = crate::capture::layout::wait_for(&kwin_output, std::time::Duration::from_secs(3));
            if map.is_none() {
                tracing::warn!("output {kwin_output} not found in layout; absolute input unmapped");
            }
            let mut mine = {
                let mut t = target.lock().unwrap();
                if t.generation != gen_ {
                    return;
                }
                t.map = map;
                t.generation += 1;
                t.generation
            };
            follow_layout(&target, &kwin_output, logical, scale, map, &mut mine);
        });
    }
    *state.session.lock().unwrap() = Some(Session { app, video, _audio: audio, adaptive: adaptive::Controller::new(bitrate, Instant::now()), adapt_enabled: settings.adaptive_bitrate, cursor_metadata, cursor_epoch:u64::from_le_bytes(token[..8].try_into().unwrap()) });
    *state.session_owner.lock().unwrap() = owner;
    *state.status.lock().unwrap() = Status {
        streaming: true,
        client: Some(client),
        output_name: d.virtual_display.then_some(vname),
        width: w,
        height: h,
        fps,
        codec: Some(format!("{:?}", req.codec)),
        bitrate_kbps: bitrate,
        last_feedback: None,
        app_running: app_pid.is_some(), app_id, app_pid,
    };
    startup_media.1=true;
    state.notify();
    tracing::info!("streaming {w}x{h}@{fps} {:?} {} kbps", req.codec, bitrate);
    Ok(StreamInfo {
        codec: req.codec,
        color: req.color,
        width: w,
        height: h,
        fps,
        media_port: state.media.port,
        key,
        token,
        cursor_metadata,
        audio_channels: audio::CHANNELS as u8,
        audio_sample_rate: audio::SAMPLE_RATE,
    })
}

async fn input_task(conn: quinn::Connection, state: Arc<AppState>, owner: u64) {
    let injector: Arc<Mutex<Option<input::Injector>>> = Arc::new(Mutex::new(None));
    let mut changes = state.status_tx.subscribe();
    loop {
        tokio::select! {
            _ = changes.changed() => {
                if *state.session_owner.lock().unwrap() != owner { injector.lock().unwrap().take(); }
            }
            stream = conn.accept_uni() => {
                let Ok(mut rx) = stream else { return };
                let (inj, state) = (injector.clone(), state.clone());
                tokio::spawn(async move {
                    while let Ok(Some(ev)) = net::recv_msg::<InputEvent>(&mut rx).await {
                        let current = state.session_owner.lock().unwrap();
                        let mut inj = inj.lock().unwrap();
                        if *current != owner { inj.take(); continue; }
                        if inj.is_none() {
                            match input::Injector::new(state.output_target.clone()) {
                                Ok(i) => *inj = Some(i),
                                Err(e) => { tracing::error!("input disabled: {e:#}"); return; }
                            }
                        }
                        if let Err(e) = inj.as_mut().unwrap().inject(&ev) { tracing::debug!("inject {ev:?}: {e}"); }
                    }
                });
            }
        }
    }
}

async fn cursor_task(conn:quinn::Connection,state:Arc<AppState>,owner:u64) {
    let mut tick=tokio::time::interval(Duration::from_millis(4));
    let mut stream=None;let mut previous=None;let mut shape=None;let mut last_epoch=0;let mut last_sent=Instant::now();
    loop {
        tokio::select! {_=conn.closed()=>return,_=tick.tick()=>{}}
        let (mut snapshot,epoch)={
            let current=state.session_owner.lock().unwrap();
            let session=state.session.lock().unwrap();
            if *current!=owner {previous=None;shape=None;continue;}
            let Some(session)=session.as_ref() else {continue};
            if !session.cursor_metadata {continue;}
            (session.video.cursor.lock().unwrap().clone(),session.cursor_epoch)
        };
        if epoch!=last_epoch {previous=None;shape=None;last_epoch=epoch;}
        snapshot.position.epoch=epoch;
        if let Some(bitmap)=snapshot.bitmap.as_ref() && shape!=Some(bitmap.id) {
            if stream.is_none() { match conn.open_uni().await {Ok(s)=>stream=Some(s),Err(_)=>return} }
            let mut bitmap=bitmap.as_ref().clone();bitmap.epoch=epoch;
            if net::send_msg(stream.as_mut().unwrap(),&bitmap).await.is_err(){return;}
            shape=Some(bitmap.id);
        }
        if previous!=Some(snapshot.position.serial) || last_sent.elapsed()>=Duration::from_millis(250) {
            let datagram=streetlight_proto::cursor::Datagram::Cursor(snapshot.position.clone());
            let bytes=control::encode(&datagram);
            if conn.send_datagram(bytes[4..].to_vec().into()).is_ok() {
                previous=Some(snapshot.position.serial);last_sent=Instant::now();
            }
        }
    }
}

async fn feedback_task(conn: quinn::Connection, state: Arc<AppState>, owner: u64) {
    while let Ok(d) = conn.read_datagram().await {
        if let Ok(fb) = control::decode::<Feedback>(&d) {
            let current = state.session_owner.lock().unwrap();
            if *current != owner { continue; }
            let mut session = state.session.lock().unwrap();
            if let Some(session) = session.as_mut() {
                state.status.lock().unwrap().app_running = session.app.as_mut().is_some_and(|a| a.running());
                if session.adapt_enabled && let Some(kbps) = session.adaptive.observe(fb.clone(), Instant::now()) {
                    tracing::info!("adaptive bitrate target: {kbps} kbps");
                    session.video.set_bitrate(kbps);
                }
                state.status.lock().unwrap().bitrate_kbps = session.video.bitrate.load(std::sync::atomic::Ordering::Relaxed);
            }
            let mut s = state.status.lock().unwrap();
            s.last_feedback = Some(FeedbackView {
                last_frame: fb.last_frame,
                idr_requests: fb.idr_requests,
                frames_lost: fb.frames_lost,
                frames_recovered: fb.frames_recovered,
                decode_us_avg: fb.decode_us_avg,
                present_us_avg: fb.present_us_avg,
                rtt_us: fb.rtt_us,
            });
        }
    }
}

#[allow(dead_code)]
fn _codec_names(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
        Codec::Auto => "Auto",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> Identity {
        let cert = rcgen::generate_simple_self_signed(vec!["streetlight".into()]).unwrap();
        Identity { cert: cert.cert.der().clone(), key: cert.signing_key.serialize_der().into() }
    }

    async fn connect(state: Arc<AppState>, id: u64, trusted: bool) -> (quinn::Endpoint, quinn::Endpoint, quinn::Connection, quinn::SendStream, quinn::RecvStream) {
        let client_id = identity();
        if trusted {
            state.trust.lock().unwrap().clients.push(PairedClient { name: "test".into(), fingerprint: pair::fingerprint_hex(&client_id.fingerprint()), paired_at: 0 });
        }
        let server = quinn::Endpoint::server(net::server_config(&state.identity).unwrap(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(net::client_config(&client_id).unwrap());
        let ep = server.clone();
        tokio::spawn(async move {
            let conn = ep.accept().await.unwrap().await.unwrap();
            let _ = handle(state, conn, id).await;
        });
        let conn = client.connect(server.local_addr().unwrap(), "streetlight").unwrap().await.unwrap();
        let (mut tx, mut rx) = conn.open_bi().await.unwrap();
        net::send_msg(&mut tx, &ClientMsg::Hello { name: "test".into(), caps: streetlight_proto::control::ClientCaps {
            codecs: vec![Codec::Hevc], hdr: false, max_width: 1920, max_height: 1080, max_fps: 60, cursor_metadata: false, audio_channels: 2,
        }}).await.unwrap();
        assert!(matches!(net::recv_msg::<HostMsg>(&mut rx).await.unwrap(), Some(HostMsg::Welcome { paired, .. }) if paired == trusted));
        (server, client, conn, tx, rx)
    }

    #[test]
    fn codec_selection_respects_both_peers_and_explicit_preferences() {
        assert_eq!(negotiate_codec(Codec::Auto, &[Codec::Hevc], &[Codec::Av1,Codec::Hevc]).unwrap(),Codec::Hevc);
        assert_eq!(negotiate_codec(Codec::Auto, &[Codec::Av1,Codec::Hevc], &[Codec::Av1,Codec::Hevc]).unwrap(),Codec::Av1);
        assert!(negotiate_codec(Codec::Av1, &[Codec::Hevc], &[Codec::Av1,Codec::Hevc]).is_err());
        assert!(negotiate_codec(Codec::Auto, &[Codec::Hevc], &[Codec::H264]).is_err());
    }

    #[tokio::test]
    async fn concurrent_pairing_rejection_disconnect_cleanup_and_ip_limit_over_quic() {
        let state = AppState::with_trust(identity(), Settings::default(), MediaServer::bind(0).unwrap(), Trust::default());
        let (_s1, _e1, c1, mut tx1, mut rx1) = connect(state.clone(), 1, false).await;
        let (_s2, _e2, c2, mut tx2, mut rx2) = connect(state.clone(), 2, false).await;
        let message = ClientMsg::PairStart { client_name: "pair-test".into(), spake: vec![] };
        net::send_msg(&mut tx1, &message).await.unwrap();
        assert!(matches!(net::recv_msg::<HostMsg>(&mut rx1).await.unwrap(), Some(HostMsg::PairAwaitingPin)));
        net::send_msg(&mut tx2, &message).await.unwrap();
        assert!(matches!(net::recv_msg::<HostMsg>(&mut rx2).await.unwrap(), Some(HostMsg::Error(e)) if e == "pairing in progress"));
        assert_eq!(state.pending.lock().unwrap().as_ref().unwrap().owner, 1);
        c1.close(0u32.into(), b"test disconnect");
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.pending.lock().unwrap().is_some() { tokio::time::sleep(Duration::from_millis(1)).await; }
        }).await.unwrap();
        net::send_msg(&mut tx2, &message).await.unwrap();
        assert!(matches!(net::recv_msg::<HostMsg>(&mut rx2).await.unwrap(), Some(HostMsg::Error(e)) if e.contains("rate limited")));
        c2.close(0u32.into(), b"done");
    }

    #[tokio::test]
    async fn competing_start_is_refused_before_touching_capture_or_existing_owner() {
        let state = AppState::with_trust(identity(), Settings::default(), MediaServer::bind(0).unwrap(), Trust::default());
        *state.session_owner.lock().unwrap() = 99;
        let (_s, _e, conn, mut tx, mut rx) = connect(state.clone(), 2, true).await;
        let request = StreamRequest { display: streetlight_proto::control::DisplayRequest { width: 1920, height: 1080, fps: 60, scale: 1.0, virtual_display: true },
            codec: Codec::Hevc, color: ColorSpace::Sdr709, bitrate_kbps: 30_000, fec_percent: 20, max_shard_bytes: 1400, pace_mbps: 0, app: None };
        net::send_msg(&mut tx, &ClientMsg::Start(request)).await.unwrap();
        assert!(matches!(net::recv_msg::<HostMsg>(&mut rx).await.unwrap(), Some(HostMsg::Error(e)) if e == "host busy"));
        assert_eq!(*state.session_owner.lock().unwrap(), 99);
        conn.close(0u32.into(), b"done");
    }

    #[test]
    fn pending_pairing_is_not_overwritten_and_attempts_are_rate_limited() {
        let mut pending = None;
        let mut recent = HashMap::new();
        let ip = "127.0.0.1".parse().unwrap();
        let now = Instant::now();
        let mut first = reserve_pair(&mut pending, &mut recent, now, ip, 1, "first".into(), "first-fp".into()).unwrap();
        assert_eq!(reserve_pair(&mut pending, &mut recent, now, "127.0.0.2".parse().unwrap(), 2, "second".into(), "second-fp".into()).unwrap_err(), "pairing in progress");
        let original = pending.take().unwrap();
        assert_eq!(original.owner, 1);
        original.pin_tx.unwrap().send("1234".into()).unwrap();
        assert_eq!(first.try_recv().unwrap(), "1234");
        assert!(reserve_pair(&mut pending, &mut recent, now + Duration::from_secs(1), ip, 3, "retry".into(), "fp".into()).is_err());
        assert!(pending.is_none());
        assert!(reserve_pair(&mut pending, &mut recent, now + Duration::from_secs(5), ip, 4, "retry".into(), "fp".into()).is_ok());
    }
}


/// Keeps absolute input mapped while `output` is this stream's target, and
/// the output beside the others at `logical` size and `scale`
/// (`layout::arrange`). The new output can change after it first appears:
/// KScreen applies its own configuration to an output it hasn't seen, and
/// set an iPad's 2732×2048 scale-2 output (1366×1024 logical) back to scale
/// 1 (2732×2048 logical) a moment later, so a map made at first sight sent
/// touches to half their distance from the top-left. Re-reads the layout
/// every second; stops when another stream takes the target or the output
/// has been gone a while.
fn follow_layout(
    target: &input::SharedTarget,
    output: &str,
    logical: (i32, i32),
    scale: f64,
    mut last: Option<crate::capture::layout::AbsMap>,
    mine: &mut u64,
) {
    let mut missing = 0;
    // Arranging more than this means something else keeps undoing it
    let mut arrangements = 0;
    loop {
        if arrangements < 5 {
            match crate::capture::layout::arrange(output, logical, scale) {
                Ok(true) => arrangements += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("arranging {output}: {e:#}");
                    arrangements = 5;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        if target.lock().unwrap().generation != *mine {
            return;
        }
        let map = match crate::capture::layout::query(output) {
            Ok(Some(m)) => {
                missing = 0;
                m
            }
            Ok(None) => {
                missing += 1;
                if missing >= 3 {
                    return;
                }
                continue;
            }
            Err(e) => {
                tracing::warn!("output layout: {e:#}");
                return;
            }
        };
        if last == Some(map) {
            continue;
        }
        let mut t = target.lock().unwrap();
        if t.generation != *mine {
            return;
        }
        tracing::info!(?map, "output {output} moved or resized; absolute input remapped");
        t.map = Some(map);
        t.generation += 1;
        *mine = t.generation;
        last = Some(map);
    }
}
