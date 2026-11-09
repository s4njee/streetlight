//! Host web UI + JSON API, bound to loopback only (the person at the host PC
//! approves pairings and edits settings here).

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
    let app = Router::new()
        .route("/api/status", get(status))
        .route("/api/pair", post(submit_pin))
        .route("/api/clients/{fp}", delete(unpair))
        .route("/api/settings", get(get_settings).put(put_settings))
        .fallback(get(static_file))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("web UI on http://127.0.0.1:{port}/");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn static_file(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(f) => ([(header::CONTENT_TYPE, mime_guess::from_path(path).first_or_octet_stream().to_string())], f.data).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn status(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let pending = s.pending.lock().unwrap().as_ref().map(|p| json!({ "client_name": p.client_name, "fingerprint": p.fingerprint }));
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

async fn submit_pin(State(s): State<Arc<AppState>>, Json(b): Json<PinBody>) -> Json<serde_json::Value> {
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

async fn put_settings(State(s): State<Arc<AppState>>, Json(new): Json<Settings>) -> StatusCode {
    let r = new.save();
    *s.settings.lock().unwrap() = new;
    if r.is_ok() { StatusCode::NO_CONTENT } else { StatusCode::INTERNAL_SERVER_ERROR }
}

/// `streetlight-host pair 1234` — submit a PIN to a running host without a browser.
pub fn submit_pin_cli(pin: &str, port: u16) -> anyhow::Result<bool> {
    use std::io::{Read, Write};
    let body = json!({ "pin": pin }).to_string();
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    write!(
        s,
        "POST /api/pair HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    Ok(resp.contains("\"ok\":true"))
}
