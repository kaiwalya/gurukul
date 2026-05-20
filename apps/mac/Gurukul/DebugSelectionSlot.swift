import Atomics
import Foundation

/// Maximum byte length (incl. null terminator) for node ids and port names
/// stored in `DebugSelectionSnapshot`. Engine ids are short (`pitch_yin`,
/// `vibrato_det`, etc.) so 31 bytes + NUL is generous and avoids any
/// allocation on the audio thread when the writer publishes.
let kDebugIdMaxBytes = 32

/// What the audio thread needs to know per hop to publish a debug tap:
/// which node + port to read, what shape it is (so the writer doesn't
/// have to re-derive it), and whether monitor is engaged.
///
/// Empty `nodeId` (first byte == 0) means "no selection — skip the tap
/// read entirely this hop." The slot starts in this state and is reset
/// to it on any engine rebuild / reset, satisfying the
/// "monitor disengages on engine.reset" invariant from PHASE_1_4_8.md.
///
/// The strings are stored as fixed-size C buffers (null-terminated UTF-8)
/// so the audio thread can hand the pointers straight to `engine_read_port`
/// without any Swift String bridging cost.
struct DebugSelectionSnapshot {
    /// Increments on every publish from the UI side.
    var generation: UInt32
    /// PortShape rawValue (defined in 5.2). PR 5.1 carries it through but
    /// doesn't classify — the dark hardcoded selection sets it to 255 so
    /// the slot reader can tell "real selection" from "placeholder."
    var typeTag: UInt8
    /// True iff the user toggled monitor on AND `typeTag` is audio-shaped.
    /// The audio thread checks this before pushing to halOutput.writeMono.
    var monitor: Bool
    /// Null-terminated UTF-8 node id (e.g. "pitch_yin"). First byte 0 ⇒
    /// "no selection".
    var nodeIdBytes: [UInt8]
    /// Null-terminated UTF-8 port name (e.g. "f0").
    var portBytes: [UInt8]

    static let empty = DebugSelectionSnapshot(
        generation: 0,
        typeTag: 255,
        monitor: false,
        nodeIdBytes: Array(repeating: 0, count: kDebugIdMaxBytes),
        portBytes: Array(repeating: 0, count: kDebugIdMaxBytes)
    )

    /// True if `nodeIdBytes[0] != 0` — the audio thread reads this once
    /// per hop to decide whether to do anything.
    var hasSelection: Bool {
        return !nodeIdBytes.isEmpty && nodeIdBytes[0] != 0
    }
}

/// Lock-free SPSC double-buffer for `DebugSelectionSnapshot`. The UI
/// writes rarely (only when the user changes a picker), the audio thread
/// reads every hop.
///
/// Two slots are enough: the writer publishes by atomic index swap; the
/// reader copies-out before using, so there's no need for a third slot
/// to insulate against a concurrent re-write. (The triple-buffer was
/// chosen for FeatureSlot/WaveformSlot to keep the writer wait-free
/// against an in-flight reader; here the writer is the slow one and can
/// pay any contention cost.)
///
/// We mirror the WaveformSlot raw-storage pattern: per-slot string
/// buffers + scalar fields, never a struct-with-array stored directly
/// (would invoke COW on the audio thread).
final class DebugSelectionSlot {
    private let nodeIdStorage: UnsafeMutablePointer<UInt8>  // 2 * kDebugIdMaxBytes
    private let portStorage: UnsafeMutablePointer<UInt8>    // 2 * kDebugIdMaxBytes
    private let generations: UnsafeMutablePointer<UInt32>   // 2
    private let typeTags: UnsafeMutablePointer<UInt8>       // 2
    private let monitors: UnsafeMutablePointer<UInt8>       // 2 (0 / 1)
    private let latest = ManagedAtomic<UInt8>(0)
    /// Serialises concurrent writers (e.g. UI tick + engine-reset on the
    /// swap queue). Audio thread does NOT take this lock — it only reads.
    private let writerLock = ManagedAtomic<UInt8>(0)

    init() {
        nodeIdStorage = UnsafeMutablePointer<UInt8>.allocate(capacity: 2 * kDebugIdMaxBytes)
        nodeIdStorage.initialize(repeating: 0, count: 2 * kDebugIdMaxBytes)
        portStorage = UnsafeMutablePointer<UInt8>.allocate(capacity: 2 * kDebugIdMaxBytes)
        portStorage.initialize(repeating: 0, count: 2 * kDebugIdMaxBytes)
        generations = UnsafeMutablePointer<UInt32>.allocate(capacity: 2)
        generations.initialize(repeating: 0, count: 2)
        typeTags = UnsafeMutablePointer<UInt8>.allocate(capacity: 2)
        typeTags.initialize(repeating: 255, count: 2)
        monitors = UnsafeMutablePointer<UInt8>.allocate(capacity: 2)
        monitors.initialize(repeating: 0, count: 2)
    }

    deinit {
        nodeIdStorage.deallocate()
        portStorage.deallocate()
        generations.deallocate()
        typeTags.deallocate()
        monitors.deallocate()
    }

