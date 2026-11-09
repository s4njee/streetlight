import UIKit
import WebKit
import StreetlightCore

/// Root controller: video layer + touch surface underneath a transparent WKWebView that
/// runs web/client/index.html. The JS ↔ native protocol matches the Linux client.
final class StreamViewController: UIViewController, WKScriptMessageHandler {
    private let videoView = VideoView()
    private let touchView = TouchInputView()
    private var webView: WKWebView!
    private let peripherals = PeripheralInput()

    private var client: CoreClient?
    private var renderer: VideoRenderer?
    private var audio: AudioPlayer?
    private var sink: MediaSinkBox?
    private var watchdog: Timer?
    private var streaming = false { didSet { setNeedsUpdateOfPrefersPointerLocked(); setNeedsUpdateOfHomeIndicatorAutoHidden() } }
    private var pressedKeys = Set<UInt16>()

    override var prefersPointerLocked: Bool { streaming }
    override var prefersHomeIndicatorAutoHidden: Bool { streaming }
    override var prefersStatusBarHidden: Bool { true }
    override var preferredScreenEdgesDeferringSystemGestures: UIRectEdge { streaming ? .all : [] }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        for v in [videoView, touchView] as [UIView] {
            v.frame = view.bounds
            v.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            view.addSubview(v)
        }
        touchView.onMenu = { [weak self] in self?.endStream(reason: nil) }

