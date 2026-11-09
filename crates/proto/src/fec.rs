//! Frame → equal-size encrypted shards (+ Reed-Solomon parity), and the inverse.
//!
//! All packets of a frame have identical length, so the sender can hand the whole
//! batch to the kernel in one `sendmsg` with UDP GSO.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use reed_solomon_simd::{ReedSolomonDecoder, ReedSolomonEncoder};

use crate::crypto::MediaCipher;
use crate::media::{HEADER_LEN, MediaHeader, StreamKind, TAG_LEN, flags};

#[derive(Debug, Clone, Copy)]
pub struct FecConfig {
    /// Maximum shard payload bytes (even).
    pub max_shard_bytes: u16,
    /// Parity shards as a percentage of data shards.
    pub percent: u16,
    /// Minimum parity shards per frame (when percent > 0).
    pub min_parity: u16,
}

impl Default for FecConfig {
    fn default() -> Self {
        Self {
            max_shard_bytes: crate::media::DEFAULT_SHARD_BYTES,
            percent: 20,
            min_parity: 2,
        }
    }
}

/// A frame's packets laid out back to back, each `packet_len` bytes.
pub struct PacketBatch<'a> {
    pub buf: &'a [u8],
    pub packet_len: usize,
    pub count: usize,
}

impl PacketBatch<'_> {
    pub fn packets(&self) -> impl Iterator<Item = &[u8]> {
        self.buf.chunks_exact(self.packet_len)
    }
}

pub struct Packetizer {
    cfg: FecConfig,
    cipher: MediaCipher,
    encoder: Option<ReedSolomonEncoder>,
    buf: Vec<u8>,
}

impl Packetizer {
    pub fn new(cfg: FecConfig, cipher: MediaCipher) -> Self {
        assert!(cfg.max_shard_bytes >= 64 && cfg.max_shard_bytes.is_multiple_of(2));
        Self {
            cfg,
            cipher,
            encoder: None,
            buf: Vec::new(),
        }
    }

    pub fn set_fec_percent(&mut self, p: u16) {
        self.cfg.percent = p;
    }

    pub fn packetize(
        &mut self,
        stream: StreamKind,
        frame_number: u32,
        capture_us: u32,
        frame_flags: u8,
        payload: &[u8],
    ) -> PacketBatch<'_> {
        let max = self.cfg.max_shard_bytes as usize;
        let data_shards = payload.len().div_ceil(max).max(1);
        assert!(
            data_shards < 30000,
            "frame too large: {} bytes",
            payload.len()
        );
        let shard_bytes = round_even(payload.len().div_ceil(data_shards).max(2));
        let parity_shards = if self.cfg.percent == 0 {
            0
        } else {
            (data_shards * self.cfg.percent as usize)
                .div_ceil(100)
                .max(self.cfg.min_parity as usize)
                .min(30000)
        };
        let total = data_shards + parity_shards;
        let packet_len = HEADER_LEN + shard_bytes + TAG_LEN;
        self.buf.clear();
        self.buf.resize(total * packet_len, 0);

        let mut hdr = MediaHeader {
            stream,
            shard_index: 0,
            frame_number,
            data_shards: data_shards as u16,
            parity_shards: parity_shards as u16,
            frame_bytes: payload.len() as u32,
            capture_us,
            shard_bytes: shard_bytes as u16,
            flags: frame_flags,
        };

        // Lay out data shards (zero padded) in their packet slots.
        for i in 0..data_shards {
            let src = &payload
                [(i * shard_bytes).min(payload.len())..((i + 1) * shard_bytes).min(payload.len())];
            let off = i * packet_len + HEADER_LEN;
            self.buf[off..off + src.len()].copy_from_slice(src);
        }

        if parity_shards > 0 {
            let enc = match self.encoder.as_mut() {
                Some(e) => {
                    e.reset(data_shards, parity_shards, shard_bytes)
                        .expect("rs reset");
                    e
                }
                None => self.encoder.insert(
                    ReedSolomonEncoder::new(data_shards, parity_shards, shard_bytes)
                        .expect("rs new"),
                ),
            };
            for i in 0..data_shards {
                let off = i * packet_len + HEADER_LEN;
                enc.add_original_shard(&self.buf[off..off + shard_bytes])
                    .expect("rs add");
            }
            let result = enc.encode().expect("rs encode");
            for (j, rec) in result.recovery_iter().enumerate() {
                let off = (data_shards + j) * packet_len + HEADER_LEN;
                self.buf[off..off + shard_bytes].copy_from_slice(rec);
            }
        }

