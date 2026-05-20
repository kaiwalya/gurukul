import Atomics
import CoreAudio
import Foundation
import OSLog

private let log = Logger(subsystem: "com.kaiwalya.Gurukul", category: "HALOutput")

/// Direct CoreAudio HAL output path. Counterpart to the `installHALInput`
/// machinery in `AudioPipeline`.
///
/// Owns one `AudioDeviceCreateIOProcID` callback on a user-selectable
/// output device. Exposes a single-producer / single-consumer ring of mono
/// Float32 frames at the engine sample rate (48 kHz). The cabinet's audio
/// thread writes; the output IO proc reads, fans the mono samples across
/// the device's native channels, and hands the result to CoreAudio.
///
/// Phase 1.4.8 PR 3 ships this **dark** — the ring is never written to by
/// the production code path, so the device renders silence. The only
/// caller that pushes samples is the debug-menu sidetone toggle, which is
/// `#if DEBUG`-gated and exists purely so the developer can manually
/// verify the IO proc fires and the sample clock advances.
///
/// PR 5 (debug pane) introduces the first real producer: a user-selected
/// node-output port whose samples are pushed into the ring when the
/// monitor toggle is on.
///
/// Threading
/// ---------
/// - `start()` / `stop()` / `swapDevice()` run on the main thread.
/// - `writeMono(_:count:)` is realtime-safe and is intended to be called
///   from the input HAL IO proc (the same audio thread that calls
///   `engine_process_block`). Single producer.
/// - The output IO proc is the single consumer. It runs on CoreAudio's
///   output device thread, which is a distinct thread from the input IO
///   proc — hence the SPSC ring rather than a plain shared scratch.
nonisolated final class HALOutput {
    /// Engine sample rate. The ring stores frames at this rate; the
    /// device's native rate must match (we don't resample on output for
    /// the same reasons we don't on input — see `installHALInput`).
    private let sampleRate: UInt32

    /// Number of frames the ring can hold. 8192 mono frames = ~170 ms at
    /// 48 kHz, comfortably above any device buffer cycle we've seen.
    /// Power of two so head/tail wrapping is a mask.
    private let ringCapacity: Int = 8192
    private let ringMask: Int

    /// Lock-free SPSC ring. Writer publishes via `head`, reader consumes
    /// via `tail`. Capacity = `ringCapacity`; element count is
    /// `(head - tail) & ringMask`. Pre-allocated; never resized.
    private let ring: UnsafeMutableBufferPointer<Float>
    private let head = ManagedAtomic<Int>(0)
    private let tail = ManagedAtomic<Int>(0)

    /// IO proc handle. Nil when the device is not engaged.
    private var procID: AudioDeviceIOProcID?
    private var deviceID: AudioDeviceID = kAudioObjectUnknown

    /// Native output stream format. Captured at install so the IO proc
    /// knows how many channels to fan the mono ring across, and whether
    /// the device buffer is interleaved.
    private var deviceFormat: AudioStreamBasicDescription = AudioStreamBasicDescription()

    /// Set BEFORE `AudioDeviceStop`. The IO proc bails immediately if
    /// set, so late callbacks after stop cannot touch torn-down state.
    /// Mirrors the M3 input-side pattern in AudioPipeline.
    private let stopping = ManagedAtomic<Bool>(false)

    /// True while the device is started.
    private(set) var isRunning: Bool = false

    /// Diagnostic counters. Reset on `start`. While this PR ships dark
    /// (no production writer), the IO proc still fires on cadence — so
    /// `underflowCount` is *expected* to be high (one per callback,
    /// roughly). Once PR 5 (debug pane monitor) starts pushing samples
    /// into the ring, underflows should drop to zero in steady state and
    /// rise only during ring/buffer mismatches.
    private let callbackCount = ManagedAtomic<UInt64>(0)
    private let underflowCount = ManagedAtomic<UInt64>(0)
    /// Frames the producer wanted to write but couldn't fit in the ring.
    /// Symmetric with `underflowCount`: useful for diagnosing whether the
    /// audio thread is producing faster than the device drains (vs. the
    /// opposite). Expected to remain zero in steady state.
    private let dropCount = ManagedAtomic<UInt64>(0)

    init(sampleRate: UInt32) {
        self.sampleRate = sampleRate
        precondition(
            ringCapacity > 0 && (ringCapacity & (ringCapacity - 1)) == 0,
            "ringCapacity must be a power of two"
        )
        self.ringMask = ringCapacity - 1
        let buf = UnsafeMutableBufferPointer<Float>.allocate(capacity: ringCapacity)
        buf.initialize(repeating: 0)
        self.ring = buf
    }

    deinit {
        // Best-effort teardown if start/stop wasn't called.
        if procID != nil {
            stopping.store(true, ordering: .relaxed)
            if let pid = procID {
                AudioDeviceStop(deviceID, pid)
                AudioDeviceDestroyIOProcID(deviceID, pid)
            }
        }
        ring.deallocate()
    }

    // MARK: - Lifecycle

    /// Engage the system default output device, install the IO proc, and
    /// start the device. Throws if any CoreAudio step fails — caller
    /// should log and continue without output (input still works).
    func start() throws {
        let id = try defaultOutputDeviceID()
        try start(deviceID: id)
    }

    /// Engage the given output device. Used by the device-swap path.
    func start(deviceID: AudioDeviceID) throws {
        if isRunning { stop() }
        self.deviceID = deviceID

        let asbd = try streamFormat(for: deviceID)
        deviceFormat = asbd

        let isFloat = (asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0
        if !isFloat || asbd.mBitsPerChannel != 32 {
            throw outputError(
                "device output format not Float32 (bits=\(asbd.mBitsPerChannel), flags=\(asbd.mFormatFlags))"
            )
        }
        if UInt32(asbd.mSampleRate.rounded()) != sampleRate {
            throw outputError(
                "device sample rate \(asbd.mSampleRate) != engine rate \(sampleRate); output resampling not implemented"
            )
        }

        // Clear ring on every (re-)start so a previous device's tail
        // doesn't leak into the new session's first callbacks.
        ring.update(repeating: 0)
        head.store(0, ordering: .relaxed)
        tail.store(0, ordering: .relaxed)
        callbackCount.store(0, ordering: .relaxed)
        underflowCount.store(0, ordering: .relaxed)
        dropCount.store(0, ordering: .relaxed)

        // Install the IO proc.
        var pid: AudioDeviceIOProcID?
        let createStatus = AudioDeviceCreateIOProcID(
            deviceID,
            Self.ioProc,
            Unmanaged.passUnretained(self).toOpaque(),
            &pid
        )
        try check(createStatus, "AudioDeviceCreateIOProcID (output)")
        guard let pid else {
            throw outputError("AudioDeviceCreateIOProcID returned nil procID")
        }
        procID = pid

        stopping.store(false, ordering: .relaxed)
        let startStatus = AudioDeviceStart(deviceID, pid)
        try check(startStatus, "AudioDeviceStart (output)")
        isRunning = true

        let name = deviceNameOrUnknown(for: deviceID)
        log.info("""
        HAL output engaged: device='\(name, privacy: .public)' id=\(deviceID, privacy: .public)
          native=\(self.describe(asbd), privacy: .public)
        """)
    }

    func stop() {
        stopping.store(true, ordering: .relaxed)
        if let pid = procID {
            AudioDeviceStop(deviceID, pid)
            AudioDeviceDestroyIOProcID(deviceID, pid)
        }
        procID = nil
        isRunning = false
        let cb = callbackCount.load(ordering: .relaxed)
        let un = underflowCount.load(ordering: .relaxed)
        let dr = dropCount.load(ordering: .relaxed)
        log.info(
            "HAL output stopped — callbacks=\(cb, privacy: .public), underflows=\(un, privacy: .public), drops=\(dr, privacy: .public)"
        )
    }

    // MARK: - Producer API (audio thread)

    /// Write `count` mono Float32 frames into the ring. Realtime-safe:
    /// no allocation, no locks, single-producer.
    ///
    /// Returns the number of frames actually written, which equals
    /// `count` unless the ring would overflow — in that case we drop the
    /// excess (audio callbacks falling behind is a bug elsewhere; we
    /// favour fresh output over blocking).
    ///
    /// Head/tail are monotonically increasing Ints; the ring index is
    /// `head & ringMask`. Empty iff `head == tail`; full iff
    /// `head - tail == ringCapacity`.
    @discardableResult
    func writeMono(_ src: UnsafePointer<Float>, count: Int) -> Int {
        if count <= 0 { return 0 }

        let curHead = head.load(ordering: .relaxed)
        let curTail = tail.load(ordering: .acquiring)
        let used = curHead - curTail
        let free = ringCapacity - used
        if free <= 0 {
            dropCount.wrappingIncrement(by: UInt64(count), ordering: .relaxed)
            return 0
        }
        let toCopy = min(count, free)
        if toCopy < count {
            dropCount.wrappingIncrement(by: UInt64(count - toCopy), ordering: .relaxed)
        }

        let baseIdx = curHead & ringMask
        let firstSpan = min(toCopy, ringCapacity - baseIdx)
        let secondSpan = toCopy - firstSpan
        let dst = ring.baseAddress!
        if firstSpan > 0 {
            memcpy(dst.advanced(by: baseIdx), src, firstSpan * MemoryLayout<Float>.size)
        }
        if secondSpan > 0 {
            memcpy(dst, src.advanced(by: firstSpan), secondSpan * MemoryLayout<Float>.size)
        }

        head.store(curHead + toCopy, ordering: .releasing)
        return toCopy
    }

    // MARK: - IO proc (consumer)

    private static let ioProc: AudioDeviceIOProc = {
        _, _, _, _, outOutputData, _, clientData in
        guard let clientData else { return noErr }
        let output = Unmanaged<HALOutput>.fromOpaque(clientData).takeUnretainedValue()
        return output.handle(output: outOutputData)
    }

    private func handle(output: UnsafeMutablePointer<AudioBufferList>?) -> OSStatus {
        if stopping.load(ordering: .relaxed) {
            return noErr
        }
        guard let output else { return noErr }
        callbackCount.wrappingIncrement(ordering: .relaxed)

        let listPtr = UnsafeMutableAudioBufferListPointer(output)
        guard listPtr.count > 0 else { return noErr }

        let nChannels = Int(deviceFormat.mChannelsPerFrame)
        let isNonInterleaved =
            (deviceFormat.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0

        let firstBuf = listPtr[0]
        let bytesPerSample = MemoryLayout<Float>.size
        let frames: Int
        if isNonInterleaved {
            frames = Int(firstBuf.mDataByteSize) / bytesPerSample
        } else {
            frames = Int(firstBuf.mDataByteSize) / (bytesPerSample * max(nChannels, 1))
        }
        if frames <= 0 { return noErr }

        // Defensive zero-fill of every buffer in the list. CoreAudio
        // hands us buffers whose contents are undefined; if a future
        // device exposes hidden streams (more buffers than channels we
        // intend to drive), zeroing first guarantees they render silence
        // instead of leaked memory.
        for i in 0..<listPtr.count {
            let buf = listPtr[i]
            if let data = buf.mData {
                memset(data, 0, Int(buf.mDataByteSize))
            }
        }

        // Consume up to `frames` mono samples from the ring.
        let curTail = tail.load(ordering: .relaxed)
        let curHead = head.load(ordering: .acquiring)
        let available = curHead - curTail
        let toRead = min(frames, available)
        if toRead < frames {
            underflowCount.wrappingIncrement(ordering: .relaxed)
        }

        // Fan the mono samples across all channels. Frames beyond
        // `toRead` are already zero (upfront defensive fill above).
        if isNonInterleaved {
            for c in 0..<min(nChannels, listPtr.count) {
                let buf = listPtr[c]
                guard let dst = buf.mData?.bindMemory(to: Float.self, capacity: frames) else {
                    continue
                }
                for f in 0..<toRead {
                    let ringIdx = (curTail + f) & ringMask
                    dst[f] = ring[ringIdx]
                }
            }
        } else {
            guard let dst = firstBuf.mData?.bindMemory(
                to: Float.self,
                capacity: frames * nChannels
            ) else {
                return noErr
            }
            for f in 0..<toRead {
                let ringIdx = (curTail + f) & ringMask
                let sample = ring[ringIdx]
                let base = f * nChannels
                for c in 0..<nChannels {
                    dst[base + c] = sample
                }
            }
        }

        // Advance tail (release so writer sees it).
        tail.store(curTail + toRead, ordering: .releasing)
        return noErr
    }

    // MARK: - Diagnostics

    /// Snapshot of callback / underflow / drop counters since the last
    /// `start`. While this PR ships dark, callbacks > 0 and underflows ≈
    /// callbacks is expected. Drops should be zero unless the cabinet's
    /// audio thread is pushing samples faster than the device drains.
    func diagnostics() -> (callbacks: UInt64, underflows: UInt64, drops: UInt64) {
        (
            callbackCount.load(ordering: .relaxed),
            underflowCount.load(ordering: .relaxed),
            dropCount.load(ordering: .relaxed)
        )
    }

    // MARK: - Helpers

    private func defaultOutputDeviceID() throws -> AudioDeviceID {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain
        )
        var id: AudioDeviceID = 0
        var size = UInt32(MemoryLayout<AudioDeviceID>.size)
        let status = AudioObjectGetPropertyData(
            AudioObjectID(kAudioObjectSystemObject),
            &address,
            0, nil,
            &size,
            &id
        )
        try check(status, "AudioObjectGetPropertyData(DefaultOutputDevice)")
        guard id != kAudioObjectUnknown else {
            throw outputError("default output device is kAudioObjectUnknown")
        }
        return id
    }

    private func streamFormat(for deviceID: AudioDeviceID) throws -> AudioStreamBasicDescription {
        var asbd = AudioStreamBasicDescription()
        var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
        var addr = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyStreamFormat,
            mScope: kAudioObjectPropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMain
        )
        let status = AudioObjectGetPropertyData(
            deviceID,
            &addr,
            0, nil,
            &size,
            &asbd
        )
        try check(status, "GetProperty(StreamFormat) on output device")
        return asbd
    }

    private func deviceNameOrUnknown(for deviceID: AudioDeviceID) -> String {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioObjectPropertyName,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain
        )
        var name: Unmanaged<CFString>?
        var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
        let status = AudioObjectGetPropertyData(
            deviceID,
            &address,
            0, nil,
            &size,
            &name
        )
        guard status == noErr, let n = name?.takeRetainedValue() else {
            return "<unknown>"
        }
        return n as String
    }

    private func describe(_ asbd: AudioStreamBasicDescription) -> String {
        let flags = asbd.mFormatFlags
        let isFloat = (flags & kAudioFormatFlagIsFloat) != 0
        let isNonInterleaved = (flags & kAudioFormatFlagIsNonInterleaved) != 0
        let kind = isFloat ? "Float\(asbd.mBitsPerChannel)" : "Int\(asbd.mBitsPerChannel)"
        let layout = isNonInterleaved ? "non-interleaved" : "interleaved"
        return "\(Int(asbd.mSampleRate)) Hz, \(asbd.mChannelsPerFrame) ch, \(kind), \(layout)"
    }

    private func check(_ status: OSStatus, _ what: String) throws {
        if status != noErr {
            throw outputError("\(what) failed: OSStatus \(status)")
        }
    }

    private func outputError(_ message: String) -> NSError {
        NSError(
            domain: "Gurukul.HALOutput",
            code: -1,
            userInfo: [NSLocalizedDescriptionKey: message]
        )
    }
}
