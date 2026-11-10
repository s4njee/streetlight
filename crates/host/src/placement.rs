//! Windows of an app stream onto the stream's monitor.
//!
//! KWin opens a new window on the active screen, which at the start of a
//! stream is the PC's own (Steam Big Picture opened on the main monitor,
//! not the stream's). While a stream runs an app, a KWin script sends every
//! new normal window to the stream's output, keeping full screen if the
//! window had it, and sends it back there if it goes full screen elsewhere
//! (games pick a monitor of their own). Only windows that open during the
//! stream are touched; the script is unloaded when the stream ends.
//! Loaded through KWin's scripting D-Bus API with `busctl`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

const PLUGIN: &str = "streetlight-placement";

pub struct Placement {
    file: PathBuf,
}

fn busctl(args: &[&str]) -> Result<String> {
    let out = Command::new("busctl").arg("--user").arg("call").arg("org.kde.KWin").args(args).output().context("running busctl")?;
    if !out.status.success() {
        bail!("busctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn script(output: &str) -> String {
    // The name as a JS string literal
    let name = serde_json::to_string(output).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"// Streetlight: windows opened during an app stream go to its monitor
const NAME = {name};
function target() {{ return workspace.screens.find(o => o.name === NAME); }}
function place(w) {{
    const t = target();
    if (!t || !w || !w.normalWindow || w.output === t) return;
    const full = w.fullScreen;
    workspace.sendClientToScreen(w, t);
    if (full) {{ w.fullScreen = false; w.fullScreen = true; }}
}}
workspace.windowAdded.connect(w => {{
    place(w);
    // A game going full screen on another monitor comes back
    w.fullScreenChanged.connect(() => {{ if (w.fullScreen) place(w); }});
}});
"#
    )
}

impl Placement {
    /// Starts placing new windows on `output` (a KWin connector name,
    /// `Virtual-…`) until dropped.
    pub fn start(output: &str) -> Result<Self> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).context("XDG_RUNTIME_DIR not set")?.join("streetlight");
        std::fs::create_dir_all(&dir)?;
        let file = dir.join("placement.js");
        std::fs::write(&file, script(output))?;
        // One stream at a time: a script left by a host that didn't stop cleanly goes first
        let _ = busctl(&["/Scripting", "org.kde.kwin.Scripting", "unloadScript", "s", PLUGIN]);
        let loaded = busctl(&["/Scripting", "org.kde.kwin.Scripting", "loadScript", "ss", &file.to_string_lossy(), PLUGIN])?;
        // "i 3": the script's id
        let id: i32 = loaded.split_whitespace().nth(1).and_then(|s| s.parse().ok()).context("loadScript's reply")?;
        anyhow::ensure!(id >= 0, "KWin didn't load the placement script");
        busctl(&[&format!("/Scripting/Script{id}"), "org.kde.kwin.Script", "run"])?;
        tracing::info!(output, "new windows go to the stream's monitor");
        Ok(Self { file })
    }
}

impl Drop for Placement {
    fn drop(&mut self) {
        if let Err(e) = busctl(&["/Scripting", "org.kde.kwin.Scripting", "unloadScript", "s", PLUGIN]) {
            tracing::warn!("unloading the placement script: {e:#}");
        }
        let _ = std::fs::remove_file(&self.file);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn script_quotes_the_output_name() {
        let s = super::script(r#"Virtual-a"b"#);
        assert!(s.contains(r#"const NAME = "Virtual-a\"b";"#));
    }
}
