import AVFoundation
import Foundation
import OSLog

/// Unified-logging channel for the audio pipeline. Read from the terminal with:
///   log stream --predicate 'subsystem == "com.kaiwalya.Gurukul"' --info --debug
private let log = Logger(subsystem: "com.kaiwalya.Gurukul", category: "AudioPipeline")

/// Drives the gurukul engine from a live AVAudioEngine microphone tap.
///
/// Owns one `GurukulEngine` and one `AVAudioEngine`. On `start()` it installs
/// an input tap, and for every buffer the tap delivers it copies the samples
/// into the engine's `mic` in-port, calls `engine_process_block`, and reads
/// the last sample of the `pitch` out-port — which YIN produces as Hz.
///
/// Lives off the audio thread for `start` / `stop` / `reset`; the tap callback
/// runs on AVAudioEngine's render thread and is the only place we touch the
/// FFI hot path.
final class AudioPipeline {
    private let avEngine = AVAudioEngine()
    private var enginePtr: OpaquePointer?
    private var micHandle: UInt32 = GURUKUL_INVALID_PORT
    private var pitchHandle: UInt32 = GURUKUL_INVALID_PORT

    /// Monotonic clock anchor set on first successful `start()`. Logged with
    /// every pitch reading so wall-clock drift vs. sample-clock is visible.
    private var startInstant: ContinuousClock.Instant?

    /// Total frames the engine has processed since start. Bumped by every
    /// `engine_process_block(n)` call. Logged alongside wall-clock-dt so any
    /// underrun/overrun (sample-clock falling behind wall-clock) is obvious.
    private var sampleClock: UInt64 = 0

    /// Sample rate the engine is built for. The mic tap is configured to
    /// deliver buffers at this rate via an AVAudioConverter setup below.
    private let sampleRate: UInt32 = 48000

    /// Maximum frames per process_block call. AVAudioEngine delivers buffers
    /// of varying sizes (typically 256-4096); the engine clamps to block_size
    /// and we feed it in chunks if a buffer is larger.
    private let blockSize: Int = 1024