    /// UI / control thread publishes. Strings longer than
    /// `kDebugIdMaxBytes - 1` UTF-8 bytes are truncated (we don't have
    /// node ids that long today; assert in debug if we ever do).
    func store(
        generation: UInt32,
        nodeId: String,
        port: String,
        typeTag: UInt8,
        monitor: Bool
    ) {
        // Spin to serialise concurrent writers. The audio thread never
        // contends here.
        while !writerLock.weakCompareExchange(
            expected: 0,
            desired: 1,
            ordering: .acquiring
        ).exchanged {}
        defer { writerLock.store(0, ordering: .releasing) }

        let cur = latest.load(ordering: .relaxed)
        let target: UInt8 = cur == 0 ? 1 : 0

        let nodeBase = nodeIdStorage.advanced(by: Int(target) * kDebugIdMaxBytes)
        let portBase = portStorage.advanced(by: Int(target) * kDebugIdMaxBytes)
        writeCString(nodeId, into: nodeBase, capacity: kDebugIdMaxBytes)
        writeCString(port, into: portBase, capacity: kDebugIdMaxBytes)
        generations[Int(target)] = generation
        typeTags[Int(target)] = typeTag
        monitors[Int(target)] = monitor ? 1 : 0
        latest.store(target, ordering: .releasing)
    }

    /// Clear the slot — equivalent to publishing `.empty`. Called from
    /// engine rebuild / reset paths.
    func clear() {
        store(generation: 0, nodeId: "", port: "", typeTag: 255, monitor: false)
    }

    /// Audio thread reads. Copies into a fresh snapshot.
    func load() -> DebugSelectionSnapshot {
        let idx = latest.load(ordering: .acquiring)
        let nodeBase = nodeIdStorage.advanced(by: Int(idx) * kDebugIdMaxBytes)
        let portBase = portStorage.advanced(by: Int(idx) * kDebugIdMaxBytes)
        var nodeBytes = Array(repeating: UInt8(0), count: kDebugIdMaxBytes)
        var portBytes = Array(repeating: UInt8(0), count: kDebugIdMaxBytes)
        nodeBytes.withUnsafeMutableBufferPointer { dst in
            if let dstBase = dst.baseAddress {
                dstBase.update(from: nodeBase, count: kDebugIdMaxBytes)
            }
        }
        portBytes.withUnsafeMutableBufferPointer { dst in
            if let dstBase = dst.baseAddress {
                dstBase.update(from: portBase, count: kDebugIdMaxBytes)
            }
        }
        return DebugSelectionSnapshot(
            generation: generations[Int(idx)],
            typeTag: typeTags[Int(idx)],
            monitor: monitors[Int(idx)] != 0,
            nodeIdBytes: nodeBytes,
            portBytes: portBytes
        )
    }

    /// Audio-thread fast path: returns true with `nodeBase` / `portBase`
    /// pointing at the current slot's null-terminated buffers if a
    /// selection exists; false otherwise. No allocation, no copy.
    ///
    /// The returned pointers are valid until the next `store` / `clear`
    /// call. The audio thread holds them only for the duration of one
    /// `engine_read_port` invocation.
    ///
    /// **Writer-cadence precondition: at most one `store`/`clear` may
    /// occur per audio hop.** Today this holds because writes come from
    /// either (a) the UI tick on user picker change, which can't fire
    /// twice in one hop interval (~10 ms at 48 kHz hop=512), or (b) the
    /// engine swap queue during reset, which is serial and only runs
    /// after the audio thread has been stopped via `AudioDeviceStop`. A
    /// second writer landing in the same hop would target the slot the
    /// audio thread is reading and overwrite it mid-FFI-call. If any
    /// future caller breaks this invariant we need a third buffer or a
    /// generation-check on the audio side.
    func borrow(
        nodeBase: inout UnsafePointer<UInt8>?,
        portBase: inout UnsafePointer<UInt8>?,
        typeTag: inout UInt8,
        monitor: inout Bool
    ) -> Bool {
        let idx = latest.load(ordering: .acquiring)
        let nodePtr = UnsafePointer(nodeIdStorage.advanced(by: Int(idx) * kDebugIdMaxBytes))
        let portPtr = UnsafePointer(portStorage.advanced(by: Int(idx) * kDebugIdMaxBytes))
        if nodePtr[0] == 0 {
            return false
        }
        nodeBase = nodePtr
        portBase = portPtr
        typeTag = typeTags[Int(idx)]
        monitor = monitors[Int(idx)] != 0
        return true
    }
}

/// Write `s` as null-terminated UTF-8 into `dst[0..<capacity]`. Truncates
/// at `capacity - 1` bytes, always writes the terminator. Allocates only
/// the temporary `Array(s.utf8)` — fine because writes come from the UI
/// thread, never the audio thread.
private func writeCString(_ s: String, into dst: UnsafeMutablePointer<UInt8>, capacity: Int) {
    let bytes = Array(s.utf8)
    let copy = min(bytes.count, capacity - 1)
    for i in 0..<copy {
        dst[i] = bytes[i]
    }
    dst[copy] = 0
}
