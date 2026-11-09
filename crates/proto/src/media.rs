//! Media packet header (see docs/ARCHITECTURE.md).

pub const MAGIC: u8 = 0x5A;
pub const HEADER_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
/// Default shard payload. 24 hdr + 1400 + 16 tag = 1440 B, fits a 1500 MTU over IPv4/IPv6 UDP.
pub const DEFAULT_SHARD_BYTES: u16 = 1400;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Video = 0,
    Audio = 1,
    /// Client → host hole-punch / address discovery. Payload = session token.
    Probe = 2,
}

impl StreamKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Video),
            1 => Some(Self::Audio),
            2 => Some(Self::Probe),
            _ => None,
        }
    }
}

pub mod flags {
    pub const KEYFRAME: u8 = 1 << 0;
    pub const END_OF_STREAM: u8 = 1 << 1;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    pub stream: StreamKind,
    pub shard_index: u16,
    pub frame_number: u32,
    pub data_shards: u16,
    pub parity_shards: u16,
    pub frame_bytes: u32,
    pub capture_us: u32,
    pub shard_bytes: u16,
    pub flags: u8,
}

impl MediaHeader {
    pub fn write(&self, out: &mut [u8]) {
        out[0] = MAGIC;
        out[1] = self.stream as u8;
        out[2..4].copy_from_slice(&self.shard_index.to_le_bytes());
        out[4..8].copy_from_slice(&self.frame_number.to_le_bytes());
        out[8..10].copy_from_slice(&self.data_shards.to_le_bytes());
        out[10..12].copy_from_slice(&self.parity_shards.to_le_bytes());
        out[12..16].copy_from_slice(&self.frame_bytes.to_le_bytes());
        out[16..20].copy_from_slice(&self.capture_us.to_le_bytes());
        out[20..22].copy_from_slice(&self.shard_bytes.to_le_bytes());
        out[22] = self.flags;
        out[23] = 0;
    }

    pub fn read(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN || buf[0] != MAGIC {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
        Some(Self {
            stream: StreamKind::from_u8(buf[1])?,
            shard_index: u16_at(2),
            frame_number: u32_at(4),
            data_shards: u16_at(8),
            parity_shards: u16_at(10),
            frame_bytes: u32_at(12),
            capture_us: u32_at(16),
            shard_bytes: u16_at(20),
            flags: buf[22],
        })
    }

    /// 96-bit GCM nonce, unique per (stream, frame, shard) within a session key.
    pub fn nonce(&self) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[0] = self.stream as u8;
        n[2..4].copy_from_slice(&self.shard_index.to_le_bytes());
        n[4..8].copy_from_slice(&self.frame_number.to_le_bytes());
        n
    }

    pub fn total_shards(&self) -> usize {
        self.data_shards as usize + self.parity_shards as usize
    }
}