    /// The world driving the engine in this skeleton: one mic in-port, a YIN
    /// pitch tracker, one pitch out-port. Hardcoded for now — later phases
    /// load worlds from disk or the editor.
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
        startInstant = ContinuousClock.now
        sampleClock = 0
        log.info("started — sample rate \(self.sampleRate, privacy: .public) Hz, block size \(self.blockSize, privacy: .public)")
    }

    func stop() {
        avEngine.inputNode.removeTap(onBus: 0)
        avEngine.stop()
        if let ptr = enginePtr {
            engine_reset(ptr)
        }
        log.info("stopped")
    }

    deinit {
        if let ptr = enginePtr {
            engine_free(ptr)
        }
    }

    // MARK: - Engine setup

    private func buildEngineIfNeeded() throws {
        guard enginePtr == nil else { return }

        var ptr: OpaquePointer?
        let rc = Self.worldJSON.withCString { cstr in
            engine_build(cstr, sampleRate, blockSize, &ptr)
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

        // We want mono Float32 at our engine's sample rate. AVAudioEngine will
        // give us whatever the hardware provides; install the tap in the
        // hardware format and convert per buffer.
        guard let engineFormat = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: Double(sampleRate),
            channels: 1,
            interleaved: false
        ) else {
            throw pipelineError("could not construct engine AVAudioFormat")
        }
        let converter = AVAudioConverter(from: hwFormat, to: engineFormat)
        guard let converter else {
            throw pipelineError("could not construct AVAudioConverter")
        }

        log.info("hw input format: \(hwFormat, privacy: .public)")
        let tapBufferSize: AVAudioFrameCount = 4096
        inputNode.installTap(
            onBus: 0,
            bufferSize: tapBufferSize,
            format: hwFormat
        ) { [weak self] buffer, _ in
            self?.handleInputBuffer(buffer, converter: converter, engineFormat: engineFormat)
        }
        log.info("tap installed on input bus 0 (buffer \(tapBufferSize, privacy: .public) frames)")
    }

    /// Counter so we can periodically confirm buffers are flowing without
    /// flooding the log on every callback.
    private var bufferCount: Int = 0

    private func handleInputBuffer(
        _ buffer: AVAudioPCMBuffer,
        converter: AVAudioConverter,
        engineFormat: AVAudioFormat
    ) {
        bufferCount += 1
        if bufferCount % 25 == 1 {
            // Compute peak amplitude over the raw (hw-format) buffer so we
            // can tell whether any signal is arriving at all, independent
            // of YIN's detection threshold.
            var peak: Float = 0
            if let ch = buffer.floatChannelData?[0] {
                for i in 0..<Int(buffer.frameLength) {
                    let v = abs(ch[i])
                    if v > peak { peak = v }
                }
            }
            log.info("tap #\(self.bufferCount, privacy: .public) frames=\(buffer.frameLength, privacy: .public) peak=\(peak, format: .fixed(precision: 4), privacy: .public)")
        }
        // TODO(1.4.6): RT-safe — pre-allocate this buffer once at installTap
        // time and reuse it across callbacks; replace the per-pitch print()
        // with an atomic Float that the UI reads on a display-link timer.
        // Convert to mono Float32 at the engine's sample rate.
        let capacity = AVAudioFrameCount(Double(buffer.frameLength) *
            engineFormat.sampleRate / buffer.format.sampleRate) + 64
        guard let converted = AVAudioPCMBuffer(
            pcmFormat: engineFormat,
            frameCapacity: capacity
        ) else {
            return
        }
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
        guard let src = converted.floatChannelData?[0] else { return }
        feedEngine(src: src, frames: Int(converted.frameLength))
    }

    private func feedEngine(src: UnsafePointer<Float>, frames: Int) {
        guard let ptr = enginePtr else { return }

        // Walk the converted buffer in block_size chunks.
        var offset = 0
        while offset < frames {
            let n = min(blockSize, frames - offset)

            // 1. Fetch the writable in-port slice.
            var inPtr: UnsafeMutablePointer<Float>?
            var inLen: Int = 0
            let rc1 = engine_in_port(ptr, micHandle, &inPtr, &inLen)
            guard rc1 == GURUKUL_OK, let writableMic = inPtr, inLen >= n else {
                log.error("engine_in_port failed rc=\(rc1, privacy: .public)")
                return
            }
            writableMic.update(from: src.advanced(by: offset), count: n)

            // 2. Process the block.
            let rc2 = engine_process_block(ptr, n)
            guard rc2 == GURUKUL_OK else {
                log.error("engine_process_block failed rc=\(rc2, privacy: .public)")
                return
            }
            sampleClock &+= UInt64(n)

            // 3. Read the pitch out-port.
            var outPtr: UnsafePointer<Float>?
            var outLen: Int = 0
            let rc3 = engine_out_port(ptr, pitchHandle, &outPtr, &outLen)
            guard rc3 == GURUKUL_OK, let pitchBuf = outPtr, outLen > 0 else {
                log.error("engine_out_port failed rc=\(rc3, privacy: .public)")
                return
            }
            let lastPitch = pitchBuf[outLen - 1]
            if lastPitch > 0 {
                let dt = startInstant.map { ContinuousClock.now - $0 } ?? .zero
                let dtSec = Double(dt.components.seconds) +
                    Double(dt.components.attoseconds) * 1e-18
                let sampSec = Double(sampleClock) / Double(sampleRate)
                let lagMs = (dtSec - sampSec) * 1000
                log.debug("pitch \(lastPitch, format: .fixed(precision: 1), privacy: .public) Hz t=\(dtSec, format: .fixed(precision: 3), privacy: .public)s lag=\(lagMs, format: .fixed(precision: 1), privacy: .public)ms")
            }

            offset += n
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
