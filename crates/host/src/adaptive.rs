//! Conservative, bounded bitrate control from cumulative client feedback.
//!
//! Two signals, treated differently:
//!
//! - **Loss** (frames lost, IDR requests, or more than 2 % of frames rebuilt
//!   by FEC): the link is dropping packets. Two bad one-second windows in a
//!   row cut 15 %, down to a fifth of the request.
//! - **Queueing delay** (RTT at least double the recent baseline and 15 ms
//!   above it): buffers are filling. Three bad windows in a row cut 10 %,
//!   but never below half the request. The baseline is the lowest RTT of
//!   the last 10–20 s, not of the whole stream: on Wi-Fi the RTT wanders
//!   between 5 and 15 ms with no congestion at all, and an iPad stream at
//!   2732×2048 was once cut from 30 to 8 Mbit/s with no frame lost because a
//!   5 ms floor made 14 ms look like congestion.
//!
//! Clean windows for 3 s raise the rate 10 % at a time, up to the request.
use std::time::{Duration, Instant};
use streetlight_proto::control::Feedback;

/// How long each half of the RTT baseline's window lasts
const RTT_BUCKET: Duration = Duration::from_secs(10);
/// Queueing delay that counts as congestion, on top of doubling the baseline
const RTT_QUEUE_US: u32 = 15_000;

pub struct Controller {
    ceiling: u32,
    current: u32,
    previous: Option<Feedback>,
    window_start: Instant,
    last_change: Instant,
    clean_since: Instant,
    lost: u32,
    recovered: u32,
    frames: u32,
    idrs: u32,
    /// Consecutive windows with loss, and with queueing delay
    loss_windows: u8,
    delay_windows: u8,
    /// Lowest RTT in the previous and the current bucket (0: none yet)
    rtt_min_prev: u32,
    rtt_min_cur: u32,
    rtt_bucket_start: Instant,
    rtt_rising: bool,
}

impl Controller {
    pub fn new(ceiling: u32, now: Instant) -> Self {
        Self {
            ceiling,
            current: ceiling,
            previous: None,
            window_start: now,
            last_change: now,
            clean_since: now,
            lost: 0,
            recovered: 0,
            frames: 0,
            idrs: 0,
            loss_windows: 0,
            delay_windows: 0,
            rtt_min_prev: 0,
            rtt_min_cur: 0,
            rtt_bucket_start: now,
            rtt_rising: false,
        }
    }

    pub fn set(&mut self, kbps: u32, now: Instant) -> u32 {
        self.current = kbps.clamp(self.floor(), self.ceiling);
        self.last_change = now;
        self.clean_since = now;
        self.current
    }

    /// The lowest rate loss can take it to
    fn floor(&self) -> u32 {
        (self.ceiling / 5).max(1000).min(self.ceiling)
    }

    /// The lowest rate delay alone can take it to
    fn delay_floor(&self) -> u32 {
        (self.ceiling / 2).max(self.floor())
    }

    /// The lowest RTT of the last 10–20 s
    fn rtt_base(&self) -> u32 {
        match (self.rtt_min_prev, self.rtt_min_cur) {
            (0, c) => c,
            (p, 0) => p,
            (p, c) => p.min(c),
        }
    }

    fn observe_rtt(&mut self, rtt: u32, now: Instant) {
        if rtt == 0 {
            return;
        }
        if now.duration_since(self.rtt_bucket_start) >= RTT_BUCKET {
            self.rtt_min_prev = self.rtt_min_cur;
            self.rtt_min_cur = 0;
            self.rtt_bucket_start = now;
        }
        let base = self.rtt_base();
        if base != 0 && rtt > base.saturating_mul(2) && rtt > base.saturating_add(RTT_QUEUE_US) {
            self.rtt_rising = true;
        }
        self.rtt_min_cur = if self.rtt_min_cur == 0 { rtt } else { self.rtt_min_cur.min(rtt) };
    }

