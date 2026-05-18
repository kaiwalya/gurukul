import AVFoundation
import Foundation
import OSLog

/// Unified-logging channel for the audio pipeline. Read from the terminal with:
///   log stream --predicate 'subsystem == "com.kaiwalya.Gurukul"' --info --debug
private let log = Logger(subsystem: "com.kaiwalya.Gurukul", category: "AudioPipeline")

/// Drives the gurukul engine from a live AVAudioEngine microphone tap.
///
/// Owns one `GurukulEngine` and one `AVAudioEngine`. On `start()` it installs
/// an input tap, accumulates incoming frames into a pre-allocated scratch
/// buffer, and drains the engine in hop-aligned chunks. The latest pitch
/// reading lands in a lock-free `PitchSlot` the UI polls at 30 Hz.
///
/// Lives off the audio thread for `start` / `stop`; the tap callback runs on
/// AVAudioEngine's render thread and is the only place we touch the FFI hot
/// path. The render thread must not allocate, lock, or call into Swift
/// runtime functions that can — see the audit at the end of this file.
/// Project default is `MainActor`; the pipeline is explicitly `nonisolated`
/// because the audio render thread runs the tap callback and cannot be on the
/// main actor. SwiftUI touches `pitchSlot` (lock-free) and the
/// `start`/`stop` methods, which are quick and safe to call from the main
/// thread.
nonisolated final class AudioPipeline {
    /// Public, read-only handle the view polls every UI tick.
    let pitchSlot = PitchSlot()

    private let avEngine = AVAudioEngine()
    private var enginePtr: OpaquePointer?
    private var micHandle: UInt32 = GURUKUL_INVALID_PORT
    private var pitchHandle: UInt32 = GURUKUL_INVALID_PORT

    /// Sample rate the engine is built for. The mic tap is configured to
    /// deliver buffers at this rate via an AVAudioConverter setup below.
    private let sampleRate: UInt32 = 48000

    /// Hop size that matches PitchYin's `hop` param in the world JSON. The
    /// engine is fed exactly `hop` frames per `process_block` call so YIN's
    /// hop counter never straddles a buffer boundary with stale state.
    private let hop: Int = 512

    /// Per-hop RMS gate. YIN happily locks onto noise-floor content at any
    /// input level, so even mic-muted we'd see a stable bogus pitch. We
    /// publish the YIN value only when the hop's RMS is above this floor;
    /// below it, we publish NaN ("block ran, no detection") so the UI dims.
    /// -50 dBFS ≈ 0.00316 linear; tune as we get more devices on the
    /// matrix.
    private let silenceRmsFloor: Float = 0.00316

    /// Pre-allocated scratch that holds converted mono float frames between
    /// callbacks. Sized generously (max tap delivery + a few hops of slack)
    /// so we never have to grow it on the audio thread. Owned by the
    /// pipeline; lives until `deinit`.
    private var scratch: UnsafeMutableBufferPointer<Float>?
    private var scratchFill: Int = 0
    private var scratchCapacity: Int = 0

    /// Monotonic sequence number stamped on every block we process. UI uses
    /// it to tell "no new block" from "block with no detection".
    private var seq: UInt32 = 0

    /// Re-usable destination for the tap → engine-format conversion. Stays
    /// the same shape callback after callback because both source format
    /// and target format are constant. Nil when the hw format already
    /// matches the engine format (the common case on macOS with built-in
    /// mics + display audio) — we just hand the raw buffer through.
    private var convertedBuffer: AVAudioPCMBuffer?
    private var converter: AVAudioConverter?

    /// The world driving the engine in this skeleton: one mic in-port, a YIN
    /// pitch tracker, one pitch out-port.
    private static let worldJSON: String = """
    {
      "world_version": 1,
      "in_ports": [
        { "id": "mic" }
      ],
      "out_ports": [
        { "id": "pitch" }
      ],
      "nodes": [
        {
          "id": "yin",
          "type": "PitchYin",
          "params": {
            "window": 2048,
            "hop": 512,
            "fmin_hz": 70.0,
            "fmax_hz": 1000.0,
            "threshold": 0.15
          }
        }
      ],
      "connections": [
        { "from": "mic", "to": "yin.audio_in" },
        { "from": "yin.f0", "to": "pitch" }
      ]
    }
    """

    // MARK: - Lifecycle

    func start() throws {
        try buildEngineIfNeeded()
        try installTap()
        try avEngine.start()
        let inLat = avEngine.inputNode.presentationLatency
        let outLat = avEngine.inputNode.outputPresentationLatency
        log.info("started — sample rate \(self.sampleRate, privacy: .public) Hz, hop \(self.hop, privacy: .public), inputPresentationLatency=\(inLat * 1000, format: .fixed(precision: 1), privacy: .public)ms, outputPresentationLatency=\(outLat * 1000, format: .fixed(precision: 1), privacy: .public)ms")
    }

    func stop() {
        // Remove the tap first so no new callbacks land while we're tearing
        // down the engine. AVAudioEngine's removeTap is documented to wait
        // for in-flight callbacks to drain before returning.
        avEngine.inputNode.removeTap(onBus: 0)
        avEngine.stop()
        if let ptr = enginePtr {
            engine_reset(ptr)
        }
        scratchFill = 0
        log.info("stopped")
    }

    deinit {
        // No MainActor work here — just free C memory and the scratch.
        if let ptr = enginePtr {
            engine_free(ptr)
        }
        if let scratch = scratch {
            scratch.deallocate()
        }
    }

    // MARK: - Engine setup

    private func buildEngineIfNeeded() throws {
        guard enginePtr == nil else { return }

        var ptr: OpaquePointer?
        let rc = Self.worldJSON.withCString { cstr in
            engine_build(cstr, sampleRate, hop, &ptr)
        }
        guard rc == GURUKUL_OK, let built = ptr else {
            throw pipelineError("engine_build failed (rc=\(rc))")
        }
        enginePtr = built

        micHandle = "mic".withCString { engine_resolve_in_port(built, $0) }
        pitchHandle = "pitch".withCString { engine_resolve_out_port(built, $0) }
        guard micHandle != GURUKUL_INVALID_PORT, pitchHandle != GURUKUL_INVALID_PORT else {
            throw pipelineError("resolve_in_port/out_port failed")
        }
    }

    // MARK: - Audio tap

    private func installTap() throws {
        let inputNode = avEngine.inputNode
        let hwFormat = inputNode.inputFormat(forBus: 0)
        guard hwFormat.sampleRate > 0 else {
            throw pipelineError("input format has zero sample rate — is mic permission granted?")
        }

        guard let engineFormat = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: Double(sampleRate),
            channels: 1,
            interleaved: false
        ) else {
            throw pipelineError("could not construct engine AVAudioFormat")
        }

        let needsConversion = hwFormat.sampleRate != engineFormat.sampleRate
            || hwFormat.channelCount != engineFormat.channelCount
            || hwFormat.commonFormat != engineFormat.commonFormat

        // AVAudioEngine treats bufferSize as a hint. Asking for 512 frames
        // (≈10.7 ms at 48 kHz) shrinks tap-side queueing to about one IO
        // cycle — without it AVAudioEngine batched ~100 ms of audio into
        // 200 ms-wall-clock taps on the LG display mic.
        let tapBufferSize: AVAudioFrameCount = 512

        // Worst-case scratch: in practice AVAudioEngine ignores small
        // bufferSize hints somewhat and may hand us larger chunks (we've
        // seen 4800-frame buffers on the LG mic). Size for that worst case
        // so we never overflow.
        let maxObservedTapFrames = 8192
        let scratchCap = maxObservedTapFrames + hop
        let buf = UnsafeMutableBufferPointer<Float>.allocate(capacity: scratchCap)
        buf.initialize(repeating: 0)
        scratch = buf
        scratchCapacity = scratchCap
        scratchFill = 0

        if needsConversion {
            let converter = AVAudioConverter(from: hwFormat, to: engineFormat)
            guard let converter else {
                throw pipelineError("could not construct AVAudioConverter")
            }
            let ratio = engineFormat.sampleRate / hwFormat.sampleRate
            let convertedCapacity = AVAudioFrameCount(Double(maxObservedTapFrames) * ratio + 64)
            guard let converted = AVAudioPCMBuffer(
                pcmFormat: engineFormat,
                frameCapacity: convertedCapacity
            ) else {
                throw pipelineError("could not allocate converter output buffer")
            }
            self.converter = converter
            self.convertedBuffer = converted
            log.info("conversion path engaged: hw \(hwFormat, privacy: .public) → engine \(engineFormat, privacy: .public), converted capacity=\(convertedCapacity, privacy: .public)")
        } else {
            self.converter = nil
            self.convertedBuffer = nil
            log.info("conversion bypassed: hw format already matches engine \(engineFormat, privacy: .public)")
        }

        log.info("hw input format: \(hwFormat, privacy: .public), scratch=\(scratchCap, privacy: .public) frames")
        inputNode.installTap(
            onBus: 0,
            bufferSize: tapBufferSize,
            format: hwFormat
        ) { [weak self] buffer, when in
            self?.handleInputBuffer(buffer, when: when)
        }
        log.info("tap installed on input bus 0 (buffer \(tapBufferSize, privacy: .public) frames, hop \(self.hop, privacy: .public))")
    }

    /// Counter so we can periodically confirm buffers are flowing without
    /// flooding the log on every callback.
    private var bufferCount: Int = 0

    private func handleInputBuffer(
        _ buffer: AVAudioPCMBuffer,
        when _: AVAudioTime
    ) {
        bufferCount += 1

        let frames: Int
        let src: UnsafePointer<Float>

        if let converter = converter, let converted = convertedBuffer {
            converted.frameLength = 0
            var error: NSError?
            var didFeed = false
            let status = converter.convert(to: converted, error: &error) { _, outStatus in
                if didFeed {
                    outStatus.pointee = .noDataNow
                    return nil
                }
                didFeed = true
                outStatus.pointee = .haveData
                return buffer
            }
            guard status != .error, error == nil else {
                log.error("converter error: \(error?.localizedDescription ?? "unknown", privacy: .public)")
                return
            }
            guard let ch = converted.floatChannelData?[0] else { return }
            src = UnsafePointer(ch)
            frames = Int(converted.frameLength)
        } else {
            // Bypass: hw format already matches the engine format.
            guard let ch = buffer.floatChannelData?[0] else { return }
            src = UnsafePointer(ch)
            frames = Int(buffer.frameLength)
        }

        appendAndDrain(src: src, count: frames)

        if bufferCount % 25 == 1 {
            var peak: Float = 0
            for i in 0..<frames {
                let v = abs(src[i])
                if v > peak { peak = v }
            }
            // scratchFill after drain shows whether we're keeping up with
            // realtime. Steady state should sit at 0..<hop. If it grows
            // monotonically, sample-clock drift is the lag source.
            log.info("tap #\(self.bufferCount, privacy: .public) frames=\(frames, privacy: .public) peak=\(peak, format: .fixed(precision: 4), privacy: .public) scratchFill=\(self.scratchFill, privacy: .public)")
        }
    }

    /// Append `count` newly-converted frames into the scratch buffer, then
    /// drain in hop-aligned chunks while there's enough fill. Any leftover
    /// frames (< hop) stay in the scratch for the next callback so YIN sees
    /// a continuous stream.
    private func appendAndDrain(src: UnsafePointer<Float>, count: Int) {
        guard let scratch = scratch else { return }
        guard let ptr = enginePtr else { return }

        // If the incoming chunk would overflow the scratch, drop the oldest
        // frames. This should never fire in practice — scratch is sized for
        // ~2× tap delivery — but it's a soft guard rather than a crash.
        if scratchFill + count > scratchCapacity {
            let overflow = scratchFill + count - scratchCapacity
            if overflow < scratchFill {
                // Drop the OLDEST frames: shift the tail [overflow ..<
                // scratchFill] down to start at index 0.
                scratch.baseAddress!
                    .update(
                        from: scratch.baseAddress!.advanced(by: overflow),
                        count: scratchFill - overflow
                    )
                scratchFill -= overflow
            } else {
                scratchFill = 0
            }
            log.error("scratch overflow — dropped \(overflow, privacy: .public) frames")
        }

        scratch.baseAddress!.advanced(by: scratchFill)
            .update(from: src, count: count)
        scratchFill += count

        // Drain hop-aligned chunks.
        var consumed = 0
        while scratchFill - consumed >= hop {
            var inPtr: UnsafeMutablePointer<Float>?
            var inLen: Int = 0
            let rc1 = engine_in_port(ptr, micHandle, &inPtr, &inLen)
            guard rc1 == GURUKUL_OK, let writableMic = inPtr, inLen >= hop else {
                log.error("engine_in_port failed rc=\(rc1, privacy: .public)")
                return
            }
            let hopStart = scratch.baseAddress!.advanced(by: consumed)
            writableMic.update(from: hopStart, count: hop)

            // RMS over the hop we're about to feed. If below the floor, we'll
            // still process the block (so PitchYin's window stays in sync)
            // but publish NaN instead of the YIN output — the UI sees
            // "silence detected" and dims.
            var sumSq: Float = 0
            for i in 0..<hop {
                let s = hopStart[i]
                sumSq += s * s
            }
            let rms = sqrtf(sumSq / Float(hop))

            let rc2 = engine_process_block(ptr, hop)
            guard rc2 == GURUKUL_OK else {
                log.error("engine_process_block failed rc=\(rc2, privacy: .public)")
                return
            }

            var outPtr: UnsafePointer<Float>?
            var outLen: Int = 0
            let rc3 = engine_out_port(ptr, pitchHandle, &outPtr, &outLen)
            guard rc3 == GURUKUL_OK, let pitchBuf = outPtr, outLen > 0 else {
                log.error("engine_out_port failed rc=\(rc3, privacy: .public)")
                return
            }
            let lastPitch = pitchBuf[outLen - 1]

            seq &+= 1
            // Two ways to publish NaN ("block ran, no detection"):
            //   - signal energy below the silence floor (mic muted / room),
            //   - YIN itself returned 0 Hz (unvoiced / no clear period).
            let voiced = rms >= silenceRmsFloor && lastPitch > 0
            let publishedHz: Float = voiced ? lastPitch : .nan
            pitchSlot.store(seq: seq, hz: publishedHz)

            consumed += hop
        }

        if consumed > 0 {
            let leftover = scratchFill - consumed
            if leftover > 0 {
                scratch.baseAddress!.update(
                    from: scratch.baseAddress!.advanced(by: consumed),
                    count: leftover
                )
            }
            scratchFill = leftover
        }
    }

    // MARK: - Helpers

    private func pipelineError(_ message: String) -> NSError {
        var details = message
        if let cstr = engine_last_error_message() {
            details += " — engine: \(String(cString: cstr))"
        }
        return NSError(
            domain: "Gurukul.AudioPipeline",
            code: -1,
            userInfo: [NSLocalizedDescriptionKey: details]
        )
    }
}

// MARK: - RT-safety audit
//
// The audio-thread callback (`handleInputBuffer` → `appendAndDrain`) is on the
// hot path. It must not allocate, lock, or call Swift-runtime functions that
// can. The current state:
//
// - scratch buffer is pre-allocated in `installTap` and reused forever.
// - converter output buffer is pre-allocated and reused; converter.convert
//   writes into it in place. (Note: the AVAudioConverter internals may still
//   allocate on certain SR ratios; remove with a manual resample later.)
// - PitchSlot.store is a single ManagedAtomic<UInt64> store — lock-free.
// - log.error and log.info on the hot path fire only on rare branches (every
//   25 callbacks for the tap counter, or on actual errors); they are not in
//   the per-block inner loop.
// - No NSError construction on the inner loop.
//
// Known violation (deferred to 1.4.7): the AVAudioConverter path itself is
// not strictly allocation-free across all hardware sample rates. The fix is
// to replace AVAudioConverter with a manual resample using a pre-allocated
// kernel, which is a larger change than 1.4.6's scope.
