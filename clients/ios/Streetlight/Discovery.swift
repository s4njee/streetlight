import Foundation
import Network

/// Bonjour discovery via NWBrowser (no multicast entitlement needed), resolving each
/// service to an IP:port by briefly opening a UDP NWConnection.
enum Discovery {
    struct Host: Encodable { let name: String; let addr: String }

    static func browse(timeout: TimeInterval = 1.5, completion: @escaping ([Host]) -> Void) {
        let browser = NWBrowser(for: .bonjour(type: "_streetlight._udp", domain: nil), using: .udp)
        let lock = NSLock()
        var found: [Host] = []
        var conns: [NWConnection] = []
        browser.browseResultsChangedHandler = { results, _ in
            for r in results {
                guard case let .service(name, _, _, _) = r.endpoint else { continue }
                let conn = NWConnection(to: r.endpoint, using: .udp)
                conn.stateUpdateHandler = { state in
                    if case .ready = state, case let .hostPort(host, port)? = conn.currentPath?.remoteEndpoint {
                        var h = "\(host)"
                        if let pct = h.firstIndex(of: "%") { h = String(h[..<pct]) } // strip IPv6 scope
                        let addr = h.contains(":") ? "[\(h)]:\(port)" : "\(h):\(port)"
                        lock.lock()
                        if !found.contains(where: { $0.addr == addr || $0.name == name }) { found.append(Host(name: name, addr: addr)) }
                        lock.unlock()
                        conn.cancel()
                    }
                }
                lock.lock(); conns.append(conn); lock.unlock()
                conn.start(queue: .global())
            }
        }
        browser.start(queue: .global())
        DispatchQueue.global().asyncAfter(deadline: .now() + timeout) {
            browser.cancel()
            lock.lock()
            conns.forEach { $0.cancel() }
            let result = found
            lock.unlock()
            completion(result)
        }
    }
}