    pub fn observe(&mut self, fb: Feedback, now: Instant) -> Option<u32> {
        let old = self.previous.replace(fb.clone())?;
        // Stream counters may restart; never treat a reset as billions of losses.
        if fb.frames_lost < old.frames_lost
            || fb.frames_recovered < old.frames_recovered
            || fb.idr_requests < old.idr_requests
        {
            self.reset_window(now);
            return None;
        }
        self.lost = self.lost.saturating_add(fb.frames_lost - old.frames_lost);
        self.recovered = self
            .recovered
            .saturating_add(fb.frames_recovered - old.frames_recovered);
        self.idrs = self.idrs.saturating_add(fb.idr_requests - old.idr_requests);
        self.frames = self
            .frames
            .saturating_add(fb.last_frame.wrapping_sub(old.last_frame).min(1000));
        self.observe_rtt(fb.rtt_us, now);
        if now.duration_since(self.window_start) < Duration::from_secs(1) {
            return None;
        }
        let active = self.frames != 0 || self.lost != 0;
        let lossy = active && (self.lost != 0 || self.idrs != 0 || self.recovered > (self.frames / 50).max(2));
        let delayed = active && self.rtt_rising;
        self.loss_windows = if lossy { self.loss_windows.saturating_add(1) } else { 0 };
        self.delay_windows = if delayed { self.delay_windows.saturating_add(1) } else { 0 };
        if lossy || delayed || !active {
            self.clean_since = now;
        }
        let settled = now.duration_since(self.last_change) >= Duration::from_secs(2);
        let next = if self.loss_windows >= 2 && settled {
            ((self.current as u64 * 85 / 100) as u32).max(self.floor())
        } else if self.delay_windows >= 3 && settled && self.current > self.delay_floor() {
            ((self.current as u64 * 90 / 100) as u32).max(self.delay_floor())
        } else if active
            && !lossy
            && !delayed
            && now.duration_since(self.clean_since) >= Duration::from_secs(3)
            && now.duration_since(self.last_change) >= Duration::from_secs(3)
        {
            (self.current as u64 + (self.current / 10).max(1) as u64).min(self.ceiling as u64) as u32
        } else {
            self.current
        };
        self.reset_window(now);
        if next == self.current {
            None
        } else {
            self.current = next;
            self.last_change = now;
            self.clean_since = now;
            Some(next)
        }
    }

    fn reset_window(&mut self, now: Instant) {
        self.window_start = now;
        self.lost = 0;
        self.recovered = 0;
        self.frames = 0;
        self.idrs = 0;
        self.rtt_rising = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fb(frame: u32, lost: u32, rtt: u32) -> Feedback {
        Feedback {
            last_frame: frame,
            frames_lost: lost,
            frames_recovered: 0,
            idr_requests: 1,
            decode_us_avg: 0,
            present_us_avg: 0,
            rtt_us: rtt,
        }
    }
    fn secs(now: Instant, s: u64) -> Instant {
        now + Duration::from_secs(s)
    }

    #[test]
    fn sustained_loss_decreases_and_clean_windows_recover_within_request() {
        let now = Instant::now();
        let mut c = Controller::new(30_000, now);
        assert_eq!(c.observe(fb(0, 0, 5000), now), None);
        assert_eq!(c.observe(fb(60, 3, 5000), secs(now, 1)), None);
        assert_eq!(c.observe(fb(120, 6, 5000), secs(now, 2)), Some(25_500));
        // Clean for 3 s after the change: back up 10 %
        for t in 3..5 {
            assert_eq!(c.observe(fb(t * 60, 6, 5000), secs(now, t as u64)), None);
        }
        assert_eq!(c.observe(fb(300, 6, 5000), secs(now, 5)), Some(28_050));
        assert_eq!(c.set(100_000, now), 30_000);
        assert_eq!(c.set(1, now), 6000);
    }

    #[test]
    fn wifi_jitter_is_not_congestion() {
        // The iPad's case: 5 ms at best, wandering to 14 ms with nothing lost
        let now = Instant::now();
        let mut c = Controller::new(40_000, now);
        c.observe(fb(0, 0, 5000), now);
        for t in 1..30u32 {
            let rtt = if t % 2 == 0 { 14_000 } else { 6_000 };
            assert_eq!(c.observe(fb(t * 60, 0, rtt), secs(now, t as u64)), None, "t={t}");
        }
    }

    #[test]
    fn queueing_delay_cuts_gently_and_not_below_half() {
        let now = Instant::now();
        let mut c = Controller::new(10_000, now);
        c.observe(fb(0, 0, 1000), now);
        c.observe(fb(60, 0, 1000), secs(now, 1));
        assert_eq!(c.observe(fb(120, 0, 20_000), secs(now, 2)), None);
        assert_eq!(c.observe(fb(180, 0, 20_000), secs(now, 3)), None);
        assert_eq!(c.observe(fb(240, 0, 20_000), secs(now, 4)), Some(9000));
        let mut rate = 9000;
        for t in 5..40u32 {
            if let Some(r) = c.observe(fb(t * 60, 0, 20_000), secs(now, t as u64)) {
                rate = r;
            }
        }
        // While the delay lasts the baseline rises to it; either way, never
        // below half the request
        assert!(rate >= 5000, "rate {rate}");
    }

    #[test]
    fn counter_resets_are_safe() {
        let now = Instant::now();
        let mut c = Controller::new(10_000, now);
        c.observe(fb(0, 5, 1000), now);
        assert_eq!(c.observe(fb(0, 0, 1000), secs(now, 1)), None);
        for t in 2..12 {
            assert_eq!(c.observe(fb(0, 0, 1000), secs(now, t)), None);
        }
    }
}