        for i in 0..total {
            hdr.shard_index = i as u16;
            let pkt = &mut self.buf[i * packet_len..(i + 1) * packet_len];
            hdr.write(&mut pkt[..HEADER_LEN]);
            self.cipher.seal(&hdr, pkt);
        }

        PacketBatch {
            buf: &self.buf,
            packet_len,
            count: total,
        }
    }
}

fn round_even(n: usize) -> usize {
    (n + 1) & !1
}

#[derive(Debug)]
pub enum Assembled {
    Frame {
        frame_number: u32,
        data: Vec<u8>,
        keyframe: bool,
        capture_us: u32,
        recovered: usize,
    },
    /// Frames in `first..=last` will never complete.
    Lost { first: u32, last: u32 },
}

struct Partial {
    hdr: MediaHeader,
    data: Vec<u8>,
    have: Vec<bool>,
    data_have: usize,
    recovery: Vec<(usize, Vec<u8>)>,
}

/// Per-stream reassembler. Delivers in order, allowing a 2 ms reordering window
/// before a newer complete frame declares an older incomplete frame lost.
/// Call `poll` on receive timeouts so a trailing gap cannot stall delivery.
pub struct Reassembler {
    stream: StreamKind,
    cipher: MediaCipher,
    pending: BTreeMap<u32, Partial>,
    complete: BTreeMap<u32, Assembled>,
    gap_since: Option<Instant>,
    /// Next frame number we expect to deliver.
    next: Option<u32>,
    decoder: Option<ReedSolomonDecoder>,
    pub stats: ReassemblyStats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReassemblyStats {
    pub packets: u64,
    pub bad_packets: u64,
    pub frames: u64,
    pub frames_recovered: u64,
    pub frames_lost: u64,
}

impl Reassembler {
    pub fn new(stream: StreamKind, cipher: MediaCipher) -> Self {
        Self {
            stream,
            cipher,
            pending: BTreeMap::new(),
            complete: BTreeMap::new(),
            gap_since: None,
            next: None,
            decoder: None,
            stats: Default::default(),
        }
    }

    /// Feed one datagram. `out` receives zero or more events.
    pub fn push(&mut self, packet: &mut [u8], out: &mut Vec<Assembled>) {
        self.stats.packets += 1;
        let Some(hdr) = MediaHeader::read(packet) else {
            self.stats.bad_packets += 1;
            return;
        };
        if hdr.stream != self.stream
            || hdr.data_shards == 0
            || hdr.shard_bytes < 2
            || !hdr.shard_bytes.is_multiple_of(2)
            || hdr.frame_bytes as usize > hdr.data_shards as usize * hdr.shard_bytes as usize
            || hdr.shard_index as usize >= hdr.total_shards()
            || packet.len() != HEADER_LEN + hdr.shard_bytes as usize + TAG_LEN
        {
            self.stats.bad_packets += 1;
            return;
        }
        if let Some(next) = self.next
            && (hdr.frame_number.wrapping_sub(next) as i32) < 0
        {
            return; // late packet for a frame already delivered or dropped
        }
        let Some(shard) = self.cipher.open(&hdr, packet) else {
            self.stats.bad_packets += 1;
            return;
        };

        self.next.get_or_insert(hdr.frame_number);
        if self.complete.contains_key(&hdr.frame_number) {
            return;
        }
        let sb = hdr.shard_bytes as usize;
        let part = self
            .pending
            .entry(hdr.frame_number)
            .or_insert_with(|| Partial {
                hdr,
                data: vec![0; hdr.data_shards as usize * sb],
                have: vec![false; hdr.total_shards()],
                data_have: 0,
                recovery: Vec::new(),
            });
        let idx = hdr.shard_index as usize;
        if part.hdr.data_shards != hdr.data_shards
            || part.hdr.parity_shards != hdr.parity_shards
            || part.hdr.shard_bytes != hdr.shard_bytes
            || part.hdr.frame_bytes != hdr.frame_bytes
            || part.hdr.flags != hdr.flags
            || part.hdr.capture_us != hdr.capture_us
            || part.have[idx]
        {
            return;
        }
        part.have[idx] = true;
        let data_shards = hdr.data_shards as usize;
        if idx < data_shards {
            part.data[idx * sb..(idx + 1) * sb].copy_from_slice(shard);
            part.data_have += 1;
        } else {
            part.recovery.push((idx - data_shards, shard.to_vec()));
        }

        if part.data_have + part.recovery.len() < data_shards {
            return;
        }

        // Complete (possibly after recovery).
        let mut part = self.pending.remove(&hdr.frame_number).unwrap();
        let mut recovered = 0;
        if part.data_have < data_shards {
            let dec = match self.decoder.as_mut() {
                Some(d) => {
                    d.reset(data_shards, part.hdr.parity_shards as usize, sb)
                        .expect("rs reset");
                    d
                }
                None => self.decoder.insert(
                    ReedSolomonDecoder::new(data_shards, part.hdr.parity_shards as usize, sb)
                        .expect("rs new"),
                ),
            };
            for i in 0..data_shards {
                if part.have[i] {
                    dec.add_original_shard(i, &part.data[i * sb..(i + 1) * sb])
                        .expect("rs add");
                }
            }
            for (j, r) in &part.recovery {
                dec.add_recovery_shard(*j, r).expect("rs add rec");
            }
            match dec.decode() {
                Ok(res) => {
                    for (i, s) in res.restored_original_iter() {
                        part.data[i * sb..(i + 1) * sb].copy_from_slice(s);
                        recovered += 1;
                    }
                }
                Err(_) => {
                    self.stats.bad_packets += 1;
                    return;
                }
            }
        }
        part.data.truncate(part.hdr.frame_bytes as usize);

        self.complete.insert(
            hdr.frame_number,
            Assembled::Frame {
                frame_number: hdr.frame_number,
                data: part.data,
                keyframe: part.hdr.flags & flags::KEYFRAME != 0,
                capture_us: part.hdr.capture_us,
                recovered,
            },
        );
        self.poll(out);
    }

