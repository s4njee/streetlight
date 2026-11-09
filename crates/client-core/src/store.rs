//! Hosts this client has paired with (pinned certificate fingerprints).

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownHost {
    pub fingerprint: String,
    pub name: String,
    pub last_addr: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnownHosts {
    pub hosts: Vec<KnownHost>,
}

impl KnownHosts {
    pub fn load(dir: &Path) -> Self {
        std::fs::read(dir.join("hosts.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("hosts.json"), serde_json::to_vec_pretty(self).unwrap())
    }

    pub fn get(&self, fp: &str) -> Option<&KnownHost> {
        self.hosts.iter().find(|h| h.fingerprint == fp)
    }

    pub fn insert(&mut self, fingerprint: String, name: String, last_addr: String) {
        self.hosts.retain(|h| h.fingerprint != fingerprint);
        self.hosts.push(KnownHost { fingerprint, name, last_addr });
    }

    pub fn remove(&mut self, fp: &str) {
        self.hosts.retain(|h| h.fingerprint != fp);
    }
}
