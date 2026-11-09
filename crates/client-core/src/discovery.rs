//! mDNS discovery of hosts on the LAN.

use std::time::{Duration, Instant};

use serde::Serialize;
use streetlight_proto::control::MDNS_SERVICE;

#[derive(Debug, Clone, Serialize)]
pub struct Discovered {
    pub name: String,
    pub addr: String,
}

/// Browse for `timeout` and return every host seen.
pub fn browse(timeout: Duration) -> Vec<Discovered> {
    let Ok(d) = mdns_sd::ServiceDaemon::new() else { return vec![] };
    let Ok(rx) = d.browse(MDNS_SERVICE) else { return vec![] };
    let deadline = Instant::now() + timeout;
    let mut out: Vec<Discovered> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                let name = info.get_fullname().trim_end_matches(MDNS_SERVICE).trim_end_matches('.').to_string();
                let port = info.get_port();
                // Prefer IPv4 for predictability on home LANs.
                let mut addrs: Vec<_> = info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                addrs.sort_by_key(|a| !a.is_ipv4());
                if let Some(ip) = addrs.first() {
                    let addr = std::net::SocketAddr::new(*ip, port).to_string();
                    if !out.iter().any(|h| h.addr == addr) {
                        out.push(Discovered { name, addr });
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = d.shutdown();
    out
}
