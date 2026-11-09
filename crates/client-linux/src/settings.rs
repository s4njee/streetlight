//! Client stream preferences (shared shape with the web UI's settings form).

use serde::{Deserialize, Serialize};
use streetlight_client_core::{Codec, ColorSpace, DisplayRequest, StreamRequest};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "native" = match this display, otherwise "WxH".
    pub resolution: String,
    /// 0 = match display refresh.
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    pub fec_percent: u16,
    pub pace_mbps: u32,
    pub virtual_display: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resolution: "native".into(),
            fps: 0,
            bitrate_kbps: 80_000,
            codec: Codec::Hevc,
            fec_percent: 20,
            pace_mbps: 0,
            virtual_display: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub size: Option<(u32, u32)>,
    pub fps: Option<u32>,
    pub bitrate_kbps: Option<u32>,
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read(crate::data_dir().join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(crate::data_dir())?;
        std::fs::write(crate::data_dir().join("settings.json"), serde_json::to_vec_pretty(self).unwrap())
    }

    pub fn request(&self, app: Option<String>, o: &Overrides, native: Option<(u32, u32, u32)>) -> StreamRequest {
        let (nw, nh, nhz) = native.unwrap_or((1920, 1080, 60));
        let size = o.size.unwrap_or_else(|| {
            self.resolution
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                .unwrap_or((nw, nh))
        });
        let fps = o.fps.unwrap_or(if self.fps == 0 { nhz.max(30) } else { self.fps });
        StreamRequest {
            display: DisplayRequest { width: size.0, height: size.1, fps, scale: 1.0, virtual_display: self.virtual_display },
            codec: self.codec,
            color: ColorSpace::Sdr709,
            bitrate_kbps: o.bitrate_kbps.unwrap_or(self.bitrate_kbps),
            fec_percent: self.fec_percent,
            max_shard_bytes: 1400,
            pace_mbps: self.pace_mbps,
            app,
        }
    }
}
