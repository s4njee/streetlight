import Foundation
import StreetlightCore

/// Swift wrapper over the Rust client-core C ABI.
final class CoreClient {
    let ptr: OpaquePointer
    private init(_ p: OpaquePointer) { ptr = p }
    deinit { sl_free(ptr) }

    static var dataDir: String = {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        let dir = base.appendingPathComponent("streetlight", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.path
    }()

    struct Welcome: Decodable { let host_name: String; let paired: Bool; let host_fingerprint: String }

    static func connect(host: String, name: String) throws -> (CoreClient, Welcome) {
        let caps = #"{"codecs":["Hevc","H264"],"hdr":false,"max_width":3840,"max_height":2160,"max_fps":120,"audio_channels":2}"#
        var welcome: UnsafeMutablePointer<CChar>?
        var err: UnsafeMutablePointer<CChar>?
        guard let p = sl_connect(host, name, dataDir, caps, &welcome, &err) else {
            throw CoreError(take(err) ?? "connect failed")
        }
        let w = try JSONDecoder().decode(Welcome.self, from: Data((take(welcome) ?? "{}").utf8))
        return (CoreClient(p), w)
    }

    /// Blocks until the host user types the PIN.
    func pair(pin: String) throws -> Bool {
        var err: UnsafeMutablePointer<CChar>?
        let r = sl_pair(ptr, pin, &err)
        if r < 0 { throw CoreError(Self.take(err) ?? "pairing failed") }
        return r == 1
    }

    func appsJSON() throws -> String {
        var err: UnsafeMutablePointer<CChar>?
        guard let s = Self.take(sl_apps_json(ptr, &err)) else { throw CoreError(Self.take(err) ?? "apps failed") }
        return s
    }

    func start(requestJSON: String, sink: MediaSinkBox) throws -> String {
        var err: UnsafeMutablePointer<CChar>?
        let ctx = Unmanaged.passUnretained(sink).toOpaque()
        guard let s = Self.take(sl_start(ptr, requestJSON, mediaVideoCallback, mediaAudioCallback, ctx, &err)) else {
            throw CoreError(Self.take(err) ?? "start failed")
        }
        return s
    }

    func stop() { sl_stop(ptr) }
    func requestIdr() { sl_request_idr(ptr) }
    func reportTiming(decodeUs: UInt32, presentUs: UInt32) { sl_report_timing(ptr, decodeUs, presentUs) }
    var statsJSON: String { Self.take(sl_stats_json(ptr)) ?? "null" }
    var stoppedReason: String? { Self.take(sl_stopped_reason(ptr)) }

    static func knownHostsJSON() -> String { take(sl_known_hosts_json(dataDir)) ?? "[]" }
    static func forgetHost(_ fp: String) { sl_forget_host(dataDir, fp) }
    static func randomPin() -> String { take(sl_random_pin()) ?? "0000" }

    static func take(_ p: UnsafeMutablePointer<CChar>?) -> String? {
        guard let p else { return nil }
        defer { sl_string_free(p) }
        return String(cString: p)
    }
}

struct CoreError: LocalizedError {
    let message: String
    init(_ m: String) { message = m }
    var errorDescription: String? { message }
}

/// Receives media from the Rust media thread.
final class MediaSinkBox {
    let video: (UnsafeBufferPointer<UInt8>, UInt32, Bool, UInt64) -> Void
    let audio: (UnsafeBufferPointer<UInt8>) -> Void
    init(video: @escaping (UnsafeBufferPointer<UInt8>, UInt32, Bool, UInt64) -> Void,
         audio: @escaping (UnsafeBufferPointer<UInt8>) -> Void) {
        self.video = video
        self.audio = audio
    }
}

private let mediaVideoCallback: sl_video_cb = { ctx, data, len, frame, key, received in
    guard let ctx, let data else { return }
    let sink = Unmanaged<MediaSinkBox>.fromOpaque(ctx).takeUnretainedValue()
    sink.video(UnsafeBufferPointer(start: data, count: len), frame, key, received)
}

private let mediaAudioCallback: sl_audio_cb = { ctx, data, len in
    guard let ctx, let data else { return }
    let sink = Unmanaged<MediaSinkBox>.fromOpaque(ctx).takeUnretainedValue()
    sink.audio(UnsafeBufferPointer(start: data, count: len))
}
