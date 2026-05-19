import Atomics
import Foundation

/// Number of horizontal "pixels" of waveform we publish. With a 1 s
/// window that's 150 buckets ≈ 320 samples each at 48 kHz, which is
/// enough resolution to see attack envelopes and roughly how the wave
/// is behaving without putting kilobytes into the SPSC slot every hop.
let kWaveformBuckets = 150

/// One screen-pixel of waveform = the min and max sample value across
/// the audio frames that fell into that bucket. Drawn as a vertical
/// line spanning [min, max] in pixel space.
struct WaveformBucket {
    var lo: Float
    var hi: Float

    static let zero = WaveformBucket(lo: 0, hi: 0)
}

/// Fixed-size snapshot of the most-recent 1 s of audio, decimated into
/// `kWaveformBuckets` min/max pairs. `seq` matches the FeatureSnapshot
/// at the time of publish so UI code can correlate them, but the two
/// slots are otherwise independent.
struct WaveformSnapshot {
    var seq: UInt32
    var buckets: [WaveformBucket]

    static let empty = WaveformSnapshot(
        seq: 0,
        buckets: Array(repeating: .zero, count: kWaveformBuckets)
    )
}

/// Triple-buffered SPSC slot — same design as `FeatureSlot`, just with
/// a different payload. Writer is the audio thread; reader is the UI.
///
/// Buckets are stored as raw Float arrays (one per slot) so the writer
/// can update them in place without allocation. The reader copies the
/// active slot into a fresh `WaveformSnapshot` on `load()`.
final class WaveformSlot {
    private let bucketsPerSlot = kWaveformBuckets
    private let storage: UnsafeMutablePointer<Float>  // 3 * 2 * buckets
    private let seqs: UnsafeMutablePointer<UInt32>    // 3
    private let latest = ManagedAtomic<UInt8>(0)
    private let readerClaim = ManagedAtomic<UInt8>(255)

    init() {
        let perSlot = 2 * bucketsPerSlot
        storage = UnsafeMutablePointer<Float>.allocate(capacity: 3 * perSlot)
        storage.initialize(repeating: 0, count: 3 * perSlot)
        seqs = UnsafeMutablePointer<UInt32>.allocate(capacity: 3)
        seqs.initialize(repeating: 0, count: 3)
    }

    deinit {
        storage.deallocate()
        seqs.deallocate()
    }

    /// Audio thread writes. `lo` and `hi` must each be `kWaveformBuckets`
    /// long. Wait-free.
    func store(seq: UInt32, lo: UnsafePointer<Float>, hi: UnsafePointer<Float>) {
        let cur = latest.load(ordering: .relaxed)
        let claim = readerClaim.load(ordering: .relaxed)
        let target: UInt8
        if cur != 0 && claim != 0 {
            target = 0
        } else if cur != 1 && claim != 1 {
            target = 1
        } else {
            target = 2
        }
        let base = storage.advanced(by: Int(target) * 2 * bucketsPerSlot)
        base.update(from: lo, count: bucketsPerSlot)
        base.advanced(by: bucketsPerSlot).update(from: hi, count: bucketsPerSlot)
        seqs[Int(target)] = seq
        latest.store(target, ordering: .releasing)
    }

    /// UI thread reads.
    func load() -> WaveformSnapshot {
        while true {
            let idx = latest.load(ordering: .acquiring)
            readerClaim.store(idx, ordering: .relaxed)
            let idx2 = latest.load(ordering: .acquiring)
            if idx2 == idx {
                let base = storage.advanced(by: Int(idx) * 2 * bucketsPerSlot)
                var buckets = Array(repeating: WaveformBucket.zero, count: bucketsPerSlot)
                for i in 0..<bucketsPerSlot {
                    buckets[i] = WaveformBucket(
                        lo: base[i],
                        hi: base[bucketsPerSlot + i]
                    )
                }
                let seq = seqs[Int(idx)]
                readerClaim.store(255, ordering: .relaxed)
                return WaveformSnapshot(seq: seq, buckets: buckets)
            }
            // retry with newer idx
        }
    }
}
