use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("streetlight")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub host_name: String,
    pub render_node: String,
    /// Upper bound applied to client bitrate requests.
    pub max_bitrate_kbps: u32,
    /// "CBR" | "VBR" | "QVBR"
    pub rc_mode: String,
    /// Draw the cursor into the video (true) or hide it.
    pub embed_cursor: bool,
    pub apps: Vec<App>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct App {
    pub id: String,
    pub name: String,
    /// Shell command run (detached) when a client launches this app.
    pub command: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            host_name: hostname(),
            render_node: "/dev/dri/renderD128".into(),
            max_bitrate_kbps: 300_000,
            rc_mode: "VBR".into(),
            embed_cursor: true,
            apps: vec![
                App { id: "desktop".into(), name: "Desktop".into(), command: None },
                App { id: "steam".into(), name: "Steam Big Picture".into(), command: Some("steam steam://open/bigpicture".into()) },
            ],
        }
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "streetlight-host".into())
}

impl Settings {
    pub fn load() -> Self {
        let p = config_dir().join("settings.json");
        match std::fs::read(&p) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!("{}: {e}; using defaults", p.display());
                Self::default()
            }),
            Err(_) => {
                let s = Self::default();
                let _ = s.save();
                s
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(config_dir())?;
        std::fs::write(config_dir().join("settings.json"), serde_json::to_vec_pretty(self).unwrap())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Trust {
    pub clients: Vec<PairedClient>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedClient {
    pub name: String,
    pub fingerprint: String,
    pub paired_at: u64,
}

impl Trust {
    pub fn load() -> Self {
        std::fs::read(config_dir().join("clients.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(config_dir())?;
        std::fs::write(config_dir().join("clients.json"), serde_json::to_vec_pretty(self).unwrap())
    }

    pub fn is_trusted(&self, fp: &str) -> bool {
        self.clients.iter().any(|c| c.fingerprint == fp)
    }
}