        let ucc = WKUserContentController()
        ucc.add(self, name: "streetlight")
        let cfg = WKWebViewConfiguration()
        cfg.userContentController = ucc
        webView = WKWebView(frame: view.bounds, configuration: cfg)
        webView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        webView.isOpaque = false
        webView.backgroundColor = .clear
        webView.scrollView.backgroundColor = .clear
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        #if DEBUG
        if #available(iOS 16.4, *) { webView.isInspectable = true }
        #endif
        view.addSubview(webView)
        if let url = Bundle.main.url(forResource: "index", withExtension: "html") {
            webView.loadFileURL(url, allowingReadAccessTo: url.deletingLastPathComponent())
        }
    }

    // MARK: - bridge

    private func js(_ script: String) {
        DispatchQueue.main.async { self.webView.evaluateJavaScript(script) }
    }

    private func resolve(_ id: Int, _ result: Result<String, Error>) {
        switch result {
        case let .success(json): js("window.streetlight.__resolve(\(id), true, \(json))")
        case let .failure(e): js("window.streetlight.__resolve(\(id), false, \(jsonString(e.localizedDescription)))")
        }
    }

    private func event(_ name: String, _ json: String) {
        js("window.streetlight.__event(\(jsonString(name)), \(json))")
    }

    private func background(_ id: Int, _ work: @escaping () throws -> String) {
        DispatchQueue.global(qos: .userInitiated).async {
            let r = Result { try work() }
            DispatchQueue.main.async { self.resolve(id, r) }
        }
    }

    func userContentController(_ ucc: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let body = message.body as? String,
              let msg = try? JSONSerialization.jsonObject(with: Data(body.utf8)) as? [String: Any] else { return }
        let id = msg["id"] as? Int ?? 0
        let cmd = msg["cmd"] as? String ?? ""
        let args = msg["args"] as? [String: Any] ?? [:]
        switch cmd {
        case "discover":
            Discovery.browse { hosts in
                let json = String(data: (try? JSONEncoder().encode(hosts)) ?? Data("[]".utf8), encoding: .utf8)!
                DispatchQueue.main.async { self.resolve(id, .success(json)) }
            }
        case "knownHosts":
            resolve(id, .success(CoreClient.knownHostsJSON()))
        case "forgetHost":
            CoreClient.forgetHost(args["fingerprint"] as? String ?? "")
            resolve(id, .success("null"))
        case "connect":
            let host = args["host"] as? String ?? ""
            DispatchQueue.global(qos: .userInitiated).async {
                do {
                    let (c, w) = try CoreClient.connect(host: host, name: UIDevice.current.name)
                    DispatchQueue.main.async {
                        self.client = c
                        self.touchView.client = c
                        self.peripherals.client = c
                        let j = #"{"host_name":\#(jsonString(w.host_name)),"paired":\#(w.paired),"host_fingerprint":"\#(w.host_fingerprint)"}"#
                        self.resolve(id, .success(j))
                    }
                } catch {
                    DispatchQueue.main.async { self.resolve(id, .failure(error)) }
                }
            }
        case "disconnect":
            endStream(reason: nil)
            client = nil
            resolve(id, .success("null"))
        case "pair":
            guard let c = client else { return resolve(id, .failure(CoreError("not connected"))) }
            let pin = CoreClient.randomPin()
            event("pairPin", #"{"pin":"\#(pin)"}"#)
            background(id) { #"{"ok":\#(try c.pair(pin: pin))}"# }
        case "apps":
            guard let c = client else { return resolve(id, .failure(CoreError("not connected"))) }
            background(id) { try c.appsJSON() }
        case "getSettings":
            resolve(id, .success(Settings.load().json))
        case "setSettings":
            if let data = try? JSONSerialization.data(withJSONObject: args), let s = try? JSONDecoder().decode(Settings.self, from: data) {
                s.save()
                resolve(id, .success("null"))
            } else {
                resolve(id, .failure(CoreError("invalid settings")))
            }
        case "start":
            startStream(id: id, app: args["app"] as? String)
        case "stop":
            endStream(reason: nil)
            resolve(id, .success("null"))
        case "stats":
            resolve(id, .success(client?.statsJSON ?? "null"))
        default:
            resolve(id, .failure(CoreError("unknown command \(cmd)")))
        }
    }

    // MARK: - streaming

    private func startStream(id: Int, app: String?) {
        guard let c = client else { return resolve(id, .failure(CoreError("not connected"))) }
        let settings = Settings.load()
        let screen = view.window?.windowScene?.screen ?? UIScreen.main
        let native = screen.nativeBounds.size
        let (nw, nh) = (Int(max(native.width, native.height)), Int(min(native.width, native.height)))
        var (w, h) = (nw, nh)
        let parts = settings.resolution.split(separator: "x").compactMap { Int($0) }
        if settings.resolution != "native", parts.count == 2 { (w, h) = (parts[0], parts[1]) }
        let fps = settings.fps == 0 ? screen.maximumFramesPerSecond : settings.fps
        let scale = settings.resolution == "native" ? Double(screen.nativeScale) : 1.0
        let appJSON = app.map { jsonString($0) } ?? "null"
        let req = """
        {"display":{"width":\(w),"height":\(h),"fps":\(fps),"scale":\(scale),"virtual_display":\(settings.virtual_display)},\
        "codec":"\(settings.codec == "H264" ? "H264" : "Hevc")","color":"Sdr709","bitrate_kbps":\(settings.bitrate_kbps),\
        "fec_percent":\(settings.fec_percent),"max_shard_bytes":1400,"pace_mbps":\(settings.pace_mbps),"app":\(appJSON)}
        """

        let renderer = VideoRenderer(layer: videoView.displayLayer, codec: settings.codec == "H264" ? .h264 : .hevc) { [weak c] in c?.requestIdr() }
        let audio = AudioPlayer()
        let sink = MediaSinkBox(
            // AVSampleBufferDisplayLayer decodes internally, so only network timing is measured here.
            video: { data, _, key, _ in renderer.submit(data, keyframe: key) },
            audio: { data in audio?.push(data) })
        self.renderer = renderer
        self.audio = audio
        self.sink = sink

        background(id) { [weak self] in
            let info = try c.start(requestJSON: req, sink: sink)
            DispatchQueue.main.async { self?.didStart(info: info) }
            return info
        }
    }

    private func didStart(info: String) {
        if let d = try? JSONSerialization.jsonObject(with: Data(info.utf8)) as? [String: Any],
           let w = d["width"] as? Int, let h = d["height"] as? Int {
            touchView.streamSize = CGSize(width: w, height: h)
        }
        touchView.touchscreenMode = Settings.load().touch_mode == "touchscreen"
        streaming = true
        webView.isHidden = true
        becomeFirstResponder()
        watchdog = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            if let r = self?.client?.stoppedReason { self?.endStream(reason: r) }
        }
    }

    private func endStream(reason: String?) {
        guard streaming else { return }
        streaming = false
        watchdog?.invalidate()
        releaseKeys()
        client?.stop()
        renderer?.reset()
        renderer = nil
        audio = nil
        sink = nil
        webView.isHidden = false
        event("streamEnded", #"{"reason":\#(reason.map(jsonString) ?? "null")}"#)
    }

    // MARK: - hardware keyboard

    override var canBecomeFirstResponder: Bool { true }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        guard streaming, let p = client?.ptr else { return super.pressesBegan(presses, with: event) }
        for press in presses {
            guard let key = press.key else { continue }
            // Ctrl+Alt+Shift+Q returns to the menu, like the Linux client.
            if key.keyCode == .keyboardQ, key.modifierFlags.isSuperset(of: [.control, .alternate, .shift]) {
                endStream(reason: nil)
                return
            }
            if let code = evdevCode(for: key), pressedKeys.insert(code).inserted { sl_input_key(p, code, true) }
        }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        guard streaming, let p = client?.ptr else { return super.pressesEnded(presses, with: event) }
        for press in presses {
            guard let key = press.key, let code = evdevCode(for: key) else { continue }
            if pressedKeys.remove(code) != nil { sl_input_key(p, code, false) }
        }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        pressesEnded(presses, with: event)
    }

    private func releaseKeys() {
        guard let p = client?.ptr else { pressedKeys.removeAll(); return }
        for k in pressedKeys { sl_input_key(p, k, false) }
        pressedKeys.removeAll()
    }
}

