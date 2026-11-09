//! UDP media sender: packetise + FEC + encrypt, then GSO-batched, optionally paced sends.

use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use quinn_udp::{Transmit, UdpSockRef, UdpSocketState};
use streetlight_proto::crypto::MediaCipher;
use streetlight_proto::fec::{FecConfig, Packetizer};
use streetlight_proto::media::{HEADER_LEN, MediaHeader, StreamKind, flags};

pub enum MediaMsg {
    Video {
        data: Vec<u8>,
        keyframe: bool,
        capture_us: u32,
    },
    Audio {
        data: Vec<u8>,
        capture_us: u32,
    },
}

#[derive(Clone)]
pub struct SessionKeys {
    pub key: [u8; 16],
    pub token: [u8; 16],
    pub fec: FecConfig,
    /// 0 = unpaced.
    pub pace_mbps: u32,
}

struct Shared {
    session: Option<SessionKeys>,
    peer: Option<SocketAddr>,
    generation: u64,
    idr_requests: u64,
}

#[derive(Clone)]
pub struct MediaServer {
    shared: Arc<Mutex<Shared>>,
    tx: SyncSender<MediaMsg>,
    pub port: u16,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct SendStats {
    pub frames: u64,
    pub keyframes: u64,
    pub bytes: u64,
    pub dropped: u64,
}

impl MediaServer {
    pub fn bind(port: u16) -> Result<Self> {
        let sock = UdpSocket::bind(("::", port)).or_else(|_| UdpSocket::bind(("0.0.0.0", port)))?;
        let port = sock.local_addr()?.port();
        let _ = socket2::SockRef::from(&sock).set_send_buffer_size(8 << 20);
        let state = UdpSocketState::new(UdpSockRef::from(&sock))?;
        sock.set_nonblocking(false)?;
        tracing::info!(
            "media socket on UDP {port} (GSO segments: {})",
            state.max_gso_segments()
        );
        let sock = Arc::new(sock);
        let shared = Arc::new(Mutex::new(Shared {
            session: None,
            peer: None,
            generation: 0,
            idr_requests: 0,
        }));
        // Small queue: if the network can't keep up we drop rather than build latency.
        let (tx, rx) = sync_channel::<MediaMsg>(4);

        {
            let (sock, shared) = (sock.clone(), shared.clone());
            thread::Builder::new()
                .name("media-rx".into())
                .spawn(move || probe_loop(&sock, &shared))?;
        }
        {
            let shared = shared.clone();
            thread::Builder::new()
                .name("media-tx".into())
                .spawn(move || send_loop(&sock, &state, &shared, rx))?;
        }
        Ok(Self { shared, tx, port })
    }

    pub fn begin(&self, keys: SessionKeys) {
        let mut s = self.shared.lock().unwrap();
        s.session = Some(keys);
        s.idr_requests = 0;
        s.peer = None;
        s.generation += 1;
    }

    pub fn record_idr_request(&self) {
        self.shared.lock().unwrap().idr_requests += 1;
    }

    pub fn end(&self) {
        let mut s = self.shared.lock().unwrap();
        s.session = None;
        s.peer = None;
        s.generation += 1;
    }

