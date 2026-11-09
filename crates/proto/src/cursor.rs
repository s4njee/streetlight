//! Cursor pixels travel reliably; frequent positions use independent QUIC datagrams.
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bitmap {
    pub epoch: u64,
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    /// Tightly packed premultiplied RGBA8, at most 256×256.
    pub rgba: Vec<u8>,
}
impl Bitmap {
    pub fn valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= 256
            && self.height <= 256
            && self.rgba.len() == self.width as usize * self.height as usize * 4
            && self.hotspot_x >= 0
            && self.hotspot_y >= 0
            && self.hotspot_x < self.width as i32
            && self.hotspot_y < self.height as i32
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Position {
    pub epoch: u64,
    pub serial: u32,
    pub shape_id: u32,
    pub x: i32,
    pub y: i32,
    pub source_width: u32,
    pub source_height: u32,
    pub visible: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Datagram {
    Cursor(Position),
}
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub position: Position,
    pub bitmap: Option<Arc<Bitmap>>,
}
