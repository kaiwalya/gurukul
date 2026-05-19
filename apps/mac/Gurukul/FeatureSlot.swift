import Atomics
import Foundation

/// Coherent snapshot of all features the audio pipeline publishes per
/// drained hop. Plain value type — the synchronisation happens in
/// `FeatureSlot`.
///
/// `discontinuity == true` flags the first publish after a stream-level
/// break (device swap, start). The UI uses it to clear ring buffers so
/// stale points from the previous device don't haunt the trace.
struct FeatureSnapshot {
    var seq: UInt32
    var hz: Float
    var onset: Float
    var breath: Float
    var vibratoRate: Float
    var vibratoDepth: Float
    var discontinuity: Bool

    static let empty = FeatureSnapshot(
        seq: 0,
        hz: .nan,
        onset: 0,
        breath: 0,
        vibratoRate: 0,
        vibratoDepth: 0,
        discontinuity: false
    )
}

/// Lock-free, wait-free, single-producer / single-consumer triple-buffer
/// for `FeatureSnapshot`. The audio thread writes after each drained hop;
/// the UI thread reads at 30 Hz.
///
/// Algorithm:
///
///   - Three pre-allocated slots, one atomic "latest published index".
///   - The reader marks the slot it is reading via `readerClaim`; the
///     writer never picks that slot. With one writer + one reader + three
///     slots, the writer always has at least one free slot to write into,
///     so the writer is wait-free.
///   - Writer: pick a slot != `latest` && != `readerClaim`, write fields,
///     publish via a `.release` store on `latest`. The `release` orders
///     the field stores before the index publish.
///   - Reader: `.acquire`-load `latest`, mark `readerClaim`, re-load
///     `latest` with `.acquire` to confirm the slot is still the latest.
///     In single-reader steady state the re-load equals the first load
///     ~100% of the time; if it differs, the new index is fresher and
///     also safe (writer picked a different slot than `readerClaim`).
///
/// Why not seqlock: Swift's memory model does not permit plain
/// non-atomic reads/writes racing with atomic publishes — that's UB on
/// data race. A correct Swift seqlock requires every field to be a
/// `ManagedAtomic<UInt32>` with bitPattern dance and explicit fences.
/// Triple-buffer sidesteps the issue: the only memory shared between
/// threads at any instant is the index atomic and the slot the writer
/// is not currently writing.
///
/// Why not 5× independent `PitchSlot`-style atomics: across-hop tearing
/// — reader could see pitch from hop N + onset from hop N+1, which would
/// place an onset tick at the wrong x-position on the trace.
///
/// **Single-reader assumption:** this type is correct only for one
/// reader. Today that reader is `ContentView` on `MainActor`.
final class FeatureSlot {
    private let slots: UnsafeMutablePointer<FeatureSnapshot>
    private let latest = ManagedAtomic<UInt8>(0)
    /// 255 = no claim. Otherwise 0/1/2.
    private let readerClaim = ManagedAtomic<UInt8>(255)

    #if DEBUG
    /// Counts how often the reader's re-load disagreed with the first
    /// load — i.e. the writer published a new index between the two
    /// reader loads. Should be <1% of reads on a 30 Hz reader vs 94 Hz
    /// writer.
    private let readerRetries = ManagedAtomic<UInt64>(0)
    private let readerReads = ManagedAtomic<UInt64>(0)
    #endif

    init() {
        slots = UnsafeMutablePointer<FeatureSnapshot>.allocate(capacity: 3)
        for i in 0..<3 {
            slots[i] = .empty
        }
    }

    deinit {
        slots.deallocate()
    }

    /// Audio thread writes. Wait-free.
    func store(_ snapshot: FeatureSnapshot) {
        let cur = latest.load(ordering: .relaxed)
        let claim = readerClaim.load(ordering: .relaxed)
        // Three slots, two excluded — pick the third.
        let target: UInt8
        if cur != 0 && claim != 0 {
            target = 0
        } else if cur != 1 && claim != 1 {
            target = 1
        } else {
            target = 2
        }
        slots[Int(target)] = snapshot
        latest.store(target, ordering: .releasing)
    }

    /// UI thread reads.
    func load() -> FeatureSnapshot {
        #if DEBUG
        _ = readerReads.wrappingIncrementThenLoad(ordering: .relaxed)
        #endif
        while true {
            let idx = latest.load(ordering: .acquiring)
            readerClaim.store(idx, ordering: .relaxed)
            let idx2 = latest.load(ordering: .acquiring)
            if idx2 == idx {
                let s = slots[Int(idx)]
                readerClaim.store(255, ordering: .relaxed)
                return s
            }
            #if DEBUG
            _ = readerRetries.wrappingIncrementThenLoad(ordering: .relaxed)
            #endif
            // Loop with the newer index. At most one extra iteration
            // in single-reader steady state.
        }
    }

    #if DEBUG
    /// Returns `(reads, retries)` since this slot was created. Logged
    /// once on `AudioPipeline.stop()` so we can confirm the
    /// triple-buffer is behaving in production conditions.
    func debugCounters() -> (reads: UInt64, retries: UInt64) {
        return (
            readerReads.load(ordering: .relaxed),
            readerRetries.load(ordering: .relaxed)
        )
    }
    #endif
}
