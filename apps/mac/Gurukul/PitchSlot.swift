import Atomics
import Foundation

/// Lock-free single-producer / single-consumer slot carrying the latest pitch
/// reading from the audio thread to the UI thread.
///
/// The slot packs (sequence number, hz) into a single 64-bit atomic, so a
/// reader sees either an old `(seq, hz)` or a new `(seq, hz)` — never a tear
/// where one half is fresh and the other is stale. The sequence number lets
/// the reader distinguish "no new block since last tick" from "same pitch
/// held"; the producer uses `Float.nan` as the hz value when a block produced
/// no pitch (silence / unvoiced).
///
/// `os_unfair_lock` is deliberately not used here — it can priority-invert
/// when contended with the audio render thread.
///
/// **Ordering note:** `.relaxed` is sound here only because the slot *is* the
/// entire published state. If we ever start publishing side-channel data
/// alongside (e.g. a feature buffer or a status flag in another atomic),
/// this must become `release` on the producer and `acquire` on the consumer
/// so the related stores cannot be reordered across the slot store.
final class PitchSlot {
    private let packed = ManagedAtomic<UInt64>(0)

    /// Audio thread writes. `seq` is expected to be monotonically increasing.
    /// `hz` of `Float.nan` is the "block had no detection" sentinel.
    func store(seq: UInt32, hz: Float) {
        let packedValue = (UInt64(seq) << 32) | UInt64(hz.bitPattern)
        packed.store(packedValue, ordering: .relaxed)
    }

    /// UI thread reads. Returns the most recent (seq, hz) pair.
    func load() -> (seq: UInt32, hz: Float) {
        let value = packed.load(ordering: .relaxed)
        let seq = UInt32(truncatingIfNeeded: value >> 32)
        let hz = Float(bitPattern: UInt32(truncatingIfNeeded: value))
        return (seq, hz)
    }
}
