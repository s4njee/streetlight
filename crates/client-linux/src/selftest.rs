//! `streetlight-client <host> --selftest [--clicks]`: scripted end-to-end check of the
//! input and audio paths (diagnostics; audio is analysed, not played).

use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Result, bail};
use streetlight_client_core::{Client, GamepadState, InputEvent, MediaSink, MouseButton, TouchPhase, VideoFrame, pad_buttons};

use crate::settings::{Overrides, Settings};

#[derive(Default)]
struct Collected {
    video_frames: u64,
    audio_packets: u64,
    pcm: Vec<f32>,
}

struct Sink {
    dec: opus::Decoder,
    buf: Vec<f32>,
    out: Arc<Mutex<Collected>>,
}

impl MediaSink for Sink {
    fn audio_lost(&mut self) { self.audio(&[]); }
    fn video(&mut self, _: VideoFrame) {
        self.out.lock().unwrap().video_frames += 1;
    }
    fn audio(&mut self, p: &[u8]) {
        let mut c = self.out.lock().unwrap();
        c.audio_packets += 1;
        if let Ok(n) = self.dec.decode_float(p, if p.is_empty() { &mut self.buf[..480] } else { &mut self.buf }, p.is_empty()) {
            // Keep the left channel only.
            c.pcm.extend(self.buf[..n * 2].iter().step_by(2));
        }
    }
}

fn say(s: &str) {
    println!("SELFTEST {s}");
}

fn fresh_gate(path: &std::path::Path) -> bool {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
        .and_then(|t| t.elapsed().ok()).is_some_and(|age| age < Duration::from_millis(350))
        && std::fs::read_to_string(path).is_ok_and(|s| s.starts_with("ready "))
}

