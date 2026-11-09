//! streetlight-host: game/desktop streaming server.

use streetlight_host::{capture, config, encode, media, server, video, web};

use std::io::Write;

use anyhow::Result;
use streetlight_proto::control::{CONTROL_PORT, Codec, MDNS_SERVICE, MEDIA_PORT, WEB_PORT};

const USAGE: &str = "usage:
  streetlight-host [serve]              run the host (QUIC 47800, media 47801, web UI 127.0.0.1:47990)
  streetlight-host pair <PIN>           approve a pending pairing with the PIN shown on the client
  streetlight-host capture-test [secs] [out.hevc] [WxH] [--hdr]   capture a virtual display to a file";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("serve") => serve(),
        Some("pair") => {
            let pin = args.get(1).ok_or_else(|| anyhow::anyhow!("{USAGE}"))?;
            if web::submit_pin_cli(pin, WEB_PORT)? {
                println!("PIN accepted; the client will finish pairing.");
            } else {
                println!("No pairing request is waiting (or the host isn't running).");
            }
            Ok(())
        }
        Some("capture-test") => capture_test(&args[1..]),
        _ => {
            eprintln!("{USAGE}");
            Ok(())
        }
    }
}

fn serve() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async {
        let settings = config::Settings::load();
        let identity = streetlight_proto::net::Identity::load_or_create(&config::config_dir(), "host")?;
        let media = media::MediaServer::bind(MEDIA_PORT)?;
        let state = server::AppState::new(identity, settings, media);
        let _mdns = advertise(&state.settings.lock().unwrap().host_name);
        tokio::select! {
            r = server::run(state.clone(), CONTROL_PORT) => r,
            r = web::run(state.clone(), WEB_PORT) => r,
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    })
}

fn advertise(name: &str) -> Option<mdns_sd::ServiceDaemon> {
    let d = mdns_sd::ServiceDaemon::new().ok()?;
    let host = format!("{}.local.", name.replace(' ', "-"));
    let info = mdns_sd::ServiceInfo::new(MDNS_SERVICE, name, &host, "", CONTROL_PORT, &[("v", env!("CARGO_PKG_VERSION"))][..])
        .ok()?
        .enable_addr_auto();
    d.register(info).ok()?;
    tracing::info!("advertising {MDNS_SERVICE} as '{name}'");
    Some(d)
}

fn capture_test(args: &[String]) -> Result<()> {
    let secs: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(5);
    let out = args.get(1).cloned().unwrap_or("capture.hevc".into());
    let (w, h) = args
        .get(2)
        .and_then(|s| s.split_once('x'))
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .unwrap_or((1920u32, 1080u32));
    let settings = config::Settings::load();
    let hdr=args.iter().any(|a|a=="--hdr");
    let metadata=if hdr {let m=settings.hdr_metadata.clone().ok_or_else(||anyhow::anyhow!("HDR requires configured mastering metadata"))?;m.validate()?;Some(m)}else{None};
    let mut file = std::fs::File::create(&out)?;
    let cfg = video::VideoConfig {
        source: capture::virtual_output::Source::Virtual { name: "streetlight-test".into(), width: w, height: h, scale: 1.0 },
        cursor: capture::virtual_output::CursorMode::Embedded,
        encoder: encode::EncoderConfig {
            codec: Codec::Hevc,
            width: w,
            height: h,
            fps: 60,
            bitrate_kbps: 40_000,
            ten_bit: hdr,
            hdr_metadata: metadata,
            color: if hdr {streetlight_proto::control::ColorSpace::Hdr10}else{streetlight_proto::control::ColorSpace::Sdr709},
            render_node: settings.render_node,
            rc_mode: settings.rc_mode,
        },
        force_idr: Default::default(),
    };
    let (mut n, mut total_us) = (0u64, 0u64);
    let pipe = video::VideoPipeline::start(cfg, move |f| {
        n += 1;
        total_us += f.encode_us as u64;
        file.write_all(&f.data).unwrap();
        if n % 60 == 1 || f.keyframe {
            tracing::info!("frame {n}: {} bytes key={} encode={}µs avg={}µs", f.data.len(), f.keyframe, f.encode_us, total_us / n);
        }
    })?;
    for _ in 0..secs {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if std::env::var_os("SL_TEST_IDR").is_some() {
            pipe.request_idr();
        }
    }
    drop(pipe);
    println!("wrote {out}");
    Ok(())
}
