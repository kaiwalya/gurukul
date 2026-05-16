import AVFoundation
import Foundation

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
          "kind": "pitch-yin",
          "params": {
            "threshold": 0.15,
            "min_hz": 70.0,
            "max_hz": 1000.0,
            "window_size": 2048
          }
        }
      ],
      "connections": [
        { "from": "mic", "to": "yin.audio" },
        { "from": "yin.pitch_hz", "to": "pitch" }
      ]
    }
    """

    // MARK: - Lifecycle

    func start() throws {
        try buildEngineIfNeeded()
        try installTap()
        try avEngine.start()
        print("[AudioPipeline] started — sample rate \(sampleRate) Hz, block size \(blockSize)")
    }

    func stop() {
        avEngine.inputNode.removeTap(onBus: 0)
        avEngine.stop()
        if let ptr = enginePtr {
            engine_reset(ptr)
        }
        print("[AudioPipeline] stopped")
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

        let tapBufferSize: AVAudioFrameCount = 4096
        inputNode.installTap(
            onBus: 0,
            bufferSize: tapBufferSize,
            format: hwFormat
        ) { [weak self] buffer, _ in
            self?.handleInputBuffer(buffer, converter: converter, engineFormat: engineFormat)
        }
    }

    private func handleInputBuffer(
        _ buffer: AVAudioPCMBuffer,
        converter: AVAudioConverter,
        engineFormat: AVAudioFormat
    ) {
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
            print("[AudioPipeline] converter error: \(error?.localizedDescription ?? "unknown")")
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
                print("[AudioPipeline] engine_in_port failed rc=\(rc1)")
                return
            }
            writableMic.update(from: src.advanced(by: offset), count: n)

            // 2. Process the block.
            let rc2 = engine_process_block(ptr, n)
            guard rc2 == GURUKUL_OK else {
                print("[AudioPipeline] engine_process_block failed rc=\(rc2)")
                return
            }

            // 3. Read the pitch out-port.
            var outPtr: UnsafePointer<Float>?
            var outLen: Int = 0
            let rc3 = engine_out_port(ptr, pitchHandle, &outPtr, &outLen)
            guard rc3 == GURUKUL_OK, let pitchBuf = outPtr, outLen > 0 else {
                print("[AudioPipeline] engine_out_port failed rc=\(rc3)")
                return
            }
            let lastPitch = pitchBuf[outLen - 1]
            if lastPitch > 0 {
                print(String(format: "[pitch] %.1f Hz", lastPitch))
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
