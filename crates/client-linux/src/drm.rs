//! Linux DRM discovery; card numbers are not stable across boots or machines.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Node {
    pub path: PathBuf,
    pub driver: String,
}

pub fn nodes(render: bool) -> Vec<Node> {
    let mut paths = Vec::new();
    // Prefer stable by-path links; canonicalize for driver lookup and deduplication.
    if let Ok(entries) = std::fs::read_dir("/dev/dri/by-path") {
        for entry in entries.flatten() {
            if let Ok(path) = entry.path().canonicalize() { paths.push(path); }
        }
    }
    if let Ok(entries) = std::fs::read_dir("/dev/dri") {
        paths.extend(entries.flatten().map(|e| e.path()));
    }
    paths.sort();
    paths.dedup();
    let mut nodes: Vec<_> = paths.into_iter().filter_map(|path| {
        let name = path.file_name()?.to_str()?;
        let prefix = if render { "renderD" } else { "card" };
        let number = name.strip_prefix(prefix)?;
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) { return None; }
        let driver = std::fs::read_link(Path::new("/sys/class/drm").join(name).join("device/driver"))
            .ok().and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned())).unwrap_or_default();
        Some(Node { path, driver })
    }).collect();
    if !render {
        nodes.sort_by_key(|n| match n.driver.as_str() { "vc4-drm" => 0, "v3d" => 1, _ => 2 });
    }
    nodes
}

pub fn decoder_candidates(drm: bool) -> Vec<PathBuf> {
    let env = if drm { "SL_DRM_DEVICE" } else { "SL_VAAPI_DEVICE" };
    if let Some(path) = std::env::var_os(env) { return vec![path.into()]; }
    nodes(!drm).into_iter().map(|n| n.path).collect()
}

pub fn has_request_decoder() -> bool {
    std::fs::read_dir("/sys/class/video4linux").is_ok_and(|entries|
        entries.flatten().any(|e| std::fs::read_to_string(e.path().join("name"))
            .is_ok_and(|name| name.trim() == "rpi-hevc-dec")))
}
