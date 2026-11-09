//! Video pipeline: KWin screencast → PipeWire DMA-BUF → VA import → VPP → encoder.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result};
use pipewire as pw;

use crate::capture::{pipewire as pwcap, virtual_output};
use crate::encode::{Encoder, EncoderConfig};
use crate::gpu::{modifiers, va};

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Host monotonic µs when the encoder delivered the frame.
    pub encoded_us: u64,
    /// µs spent in import + VPP + encode.
    pub encode_us: u32,
}

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub source: virtual_output::Source,
    pub cursor: virtual_output::CursorMode,
    pub encoder: EncoderConfig,
    /// Shared "encode the next frame as IDR" flag (also set by the sender on drops).
    pub force_idr: Arc<AtomicBool>,
}

pub struct VideoPipeline {
    pub force_idr: Arc<AtomicBool>,
    stop: Option<pw::channel::Sender<pwcap::Control>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
    _screencast: virtual_output::ScreencastHandle,
}

impl VideoPipeline {
    /// Starts capture+encode; `sink` runs on the capture thread for every access unit.
    pub fn start(cfg: VideoConfig, sink: impl FnMut(EncodedFrame) + Send + 'static) -> Result<Self> {
        let formats = modifiers::query(
            &cfg.encoder.render_node,
            &[va::drm_fourcc::XRGB8888, va::drm_fourcc::ARGB8888, va::drm_fourcc::XBGR8888, va::drm_fourcc::ABGR8888],
        )
        .context("query DMA-BUF modifiers")?;
        tracing::debug!("import modifiers: {formats:x?}");

        let screencast = virtual_output::start(cfg.source.clone(), cfg.cursor)?;
        tracing::info!("screencast node {}", screencast.node_id);
        let node = screencast.node_id;

        let force_idr = cfg.force_idr.clone();
        force_idr.store(true, Ordering::Relaxed);
        let (stop_tx, stop_rx) = pw::channel::channel();
        let idr = force_idr.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread = thread::Builder::new().name("video".into()).spawn(move || -> Result<()> {
            let mut enc = match Encoder::new(cfg.encoder.clone()) {
                Ok(e) => {
                    let _ = ready_tx.send(Ok(()));
                    e
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow::anyhow!("{e:#}")));
                    return Err(e);
                }
            };
            let dpy = enc.va;
            let mut cache: HashMap<usize, va::ffi::VASurfaceID> = HashMap::new();
            let mut sink = sink;
            let mut src_size = (0, 0);
            let mut failed_imports = 0u32;
            let size = (cfg.encoder.width, cfg.encoder.height);
            pwcap::run(node, &formats, size, cfg.encoder.fps, stop_rx, move |ev| match ev {
                pwcap::Event::FormatChanged { width, height, .. } => {
                    for (_, s) in cache.drain() {
                        dpy.destroy_surface(s);
                    }
                    src_size = (width, height);
                    idr.store(true, Ordering::Relaxed);
                }
                pwcap::Event::BufferRemoved(k) => {
                    if let Some(s) = cache.remove(&k) {
                        dpy.destroy_surface(s);
                    }
                }
                pwcap::Event::Frame(f) => {
                    let t0 = streetlight_proto::now_us();
                    let surf = match cache.get(&f.buffer_key) {
                        Some(s) => *s,
                        None => match dpy.import(f.desc) {
                            Ok(s) => {
                                tracing::debug!("imported buffer {:#x} → surface {s}", f.buffer_key);
                                cache.insert(f.buffer_key, s);
                                s
                            }
                            Err(e) => {
                                failed_imports += 1;
                                if failed_imports < 5 {
                                    tracing::error!("dmabuf import failed: {e:#} ({:?})", f.desc);
                                }
                                return;
                            }
                        },
                    };
                    let force = idr.swap(false, Ordering::Relaxed);
                    let r = enc.encode_surface(surf, src_size, force, || {}, |data, key| {
                        let now = streetlight_proto::now_us();
                        sink(EncodedFrame { data: data.to_vec(), keyframe: key, encoded_us: now, encode_us: (now - t0) as u32 });
                    });
                    if let Err(e) = r {
                        tracing::error!("encode: {e:#}");
                    }
                }
            })
        })?;
        ready_rx.recv().context("video thread died")??;
        Ok(Self { force_idr, stop: Some(stop_tx), thread: Some(thread), _screencast: screencast })
    }

    pub fn request_idr(&self) {
        self.force_idr.store(true, Ordering::Relaxed);
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(pwcap::Control::Stop);
        }
        if let Some(t) = self.thread.take() {
            match t.join() {
                Ok(Err(e)) => tracing::warn!("video thread: {e:#}"),
                Err(_) => tracing::warn!("video thread panicked"),
                _ => {}
            }
        }
    }
}
