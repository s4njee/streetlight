//! Test-only UDP impairments, disabled unless SL_TEST_* is set. Never use in production.
use anyhow::{Context, Result, bail};
use rand::{Rng, SeedableRng};
use std::time::{Duration, Instant};

pub(crate) struct Impairment {
    rng: rand::rngs::StdRng,
    drop: f64,
    burst_len: usize,
    burst_probability: f64,
    burst_left: usize,
    reorder: f64,
    held: Option<(Instant, Vec<u8>)>,
    schedule: Vec<(Duration, f64)>,
    started: Instant,
}

impl Impairment {
    pub(crate) fn from_env() -> Result<Self> {
        let value = |key| std::env::var(key).ok();
        let mut result = Self::parse(
            value("SL_TEST_DROP").as_deref(),
            value("SL_TEST_BURST").as_deref(),
            value("SL_TEST_REORDER").as_deref(),
            value("SL_TEST_SEED").as_deref(),
        )?;
        if let Some(schedule) = value("SL_TEST_DROP_SCHEDULE") {
            result.schedule = parse_schedule(&schedule)?;
            tracing::warn!(?result.schedule, "TEST ONLY: time-varying packet loss enabled");
        }
        Ok(result)
    }

    fn parse(
        drop: Option<&str>,
        burst: Option<&str>,
        reorder: Option<&str>,
        seed: Option<&str>,
    ) -> Result<Self> {
        fn probability(s: &str) -> Result<f64> {
            let p: f64 = s.parse().context("invalid impairment probability")?;
            if !(0.0..=1.0).contains(&p) {
                bail!("impairment probability must be in [0,1]");
            }
            Ok(p)
        }
        let (burst_len, burst_probability) = if let Some(b) = burst {
            let (n, p) = b
                .split_once(':')
                .context("SL_TEST_BURST must be N:probability")?;
            let n: usize = n.parse()?;
            if n == 0 || n > 1_000_000 {
                bail!("burst length must be 1..=1000000");
            }
            (n, probability(p)?)
        } else {
            (0, 0.0)
        };
        let result = Self {
            rng: rand::rngs::StdRng::seed_from_u64(
                seed.map(str::parse).transpose()?.unwrap_or(0x535452454554),
            ),
            drop: probability(drop.unwrap_or("0"))?,
            burst_len,
            burst_probability,
            burst_left: 0,
            reorder: probability(reorder.unwrap_or("0"))?,
            held: None,
            schedule: Vec::new(),
            started: Instant::now(),
        };
        if result.drop > 0.0 || result.burst_probability > 0.0 || result.reorder > 0.0 {
            tracing::warn!(
                drop = result.drop,
                burst_len,
                burst_probability,
                reorder = result.reorder,
                "TEST ONLY: media impairment enabled"
            );
        }
        Ok(result)
    }

    /// Returns current packet followed by a held packet when reordering. A held
    /// trailing packet is released by poll after 1 ms, even on an idle socket.
    pub(crate) fn push(&mut self, packet: &[u8], out: &mut Vec<Vec<u8>>) -> bool {
        let age = self.started.elapsed();
        while self.schedule.first().is_some_and(|(at, _)| age >= *at) {
            self.drop = self.schedule.remove(0).1;
            tracing::info!("TEST ONLY: packet drop probability now {}", self.drop);
        }
        let dropped = if self.burst_left > 0 {
            self.burst_left -= 1;
            true
        } else if self.burst_probability > 0.0 && self.rng.random_bool(self.burst_probability) {
            self.burst_left = self.burst_len - 1;
            true
        } else {
            self.drop > 0.0 && self.rng.random_bool(self.drop)
        };
        if dropped {
            return true;
        }
        if let Some((_, held)) = self.held.take() {
            out.push(packet.to_vec());
            out.push(held);
        } else if self.reorder > 0.0 && self.rng.random_bool(self.reorder) {
            self.held = Some((Instant::now(), packet.to_vec()));
        } else {
            out.push(packet.to_vec());
        }
        false
    }

    pub(crate) fn poll(&mut self, out: &mut Vec<Vec<u8>>) {
        if self
            .held
            .as_ref()
            .is_some_and(|(t, _)| t.elapsed() >= Duration::from_millis(1))
        {
            out.push(self.held.take().unwrap().1);
        }
    }
}

fn parse_schedule(value: &str) -> Result<Vec<(Duration, f64)>> {
    let mut out = Vec::new();
    for stage in value.split(',') {
        let (at,p) = stage.split_once(':').context("SL_TEST_DROP_SCHEDULE expects seconds:probability,...")?;
        let at: u64 = at.parse()?; let p: f64 = p.parse()?;
        anyhow::ensure!(at <= 3600 && (0.0..=1.0).contains(&p) && out.len() < 100, "invalid loss schedule");
        let at = Duration::from_secs(at);
        anyhow::ensure!(out.last().is_none_or(|(prev,_)| *prev < at), "schedule times must increase");
        out.push((at,p));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn impairment_is_validated_and_deterministic() {
        for bad in ["NaN", "-0.1", "1.01", "oops"] {
            assert!(Impairment::parse(Some(bad), None, None, None).is_err());
        }
        assert!(Impairment::parse(None, Some("0:0.1"), None, None).is_err());
        let mut a =
            Impairment::parse(Some("0.02"), Some("20:0.01"), Some("0.01"), Some("7")).unwrap();
        let mut b =
            Impairment::parse(Some("0.02"), Some("20:0.01"), Some("0.01"), Some("7")).unwrap();
        let (mut x, mut y) = (Vec::new(), Vec::new());
        for n in 0u32..10_000 {
            assert_eq!(
                a.push(&n.to_le_bytes(), &mut x),
                b.push(&n.to_le_bytes(), &mut y)
            );
        }
        assert_eq!(x, y);
        assert!(x.len() < 9800 && x.len() > 6000);
    }
    #[test]
    fn disabled_is_identity_and_reorder_swaps_adjacent_packets() {
        let mut out = Vec::new();
        let mut off = Impairment::parse(None, None, None, None).unwrap();
        off.push(&[1], &mut out);
        off.push(&[2], &mut out);
        assert_eq!(out, vec![vec![1], vec![2]]);
        out.clear();
        let mut reorder = Impairment::parse(None, None, Some("1"), None).unwrap();
        reorder.push(&[1], &mut out);
        assert!(out.is_empty());
        reorder.push(&[2], &mut out);
        assert_eq!(out, vec![vec![2], vec![1]]);
    }
    #[test]
    fn schedule_validates_and_switches_loss_without_restarting_receiver() {
        for bad in ["", "0:NaN", "1:0.1,0:0", "0:1.1", "3601:0"] { assert!(parse_schedule(bad).is_err()); }
        let mut impairment = Impairment::parse(None,None,None,None).unwrap();
        impairment.schedule = parse_schedule("0:1,5:0").unwrap();
        assert!(impairment.push(&[1],&mut Vec::new()));
        impairment.started = Instant::now()-Duration::from_secs(6);
        assert!(!impairment.push(&[1],&mut Vec::new()));
    }
    #[test]
    fn burst_drops_exactly_n_packets() {
        let mut burst = Impairment::parse(None, Some("20:1"), None, None).unwrap();
        let mut out = Vec::new();
        assert!(burst.push(&[1], &mut out));
        burst.burst_probability = 0.0;
        for _ in 1..20 {
            assert!(burst.push(&[1], &mut out));
        }
        assert!(!burst.push(&[2], &mut out));
        assert_eq!(out, vec![vec![2]]);
    }
}
