//! streetlight-client for Linux / Raspberry Pi 5.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use streetlight_client_core::{self as core, ClientCaps, Codec, ColorSpace, ConnectOptions, DisplayRequest, MediaSink, StreamRequest, VideoFrame};

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| ".".into())
        .join("streetlight-client")
}

struct Args {
    host: Option<String>,
    dump: Option<String>,
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
    seconds: u64,
}

fn parse() -> Args {
    let mut a = Args { host: None, dump: None, size: (1920, 1080), fps: 60, bitrate: 50_000, seconds: 10 };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--dump" => a.dump = it.next(),
            "--size" => {
                if let Some((w, h)) = it.next().and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))) {
                    a.size = (w, h)
                }
            }
            "--fps" => a.fps = it.next().and_then(|s| s.parse().ok()).unwrap_or(60),
            "--bitrate" => a.bitrate = it.next().and_then(|s| s.parse().ok()).unwrap_or(50_000),
            "--seconds" => a.seconds = it.next().and_then(|s| s.parse().ok()).unwrap_or(10),
            h if !h.starts_with('-') => a.host = Some(h.to_string()),
            _ => {}
        }
    }
    a
}

struct DumpSink(std::fs::File, u64);
impl MediaSink for DumpSink {
    fn video(&mut self, f: VideoFrame) {
        self.0.write_all(&f.data).unwrap();
        self.1 += 1;
    }
    fn audio(&mut self, _: &[u8]) {}
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = parse();
    let host = match args.host.clone() {
        Some(h) => h,
        None => {
            let found = core::discovery::browse(Duration::from_secs(2));
            println!("discovered: {found:?}");
            match found.first() {
                Some(h) => h.addr.clone(),
                None => bail!("no host found; pass an address"),
            }
        }
    };
    let caps = ClientCaps { codecs: vec![Codec::Hevc], hdr: false, max_width: 3840, max_height: 2160, max_fps: 120, audio_channels: 2 };
    let (client, welcome) = core::Client::connect(ConnectOptions { host, client_name: hostname(), data_dir: data_dir(), caps })?;
    println!("connected to '{}' (paired: {})", welcome.host_name, welcome.paired);
    if !welcome.paired {
        let pin = core::random_pin();
        println!("\n  Pairing PIN: {pin}\n  Enter it in the host web UI (http://127.0.0.1:47990) or run `streetlight-host pair {pin}` on the host.\n");
        if !client.pair(&pin)? {
            bail!("pairing failed");
        }
        println!("paired!");
    }
    println!("apps: {:?}", client.apps()?);
    let Some(dump) = args.dump else {
        println!("(GUI not built yet; use --dump out.hevc)");
        return Ok(());
    };
    let req = StreamRequest {
        display: DisplayRequest { width: args.size.0, height: args.size.1, fps: args.fps, scale: 1.0, virtual_display: true },
        codec: Codec::Hevc,
        color: ColorSpace::Sdr709,
        bitrate_kbps: args.bitrate,
        fec_percent: 20,
        max_shard_bytes: 1400,
        pace_mbps: 0,
        app: None,
    };
    let info = client.start(req, DumpSink(std::fs::File::create(&dump)?, 0))?;
    println!("stream: {}x{}@{} {:?}", info.width, info.height, info.fps, info.codec);
    for _ in 0..args.seconds {
        std::thread::sleep(Duration::from_secs(1));
        let s = client.stats();
        println!(
            "fps {:5.1}  {:6.1} Mbit/s  frames {} lost {} recovered {} pkts {} bad {}  net-latency {:.2} ms  rtt {:.2} ms",
            s.fps, s.mbps, s.frames, s.frames_lost, s.frames_recovered, s.packets, s.bad_packets, s.network_latency_ms, s.rtt_ms
        );
        if let Some(r) = client.stopped_reason() {
            println!("stopped: {r}");
            break;
        }
    }
    client.stop();
    Ok(())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "linux-client".into())
}