pub fn run(client: Arc<Client>, clicks: bool) -> Result<()> {
    let out = Arc::new(Mutex::new(Collected::default()));
    let sink = Sink { dec: opus::Decoder::new(48_000, opus::Channels::Stereo)?, buf: vec![0.0; 5760 * 2], out: out.clone() };
    let o = Overrides { size: Some((1920, 1080)), fps: Some(60), bitrate_kbps: Some(30_000), ..Default::default() };
    let info = client.start(Settings::default().request(None, &o, None), sink)?;
    say(&format!("stream {}x{}", info.width, info.height));
    sleep(Duration::from_secs(5)); // harness opens the probe window on the virtual output now

    let send = |e: InputEvent| client.send_input(e);
    // Keyboard/mouse only go out once the probe window confirms it is focused on the
    // streamed output, so stray events can't land in the user's other windows.
    let gate = std::env::var_os("SL_SELFTEST_GATE");
    let gated = gate.as_ref().is_some_and(|g| {
        (0..100).any(|_| {
            sleep(Duration::from_millis(100));
            fresh_gate(std::path::Path::new(g))
        })
    });
    if !gated {
        say("no probe gate; skipping keyboard/mouse");
    }
    let send_km = |e: InputEvent| -> Result<()> {
        if gated {
            let release = matches!(e, InputEvent::Key { down: false, .. } | InputEvent::MouseButton { down: false, .. }
                | InputEvent::Touch { phase: TouchPhase::Up, .. });
            if !release && !gate.as_ref().is_some_and(|g| fresh_gate(std::path::Path::new(g))) {
                bail!("probe lost focus/fullscreen; stopping input self-test");
            }
            client.send_input(e);
        }
        Ok(())
    };
    say("abs moves");
    for (x, y) in [(0.25, 0.25), (0.5, 0.5), (0.75, 0.75), (0.5, 0.5)] {
        send_km(InputEvent::MouseAbs { x: (x * 65535.0) as u16, y: (y * 65535.0) as u16 })?;
        sleep(Duration::from_millis(300));
    }
    say("relative moves +30,+20");
    for _ in 0..3 {
        send_km(InputEvent::MouseMove { dx: 10, dy: 0 })?;
        sleep(Duration::from_millis(50));
    }
    send_km(InputEvent::MouseMove { dx: 0, dy: 20 })?;
    sleep(Duration::from_millis(300));
    say("keys RightShift RightCtrl a");
    for code in [54u16, 97, 30] {
        send_km(InputEvent::Key { code, down: true })?;
        sleep(Duration::from_millis(40));
        send_km(InputEvent::Key { code, down: false })?;
        sleep(Duration::from_millis(100));
    }
    say("scroll down, up");
    send_km(InputEvent::Scroll { dx: 0, dy: -120 })?;
    sleep(Duration::from_millis(200));
    send_km(InputEvent::Scroll { dx: 0, dy: 120 })?;
    sleep(Duration::from_millis(200));
    say("gamepad A + left stick");
    let mut pad = GamepadState { id: 0, ..Default::default() };
    send(InputEvent::Gamepad(pad));
    sleep(Duration::from_millis(500));
    pad.buttons = pad_buttons::A;
    pad.lx = 20000;
    send(InputEvent::Gamepad(pad));
    sleep(Duration::from_millis(100));
    pad.buttons = 0;
    pad.lx = 0;
    send(InputEvent::Gamepad(pad));
    sleep(Duration::from_millis(300));
    if clicks {
        say("clicks: back + left at centre");
        send_km(InputEvent::MouseAbs { x: 32768, y: 32768 })?;
        sleep(Duration::from_millis(200));
        for b in [MouseButton::Back, MouseButton::Left] {
            send_km(InputEvent::MouseButton { button: b, down: true })?;
            sleep(Duration::from_millis(60));
            send_km(InputEvent::MouseButton { button: b, down: false })?;
            sleep(Duration::from_millis(200));
        }
        say("touch drag 0.3,0.3 → 0.4,0.4");
        let p = |v: f64| (v * 65535.0) as u16;
        send_km(InputEvent::Touch { id: 0, phase: TouchPhase::Down, x: p(0.3), y: p(0.3) })?;
        for i in 1..=5 {
            sleep(Duration::from_millis(30));
            let v = 0.3 + 0.02 * i as f64;
            send_km(InputEvent::Touch { id: 0, phase: TouchPhase::Move, x: p(v), y: p(v) })?;
        }
        send_km(InputEvent::Touch { id: 0, phase: TouchPhase::Up, x: p(0.4), y: p(0.4) })?;
        sleep(Duration::from_millis(300));
    }
    send(InputEvent::GamepadDisconnected { id: 0 });
    say("collecting audio");
    sleep(Duration::from_secs(4));
    let cursor=client.cursor();let stats=client.stats();
    say(&format!("cursor positions {} bitmaps {} valid_bitmap {}",stats.cursor_positions,stats.cursor_bitmaps,
        cursor.bitmap.as_ref().is_some_and(|b|b.valid())));
    client.stop();

    let c = out.lock().unwrap();
    say(&format!("video frames {}, audio packets {} ({:.1} s)", c.video_frames, c.audio_packets, c.audio_packets as f64 * 0.005));
    // Analyse the loudest 1 s window: RMS and zero-crossing frequency.
    let win = 48_000;
    if c.pcm.len() >= win {
        let (mut best, mut best_rms) = (0, 0.0f64);
        let mut i = 0;
        while i + win <= c.pcm.len() {
            let rms = (c.pcm[i..i + win].iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / win as f64).sqrt();
            if rms > best_rms {
                (best, best_rms) = (i, rms);
            }
            i += win / 4;
        }
        let w = &c.pcm[best..best + win];
        let zc = w.windows(2).filter(|p| (p[0] < 0.0) != (p[1] < 0.0)).count();
        say(&format!("audio loudest 1 s: rms {:.4} ({:.1} dBFS), ~{} Hz", best_rms, 20.0 * best_rms.max(1e-9).log10(), zc / 2));
    } else {
        say("audio: not enough samples");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_gate_requires_a_recent_probe_heartbeat() {
        let path = std::env::temp_dir().join(format!("streetlight-gate-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        assert!(!fresh_gate(&path));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        use std::io::Write;
        file.write_all(b"ready 1234").unwrap();
        assert!(fresh_gate(&path));
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()-Duration::from_secs(1))).unwrap();
        assert!(!fresh_gate(&path));
        drop(file);
        std::fs::remove_file(path).unwrap();
    }
}
