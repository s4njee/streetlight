//! Video pipeline: KWin screencast → PipeWire DMA-BUF → VA import → VPP → encoder.

use std::collections::HashMap;
use std::sync::{Arc,Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// PipeWire source timestamp on host CLOCK_MONOTONIC, before encoding.
    pub capture_us: u64,
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
    desired_bitrate: Arc<AtomicU32>,
    pub bitrate: Arc<AtomicU32>,
    pub cursor: Arc<Mutex<streetlight_proto::cursor::Snapshot>>,
    stop: Option<pw::channel::Sender<pwcap::Control>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
    _screencast: virtual_output::ScreencastHandle,
}

fn update_cursor(cursor:&mut streetlight_proto::cursor::Snapshot,update:crate::capture::cursor::Update,size:(u32,u32)) {
    let serial=cursor.position.serial.wrapping_add(1);
    let mut shape_id=cursor.position.shape_id;
    if let Some(mut bitmap)=update.bitmap {
        bitmap.id=0;
        if cursor.bitmap.as_ref().is_none_or(|old|{let mut old=old.as_ref().clone();old.id=0;old!=bitmap}) {
            shape_id=shape_id.wrapping_add(1);bitmap.id=shape_id;cursor.bitmap=Some(Arc::new(bitmap));
        }
    }
    cursor.position=streetlight_proto::cursor::Position{epoch:0,serial,shape_id,x:update.x,y:update.y,
        source_width:size.0,source_height:size.1,visible:update.visible};
}

fn test_cursor(frame:u32,size:(u32,u32))->crate::capture::cursor::Update {
    let mut rgba=vec![0u8;32*32*4];
    for y in 1..28 {for x in 1..=(y/2+1) {
        let border=x==1||x==y/2+1||y==27;let shade=if border{0}else{255};
        rgba[(y*32+x)*4..(y*32+x)*4+4].copy_from_slice(&[shade,shade,shade,255]);
    }}
    crate::capture::cursor::Update{x:(frame.wrapping_mul(8)%(size.0.saturating_sub(64).max(1))) as i32+32,
        y:(size.1/2) as i32,visible:true,
        bitmap:Some(streetlight_proto::cursor::Bitmap{epoch:0,id:0,width:32,height:32,hotspot_x:1,hotspot_y:1,rgba})}
}