    pub fn poll(&mut self, out: &mut Vec<Assembled>) {
        self.poll_at(Instant::now(), out);
    }

    fn poll_at(&mut self, now: Instant, out: &mut Vec<Assembled>) {
        while let Some(next) = self.next {
            if let Some(frame) = self.complete.remove(&next) {
                self.stats.frames += 1;
                if matches!(frame, Assembled::Frame { recovered, .. } if recovered > 0) {
                    self.stats.frames_recovered += 1;
                }
                out.push(frame);
                self.pending.remove(&next);
                self.next = Some(next.wrapping_add(1));
                self.gap_since = None;
                continue;
            }
            let Some(n) = self
                .complete
                .keys()
                .copied()
                .min_by_key(|n| n.wrapping_sub(next))
            else {
                break;
            };
            let since = *self.gap_since.get_or_insert(now);
            if now.duration_since(since) < Duration::from_millis(2) {
                break;
            }
            out.push(Assembled::Lost {
                first: next,
                last: n.wrapping_sub(1),
            });
            self.stats.frames_lost += n.wrapping_sub(next) as u64;
            self.pending.retain(|&k, _| (k.wrapping_sub(n) as i32) >= 0);
            self.next = Some(n);
            self.gap_since = None;
        }
    }

    /// Forget sequencing (e.g. after the host restarts the stream).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.complete.clear();
        self.gap_since = None;
        self.next = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Packetizer, Reassembler) {
        let key = MediaCipher::random_key();
        (
            Packetizer::new(FecConfig::default(), MediaCipher::new(&key)),
            Reassembler::new(StreamKind::Video, MediaCipher::new(&key)),
        )
    }

