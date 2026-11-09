//! Host web UI + JSON API, bound to loopback only (the person at the host PC
//! approves pairings and edits settings here).

use anyhow::{Context, bail};
use axum::extract::Request;
use axum::http::Method;
use axum::middleware::{self, Next};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rust_embed::Embed;
use serde::Deserialize;
use serde_json::json;
use streetlight_proto::pair;

use crate::config::Settings;
use crate::server::AppState;

#[derive(Embed)]
#[folder = "../../web/host/"]
struct Assets;

pub async fn run(state: Arc<AppState>, port: u16) -> anyhow::Result<()> {
    // Bind before publishing a new token: a second host must not invalidate the first.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let token = new_token();
    write_token(&token_path()?, &token)?;
    let app = router(state, token);
    tracing::info!("web UI on http://127.0.0.1:{port}/");
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn token_path() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR")
            .context("XDG_RUNTIME_DIR is required for the host API token")?,
    )
    .join("streetlight/token"))
}

/// Publish atomically without following a token symlink. The containing directory
/// must belong to this user and be private, including when it already exists.
pub fn write_token(path: &FsPath, token: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let dir = path.parent().context("token directory")?;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    check_private(dir, true)?;
    let tmp = dir.join(format!(".token-{}", new_token()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(token.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.context("publish API token")
}

fn check_private(path: &FsPath, directory: bool) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o777 != if directory { 0o700 } else { 0o600 }
        || if directory {
            !meta.is_dir()
        } else {
            !meta.is_file()
        }
    {
        bail!(
            "{} must be owned by this user with mode {}",
            path.display(),
            if directory { "0700" } else { "0600" }
        );
    }
    Ok(())
}

const AUTHORITIES: [&str; 3] = ["127.0.0.1:47990", "localhost:47990", "[::1]:47990"];

async fn guard(State(token): State<Arc<String>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let single = |name: &str| {
        let mut values = headers.get_all(name).iter();
        let first = values.next().and_then(|v| v.to_str().ok());
        if values.next().is_some() { None } else { first }
    };
    let host_ok = single("host").is_some_and(|h| AUTHORITIES.contains(&h));
    let origin_ok = !headers.contains_key("origin")
        || single("origin").is_some_and(|o| AUTHORITIES.iter().any(|h| o == format!("http://{h}")));
    let mutation = request.method() != Method::GET;
    let token_ok = single("x-streetlight-token").is_some_and(|t| t == token.as_str());
    if !host_ok || (mutation && (!origin_ok || !token_ok)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}

/// The fixed Host allowlist remains in force when tests serve this on an ephemeral port.
pub fn router(state: Arc<AppState>, token: String) -> Router {
    let token = Arc::new(token);
    Router::new()
        .route("/api/status", get(status))
        .route("/api/pair", post(submit_pin))
        .route("/api/clients/{fp}", delete(unpair))
        .route("/api/settings", get(get_settings).put(put_settings))
        .fallback(get(static_file).with_state(token.clone()))
        .with_state(state)
        .layer(middleware::from_fn_with_state(token, guard))
}

async fn static_file(State(token): State<Arc<String>>, uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(f) if path == "index.html" => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            String::from_utf8_lossy(&f.data).replace("__STREETLIGHT_TOKEN__", &token),
        )
            .into_response(),
        Some(f) => (
            [(
                header::CONTENT_TYPE,
                mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .to_string(),
            )],
            f.data,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn status(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let pending = s
        .pending
        .lock()
        .unwrap()
        .as_ref()
        .map(|p| json!({ "client_name": p.client_name, "fingerprint": p.fingerprint }));
    Json(json!({
        "host_name": s.settings.lock().unwrap().host_name,
        "host_fingerprint": pair::fingerprint_hex(&s.identity.fingerprint()),
        "version": env!("CARGO_PKG_VERSION"),
        "status": *s.status.lock().unwrap(),
        "pending": pending,
        "clients": s.trust.lock().unwrap().clients,
    }))
}

#[derive(Deserialize)]
struct PinBody {
    pin: String,
}

async fn submit_pin(
    State(s): State<Arc<AppState>>,
    Json(b): Json<PinBody>,
) -> Json<serde_json::Value> {
    let pin: String = b.pin.chars().filter(|c| c.is_ascii_digit()).collect();
    Json(json!({ "ok": pin.len() == 4 && s.submit_pin(pin) }))
}

async fn unpair(State(s): State<Arc<AppState>>, Path(fp): Path<String>) -> StatusCode {
    let mut t = s.trust.lock().unwrap();
    t.clients.retain(|c| c.fingerprint != fp);
    match t.save() {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn get_settings(State(s): State<Arc<AppState>>) -> Json<Settings> {
    Json(s.settings.lock().unwrap().clone())
}

#[derive(Deserialize)]
struct SettingsUpdate {
    #[serde(flatten)]
    settings: Settings,
    #[serde(default)]
    confirm: bool,
}

async fn put_settings(
    State(s): State<Arc<AppState>>,
    Json(update): Json<SettingsUpdate>,
) -> StatusCode {
    let mut current = s.settings.lock().unwrap();
    // Keep ordering within duplicate IDs: launch selects the first matching app,
    // so moving/removing a commandless shadow entry can change what gets executed.
    let commands = |settings: &Settings| {
        let mut by_id = std::collections::BTreeMap::<String, Vec<Option<String>>>::new();
        for app in &settings.apps {
            by_id.entry(app.id.clone()).or_default().push(app.command.clone());
        }
        by_id.retain(|_, commands| commands.iter().any(Option::is_some));
        by_id
    };
    let changed = commands(&current) != commands(&update.settings);
    if changed && !update.confirm {
        return StatusCode::CONFLICT;
    }
    match update.settings.save() {
        Ok(()) => {
            if changed {
                tracing::warn!(old = ?commands(&current), new = ?commands(&update.settings), "app command configuration changed via web API");
            }
            *current = update.settings;
            StatusCode::NO_CONTENT
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// `streetlight-host pair 1234` — submit a PIN to a running host without a browser.
pub fn submit_pin_cli(pin: &str, port: u16) -> anyhow::Result<bool> {
    use std::io::{Read, Write};
    let path = token_path()?;
    check_private(path.parent().unwrap(), true)?;
    check_private(&path, false)?;
    let token = std::fs::read_to_string(&path)?;
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid host API token");
    }
    let body = json!({ "pin": pin }).to_string();
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    write!(
        s,
        "POST /api/pair HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Streetlight-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    if !resp.starts_with("HTTP/1.1 200 ") {
        bail!(
            "host API rejected pairing: {}",
            resp.lines().next().unwrap_or("empty response")
        );
    }
    Ok(resp.contains("\"ok\":true"))
}
