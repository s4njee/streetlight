//! Opus → low-latency playback (cpal / ALSA, which routes through PipeWire on Pi OS).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub static DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// Target queue depth; above MAX we drop old samples to cap latency.
const TARGET_MS: usize = 20;
const MAX_MS: usize = 60;

pub struct AudioOut {
    dec: opus::Decoder,
    queue: Arc<Mutex<VecDeque<f32>>>,
    pcm: Vec<f32>,
    underruns: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
    _stream: cpal::Stream,
}

// cpal::Stream is !Send on some backends; we only ever touch it from the media thread
// that created it, and on Linux/ALSA it is Send-safe.
unsafe impl Send for AudioOut {}

impl AudioOut {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let dev = host
            .default_output_device()
            .context("no audio output device")?;
        let cfg = cpal::StreamConfig {
            channels: CHANNELS as u16,
            sample_rate: cpal::SampleRate(RATE),
            buffer_size: cpal::BufferSize::Fixed(240),
        };
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(RATE as usize)));
        let q = queue.clone();
        let underruns = Arc::new(AtomicU64::new(0));
        let errors = Arc::new(AtomicU64::new(0));
        let under = underruns.clone();
        let err_count = errors.clone();
        let mut primed = false;
        let stream = dev
            .build_output_stream(
                &cfg,
                move |out: &mut [f32], _| {
                    let mut q = q.lock().unwrap();
                    let target = RATE as usize * CHANNELS * TARGET_MS / 1000;
                    if !primed && q.len() < target.max(out.len()) {
                        out.fill(0.0);
                        return;
                    }
                    primed = true;
                    if q.len() < out.len() { under.fetch_add(1, Ordering::Relaxed); }
                    for s in out.iter_mut() {
                        *s = q.pop_front().unwrap_or_else(|| {
                            primed = false;
                            0.0
                        });
                    }
                },
                move |e| { if err_count.fetch_add(1, Ordering::Relaxed) == 0 { tracing::warn!("audio output failed: {e}; disabling playback"); } },
                None,
            )
            .inspect(|_| {
                tracing::info!("audio output: fixed 240-frame buffer at 48000 Hz (5 ms)");
            })
            .or_else(|e| {
                tracing::info!("audio output: fixed buffer unavailable ({e}); using device default");
                // Some devices refuse a fixed buffer; fall back to the default size.
                let q = queue.clone();
                let under = underruns.clone();
                let err_count = errors.clone();
                let mut primed = false;
                dev.build_output_stream(
                    &cpal::StreamConfig {
                        buffer_size: cpal::BufferSize::Default,
                        ..cfg.clone()
                    },
                    move |out: &mut [f32], _| {
                        let mut q = q.lock().unwrap();
                        if !primed && q.len() < (RATE as usize * CHANNELS * TARGET_MS / 1000).max(out.len()) { out.fill(0.0); return; }
                        primed = true;
                        if q.len() < out.len() { under.fetch_add(1, Ordering::Relaxed); primed = false; }
                        for s in out.iter_mut() {
                            *s = q.pop_front().unwrap_or(0.0);
                        }
                    },
                    move |e| { if err_count.fetch_add(1, Ordering::Relaxed) == 0 { tracing::warn!("audio output failed: {e}; disabling playback"); } },
                    None,
                )
            })?;
        stream.play()?;
        Ok(Self {
            dec: opus::Decoder::new(RATE, opus::Channels::Stereo)?,
            queue,
            pcm: vec![0.0; 5760 * CHANNELS],
            underruns,
            errors,
            _stream: stream,
        })
    }

    pub fn counters(&self) -> (u64, u64) {
        (self.underruns.load(Ordering::Relaxed), self.errors.load(Ordering::Relaxed))
    }

    pub fn failed(&self) -> bool { self.errors.load(Ordering::Relaxed) != 0 }

    pub fn push(&mut self, packet: &[u8]) {
        if self.failed() { return; }
        let Ok(n) = self.dec.decode_float(
            packet,
            if packet.is_empty() {
                &mut self.pcm[..240 * CHANNELS]
            } else {
                &mut self.pcm
            },
            packet.is_empty(),
        ) else {
            return;
        };
        let mut q = self.queue.lock().unwrap();
        q.extend(&self.pcm[..n * CHANNELS]);
        let max = RATE as usize * CHANNELS * MAX_MS / 1000;
        if q.len() > max {
            let excess = q.len() - RATE as usize * CHANNELS * TARGET_MS / 1000;
            q.drain(..excess);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn opus_conceals_one_five_ms_packet_and_resumes() {
        let mut enc = opus::Encoder::new(
            48_000,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )
        .unwrap();
        let mut dec = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
        let mut packet = [0u8; 4000];
        let pcm: Vec<_> = (0..240)
            .flat_map(|i| {
                let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.01;
                [v, v]
            })
            .collect();
        let n = enc.encode_float(&pcm, &mut packet).unwrap();
        let mut output = [0f32; 480];
        assert_eq!(
            dec.decode_float(&packet[..n], &mut output, false).unwrap(),
            240
        );
        assert_eq!(dec.decode_float(&[], &mut output, true).unwrap(), 240);
        assert!(output.iter().all(|v| v.is_finite()));
        assert_eq!(
            dec.decode_float(&packet[..n], &mut output, false).unwrap(),
            240
        );
    }
}