/// Same shape as the Linux client's settings (see crates/client-linux/src/settings.rs).
struct Settings: Codable {
    var resolution = "native"
    var fps = 0
    var bitrate_kbps = 80_000
    var codec = "Hevc"
    var fec_percent = 20
    var pace_mbps = 600 // iPad is almost always on Wi-Fi
    var virtual_display = true
    var touch_mode = "pointer"

    static func load() -> Settings {
        guard let d = UserDefaults.standard.data(forKey: "settings"),
              let s = try? JSONDecoder().decode(Settings.self, from: d) else { return Settings() }
        return s
    }

    func save() { UserDefaults.standard.set(try? JSONEncoder().encode(self), forKey: "settings") }

    var json: String { String(data: (try? JSONEncoder().encode(self)) ?? Data("{}".utf8), encoding: .utf8)! }

    init() {}
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let d = Settings()
        resolution = (try? c.decode(String.self, forKey: .resolution)) ?? d.resolution
        fps = (try? c.decode(Int.self, forKey: .fps)) ?? d.fps
        bitrate_kbps = (try? c.decode(Int.self, forKey: .bitrate_kbps)) ?? d.bitrate_kbps
        codec = (try? c.decode(String.self, forKey: .codec)) ?? d.codec
        fec_percent = (try? c.decode(Int.self, forKey: .fec_percent)) ?? d.fec_percent
        pace_mbps = (try? c.decode(Int.self, forKey: .pace_mbps)) ?? d.pace_mbps
        virtual_display = (try? c.decode(Bool.self, forKey: .virtual_display)) ?? d.virtual_display
        touch_mode = (try? c.decode(String.self, forKey: .touch_mode)) ?? d.touch_mode
    }
}

func jsonString(_ s: String) -> String {
    let data = try? JSONSerialization.data(withJSONObject: [s])
    let arr = data.flatMap { String(data: $0, encoding: .utf8) } ?? "[\"\"]"
    return String(arr.dropFirst().dropLast())
}