    fn frame(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn roundtrip_no_loss() {
        let (mut p, mut r) = pair();
        let mut out = Vec::new();
        for (n, len) in [1usize, 7, 1400, 1401, 250_000].into_iter().enumerate() {
            let f = frame(len, n as u8);
            let b = p.packetize(StreamKind::Video, n as u32, 42, flags::KEYFRAME, &f);
            let pkts: Vec<Vec<u8>> = b.packets().map(|x| x.to_vec()).collect();
            for mut pk in pkts {
                r.push(&mut pk, &mut out);
            }
            match out.pop().unwrap() {
                Assembled::Frame {
                    data,
                    frame_number,
                    keyframe,
                    ..
                } => {
                    assert_eq!(data, f);
                    assert_eq!(frame_number, n as u32);
                    assert!(keyframe);
                }
                e => panic!("{e:?}"),
            }
        }
    }

    #[test]
    fn recovers_from_loss_and_reports_lost_frames() {
        let (mut p, mut r) = pair();
        let mut out = Vec::new();
        // Frame 0: lose 20% of data shards -> recovered.
        let f0 = frame(100_000, 1);
        let b = p.packetize(StreamKind::Video, 0, 0, 0, &f0);
        let pkts: Vec<Vec<u8>> = b.packets().map(|x| x.to_vec()).collect();
        let n = pkts.len();
        for (i, mut pk) in pkts.into_iter().enumerate() {
            if i % 6 == 0 && i < n - 3 {
                continue;
            }
            r.push(&mut pk, &mut out);
        }
        assert!(
            matches!(&out[..], [Assembled::Frame { data, recovered, .. }] if *data == f0 && *recovered > 0)
        );
        out.clear();
        // Frame 1 dropped entirely, frame 2 arrives.
        let f2 = frame(5000, 2);
        let _ = p.packetize(StreamKind::Video, 1, 0, 0, &frame(5000, 9));
        let b = p.packetize(StreamKind::Video, 2, 0, 0, &f2);
        let pkts: Vec<Vec<u8>> = b.packets().map(|x| x.to_vec()).collect();
        for mut pk in pkts {
            r.push(&mut pk, &mut out);
        }
        r.poll_at(Instant::now() + Duration::from_millis(3), &mut out);
        assert!(matches!(out[0], Assembled::Lost { first: 1, last: 1 }));
        assert!(matches!(&out[1], Assembled::Frame { frame_number: 2, data, .. } if *data == f2));
    }

    #[test]
    fn waits_for_reordered_frames_and_flushes_trailing_gap() {
        let (mut p, mut r) = pair();
        let mut out = Vec::new();
        let packets: Vec<Vec<Vec<u8>>> = (0..5)
            .map(|n| {
                p.packetize(StreamKind::Video, n, 0, 0, &frame(5000, n as u8))
                    .packets()
                    .map(<[u8]>::to_vec)
                    .collect()
            })
            .collect();
        for mut packet in packets[0].clone() {
            r.push(&mut packet, &mut out);
        }
        out.clear();
        // N+1 completes before the last shards of N; neither is spuriously lost.
        for mut packet in packets[2].clone() {
            r.push(&mut packet, &mut out);
        }
        assert!(out.is_empty());
        for mut packet in packets[1].clone() {
            r.push(&mut packet, &mut out);
        }
        assert!(matches!(
            &out[..],
            [
                Assembled::Frame {
                    frame_number: 1,
                    ..
                },
                Assembled::Frame {
                    frame_number: 2,
                    ..
                }
            ]
        ));
        out.clear();
        for mut packet in packets[4].clone() {
            r.push(&mut packet, &mut out);
        }
        assert!(out.is_empty());
        r.poll_at(Instant::now() + Duration::from_millis(3), &mut out);
        assert!(matches!(
            &out[..],
            [
                Assembled::Lost { first: 3, last: 3 },
                Assembled::Frame {
                    frame_number: 4,
                    ..
                }
            ]
        ));
        assert_eq!(r.stats.frames_lost, 1);
    }

    #[test]
    fn reordered_sequence_wraps_and_reset_discards_buffered_frames() {
        let (mut p, mut r) = pair();
        let mut out = Vec::new();
        for n in [u32::MAX - 1, 0, u32::MAX] {
            for packet in p.packetize(StreamKind::Video, n, 0, 0, &[1]).packets() {
                r.push(&mut packet.to_vec(), &mut out);
            }
        }
        assert!(matches!(
            &out[..],
            [
                Assembled::Frame {
                    frame_number: 4294967294,
                    ..
                },
                Assembled::Frame {
                    frame_number: 4294967295,
                    ..
                },
                Assembled::Frame {
                    frame_number: 0,
                    ..
                }
            ]
        ));
        assert_eq!(r.stats.frames_lost, 0);
        r.reset();
        out.clear();
        for packet in p.packetize(StreamKind::Video, 20, 0, 0, &[1]).packets() {
            r.push(&mut packet.to_vec(), &mut out);
        }
        assert!(matches!(
            &out[..],
            [Assembled::Frame {
                frame_number: 20,
                ..
            }]
        ));
    }

    #[test]
    fn rejects_tampered_packet() {
        let (mut p, mut r) = pair();
        let mut out = Vec::new();
        let b = p.packetize(StreamKind::Video, 0, 0, 0, &frame(10, 0));
        let mut pk = b.packets().next().unwrap().to_vec();
        pk[30] ^= 1;
        r.push(&mut pk, &mut out);
        assert!(out.is_empty());
        assert_eq!(r.stats.bad_packets, 1);
    }
}
