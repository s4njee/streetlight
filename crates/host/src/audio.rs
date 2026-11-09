//! Desktop audio: PipeWire default-sink monitor → Opus (5 ms frames, low delay).

use std::thread;

use anyhow::{Context, Result};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 5 ms @ 48 kHz.
pub const FRAME_SAMPLES: usize = 240;

pub struct AudioCapture {
    stop: Option<pw::channel::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl AudioCapture {
    pub fn start(sink: impl FnMut(Vec<u8>, u64) + Send + 'static) -> Result<Self> {
        let (stop_tx, stop_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread = thread::Builder::new().name("audio".into()).spawn(move || {
            if let Err(e) = run(stop_rx, sink, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })?;
        ready_rx.recv().context("audio thread died")??;
        Ok(Self { stop: Some(stop_tx), thread: Some(thread) })
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Ud<F> {
    sink: F,
    enc: opus::Encoder,
    pcm: Vec<f32>,
    out: Vec<u8>,
}

fn run<F: FnMut(Vec<u8>, u64) + 'static>(stop: pw::channel::Receiver<()>, sink: F, ready: &std::sync::mpsc::Sender<Result<()>>) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let stream = pw::stream::StreamRc::new(
        core,
        "streetlight-audio",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Game",
            *pw::keys::STREAM_CAPTURE_SINK => "true",
            *pw::keys::NODE_LATENCY => "240/48000",
        },
    )?;
    let ml = mainloop.clone();
    let _s = stop.attach(mainloop.loop_(), move |_| ml.quit());

    let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::LowDelay)?;
    enc.set_bitrate(opus::Bitrate::Bits(256_000))?;
    let ud = Ud { sink, enc, pcm: Vec::with_capacity(FRAME_SAMPLES * CHANNELS * 4), out: vec![0; 4000] };

    let _listener = stream
        .add_local_listener_with_user_data(ud)
        .process(|stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let d = &mut datas[0];
            let size = d.chunk().size() as usize;
            let Some(bytes) = d.data() else { return };
            let bytes = &bytes[..size.min(bytes.len())];
            ud.pcm.extend(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
            let frame = FRAME_SAMPLES * CHANNELS;
            let mut off = 0;
            while ud.pcm.len() - off >= frame {
                match ud.enc.encode_float(&ud.pcm[off..off + frame], &mut ud.out) {
                    Ok(n) => (ud.sink)(ud.out[..n].to_vec(), streetlight_proto::now_us()),
                    Err(e) => tracing::debug!("opus: {e}"),
                }
                off += frame;
            }
            ud.pcm.drain(..off);
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS as u32);
    let mut pos = [0; spa::param::audio::MAX_CHANNELS];
    pos[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    pos[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(pos);
    let obj = pw::spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pw::spa::pod::Value::Object(obj))
        .unwrap()
        .0
        .into_inner();
    let mut params = [Pod::from_bytes(&bytes).unwrap()];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}
