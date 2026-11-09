//! Real HTTP requests against the router on an ephemeral loopback port.
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::sync::Arc;
use streetlight_host::{config::Settings, media::MediaServer, server::AppState, web};
use streetlight_proto::net::Identity;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn request(port: u16, method: &str, headers: &str, body: &str) -> String {
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    sock.write_all(format!("{method} /api/settings HTTP/1.1\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    let mut response = String::new();
    sock.read_to_string(&mut response).await.unwrap();
    response
}

#[test]
fn host_api_security() {
    let scratch = std::env::temp_dir().join(format!("streetlight-web-test-{}", web::new_token()));
    std::fs::create_dir(&scratch).unwrap();
    // This integration-test executable has one test; set before starting threads.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", scratch.join("config"));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let token = web::new_token();
        let path = scratch.join("runtime/token");
        web::write_token(&path, &token).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777, 0o700);
        let target = scratch.join("untouched");
        std::fs::write(&target, "original").unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&target, &path).unwrap();
        web::write_token(&path, &token).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        assert!(!std::fs::symlink_metadata(&path).unwrap().is_symlink());
        std::fs::set_permissions(path.parent().unwrap(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(web::write_token(&path, &token).is_err());
        std::fs::set_permissions(path.parent().unwrap(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let identity = Identity::load_or_create(&scratch.join("identity"), "host").unwrap();
        let settings = Settings::default();
        let state = AppState::new(identity, settings.clone(), MediaServer::bind(0).unwrap());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = web::router(Arc::clone(&state), token.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let body = serde_json::to_string(&settings).unwrap();
        for host in ["evil.example:47990", "127.0.0.1", "localhost:80", "127.0.0.1:47990.evil", "127.0.0.2:47990"] {
            assert!(request(port, "GET", &format!("Host: {host}\r\n"), "").await.starts_with("HTTP/1.1 403"));
        }
        assert!(request(port, "PUT", "Host: 127.0.0.1:47990\r\n", &body).await.starts_with("HTTP/1.1 403"));
        for host in ["127.0.0.1:47990", "localhost:47990", "[::1]:47990"] {
            let headers = format!("Host: {host}\r\nOrigin: http://{host}\r\nX-Streetlight-Token: {token}\r\n");
            assert!(request(port, "PUT", &headers, &body).await.starts_with("HTTP/1.1 204"));
        }
        let valid = format!("Host: 127.0.0.1:47990\r\nX-Streetlight-Token: {token}\r\n");
        for origin in ["http://evil.example", "null", "http://localhost:47990.evil", "https://localhost:47990"] {
            assert!(request(port, "POST", &format!("{valid}Origin: {origin}\r\n"), "{}").await.starts_with("HTTP/1.1 403"));
        }
        assert!(request(port, "PUT", &format!("{valid}X-Streetlight-Token: wrong\r\n"), &body).await.starts_with("HTTP/1.1 403"));
        let mut changed = serde_json::to_value(&settings).unwrap();
        changed["apps"][0]["command"] = "echo confirmed".into();
        assert!(request(port, "PUT", &valid, &changed.to_string()).await.starts_with("HTTP/1.1 409"));
        assert!(state.settings.lock().unwrap().apps[0].command.is_none());
        changed["confirm"] = true.into();
        assert!(request(port, "PUT", &valid, &changed.to_string()).await.starts_with("HTTP/1.1 204"));
        assert_eq!(state.settings.lock().unwrap().apps[0].command.as_deref(), Some("echo confirmed"));
        // Duplicate IDs use the first entry at launch: shadowing and reordering
        // must require confirmation even when the set of command strings is unchanged.
        let original_apps = changed["apps"].clone();
        let mut shadow = changed["apps"][0].clone();
        shadow["command"] = serde_json::Value::Null;
        changed["apps"].as_array_mut().unwrap().insert(0, shadow);
        changed["confirm"] = false.into();
        assert!(request(port, "PUT", &valid, &changed.to_string()).await.starts_with("HTTP/1.1 409"));
        changed["confirm"] = true.into();
        assert!(request(port, "PUT", &valid, &changed.to_string()).await.starts_with("HTTP/1.1 204"));
        changed["apps"] = original_apps;
        changed["confirm"] = false.into();
        assert!(request(port, "PUT", &valid, &changed.to_string()).await.starts_with("HTTP/1.1 409"));
        assert!(request(port, "PUT", "Host: 127.0.0.1:47990\r\nX-Streetlight-Token: wrong\r\n", &body).await.starts_with("HTTP/1.1 403"));
        // Test the actual status, pairing and templated HTML routes too.
        for (path, method, extra, expected) in [
            ("/api/status", "GET", "Host: evil.example:47990\r\n".to_owned(), "403"),
            ("/api/pair", "POST", format!("{valid}Origin: http://evil.example\r\n"), "403"),
            ("/api/pair", "POST", valid.clone(), "200"),
            ("/", "GET", "Host: localhost:47990\r\n".to_owned(), "200"),
            ("/index.html", "GET", "Host: localhost:47990\r\n".to_owned(), "200"),
        ] {
            let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            sock.write_all(format!("{method} {path} HTTP/1.1\r\n{extra}Content-Type: application/json\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{{\"pin\":\"1234\"}}",).as_bytes()).await.unwrap();
            let mut response = String::new();
            sock.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {expected}")), "{response}");
            if path == "/" || path == "/index.html" {
                assert!(response.contains(&token));
                assert!(!response.contains("__STREETLIGHT_TOKEN__"));
                assert!(response.contains("cache-control: no-store"));
            }
        }
        server.abort();
    });
    std::fs::remove_dir_all(scratch).unwrap();
}
