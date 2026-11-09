import AVFoundation
import CoreMedia
import UIKit

/// Feeds compressed HEVC/H.264 access units straight into AVSampleBufferDisplayLayer,
/// which decodes on the hardware decoder and shows each frame immediately (no
/// presentation-timestamp scheduling, so no added buffering).
final class VideoView: UIView {
    override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }
    var displayLayer: AVSampleBufferDisplayLayer { layer as! AVSampleBufferDisplayLayer }

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        displayLayer.videoGravity = .resizeAspect
        displayLayer.preventsDisplaySleepDuringVideoPlayback = true
    }

    required init?(coder: NSCoder) { fatalError() }
}

final class VideoRenderer {
    enum Codec { case hevc, h264 }

    private let layer: AVSampleBufferDisplayLayer
    private let codec: Codec
    private var format: CMVideoFormatDescription?
    private let onNeedIdr: () -> Void

    init(layer: AVSampleBufferDisplayLayer, codec: Codec, onNeedIdr: @escaping () -> Void) {
        self.layer = layer
        self.codec = codec
        self.onNeedIdr = onNeedIdr
    }

    /// Called on the media thread with one Annex-B access unit.
    func submit(_ au: UnsafeBufferPointer<UInt8>, keyframe: Bool) {
        let nals = Self.splitAnnexB(au)
        if keyframe || format == nil {
            if let f = makeFormat(nals) { format = f }
        }
        guard let format else { return }

        // Annex B → 4-byte big-endian length prefixes; drop parameter sets / AUD.
        var avcc = Data(capacity: au.count + nals.count * 4)
        for nal in nals where !isParameterSet(nal) {
            var len = UInt32(nal.count).bigEndian
            withUnsafeBytes(of: &len) { avcc.append(contentsOf: $0) }
            avcc.append(contentsOf: nal)
        }
        guard !avcc.isEmpty else { return }

        var block: CMBlockBuffer?
        let count = avcc.count
        let mem = UnsafeMutableRawPointer.allocate(byteCount: count, alignment: 16)
        avcc.copyBytes(to: mem.assumingMemoryBound(to: UInt8.self), count: count)
        guard CMBlockBufferCreateWithMemoryBlock(allocator: kCFAllocatorDefault, memoryBlock: mem, blockLength: count,
                                                 blockAllocator: kCFAllocatorDefault, customBlockSource: nil,
                                                 offsetToData: 0, dataLength: count, flags: 0,
                                                 blockBufferOut: &block) == noErr, let block else {
            mem.deallocate()
            return
        }
        var sample: CMSampleBuffer?
        var size = count
        guard CMSampleBufferCreateReady(allocator: kCFAllocatorDefault, dataBuffer: block, formatDescription: format,
                                        sampleCount: 1, sampleTimingEntryCount: 0, sampleTimingArray: nil,
                                        sampleSizeEntryCount: 1, sampleSizeArray: &size, sampleBufferOut: &sample) == noErr,
              let sample else { return }
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true) as? [NSMutableDictionary],
           let a = attachments.first {
            a[kCMSampleAttachmentKey_DisplayImmediately] = true
            if !keyframe { a[kCMSampleAttachmentKey_NotSync] = true }
        }
        if layer.status == .failed {
            layer.flush()
            onNeedIdr()
            return
        }
        layer.enqueue(sample)
    }

    func reset() {
        format = nil
        layer.flushAndRemoveImage()
    }

    // MARK: - bitstream helpers

    private func nalType(_ nal: ArraySlice<UInt8>) -> UInt8 {
        guard let b = nal.first else { return 0xff }
        return codec == .hevc ? (b >> 1) & 0x3f : b & 0x1f
    }

    private func isParameterSet(_ nal: ArraySlice<UInt8>) -> Bool {
        let t = nalType(nal)
        return codec == .hevc ? (t == 32 || t == 33 || t == 34 || t == 35) : (t == 7 || t == 8 || t == 9)
    }

    private func makeFormat(_ nals: [ArraySlice<UInt8>]) -> CMVideoFormatDescription? {
        let wanted: [UInt8] = codec == .hevc ? [32, 33, 34] : [7, 8]
        var sets: [[UInt8]] = []
        for t in wanted {
            guard let n = nals.first(where: { nalType($0) == t }) else { return nil }
            sets.append(Array(n))
        }
        var fmt: CMVideoFormatDescription?
        let status: OSStatus = withPointers(sets) { ptrs, sizes in
            if codec == .hevc {
                return CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    allocator: kCFAllocatorDefault, parameterSetCount: sets.count, parameterSetPointers: ptrs,
                    parameterSetSizes: sizes, nalUnitHeaderLength: 4, extensions: nil, formatDescriptionOut: &fmt)
            } else {
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault, parameterSetCount: sets.count, parameterSetPointers: ptrs,
                    parameterSetSizes: sizes, nalUnitHeaderLength: 4, formatDescriptionOut: &fmt)
            }
        }
        return status == noErr ? fmt : nil
    }

    private func withPointers<R>(_ sets: [[UInt8]], _ body: (UnsafePointer<UnsafePointer<UInt8>>, UnsafePointer<Int>) -> R) -> R {
        let bufs = sets.map { s -> UnsafeMutablePointer<UInt8> in
            let p = UnsafeMutablePointer<UInt8>.allocate(capacity: s.count)
            p.initialize(from: s, count: s.count)
            return p
        }
        defer { bufs.forEach { $0.deallocate() } }
        let ptrs = bufs.map { UnsafePointer($0) }
        let sizes = sets.map(\.count)
        return ptrs.withUnsafeBufferPointer { p in sizes.withUnsafeBufferPointer { s in body(p.baseAddress!, s.baseAddress!) } }
    }

    static func splitAnnexB(_ buf: UnsafeBufferPointer<UInt8>) -> [ArraySlice<UInt8>] {
        let a = Array(buf)
        var starts: [(Int, Int)] = [] // (start code index, payload index)
        var i = 0
        while i + 3 <= a.count {
            if a[i] == 0, a[i + 1] == 0 {
                if a[i + 2] == 1 { starts.append((i, i + 3)); i += 3; continue }
                if i + 4 <= a.count, a[i + 2] == 0, a[i + 3] == 1 { starts.append((i, i + 4)); i += 4; continue }
            }
            i += 1
        }
        var out: [ArraySlice<UInt8>] = []
        for (k, s) in starts.enumerated() {
            let end = k + 1 < starts.count ? starts[k + 1].0 : a.count
            if s.1 < end { out.append(a[s.1..<end]) }
        }
        return out
    }
}