impl VideoPipeline {
    /// Starts capture+encode; `sink` runs on the capture thread for every access unit.
    pub fn start(cfg: VideoConfig, sink: impl FnMut(EncodedFrame) + Send + 'static) -> Result<Self> {
        let drm_formats=if cfg.encoder.ten_bit {vec![va::drm_fourcc::XRGB2101010,va::drm_fourcc::XBGR2101010]}
            else {vec![va::drm_fourcc::XRGB8888,va::drm_fourcc::ARGB8888,va::drm_fourcc::XBGR8888,va::drm_fourcc::ABGR8888]};
        let formats = modifiers::query(&cfg.encoder.render_node,&drm_formats)
        .context("query DMA-BUF modifiers")?;
        tracing::debug!("import modifiers: {formats:x?}");

        let screencast = virtual_output::start(cfg.source.clone(), cfg.cursor)?;
        tracing::info!("screencast node {}", screencast.node_id);
        let node = screencast.node_id;

        let force_idr = cfg.force_idr.clone();
        force_idr.store(true, Ordering::Relaxed);
        let (stop_tx, stop_rx) = pw::channel::channel();
        let idr = force_idr.clone();
        let desired_bitrate = Arc::new(AtomicU32::new(cfg.encoder.bitrate_kbps));
        let desired = desired_bitrate.clone();
        let bitrate = desired_bitrate.as_ref().load(Ordering::Relaxed);
        let bitrate = Arc::new(AtomicU32::new(bitrate));
        let applied = bitrate.clone();
        let cursor = Arc::new(Mutex::new(streetlight_proto::cursor::Snapshot::default()));
        let cursor_update = cursor.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let format_abort=stop_tx.clone();
        let thread = thread::Builder::new().name("video".into()).spawn(move || -> Result<()> {
            let mut enc = match Encoder::new(cfg.encoder.clone()) {
                Ok(e) => {
                    if !cfg.encoder.ten_bit { let _ = ready_tx.send(Ok(())); }
                    e
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow::anyhow!("{e:#}")));
                    return Err(e);
                }
            };
            let mut format_ready=cfg.encoder.ten_bit.then_some(ready_tx);
            let abort_format=format_abort;
            let dpy = enc.va;
            let mut cache: HashMap<usize, va::ffi::VASurfaceID> = HashMap::new();
            let mut sink = sink;
            let mut src_size = (0, 0);
            let mut failed_imports = 0u32;
            let size = (cfg.encoder.width, cfg.encoder.height);
            let mut cursor_size = size;
            let synthetic_cursor=std::env::var("SL_TEST_CURSOR").is_ok_and(|v|v=="1");
            if synthetic_cursor {tracing::warn!("TEST ONLY: synthetic moving cursor metadata enabled (no input injection)");}
            let mut fixture_frame=0u32;
            pwcap::run(node, &formats, size, cfg.encoder.fps, cfg.encoder.ten_bit, stop_rx, move |ev| match ev {
                pwcap::Event::Cursor(update) => {
                    if !synthetic_cursor {update_cursor(&mut cursor_update.lock().unwrap(),update,cursor_size);}
                }
                pwcap::Event::FormatChanged { width, height, hdr, .. } => {
                    if cfg.encoder.ten_bit && !hdr {
                        if let Some(ready)=format_ready.take() {
                            let _=ready.send(Err(anyhow::anyhow!("HDR capture did not negotiate 10-bit BT.2020/PQ; refusing to mislabel SDR")));
                        }
                        tracing::error!("HDR capture lost required 10-bit BT.2020/PQ format; stopping video");
                        let _=abort_format.send(pwcap::Control::Stop);return;
                    }
                    if let Some(ready)=format_ready.take() {
                        let _=ready.send(Ok(()));
                        tracing::info!("HDR capture verified: 10-bit RGB BT.2020/PQ");
                    }
                    cursor_size=(width,height);
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
                    if synthetic_cursor {fixture_frame=fixture_frame.wrapping_add(1);update_cursor(&mut cursor_update.lock().unwrap(),test_cursor(fixture_frame,cursor_size),cursor_size);}
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
                    let requested = desired.load(Ordering::Relaxed);
                    if requested != enc.config().bitrate_kbps {
                        let before = enc.config().bitrate_kbps;
                        let start = std::time::Instant::now();
                        match enc.set_bitrate(requested) {
                            Ok(()) => {
                                applied.store(requested, Ordering::Relaxed);
                                idr.store(true, Ordering::Relaxed);
                                tracing::info!("encoder bitrate: {before} → {requested} kbps; codec reopen {:.2} ms, capture retained", start.elapsed().as_secs_f64()*1000.0);
                            }
                            Err(e) => { desired.store(before, Ordering::Relaxed); tracing::warn!("bitrate change failed; retaining {before} kbps: {e:#}"); }
                        }
                    }
                    let force = idr.swap(false, Ordering::Relaxed);
                    let r = enc.encode_surface(surf, src_size, force, || {}, |data, key| {
                        let now = streetlight_proto::now_us();
                        sink(EncodedFrame { data: data.to_vec(), keyframe: key, encoded_us: now, capture_us: if f.pts_ns > 0 { f.pts_ns as u64 / 1000 } else { t0 }, encode_us: (now - t0) as u32 });
                    });
                    if let Err(e) = r {
                        tracing::error!("encode: {e:#}");
                    }
                }
            })
        })?;
        let ready=ready_rx.recv_timeout(std::time::Duration::from_secs(5))
            .context("capture format startup timed out or video thread died").and_then(|r|r);
        if let Err(e)=ready {let _=stop_tx.send(pwcap::Control::Stop);let _=thread.join();return Err(e);}
        Ok(Self { force_idr, desired_bitrate, bitrate, cursor, stop: Some(stop_tx), thread: Some(thread), _screencast: screencast })
    }

    pub fn set_bitrate(&self, kbps: u32) { self.desired_bitrate.store(kbps, Ordering::Relaxed); }

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
