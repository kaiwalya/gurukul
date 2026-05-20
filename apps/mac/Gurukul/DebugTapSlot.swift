import Atomics
import Foundation

/// Maximum samples per debug-tap publish. Matches the engine's hop size
/// (block size, currently 512). engine_read_port returns at most one
/// block of samples per call; we copy up to this many into the slot.
let kDebugTapMaxSamples = 512

/// One discriminator byte for the renderer. Matches PortShape.rawValue
/// (defined in PortShape.swift, PR 1.4.8.5.2). PR 5.1 only stores it;
/// the value is consumed by the UI in 5.2.
///
/// Sentinel 255 = "no tap published yet / cleared on rebuild".
typealias DebugTapTypeTag = UInt8
let kDebugTapTypeTagNone: DebugTapTypeTag = 255

/// A coherent snapshot of one engine port's most-recent block, published
/// by the audio thread for the debug pane to read at the UI tick.
///
/// `len == 0` is the "cleared" snapshot — the slot starts here and is
/// reset here on any engine rebuild / reset.
///
/// The payload is intentionally **data, not a callback**. When the ECS
/// visualiser refactor lands (Phase 1.5+), this shape becomes the
/// `PortBinding` component on a debug-tap entity and the refactor is
/// mechanical — `typeTag` selects which view system renders it.
struct DebugTapSnapshot {
    /// Increments on every publish. UI uses this to detect new data.
    var seq: UInt32
    /// PortShape rawValue (or `kDebugTapTypeTagNone` for cleared).
    var typeTag: DebugTapTypeTag
    /// Valid sample count in `samples`. May be < kDebugTapMaxSamples
    /// (e.g. the engine processed a partial block, or the port is a
    /// feature/control type that only writes one sample per hop).
    var len: UInt16
    /// Sample data. Indices `[0, len)` are valid; the rest is unspecified.
    var samples: [Float]

    static let empty = DebugTapSnapshot(
        seq: 0,
        typeTag: kDebugTapTypeTagNone,
        len: 0,
        samples: Array(repeating: 0, count: kDebugTapMaxSamples)
    )
}

/// Lock-free SPSC triple-buffer for `DebugTapSnapshot`, mirroring
/// `WaveformSlot`'s raw-storage pattern. The writer is the audio thread;
/// the reader is the UI tick.
///
/// Why mirror WaveformSlot (raw storage) rather than FeatureSlot (struct
/// slots): the payload contains a 512-float buffer, and copying that as
/// part of a struct assignment on the audio thread would lean on Swift's
/// COW machinery for the inner `[Float]`. Raw storage + `memcpy`
/// (`UnsafeMutablePointer.update(from:)`) avoids any retain/release.
///
/// Single-reader assumption — same as FeatureSlot / WaveformSlot. The
/// debug pane on `MainActor` is the only consumer.
final class DebugTapSlot {
    private let storage: UnsafeMutablePointer<Float>  // 3 * kDebugTapMaxSamples
    /// Per-slot header packed into a single UInt32 array entry per slot:
    /// `(seq: UInt32)`. Type and length live in separate arrays so the
    /// audio-thread store is three small writes — no struct copy.
    private let seqs: UnsafeMutablePointer<UInt32>    // 3
    private let typeTags: UnsafeMutablePointer<DebugTapTypeTag>  // 3
    private let lens: UnsafeMutablePointer<UInt16>    // 3
    private let latest = ManagedAtomic<UInt8>(0)
    private let readerClaim = ManagedAtomic<UInt8>(255)

    init() {
        storage = UnsafeMutablePointer<Float>.allocate(capacity: 3 * kDebugTapMaxSamples)
        storage.initialize(repeating: 0, count: 3 * kDebugTapMaxSamples)
        seqs = UnsafeMutablePointer<UInt32>.allocate(capacity: 3)
        seqs.initialize(repeating: 0, count: 3)
        typeTags = UnsafeMutablePointer<DebugTapTypeTag>.allocate(capacity: 3)
        typeTags.initialize(repeating: kDebugTapTypeTagNone, count: 3)
        lens = UnsafeMutablePointer<UInt16>.allocate(capacity: 3)
        lens.initialize(repeating: 0, count: 3)
    }

    deinit {
        storage.deallocate()
        seqs.deallocate()
        typeTags.deallocate()
        lens.deallocate()
    }

    /// Audio thread writes. `src` must point to at least `count` samples.
    /// `count` is clamped to `kDebugTapMaxSamples`. Wait-free.
    func store(
        seq: UInt32,
        typeTag: DebugTapTypeTag,
        src: UnsafePointer<Float>,
        count: Int
    ) {
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
        let capped = min(count, kDebugTapMaxSamples)
        let base = storage.advanced(by: Int(target) * kDebugTapMaxSamples)
        base.update(from: src, count: capped)
        seqs[Int(target)] = seq
        typeTags[Int(target)] = typeTag
        lens[Int(target)] = UInt16(capped)
        latest.store(target, ordering: .releasing)
    }

    /// Audio thread (or any thread between audio-thread runs) clears the
    /// slot. Publishes an empty snapshot so any in-flight UI read sees
    /// `len = 0` and renders nothing. Called from the engine rebuild /
    /// reset paths to enforce the "selection does not survive rebuild"
    /// invariant from PHASE_1_4_8.md.
    func clear(seq: UInt32) {
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
        seqs[Int(target)] = seq
        typeTags[Int(target)] = kDebugTapTypeTagNone
        lens[Int(target)] = 0
        latest.store(target, ordering: .releasing)
    }

    /// UI thread reads. Allocates a fresh `[Float]` for the payload.
    func load() -> DebugTapSnapshot {
        while true {
            let idx = latest.load(ordering: .acquiring)
            readerClaim.store(idx, ordering: .relaxed)
            let idx2 = latest.load(ordering: .acquiring)
            if idx2 == idx {
                let len = lens[Int(idx)]
                let typeTag = typeTags[Int(idx)]
                let seq = seqs[Int(idx)]
                let base = storage.advanced(by: Int(idx) * kDebugTapMaxSamples)
                var samples = Array(repeating: Float(0), count: kDebugTapMaxSamples)
                if len > 0 {
                    samples.withUnsafeMutableBufferPointer { dst in
                        if let dstBase = dst.baseAddress {
                            dstBase.update(from: base, count: Int(len))
                        }
                    }
                }
                readerClaim.store(255, ordering: .relaxed)
                return DebugTapSnapshot(
                    seq: seq,
                    typeTag: typeTag,
                    len: len,
                    samples: samples
                )
            }
        }
    }
}
