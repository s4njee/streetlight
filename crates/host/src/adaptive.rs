//! Conservative, bounded bitrate control from cumulative client feedback.
use std::time::{Duration, Instant};
use streetlight_proto::control::Feedback;

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
    bad_windows: u8,
    rtt_floor: u32,
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
            bad_windows: 0,
            rtt_floor: 0,
            rtt_rising: false,
        }
    }

    pub fn set(&mut self, kbps: u32, now: Instant) -> u32 {
        self.current = kbps.clamp(self.floor(), self.ceiling);
        self.last_change = now;
        self.clean_since = now;
        self.current
    }

    fn floor(&self) -> u32 {
        (self.ceiling / 5).max(1000).min(self.ceiling)
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
        if fb.rtt_us != 0 {
            if self.rtt_floor == 0 {
                self.rtt_floor = fb.rtt_us;
            }
            self.rtt_floor = self.rtt_floor.min(fb.rtt_us);
            self.rtt_rising |= fb.rtt_us
                > self
                    .rtt_floor
                    .saturating_add((self.rtt_floor / 4).max(5000));
        }
        if now.duration_since(self.window_start) < Duration::from_secs(1) {
            return None;
        }
        let active = self.frames != 0 || self.lost != 0;
        let bad = active
            && (self.lost != 0
                || self.idrs != 0
                || self.recovered > (self.frames / 50).max(2)
                || self.rtt_rising);
        self.bad_windows = if bad {
            self.bad_windows.saturating_add(1)
        } else {
            0
        };
        if bad || !active {
            self.clean_since = now;
        }
        let next = if self.bad_windows >= 2
            && now.duration_since(self.last_change) >= Duration::from_secs(2)
        {
            ((self.current as u64 * 85 / 100) as u32).max(self.floor())
        } else if active
            && !bad
            && now.duration_since(self.clean_since) >= Duration::from_secs(5)
            && now.duration_since(self.last_change) >= Duration::from_secs(5)
        {
            (self.current as u64 + (self.current / 20).max(1) as u64).min(self.ceiling as u64)
                as u32
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
    #[test]
    fn sustained_loss_decreases_and_clean_windows_recover_within_request() {
        let now = Instant::now();
        let mut c = Controller::new(30_000, now);
        assert_eq!(c.observe(fb(0, 0, 100), now), None);
        assert_eq!(
            c.observe(fb(60, 3, 100), now + Duration::from_secs(1)),
            None
        );
        assert_eq!(
            c.observe(fb(120, 6, 100), now + Duration::from_secs(2)),
            Some(25_500)
        );
        for t in 3..7 {
            assert_eq!(
                c.observe(fb(t * 60, 6, 100), now + Duration::from_secs(t as u64)),
                None
            );
        }
        assert_eq!(
            c.observe(fb(420, 6, 100), now + Duration::from_secs(7)),
            Some(26_775)
        );
        assert_eq!(c.set(100_000, now), 30_000);
        assert_eq!(c.set(1, now), 6000);
    }
    #[test]
    fn rtt_growth_requires_sustained_active_windows_and_counter_resets_are_safe() {
        let now = Instant::now();
        let mut c = Controller::new(10_000, now);
        c.observe(fb(0, 0, 1000), now);
        c.observe(fb(60, 0, 1000), now + Duration::from_secs(1));
        assert_eq!(
            c.observe(fb(120, 0, 20_000), now + Duration::from_secs(2)),
            None
        );
        assert_eq!(
            c.observe(fb(180, 0, 20_000), now + Duration::from_secs(3)),
            Some(8500)
        );
        assert_eq!(
            c.observe(fb(0, 0, 1000), now + Duration::from_secs(4)),
            None
        );
        for t in 5..12 {
            assert_eq!(
                c.observe(fb(0, 0, 1000), now + Duration::from_secs(t)),
                None
            );
        }
    }
}
