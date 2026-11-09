//! Opus → low-latency playback (cpal / ALSA, which routes through PipeWire on Pi OS).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// Target queue depth; above MAX we drop old samples to cap latency.
const TARGET_MS: usize = 20;
const MAX_MS: usize = 60;

pub struct AudioOut {
    dec: opus::Decoder,
    queue: Arc<Mutex<VecDeque<f32>>>,
    pcm: Vec<f32>,
    _stream: cpal::Stream,
}

// cpal::Stream is !Send on some backends; we only ever touch it from the media thread
// that created it, and on Linux/ALSA it is Send-safe.
unsafe impl Send for AudioOut {}

impl AudioOut {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let dev = host.default_output_device().context("no audio output device")?;
        let cfg = cpal::StreamConfig {
            channels: CHANNELS as u16,
            sample_rate: cpal::SampleRate(RATE),
            buffer_size: cpal::BufferSize::Fixed(240),
        };
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(RATE as usize)));
        let q = queue.clone();
        let mut primed = false;
        let stream = dev
            .build_output_stream(
                &cfg,
                move |out: &mut [f32], _| {
                    let mut q = q.lock().unwrap();
                    let target = RATE as usize * CHANNELS * TARGET_MS / 1000;
                    if !primed && q.len() < target {
                        out.fill(0.0);
                        return;
                    }
                    primed = true;
                    for s in out.iter_mut() {
                        *s = q.pop_front().unwrap_or_else(|| {
                            primed = false;
                            0.0
                        });
                    }
                },
                |e| tracing::warn!("audio: {e}"),
                None,
            )
            .or_else(|_| {
                // Some devices refuse a fixed buffer; fall back to the default size.
                let q = queue.clone();
                dev.build_output_stream(
                    &cpal::StreamConfig { buffer_size: cpal::BufferSize::Default, ..cfg.clone() },
                    move |out: &mut [f32], _| {
                        let mut q = q.lock().unwrap();
                        for s in out.iter_mut() {
                            *s = q.pop_front().unwrap_or(0.0);
                        }
                    },
                    |e| tracing::warn!("audio: {e}"),
                    None,
                )
            })?;
        stream.play()?;
        Ok(Self { dec: opus::Decoder::new(RATE, opus::Channels::Stereo)?, queue, pcm: vec![0.0; 5760 * CHANNELS], _stream: stream })
    }

    pub fn push(&mut self, packet: &[u8]) {
        let Ok(n) = self.dec.decode_float(packet, &mut self.pcm, false) else { return };
        let mut q = self.queue.lock().unwrap();
        q.extend(&self.pcm[..n * CHANNELS]);
        let max = RATE as usize * CHANNELS * MAX_MS / 1000;
        if q.len() > max {
            let excess = q.len() - RATE as usize * CHANNELS * TARGET_MS / 1000;
            q.drain(..excess);
        }
    }
}
