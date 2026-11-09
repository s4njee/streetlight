//! Bounded latency samples, drained by diagnostic reports (all values in µs).
use std::collections::VecDeque;

#[derive(Default)]
pub(crate) struct Window {
    capture_to_complete: VecDeque<u64>,
    decode: VecDeque<u64>,
    queue_to_present: VecDeque<u64>,
    capture_to_present: VecDeque<u64>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Percentiles {
    pub samples: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub capture_to_complete: Percentiles,
    pub decode: Percentiles,
    pub queue_to_present: Percentiles,
    pub capture_to_present: Percentiles,
}

fn push(samples: &mut VecDeque<u64>, us: u64) {
    if samples.len() == 4096 { samples.pop_front(); }
    samples.push_back(us);
}

fn drain(samples: &mut VecDeque<u64>) -> Percentiles {
    let mut values: Vec<_> = samples.drain(..).collect();
    values.sort_unstable();
    let n = values.len();
    let p = |percent: usize| if n == 0 { 0.0 } else { values[(n * percent).div_ceil(100).saturating_sub(1)] as f64 / 1000.0 };
    Percentiles { samples: n, p50_ms: p(50), p95_ms: p(95), p99_ms: p(99) }
}

impl Window {
    pub(crate) fn received(&mut self, us: u64) { push(&mut self.capture_to_complete, us); }
    pub(crate) fn decoded(&mut self, us: u64) { push(&mut self.decode, us); }
    pub(crate) fn presented(&mut self, queue_us: u64, total_us: u64) {
        push(&mut self.queue_to_present, queue_us);
        push(&mut self.capture_to_present, total_us);
    }
    pub(crate) fn drain(&mut self) -> Report {
        Report { capture_to_complete: drain(&mut self.capture_to_complete), decode: drain(&mut self.decode),
            queue_to_present: drain(&mut self.queue_to_present), capture_to_present: drain(&mut self.capture_to_present) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percentiles_use_nearest_rank_and_draining_bounds_memory() {
        let mut w = Window::default();
        for us in 1..=100 { w.decoded(us * 1000); }
        let r = w.drain();
        assert_eq!((r.decode.samples, r.decode.p50_ms, r.decode.p95_ms, r.decode.p99_ms), (100, 50.0, 95.0, 99.0));
        assert_eq!(w.drain().decode.samples, 0);
        for us in 0..5000 { w.received(us); }
        assert_eq!(w.drain().capture_to_complete.samples, 4096);
    }
}
