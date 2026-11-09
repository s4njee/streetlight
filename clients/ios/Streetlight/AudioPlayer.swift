import AVFoundation

/// Opus (5 ms, 48 kHz stereo) → AVAudioEngine with a small, bounded queue.
final class AudioPlayer {
    private let engine = AVAudioEngine()
    private let node = AVAudioPlayerNode()
    private let pcmFormat = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2)!
    private let opusFormat: AVAudioFormat
    private let converter: AVAudioConverter
    private let queue = DispatchQueue(label: "streetlight.audio")
    private var scheduled = 0
    private let maxScheduled = 8 // ≈40 ms

    init?() {
        var asbd = AudioStreamBasicDescription(mSampleRate: 48_000, mFormatID: kAudioFormatOpus, mFormatFlags: 0,
                                               mBytesPerPacket: 0, mFramesPerPacket: 240, mBytesPerFrame: 0,
                                               mChannelsPerFrame: 2, mBitsPerChannel: 0, mReserved: 0)
        guard let opus = AVAudioFormat(streamDescription: &asbd),
              let conv = AVAudioConverter(from: opus, to: AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2)!)
        else { return nil }
        opusFormat = opus
        converter = conv
        let session = AVAudioSession.sharedInstance()
        try? session.setCategory(.playback, mode: .default, options: [])
        try? session.setPreferredIOBufferDuration(0.005)
        try? session.setActive(true)
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: pcmFormat)
        do { try engine.start() } catch { return nil }
        node.play()
    }

    deinit { engine.stop() }

    func push(_ packet: UnsafeBufferPointer<UInt8>) {
        let bytes = Data(packet)
        queue.async { [weak self] in self?.decode(bytes) }
    }

    private func decode(_ bytes: Data) {
        if scheduled >= maxScheduled { return } // drop to keep latency bounded
        let input = AVAudioCompressedBuffer(format: opusFormat, packetCapacity: 1, maximumPacketSize: max(bytes.count, 1))
        bytes.withUnsafeBytes { input.data.copyMemory(from: $0.baseAddress!, byteCount: bytes.count) }
        input.byteLength = UInt32(bytes.count)
        input.packetCount = 1
        input.packetDescriptions?.pointee = AudioStreamPacketDescription(mStartOffset: 0, mVariableFramesInPacket: 0,
                                                                          mDataByteSize: UInt32(bytes.count))
        guard let out = AVAudioPCMBuffer(pcmFormat: pcmFormat, frameCapacity: 960) else { return }
        var fed = false
        var err: NSError?
        converter.convert(to: out, error: &err) { _, status in
            if fed { status.pointee = .noDataNow; return nil }
            fed = true
            status.pointee = .haveData
            return input
        }
        guard err == nil, out.frameLength > 0 else { return }
        scheduled += 1
        node.scheduleBuffer(out) { [weak self] in self?.queue.async { self?.scheduled -= 1 } }
    }
}
