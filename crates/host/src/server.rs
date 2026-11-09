//! QUIC control server: pairing, session lifecycle, input and feedback.

use std::sync::{Arc, Mutex};
use std::time::Duration;

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
use crate::{audio, input};

pub struct PendingPair {
    pub client_name: String,
    pub fingerprint: String,
    pin_tx: Option<oneshot::Sender<String>>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Status {
    pub streaming: bool,
    pub client: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Option<String>,
    pub bitrate_kbps: u32,
    pub last_feedback: Option<FeedbackView>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FeedbackView {
    pub frames_lost: u32,
    pub frames_recovered: u32,
    pub decode_us_avg: u32,
    pub present_us_avg: u32,
}

pub struct AppState {
    pub identity: Identity,
    pub settings: Mutex<Settings>,
    pub trust: Mutex<Trust>,
    pub pending: Mutex<Option<PendingPair>>,
    pub status: Mutex<Status>,
    pub media: MediaServer,
    session: Mutex<Option<Session>>,
    session_owner: Mutex<u64>,
    pub status_tx: watch::Sender<u64>,
}

struct Session {
    video: VideoPipeline,
    _audio: Option<audio::AudioCapture>,
}

impl AppState {
    pub fn new(identity: Identity, settings: Settings, media: MediaServer) -> Arc<Self> {
        Arc::new(Self {
            identity,
            settings: Mutex::new(settings),
            trust: Mutex::new(Trust::load()),
            pending: Mutex::new(None),
            status: Mutex::new(Status::default()),
            media,
            session: Mutex::new(None),
            session_owner: Mutex::new(0),
            status_tx: watch::channel(0).0,
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
            tokio::spawn(input_task(conn.clone()));
            tokio::spawn(feedback_task(conn.clone(), state.clone()));
        }
        let Some(msg) = net::recv_msg::<ClientMsg>(&mut rx).await? else { return Ok(()) };
        match msg {
            ClientMsg::Ping { client_us } => {
                net::send_msg(&mut tx, &HostMsg::Pong { client_us, host_us: streetlight_proto::now_us() }).await?;
            }
            ClientMsg::PairStart { client_name, spake } if !paired => {
                paired = pair_flow(&state, &mut tx, &mut rx, client_name, spake, &fp, &fp_hex).await?;
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
                let r = tokio::task::spawn_blocking(move || start_session(&st, req, conn_id, client)).await?;
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
                if let Some(s) = state.session.lock().unwrap().as_ref() {
                    s.video.request_idr();
                }
            }
            ClientMsg::SetBitrate { kbps } => tracing::info!("bitrate change to {kbps} kbps requested (restart required for now)"),
            ClientMsg::Hello { .. } | ClientMsg::PairStart { .. } | ClientMsg::PairConfirm { .. } => {}
        }
    }
}

async fn pair_flow(
    state: &AppState,
    tx: &mut quinn::SendStream,
    rx: &mut quinn::RecvStream,
    client_name: String,
    spake_a: Vec<u8>,
    fp: &[u8; 32],
    fp_hex: &str,
) -> Result<bool> {
    let (pin_tx, pin_rx) = oneshot::channel();
    *state.pending.lock().unwrap() = Some(PendingPair { client_name: client_name.clone(), fingerprint: fp_hex.into(), pin_tx: Some(pin_tx) });
    state.notify();
    tracing::warn!("pairing request from '{client_name}': enter the PIN shown on the client at http://127.0.0.1:{}/ or run `streetlight-host pair <PIN>`", control::WEB_PORT);
    net::send_msg(tx, &HostMsg::PairAwaitingPin).await?;

    let pin = tokio::time::timeout(Duration::from_secs(300), pin_rx).await;
    *state.pending.lock().unwrap() = None;
    state.notify();
    let Ok(Ok(pin)) = pin else {
        net::send_msg(tx, &HostMsg::PairResult { ok: false, mac: vec![] }).await?;
        return Ok(false);
    };
    let (side, spake_b) = PairSide::host(&pin);
    net::send_msg(tx, &HostMsg::PairSpake { spake: spake_b }).await?;
    let key = side.finish(&spake_a);
    let Some(ClientMsg::PairConfirm { mac }) = net::recv_msg(rx).await? else { bail!("expected PairConfirm") };
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

fn start_session(state: &AppState, req: StreamRequest, owner: u64, client: String) -> Result<StreamInfo> {
    state.stop_session(None);
    let settings = state.settings.lock().unwrap().clone();
    let d = &req.display;
    anyhow::ensure!(d.width >= 64 && d.height >= 64 && d.width <= 8192 && d.height <= 8192, "bad size");
    let (w, h) = (d.width & !1, d.height & !1);
    let fps = d.fps.clamp(1, 240);
    let bitrate = req.bitrate_kbps.clamp(1000, settings.max_bitrate_kbps);
    let ten_bit = req.color == ColorSpace::Hdr10;

    let key = streetlight_proto::crypto::MediaCipher::random_key();
    let mut token = [0u8; 16];
    rand::fill(&mut token);
    let fec = FecConfig {
        max_shard_bytes: req.max_shard_bytes.clamp(256, 1400) & !1,
        percent: req.fec_percent.min(100),
        min_parity: if req.fec_percent > 0 { 2 } else { 0 },
    };
    state.media.begin(SessionKeys { key, token, fec, pace_mbps: req.pace_mbps });

    let scale = if d.scale > 0.5 { d.scale as f64 } else { 1.0 };
    let source = if d.virtual_display {
        Source::Virtual {
            name: format!("streetlight-{}", client.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()),
            width: (w as f64 / scale).round() as u32,
            height: (h as f64 / scale).round() as u32,
            scale,
        }
    } else {
        Source::Output { name: None }
    };
    let cfg = VideoConfig {
        source,
        cursor: if settings.embed_cursor { CursorMode::Embedded } else { CursorMode::Hidden },
        encoder: EncoderConfig {
            codec: req.codec,
            width: w,
            height: h,
            fps,
            bitrate_kbps: bitrate,
            ten_bit,
            render_node: settings.render_node.clone(),
            rc_mode: settings.rc_mode.clone(),
        },
        force_idr: Default::default(),
    };
    let media = state.media.clone();
    let idr = cfg.force_idr.clone();
    let video = VideoPipeline::start(cfg, move |f| {
        let ok = media.send(MediaMsg::Video { data: f.data, keyframe: f.keyframe, capture_us: (f.encoded_us - f.encode_us as u64) as u32 });
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

    if let Some(app) = req.app.as_ref().and_then(|id| settings.apps.iter().find(|a| &a.id == id)) {
        if let Some(cmd) = &app.command {
            tracing::info!("launching {}: {cmd}", app.name);
            let _ = std::process::Command::new("sh").arg("-c").arg(cmd).spawn();
        }
    }

    *state.session.lock().unwrap() = Some(Session { video, _audio: audio });
    *state.session_owner.lock().unwrap() = owner;
    *state.status.lock().unwrap() = Status {
        streaming: true,
        client: Some(client),
        width: w,
        height: h,
        fps,
        codec: Some(format!("{:?}", req.codec)),
        bitrate_kbps: bitrate,
        last_feedback: None,
    };
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
        audio_channels: audio::CHANNELS as u8,
        audio_sample_rate: audio::SAMPLE_RATE,
    })
}

async fn input_task(conn: quinn::Connection) {
    let injector = match input::Injector::new() {
        Ok(i) => Arc::new(Mutex::new(i)),
        Err(e) => {
            tracing::error!("input disabled: {e:#}");
            return;
        }
    };
    while let Ok(mut rx) = conn.accept_uni().await {
        let inj = injector.clone();
        tokio::spawn(async move {
            while let Ok(Some(ev)) = net::recv_msg::<InputEvent>(&mut rx).await {
                if let Err(e) = inj.lock().unwrap().inject(&ev) {
                    tracing::debug!("inject {ev:?}: {e}");
                }
            }
        });
    }
}

async fn feedback_task(conn: quinn::Connection, state: Arc<AppState>) {
    while let Ok(d) = conn.read_datagram().await {
        if let Ok(fb) = control::decode::<Feedback>(&d) {
            let mut s = state.status.lock().unwrap();
            s.last_feedback = Some(FeedbackView {
                frames_lost: fb.frames_lost,
                frames_recovered: fb.frames_recovered,
                decode_us_avg: fb.decode_us_avg,
                present_us_avg: fb.present_us_avg,
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
    }
}
