//! Glue for one stream: media sink (decode on the media thread), audio, gamepads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use gtk4::glib;
use streetlight_client_core::{Client, GamepadState, InputEvent, MediaSink, StreamInfo, StreamRequest, VideoFrame, now_us, pad_buttons as pb};

use crate::audio::AudioOut;
use crate::decode::Decoder;
use crate::view::FrameMailbox;

struct Sink {
    decoder: Option<Decoder>,
    codec: streetlight_client_core::Codec,
    mailbox: Arc<FrameMailbox>,
    wake: Arc<dyn Fn() + Send + Sync>,
    audio: Option<AudioOut>,
    audio_tried: bool,
    client: std::sync::Weak<Client>,
}

impl MediaSink for Sink {
    fn video(&mut self, f: VideoFrame) {
        if self.decoder.is_none() {
            match Decoder::new(self.codec) {
                Ok(d) => self.decoder = Some(d),
                Err(e) => {
                    tracing::error!("{e:#}");
                    return;
                }
            }
        }
        let t0 = now_us();
        match self.decoder.as_mut().unwrap().decode(&f.data) {
            Ok(Some(pic)) => {
                let decode_us = (now_us() - t0) as u32;
                if self.mailbox.put(pic, f.received_us, decode_us) {
                    (self.wake)();
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("decode: {e:#}");
                if let Some(c) = self.client.upgrade() {
                    c.request_idr();
                }
            }
        }
    }

    fn audio(&mut self, packet: &[u8]) {
        if self.audio.is_none() && !self.audio_tried {
            self.audio_tried = true;
            self.audio = AudioOut::new().map_err(|e| tracing::warn!("audio disabled: {e:#}")).ok();
        }
        if let Some(a) = self.audio.as_mut() {
            a.push(packet);
        }
    }
}

pub struct Session {
    pub info: StreamInfo,
    stop: Arc<AtomicBool>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `present` is invoked on the GTK main thread whenever a new frame is waiting.
pub fn start(client: &Arc<Client>, req: StreamRequest, mailbox: Arc<FrameMailbox>, present: impl Fn() + 'static) -> Result<Session> {
    // Wake the main loop at high priority, at most once per pending frame.
    let present = glib::thread_guard::ThreadGuard::new(present);
    let present = Arc::new(present);
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let p = present.clone();
        glib::MainContext::default().invoke_with_priority(glib::Priority::HIGH, move || (p.get_ref())());
    });
    let sink = Sink {
        decoder: None,
        codec: req.codec,
        mailbox,
        wake,
        audio: None,
        audio_tried: false,
        client: Arc::downgrade(client),
    };
    let info = client.start(req, sink)?;
    let stop = Arc::new(AtomicBool::new(false));
    spawn_gamepads(Arc::downgrade(client), stop.clone());
    Ok(Session { info, stop })
}

fn spawn_gamepads(client: std::sync::Weak<Client>, stop: Arc<AtomicBool>) {
    thread::Builder::new()
        .name("gamepads".into())
        .spawn(move || {
            let Ok(mut g) = gilrs::Gilrs::new() else {
                tracing::warn!("gamepad support unavailable");
                return;
            };
            let mut ids: Vec<gilrs::GamepadId> = Vec::new();
            let mut states: Vec<GamepadState> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let Some(c) = client.upgrade() else { return };
                let mut dirty = false;
                // Block briefly for the first event, then drain whatever else is queued.
                let mut next = g.next_event_blocking(Some(Duration::from_millis(50)));
                while let Some(ev) = next {
                    next = g.next_event();
                    let idx = match ids.iter().position(|i| *i == ev.id) {
                        Some(i) => i,
                        None => {
                            ids.push(ev.id);
                            states.push(GamepadState { id: (ids.len() - 1) as u8, ..Default::default() });
                            ids.len() - 1
                        }
                    };
                    if let gilrs::EventType::Disconnected = ev.event {
                        c.send_input(InputEvent::GamepadDisconnected { id: idx as u8 });
                        continue;
                    }
                    states[idx] = snapshot(idx as u8, &g.gamepad(ev.id));
                    dirty = true;
                }
                if dirty {
                    for s in &states {
                        c.send_input(InputEvent::Gamepad(*s));
                    }
                }
            }
        })
        .ok();
}

fn snapshot(id: u8, p: &gilrs::Gamepad<'_>) -> GamepadState {
    use gilrs::{Axis, Button};
    let mut buttons = 0u32;
    for (b, bit) in [
        (Button::South, pb::A),
        (Button::East, pb::B),
        (Button::West, pb::X),
        (Button::North, pb::Y),
        (Button::LeftTrigger, pb::LB),
        (Button::RightTrigger, pb::RB),
        (Button::Select, pb::BACK),
        (Button::Start, pb::START),
        (Button::Mode, pb::GUIDE),
        (Button::LeftThumb, pb::LS),
        (Button::RightThumb, pb::RS),
        (Button::DPadUp, pb::UP),
        (Button::DPadDown, pb::DOWN),
        (Button::DPadLeft, pb::LEFT),
        (Button::DPadRight, pb::RIGHT),
    ] {
        if p.is_pressed(b) {
            buttons |= bit;
        }
    }
    let ax = |a: Axis| (p.value(a).clamp(-1.0, 1.0) * 32767.0) as i16;
    let trig = |b: Button| (p.button_data(b).map(|d| d.value()).unwrap_or(0.0).clamp(0.0, 1.0) * 255.0) as u8;
    GamepadState {
        id,
        buttons,
        lx: ax(Axis::LeftStickX),
        ly: ax(Axis::LeftStickY),
        rx: ax(Axis::RightStickX),
        ry: ax(Axis::RightStickY),
        lt: trig(Button::LeftTrigger2),
        rt: trig(Button::RightTrigger2),
    }
}