    /// Never blocks; returns false if the frame was dropped.
    pub fn send(&self, m: MediaMsg) -> bool {
        match self.tx.try_send(m) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

fn probe_loop(sock: &UdpSocket, shared: &Mutex<Shared>) {
    let mut buf = [0u8; 2048];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Some(h) = MediaHeader::read(&buf[..n]) else {
            continue;
        };
        if h.stream != StreamKind::Probe || n < HEADER_LEN + 16 {
            continue;
        }
        let mut s = shared.lock().unwrap();
        if let Some(sess) = &s.session
            && buf[HEADER_LEN..HEADER_LEN + 16] == sess.token
            && s.peer != Some(from)
        {
            tracing::info!("media peer is {from}");
            s.peer = Some(from);
        }
    }
}

fn send_loop(
    sock: &UdpSocket,
    state: &UdpSocketState,
    shared: &Mutex<Shared>,
    rx: Receiver<MediaMsg>,
) {
    let mut generation = u64::MAX;
    let mut video: Option<Packetizer> = None;
    let mut audio: Option<Packetizer> = None;
    let (mut vseq, mut aseq) = (0u32, 0u32);
    let mut pace_mbps = 0;
    let gso = state.max_gso_segments().clamp(1, 64);
    let mut stats = SendStats::default();
    let mut last_log = Instant::now();
    let mut last_idr_requests = 0;
    loop {
        let msg = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let peer = {
            let s = shared.lock().unwrap();
            if s.generation != generation {
                generation = s.generation;
                (vseq, aseq) = (0, 0);
                stats = SendStats::default();
                last_idr_requests = 0;
                last_log = Instant::now();
                if let Some(k) = &s.session {
                    video = Some(Packetizer::new(k.fec, MediaCipher::new(&k.key)));
                    let afec = FecConfig {
                        percent: 50,
                        min_parity: 1,
                        ..k.fec
                    };
                    audio = Some(Packetizer::new(afec, MediaCipher::new(&k.key)));
                    pace_mbps = k.pace_mbps;
                } else {
                    (video, audio) = (None, None);
                }
            }
            s.peer
        };
        let Some(peer) = peer else { continue };
        let (batch, paced) = match msg {
            MediaMsg::Video {
                data,
                keyframe,
                capture_us,
            } => {
                let Some(p) = video.as_mut() else { continue };
                let f = if keyframe { flags::KEYFRAME } else { 0 };
                let b = p.packetize(StreamKind::Video, vseq, capture_us, f, &data);
                vseq = vseq.wrapping_add(1);
                stats.frames += 1;
                stats.keyframes += u64::from(keyframe);
                (b, pace_mbps)
            }
            MediaMsg::Audio { data, capture_us } => {
                let Some(p) = audio.as_mut() else { continue };
                let b = p.packetize(StreamKind::Audio, aseq, capture_us, 0, &data);
                aseq = aseq.wrapping_add(1);
                (b, 0)
            }
        };
        let start = Instant::now();
        let mut sent = 0usize;
        // One GSO send must stay under the 64 KiB UDP datagram limit.
        let per_send = gso.min(65_000 / batch.packet_len).max(1);
        for chunk in batch.buf.chunks(batch.packet_len * per_send) {
            let t = Transmit {
                destination: peer,
                ecn: None,
                contents: chunk,
                segment_size: (chunk.len() > batch.packet_len).then_some(batch.packet_len),
                src_ip: None,
            };
            if let Err(e) = send_with_retry(sock, state, &t) {
                tracing::debug!("send: {e}");
                stats.dropped += 1;
            }
            sent += chunk.len();
            if paced > 0 {
                // bytes * 8 / (Mbit/s) = µs
                let due = Duration::from_micros((sent as u64 * 8) / paced as u64);
                while start.elapsed() < due {
                    std::hint::spin_loop();
                }
            }
        }
        stats.bytes += batch.buf.len() as u64;
        if last_log.elapsed() > Duration::from_secs(5) {
            let secs = last_log.elapsed().as_secs_f64();
            let idr_requests = shared.lock().unwrap().idr_requests;
            tracing::info!(
                "media: {:.1} fps, {:.1} Mbit/s (incl. FEC), {} send errors, {} IDR requests, {} keyframes sent",
                stats.frames as f64 / secs,
                stats.bytes as f64 * 8.0 / secs / 1e6,
                stats.dropped,
                idr_requests.saturating_sub(last_idr_requests),
                stats.keyframes,
            );
            last_idr_requests = idr_requests;
            stats = SendStats::default();
            last_log = Instant::now();
        }
    }
}

fn send_with_retry(
    sock: &UdpSocket,
    state: &UdpSocketState,
    t: &Transmit<'_>,
) -> std::io::Result<()> {
    for _ in 0..50 {
        match state.send(UdpSockRef::from(sock), t) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_micros(100))
            }
            r => return r,
        }
    }
    Err(std::io::ErrorKind::WouldBlock.into())
}
