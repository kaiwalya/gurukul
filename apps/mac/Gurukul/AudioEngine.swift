import Atomics
import AudioToolbox
import AVFoundation
import CoreAudio
import Foundation
import OSLog

/// Unified-logging channel for the audio pipeline. Read from the terminal with:
///   log stream --predicate 'subsystem == "com.kaiwalya.Gurukul"' --info --debug
private let log = Logger(subsystem: "com.kaiwalya.Gurukul", category: "AudioPipeline")

/// Drives the gurukul engine from a live microphone capture.
///
/// Owns one `GurukulEngine` and one of two capture paths:
///
///   1. **HAL path (preferred):** a CoreAudio
///      `kAudioUnitSubType_HALOutput` AudioUnit, input-enabled, reading
///      from the system default input device. IO procs fire at the
///      device's hardware buffer size (~10 ms), much tighter than the
///      tap path.
///   2. **AVAudioEngine tap (fallback):** the 1.4.6 path, kept verbatim
///      for devices / configurations where HAL setup fails.
///
/// The downstream is identical for both paths: accumulate frames into a
/// pre-allocated scratch buffer, drain in hop-aligned chunks, run a
/// per-hop RMS energy gate, and publish a coherent `FeatureSnapshot`
/// (pitch + onset + breath + vibrato rate/depth) into a lock-free,
/// wait-free `FeatureSlot` (triple-buffered) the UI polls at 30 Hz.
///
/// Project default is `MainActor`; the pipeline is explicitly
/// `nonisolated` because the audio render thread (HAL IO proc or tap
/// callback) cannot be on the main actor. SwiftUI touches `pitchSlot`
/// (lock-free) and the `start`/`stop` methods, which are quick and safe
/// to call from the main thread.
nonisolated final class AudioPipeline {
    /// Public, read-only handle the view polls every UI tick.
    let featureSlot = FeatureSlot()

    /// Decimated 1 s waveform snapshot, published from the same hot path.
    let waveformSlot = WaveformSlot()

    /// Most-recent block read from a user-selected engine port. Filled
    /// by the audio thread when `debugSelectionSlot` has a non-empty
    /// selection; the UI tick reads it in PR 1.4.8.5.2.
    let debugTapSlot = DebugTapSlot()

    /// Which engine port the debug pane wants tapped this hop. Written
    /// by the UI when the user changes pickers; written-to-empty on any
    /// engine rebuild / reset. Audio thread reads every hop.
    let debugSelectionSlot = DebugSelectionSlot()

    /// Monotonically incremented every time the audio thread publishes a
    /// debug tap (or clears it). Decoupled from `seq` so a re-publish of
    /// a cleared snapshot is detectable by the UI.
    private var debugTapSeq: UInt32 = 0

    /// Ring buffer of min/max pairs. One entry covers `samplesPerBucket`
    /// raw frames. `wfHead` is the index of the *next* bucket to write
    /// into; the oldest bucket is at `(wfHead) % kWaveformBuckets`.
    /// `wfFill` counts how many frames have accumulated into the current
    /// bucket; when it hits `samplesPerBucket` we close the bucket and
    /// advance head.
    private var wfLo: [Float] = Array(repeating: 0, count: kWaveformBuckets)
    private var wfHi: [Float] = Array(repeating: 0, count: kWaveformBuckets)
    private var wfHead: Int = 0
    private var wfFill: Int = 0
    private var wfCurLo: Float = 0
    private var wfCurHi: Float = 0
    /// Samples per waveform bucket. Recomputed on every (re)build as
    /// `sampleRate / kWaveformBuckets` so the waveform still represents
    /// a 1-second window across all supported rates.
    /// 48000 / 150 = 320; 96000 / 150 = 640; 44100 / 150 = 294.
    private var samplesPerBucket: Int = 320

    /// Pre-allocated scratch the audio thread copies the unwrapped ring
    /// into before calling `WaveformSlot.store`. Lives at instance scope
    /// so no allocation happens on the hot path.
    private var wfLoOut: [Float] = Array(repeating: 0, count: kWaveformBuckets)
    private var wfHiOut: [Float] = Array(repeating: 0, count: kWaveformBuckets)

/// Live engine handle. Mutated during `applySettings` rebuild. The
    /// load-bearing invariant: no IO proc may dereference this pointer
    /// while it is being reassigned. The rebuild path guarantees this by
    /// calling `AudioDeviceStop` on every audio thread before touching
    /// `enginePtr`, and by remaining on `deviceSwapQueue` for the
    /// duration so concurrent listener fires cannot race.
    private var enginePtr: OpaquePointer?
    private var micHandle: UInt32 = GURUKUL_INVALID_PORT
    private var pitchOut: UInt32 = GURUKUL_INVALID_PORT
    private var onsetOut: UInt32 = GURUKUL_INVALID_PORT
    private var breathOut: UInt32 = GURUKUL_INVALID_PORT
    private var vibratoRateOut: UInt32 = GURUKUL_INVALID_PORT
    private var vibratoDepthOut: UInt32 = GURUKUL_INVALID_PORT

    /// Set true at start of a fresh stream (start, fallback, device
    /// swap). Consumed by the next publish in `appendAndDrain`, which
    /// stamps `discontinuity = true` on that snapshot so the UI ring
    /// buffers know to clear.
    private var pendingDiscontinuity: Bool = true

    /// Sample rate the engine is built for. Mutated only on rebuild
    /// (via `applySettings`), never while audio threads are live. Both
    /// capture paths deliver frames at this rate (HAL refuses devices
    /// at a different native rate; tap path uses AVAudioConverter).
    private var sampleRate: UInt32 = Prefs.defaultSampleRate

    /// Live snapshot of the settings the pipeline is currently running
    /// against. Updated atomically on the swap queue inside
    /// `applySettings`. Reads from elsewhere (UI, listeners) must hop
    /// onto the swap queue too.
    private var currentSettings: AudioSettings = AudioSettings(
        inputDeviceUID: nil,
        outputDeviceUID: nil,
        sampleRate: Prefs.defaultSampleRate,
        bufferSize: Prefs.defaultBufferSize
    )

    /// Construct a pipeline pre-seeded with the given settings. The
    /// pipeline does not start until `start()` (or `applySettings`) is
    /// called.
    init(initialSettings: AudioSettings = AudioSettings(
        inputDeviceUID: nil,
        outputDeviceUID: nil,
        sampleRate: Prefs.defaultSampleRate,
        bufferSize: Prefs.defaultBufferSize
    )) {
        self.currentSettings = initialSettings
        self.sampleRate = initialSettings.sampleRate
        self.samplesPerBucket = Int(initialSettings.sampleRate) / kWaveformBuckets
    }

    /// Hop size that matches PitchYin's `hop` param in the world JSON.
    private let hop: Int = 512

    /// Per-hop RMS gate. YIN happily locks onto noise-floor content at
    /// any input level, so even mic-muted we'd see a stable bogus pitch.
    /// We publish the YIN value only when the hop's RMS is above this
    /// floor; below it, we publish NaN so the UI dims. -50 dBFS ≈
    /// 0.00316 linear.
    private let silenceRmsFloor: Float = 0.01

    /// Pre-allocated scratch that holds mono float frames between
    /// callbacks. Sized for `maxFramesPerSlice + hop` so the IO proc
    /// never has to grow it.
    private var scratch: UnsafeMutableBufferPointer<Float>?
    private var scratchFill: Int = 0
    private var scratchCapacity: Int = 0

    /// Monotonic sequence number stamped on every block we process.
    private var seq: UInt32 = 0

    /// Upper bound on a single IO-proc / tap-callback delivery. HAL
    /// path sets the unit's `MaximumFramesPerSlice` to this; tap path
    /// uses it as scratch sizing for the same reason.
    private let maxFramesPerSlice: Int = 4096

    // MARK: - HAL state (direct AudioDevice IO proc)

    /// IO proc handle returned by `AudioDeviceCreateIOProcID`. Nil when
    /// the fallback path is in use or no device is wired.
    private var halProcID: AudioDeviceIOProcID?

    /// Pre-allocated downmix scratch. The IO proc receives the device's
    /// native channels (potentially interleaved, potentially multi-ch)
    /// and we write the mono-downmixed result here before handing it to
    /// `appendAndDrain`. Sized to a generous upper bound on per-callback
    /// frames so the IO proc never allocates.
    private var downmixScratch: UnsafeMutableBufferPointer<Float>?

    /// The native format of the currently-engaged input device. Captured
    /// at install time so the IO proc knows how many channels to fold
    /// down per frame.
    private var halDeviceFormat: AudioStreamBasicDescription = AudioStreamBasicDescription()

    /// M3: set BEFORE `AudioDeviceStop`. The IO proc checks this at
    /// entry and bails immediately if set, so a late callback that
    /// lands after Stop returns cannot touch torn-down state.
    private let stopping = ManagedAtomic<Bool>(false)

    /// True while the HAL unit is started and running. Drives `stop()`
    /// path selection.
    private var halRunning: Bool = false

    /// Device the HAL unit was wired to at the last `installHALInput()`.
    /// Used by the default-input listener to decide whether a change
    /// actually requires re-targeting.
    private var halCurrentDeviceID: AudioDeviceID = kAudioObjectUnknown

    /// True while a default-input property listener is registered on
    /// the system object. Tracked so `stop()` can remove it
    /// idempotently and we never double-register.
    private var defaultInputListenerInstalled: Bool = false

    /// Serial queue for device-swap work. The CoreAudio property
    /// listener fires on a non-RT thread, but it's still not somewhere
    /// we want to call Stop / SetProperty / Start in-line: those can
    /// block and we don't know which queue the system used. Hopping to
    /// our own serial queue lets us swap deliberately and serialises
    /// concurrent listener fires.
    private let deviceSwapQueue = DispatchQueue(
        label: "com.kaiwalya.Gurukul.deviceSwap",
        qos: .userInitiated
    )

    // MARK: - AVAudioEngine fallback state

    private let avEngine = AVAudioEngine()
    private var convertedBuffer: AVAudioPCMBuffer?
    private var converter: AVAudioConverter?
    private var fallbackActive: Bool = false

    // MARK: - HAL output (PR 1.4.8.3)

    /// HAL output path. Owns the IO proc on the user-selected output
    /// device. Always present alongside the input path so the cabinet
    /// has an audible route the moment a consumer (PR 5 debug pane,
    /// future synth nodes) wants to push samples through it. For PR 3
    /// this ships dark: the ring is never written to except by the
    /// debug-menu sidetone toggle (`#if DEBUG`).
    private let halOutput = HALOutput(sampleRate: Prefs.defaultSampleRate)

    /// Sidetone toggle. When true, the input IO proc copies its
    /// downmixed mono scratch into the HALOutput ring, producing
    /// audible mic-to-speaker passthrough. Debug-only — exists purely
    /// so the developer can manually verify the output IO proc fires
    /// and the sample clock advances end-to-end. Production code paths
    /// never set this; PR 5 (debug pane) introduces the real consumer.
    private let sidetoneEnabled = ManagedAtomic<Bool>(false)

/// The world driving the engine: one mic in-port, four analyzers,
    /// five boundary out-ports. Node ids and boundary port ids must
    /// stay disjoint (PHASE_1_4.md §2 validation rule), hence
    /// `pitch_yin` / `onset_det` / `breath_det` / `vibrato_det` on the
    /// node side.
    private static let worldJSON: String = """
    {
      "world_version": 1,
      "in_ports": [
        { "id": "mic" }
      ],
      "out_ports": [
        { "id": "pitch" },
        { "id": "onset" },
        { "id": "breath" },
        { "id": "vibrato_rate" },
        { "id": "vibrato_depth" }
      ],
      "nodes": [
        {
          "id": "pitch_yin",
          "type": "PitchYin",
          "params": {
            "window": 2048,
            "hop": 512,
            "fmin_hz": 70.0,
            "fmax_hz": 1000.0,
            "threshold": 0.15
          }
        },
        {
          "id": "onset_det",
          "type": "Onset"
        },
        {
          "id": "breath_det",
          "type": "Breath"
        },
        {
          "id": "vibrato_det",
          "type": "Vibrato"
        }
      ],
      "connections": [
        { "from": "mic",                "to": "pitch_yin.audio_in" },
        { "from": "mic",                "to": "onset_det.audio_in" },
        { "from": "mic",                "to": "breath_det.audio_in" },
        { "from": "pitch_yin.f0",       "to": "vibrato_det.f0" },
        { "from": "pitch_yin.f0",       "to": "pitch" },
        { "from": "onset_det.onset",    "to": "onset" },
        { "from": "breath_det.breath",  "to": "breath" },
        { "from": "vibrato_det.rate",   "to": "vibrato_rate" },
        { "from": "vibrato_det.depth",  "to": "vibrato_depth" }
      ]
    }
    """

    // MARK: - Lifecycle

    /// Apply the given settings (initial start, or to swap devices /
    /// sample rate). Synchronous from the caller's perspective: returns
    /// once the new configuration is engaged (or has cleanly failed and
    /// surfaced an error).
    ///
    /// The view layer reads/writes UserDefaults via `Prefs`; the
    /// pipeline takes settings explicitly here and remains
    /// UserDefaults-ignorant.
    func applySettings(_ newSettings: AudioSettings) throws {
        // Serialise on deviceSwapQueue so an in-flight property listener
        // (e.g. default-input-changed) cannot race the rebuild. The
        // listener already hops to this queue, so a sync dispatch from
        // the caller establishes the ordering: either the listener runs
        // before us (and we see its updated state), or we run before
        // it (and it sees ours). No interleaving.
        var caught: Error?
        deviceSwapQueue.sync {
            do {
                let change = newSettings.change(from: currentSettings)
                switch change {
                case .none:
                    if enginePtr == nil {
                        try startInternal(newSettings)
                    }
                case .deviceOnly:
                    try swapDevices(to: newSettings)
                case .sampleRateChanged:
                    try rebuildAt(newSettings)
                }
            } catch {
                caught = error
            }
        }
        if let caught { throw caught }
    }

    func start() throws {
        try startInternal(currentSettings)
    }

    /// Bring the pipeline up using `settings`. Builds the engine,
    /// engages input + output HAL paths, installs listeners. Idempotent:
    /// safe to call when already running (it tears down first).
    private func startInternal(_ settings: AudioSettings) throws {
        if enginePtr != nil || halRunning || fallbackActive || halOutput.isRunning {
            stopInternal()
        }
        sampleRate = settings.sampleRate
        samplesPerBucket = Int(settings.sampleRate) / kWaveformBuckets
        currentSettings = settings
        halOutput.setSampleRate(settings.sampleRate)

        try buildEngineIfNeeded()
        allocateScratch(capacity: maxFramesPerSlice + hop)
        pendingDiscontinuity = true
        for i in 0..<kWaveformBuckets {
            wfLo[i] = 0
            wfHi[i] = 0
        }
        wfHead = 0
        wfFill = 0
        wfCurLo = 0
        wfCurHi = 0

        // Resolve the preferred input device from its UID, if any. If
        // the UID is set but the device isn't present, we fall through
        // to system default rather than refusing — better UX than a
        // hard failure.
        let preferredInputID = settings.inputDeviceUID.flatMap { uid in
            deviceID(forUID: uid, scope: kAudioObjectPropertyScopeInput)
        }

        do {
            try installHALInput(preferredDeviceID: preferredInputID)
            try startHALUnit()
            halRunning = true
            installDefaultInputListener()
            log.info("capture path: HAL")
        } catch {
            log.error("HAL setup failed: \(error.localizedDescription, privacy: .public) — falling back to AVAudioEngine tap (HIGH LATENCY)")
            tearDownHALUnit()
            try startFallback()
            fallbackActive = true
            log.info("capture path: AVAudioEngine tap (fallback)")
        }

        // Engage HAL output. Use a specific device UID if set, else
        // system default. Failure is non-fatal — input-only is still
        // useful.
        let preferredOutputID = settings.outputDeviceUID.flatMap { uid in
            deviceID(forUID: uid, scope: kAudioObjectPropertyScopeOutput)
        }
        do {
            if let id = preferredOutputID {
                try halOutput.start(deviceID: id)
            } else {
                try halOutput.start()
            }
        } catch {
            log.error("HAL output setup failed: \(error.localizedDescription, privacy: .public) — input path still active, no output route")
        }
    }

    func stop() {
        stopInternal()
        if let ptr = enginePtr {
            engine_reset(ptr)
        }
        log.info("stopped")
    }

    /// Internal teardown. Stops listeners and audio threads, frees no
    /// memory and does NOT touch `enginePtr` (callers decide whether to
    /// reset, free, or leave alone). The post-condition is the strong
    /// invariant: every audio thread has exited via `AudioDeviceStop`,
    /// so it is now safe to mutate engine state.
    private func stopInternal() {
        removeDefaultInputListener()
        if halRunning {
            stopHALUnit()
            halRunning = false
        }
        if fallbackActive {
            avEngine.inputNode.removeTap(onBus: 0)
            avEngine.stop()
            fallbackActive = false
        }
        if halOutput.isRunning {
            halOutput.stop()
        }

        // Debug-pane invariants from PHASE_1_4_8.md:
        //   - DebugTapSlot clears on engine rebuild / reset.
        //   - Monitor toggle auto-disengages on engine reset (a click
        //     into the user's headphones at a route change is worse than
        //     silence).
        // Both invariants are satisfied by publishing empty selection +
        // empty tap here, BEFORE engine_free / engine_reset run. The
        // audio thread is already stopped (stopHALUnit / halOutput.stop
        // above blocked on AudioDeviceStop), so this is just main-thread
        // memory work — no race.
        debugSelectionSlot.clear()
        debugTapSeq &+= 1
        debugTapSlot.clear(seq: debugTapSeq)

        // Sidetone defaults off on every (re)start — see invariant in
        // PHASE_1_4_8.md §"PR 5" (monitor disengages on engine.reset).
        sidetoneEnabled.store(false, ordering: .relaxed)
        scratchFill = 0
        #if DEBUG
        let counters = featureSlot.debugCounters()
        let pct: Double = counters.reads > 0
            ? Double(counters.retries) / Double(counters.reads) * 100.0
            : 0.0
        log.info("FeatureSlot reader: reads=\(counters.reads, privacy: .public) retries=\(counters.retries, privacy: .public) (\(pct, format: .fixed(precision: 2), privacy: .public)%)")
        #endif
    }

    /// Device-only swap: stop the existing HAL paths, engage the new
    /// devices, keep the engine intact. No engine rebuild — sample rate
    /// has not changed.
    private func swapDevices(to newSettings: AudioSettings) throws {
        log.info("device swap: input \(self.currentSettings.inputDeviceUID ?? "<default>", privacy: .public) → \(newSettings.inputDeviceUID ?? "<default>", privacy: .public), output \(self.currentSettings.outputDeviceUID ?? "<default>", privacy: .public) → \(newSettings.outputDeviceUID ?? "<default>", privacy: .public)")
        stopInternal()
        currentSettings = newSettings
        try startInternal(newSettings)
    }

    /// Sample-rate rebuild. Stop everything, free the engine, build a
    /// fresh one at the new rate, restart. Re-targets HAL paths at the
    /// settings' devices in the same step.
    ///
    /// The lifecycle invariant `enginePtr is only mutated when no IO
    /// proc is in flight` is preserved by `stopInternal` blocking on
    /// `AudioDeviceStop` for every running device before we touch the
    /// pointer.
    private func rebuildAt(_ newSettings: AudioSettings) throws {
        log.info("engine rebuild: \(self.sampleRate, privacy: .public) Hz → \(newSettings.sampleRate, privacy: .public) Hz")
        stopInternal()
        if let ptr = enginePtr {
            engine_free(ptr)
            enginePtr = nil
        }
        micHandle = GURUKUL_INVALID_PORT
        pitchOut = GURUKUL_INVALID_PORT
        onsetOut = GURUKUL_INVALID_PORT
        breathOut = GURUKUL_INVALID_PORT
        vibratoRateOut = GURUKUL_INVALID_PORT
        vibratoDepthOut = GURUKUL_INVALID_PORT
        try startInternal(newSettings)
        log.info("engine rebuild done at \(self.sampleRate, privacy: .public) Hz")
    }

    /// Resolve a persistent device UID to a runtime `AudioDeviceID` on
    /// the requested scope. Returns nil if the UID is not present in
    /// the current device list. CoreAudio's `kAudioHardwarePropertyTranslateUIDToDevice`
    /// is the documented path; we walk the device list instead to keep
    /// the dependency direction one-way (the catalog is the source of
    /// truth, this is just a lookup).
    private func deviceID(
        forUID uid: String,
        scope: AudioObjectPropertyScope
    ) -> AudioDeviceID? {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain
        )
        var size: UInt32 = 0
        if AudioObjectGetPropertyDataSize(
            AudioObjectID(kAudioObjectSystemObject),
            &address,
            0, nil,
            &size
        ) != noErr || size == 0 {
            return nil
        }
        let count = Int(size) / MemoryLayout<AudioDeviceID>.size
        var ids = [AudioDeviceID](repeating: 0, count: count)
        let status = ids.withUnsafeMutableBufferPointer { buf in
            AudioObjectGetPropertyData(
                AudioObjectID(kAudioObjectSystemObject),
                &address,
                0, nil,
                &size,
                buf.baseAddress!
            )
        }
        guard status == noErr else { return nil }
        // Filter: device must have channels on the requested scope.
        for id in ids {
            // UID is scope-global; channel count is scope-specific.
            var uidAddr = AudioObjectPropertyAddress(
                mSelector: kAudioDevicePropertyDeviceUID,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain
            )
            var cf: Unmanaged<CFString>?
            var uidSize = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
            let uidStatus = AudioObjectGetPropertyData(
                id, &uidAddr, 0, nil, &uidSize, &cf
            )
            guard uidStatus == noErr, let cfStr = cf?.takeRetainedValue() else {
                continue
            }
            if (cfStr as String) == uid && hasChannels(deviceID: id, scope: scope) {
                return id
            }
        }
        return nil
    }

    private func hasChannels(deviceID: AudioDeviceID, scope: AudioObjectPropertyScope) -> Bool {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyStreamConfiguration,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain
        )
        var size: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(deviceID, &address, 0, nil, &size) == noErr,
              size > 0 else {
            return false
        }
        let buffer = UnsafeMutableRawPointer.allocate(byteCount: Int(size), alignment: 16)
        defer { buffer.deallocate() }
        guard AudioObjectGetPropertyData(deviceID, &address, 0, nil, &size, buffer) == noErr else {
            return false
        }
        let list = buffer.assumingMemoryBound(to: AudioBufferList.self)
        let ablPtr = UnsafeMutableAudioBufferListPointer(list)
        var total: UInt32 = 0
        for i in 0..<ablPtr.count {
            total += ablPtr[i].mNumberChannels
        }
        return total > 0
    }

    deinit {
        // Order matters: every audio thread must have exited before we
        // free the engine or its scratch buffers. AudioDeviceStop in
        // stopInternal is blocking, so once it returns no IO proc is
        // mid-flight against the about-to-be-freed memory.
        //
        // stopInternal is private and nonisolated, safe to call here.
        // It is also idempotent — if the pipeline was never started,
        // it's a cheap no-op.
        stopInternal()
        if let ptr = enginePtr {
            engine_free(ptr)
            enginePtr = nil
        }
        if let scratch = scratch {
            scratch.deallocate()
        }
        if let downmixScratch = downmixScratch {
            downmixScratch.deallocate()
        }
    }

    // MARK: - Debug sidetone (PR 1.4.8.3)

    /// Developer-only: route the input mic into the HAL output ring so
    /// the developer can hear mic-to-speaker passthrough and confirm
    /// end-to-end that the output IO proc is alive. **Not** a user
    /// feature — gated `#if DEBUG`; off on every start.
    #if DEBUG
    func setSidetoneEnabled(_ enabled: Bool) {
        sidetoneEnabled.store(enabled, ordering: .relaxed)
        log.info("sidetone (debug) set to \(enabled, privacy: .public)")
    }

    func isSidetoneEnabled() -> Bool {
        sidetoneEnabled.load(ordering: .relaxed)
    }
    #endif

    // MARK: - Engine introspection (debug pane)

    /// List of node ids in the live engine, in topological order. Empty
    /// when the engine is not built (idle or mid-rebuild).
    ///
    /// Calls `engine_node_ids` — NOT realtime-safe. Intended for picker-
    /// open use only. The UI calls this at debug-pane appear; if it
    /// returns empty the user sees an empty picker (correct: there is
    /// nothing to tap when the engine is down).
    func nodeIds() -> [String] {
        guard let ptr = enginePtr else { return [] }
        // First call with cap=0 to learn the total count.
        let total = engine_node_ids(ptr, nil, 0)
        if total == 0 { return [] }
        var buf = [UnsafePointer<CChar>?](repeating: nil, count: total)
        let written = buf.withUnsafeMutableBufferPointer { dst -> Int in
            guard let base = dst.baseAddress else { return 0 }
            return engine_node_ids(ptr, base, total)
        }
        guard written > 0 else { return [] }
        var out: [String] = []
        out.reserveCapacity(min(written, total))
        for i in 0..<min(written, total) {
            if let cstr = buf[i] {
                out.append(String(cString: cstr))
            }
        }
        return out
    }

    /// Output port names for the given node id, in declaration order.
    /// Empty when the node id is not recognised in the current engine.
    /// Calls `engine_out_port_names` — NOT realtime-safe.
    func outPortNames(for nodeId: String) -> [String] {
        guard let ptr = enginePtr else { return [] }
        // Probe with cap=0 to learn the count.
        var total: Int = 0
        let probeRC = nodeId.withCString { nodeCStr in
            engine_out_port_names(ptr, nodeCStr, nil, 0, &total)
        }
        guard probeRC == GURUKUL_OK, total > 0 else { return [] }
        var buf = [UnsafePointer<CChar>?](repeating: nil, count: total)
        let rc = nodeId.withCString { nodeCStr -> Int32 in
            buf.withUnsafeMutableBufferPointer { dst -> Int32 in
                guard let base = dst.baseAddress else { return GURUKUL_ERR_UNKNOWN }
                var got: Int = 0
                return engine_out_port_names(ptr, nodeCStr, base, total, &got)
            }
        }
        guard rc == GURUKUL_OK else { return [] }
        var out: [String] = []
        out.reserveCapacity(total)
        for i in 0..<total {
            if let cstr = buf[i] {
                out.append(String(cString: cstr))
            }
        }
        return out
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
        pitchOut = "pitch".withCString { engine_resolve_out_port(built, $0) }
        onsetOut = "onset".withCString { engine_resolve_out_port(built, $0) }
        breathOut = "breath".withCString { engine_resolve_out_port(built, $0) }
        vibratoRateOut = "vibrato_rate".withCString { engine_resolve_out_port(built, $0) }
        vibratoDepthOut = "vibrato_depth".withCString { engine_resolve_out_port(built, $0) }
        guard micHandle != GURUKUL_INVALID_PORT,
              pitchOut != GURUKUL_INVALID_PORT,
              onsetOut != GURUKUL_INVALID_PORT,
              breathOut != GURUKUL_INVALID_PORT,
              vibratoRateOut != GURUKUL_INVALID_PORT,
              vibratoDepthOut != GURUKUL_INVALID_PORT else {
            throw pipelineError("resolve_in_port/out_port failed")
        }
    }

    private func allocateScratch(capacity: Int) {
        // Allocated once per lifetime of the pipeline; reused across
        // start/stop cycles. Resized only if a future start asks for
        // more — currently it doesn't.
        if let existing = scratch, scratchCapacity >= capacity {
            scratchFill = 0
            existing.update(repeating: 0)
            return
        }
        if let existing = scratch {
            existing.deallocate()
        }
        let buf = UnsafeMutableBufferPointer<Float>.allocate(capacity: capacity)
        buf.initialize(repeating: 0)
        scratch = buf
        scratchCapacity = capacity
        scratchFill = 0
    }

    // MARK: - HAL setup (direct AudioDevice IO proc)
    //
    // We install the IO proc directly on the input AudioDevice instead
    // of going through an AUHAL AudioUnit. AUHAL's `AudioUnitRender`
    // path is supposed to deliver every produced device sample, but in
    // practice on loopback / virtual devices (BlackHole 2ch) the unit's
    // IO proc fires at a faster cadence than the device's natural
    // buffer cycle while only delivering 512 frames per call. The
    // intervening samples are silently dropped, corrupting the signal.
    // Installing the IO proc directly gives us the device's natural
    // buffer cadence with every sample present.

    /// Engage the input HAL path. With no `preferredDeviceID`, follows
    /// the system default input. With a specific id (from a UID lookup
    /// in `AudioDeviceCatalog`), engages exactly that device.
    private func installHALInput(preferredDeviceID: AudioDeviceID? = nil) throws {
        let deviceID = try (preferredDeviceID ?? defaultInputDeviceID())
        halCurrentDeviceID = deviceID

        // Read the device's native input stream format. We'll downmix
        // to mono Float32 in the IO proc but we need to know what we're
        // starting from (channel count, sample rate, etc).
        var nativeASBD = try streamFormat(for: deviceID)
        halDeviceFormat = nativeASBD

        // Sanity-check the format. The current pipeline assumes Float32
        // samples at our engine's sample rate (48 kHz). Anything else
        // would need resampling we don't currently do — bail rather
        // than silently delivering wrong-rate audio to YIN.
        let isFloat = (nativeASBD.mFormatFlags & kAudioFormatFlagIsFloat) != 0
        if !isFloat || nativeASBD.mBitsPerChannel != 32 {
            throw pipelineError("device format not Float32 (bits=\(nativeASBD.mBitsPerChannel), flags=\(nativeASBD.mFormatFlags))")
        }
        if UInt32(nativeASBD.mSampleRate.rounded()) != sampleRate {
            throw pipelineError("device sample rate \(nativeASBD.mSampleRate) != engine rate \(sampleRate); resampling not implemented")
        }

        // BufferFrameSize controls how many frames the device hands us
        // per IO-proc callback. On BlackHole the IO proc fires on a
        // wall-clock cycle that's independent of BufferFrameSize, so
        // if BufferFrameSize is smaller than (sampleRate * cycle),
        // the device drops the extra frames. We start with the max
        // (4096 covers a ~85ms cycle at 48 kHz, far above anything
        // we've seen) so the IO proc drains the full ring each call.
        var deviceBufFrames: UInt32 = 4096
        var deviceBufAddress = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyBufferFrameSize,
            mScope: kAudioObjectPropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain
        )
        _ = AudioObjectSetPropertyData(
            deviceID,
            &deviceBufAddress,
            0, nil,
            UInt32(MemoryLayout<UInt32>.size),
            &deviceBufFrames
        )
        // Allocate the downmix scratch sized for the requested buffer
        // frame size — one device-buffer-worth of frames is the most
        // any IO-proc callback should deliver.
        if downmixScratch == nil {
            let buf = UnsafeMutableBufferPointer<Float>.allocate(capacity: maxFramesPerSlice)
            buf.initialize(repeating: 0)
            downmixScratch = buf
        }

        // Install the IO proc on the device.
        var procID: AudioDeviceIOProcID?
        let createStatus = AudioDeviceCreateIOProcID(
            deviceID,
            Self.deviceIOProc,
            Unmanaged.passUnretained(self).toOpaque(),
            &procID
        )
        try check(createStatus, "AudioDeviceCreateIOProcID")
        guard let procID else {
            throw pipelineError("AudioDeviceCreateIOProcID returned nil procID")
        }
        halProcID = procID

        let deviceName = deviceNameOrUnknown(for: deviceID)
        let nativeDesc = describe(nativeASBD)
        log.info("""
        HAL engaged (direct device IO): device='\(deviceName, privacy: .public)' id=\(deviceID, privacy: .public)
          native=\(nativeDesc, privacy: .public)
          downmixToMono=true
        """)
    }

    private func startHALUnit() throws {
        guard let procID = halProcID else {
            throw pipelineError("startHALUnit called without procID")
        }
        stopping.store(false, ordering: .relaxed)
        let status = AudioDeviceStart(halCurrentDeviceID, procID)
        try check(status, "AudioDeviceStart")
    }

    private func stopHALUnit() {
        // M3: flip the flag before AudioDeviceStop so any late callback
        // bails at the entry-check rather than touching torn-down state.
        stopping.store(true, ordering: .relaxed)
        if let procID = halProcID {
            AudioDeviceStop(halCurrentDeviceID, procID)
            AudioDeviceDestroyIOProcID(halCurrentDeviceID, procID)
        }
        halProcID = nil
    }

    private func tearDownHALUnit() {
        stopping.store(true, ordering: .relaxed)
        if let procID = halProcID {
            AudioDeviceDestroyIOProcID(halCurrentDeviceID, procID)
        }
        halProcID = nil
        if let backing = downmixScratch {
            backing.deallocate()
            downmixScratch = nil
        }
    }

    /// Read the device's input-scope StreamFormat. Used at install time
    /// to capture channel count / sample rate / format flags before
    /// installing the IO proc.
    private func streamFormat(for deviceID: AudioDeviceID) throws -> AudioStreamBasicDescription {
        var asbd = AudioStreamBasicDescription()
        var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
        var addr = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyStreamFormat,
            mScope: kAudioObjectPropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain
        )
        let status = AudioObjectGetPropertyData(
            deviceID,
            &addr,
            0, nil,
            &size,
            &asbd
        )
        try check(status, "GetProperty(StreamFormat) on input device")
        return asbd
    }

    // MARK: - Default-input change handling

    private static var defaultInputAddress = AudioObjectPropertyAddress(
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain
    )

    /// CoreAudio property-listener trampoline. Pulls the pipeline out
    /// of the refcon and hops onto the swap queue — we don't know which
    /// thread CoreAudio called us on, and the actual unit re-targeting
    /// involves Stop/SetProperty/Start which we don't want to do
    /// in-line on a system thread.
    private static let defaultInputChanged: AudioObjectPropertyListenerProc = {
        _, _, _, refcon in
        guard let refcon else { return noErr }
        let pipeline = Unmanaged<AudioPipeline>.fromOpaque(refcon).takeUnretainedValue()
        pipeline.deviceSwapQueue.async {
            pipeline.handleDefaultInputChanged()
        }
        return noErr
    }

    private func installDefaultInputListener() {
        guard !defaultInputListenerInstalled else { return }
        let status = AudioObjectAddPropertyListener(
            AudioObjectID(kAudioObjectSystemObject),
            &Self.defaultInputAddress,
            Self.defaultInputChanged,
            Unmanaged.passUnretained(self).toOpaque()
        )
        if status == noErr {
            defaultInputListenerInstalled = true
            log.info("default-input listener installed")
        } else {
            log.error("AddPropertyListener(DefaultInputDevice) failed: \(status, privacy: .public)")
        }
    }

    private func removeDefaultInputListener() {
        guard defaultInputListenerInstalled else { return }
        let status = AudioObjectRemovePropertyListener(
            AudioObjectID(kAudioObjectSystemObject),
            &Self.defaultInputAddress,
            Self.defaultInputChanged,
            Unmanaged.passUnretained(self).toOpaque()
        )
        if status != noErr {
            log.error("RemovePropertyListener(DefaultInputDevice) failed: \(status, privacy: .public)")
        }
        defaultInputListenerInstalled = false
    }

    /// Runs on `deviceSwapQueue`. Compares the new default input
    /// against what the unit is wired to and, if different, stops the
    /// unit, swaps `CurrentDevice`, restarts. If the swap fails at any
    /// step, tears the HAL unit down and routes to the AVAudioEngine
    /// fallback.
    private func handleDefaultInputChanged() {
        guard halRunning else { return }

        // If the user pinned a specific input UID, ignore default-input
        // changes — the pinned device IS the device. Only when settings
        // says "system default" (uid == nil) should this listener drive
        // a swap.
        if currentSettings.inputDeviceUID != nil {
            log.info("default input changed, but user has pinned an input — ignoring")
            return
        }

        let newID: AudioDeviceID
        do {
            newID = try defaultInputDeviceID()
        } catch {
            log.error("default-input changed but lookup failed: \(error.localizedDescription, privacy: .public)")
            return
        }
        if newID == halCurrentDeviceID { return }

        let oldName = deviceNameOrUnknown(for: halCurrentDeviceID)
        let newName = deviceNameOrUnknown(for: newID)
        log.info("default input changed: '\(oldName, privacy: .public)' (\(self.halCurrentDeviceID, privacy: .public)) → '\(newName, privacy: .public)' (\(newID, privacy: .public)) — swapping HAL device")

        do {
            try swapHALDevice(to: newID)
        } catch {
            log.error("HAL device swap failed: \(error.localizedDescription, privacy: .public) — falling back to AVAudioEngine tap")
            // Tear the partial HAL state down and bring up the
            // fallback path so the user still sees pitch readings.
            tearDownHALUnit()
            halRunning = false
            do {
                try startFallback()
                fallbackActive = true
                pendingDiscontinuity = true
                log.info("capture path: AVAudioEngine tap (post-swap fallback)")
            } catch {
                log.error("fallback start also failed: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    /// Stop the IO proc on the old device, tear it down, install + start
    /// a fresh IO proc on `newID`. Direct AudioDevice IO procs are tied
    /// to a specific device, so unlike the AUHAL path we can't just
    /// swap CurrentDevice — we must rebuild.
    private func swapHALDevice(to newID: AudioDeviceID) throws {
        // M3 mirror: flag so any in-flight IO proc bails before we
        // touch state. Cleared after restart succeeds.
        stopping.store(true, ordering: .relaxed)
        if let procID = halProcID {
            AudioDeviceStop(halCurrentDeviceID, procID)
            AudioDeviceDestroyIOProcID(halCurrentDeviceID, procID)
            halProcID = nil
        }

        // Install on the exact newID the listener reported, rather than
        // re-reading the system default. This both makes the call honest
        // (no implicit-via-side-effect coupling) and lets future callers
        // (e.g. an explicit applySettings device swap) target a specific
        // device that may not be the system default.
        try installHALInput(preferredDeviceID: newID)
        try startHALUnit()

        // Reset scratch — the device change is a discontinuity and we
        // don't want to feed a stitched-together hop into YIN. Flag
        // the next publish so the UI ring buffers clear too.
        scratchFill = 0
        pendingDiscontinuity = true
        log.info("HAL device swap complete (new id=\(self.halCurrentDeviceID, privacy: .public))")
    }

    // MARK: - Device IO proc

    /// C trampoline for `AudioDeviceCreateIOProcID`. Pulls the pipeline
    /// out of the clientData and dispatches to `handleDeviceInput`.
    private static let deviceIOProc: AudioDeviceIOProc = {
        _, _, inInputData, _, _, _, clientData in
        guard let clientData else { return noErr }
        let pipeline = Unmanaged<AudioPipeline>.fromOpaque(clientData).takeUnretainedValue()
        return pipeline.handleDeviceInput(input: inInputData)
    }

    private func handleDeviceInput(
        input: UnsafePointer<AudioBufferList>
    ) -> OSStatus {
        // M3 entry-check.
        if stopping.load(ordering: .relaxed) {
            return noErr
        }

        let listPtr = UnsafeMutableAudioBufferListPointer(
            UnsafeMutablePointer(mutating: input)
        )
        guard listPtr.count > 0 else { return noErr }

        // The device delivers either interleaved (one buffer, N channels)
        // or non-interleaved (N buffers, one channel each). Detect from
        // the captured native ASBD.
        let nChannels = Int(halDeviceFormat.mChannelsPerFrame)
        let isNonInterleaved =
            (halDeviceFormat.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0

        let firstBuf = listPtr[0]
        let bytesPerSample = MemoryLayout<Float>.size
        let framesInBuffer: Int
        if isNonInterleaved {
            framesInBuffer = Int(firstBuf.mDataByteSize) / bytesPerSample
        } else {
            framesInBuffer = Int(firstBuf.mDataByteSize) / (bytesPerSample * nChannels)
        }
        if framesInBuffer <= 0 { return noErr }
        if framesInBuffer > maxFramesPerSlice {
            log.error("device IO proc framesInBuffer=\(framesInBuffer, privacy: .public) > max=\(self.maxFramesPerSlice, privacy: .public); skipping")
            return noErr
        }

        guard let scratch = downmixScratch?.baseAddress else { return noErr }

        // Downmix to mono. Mono devices: copy through. Stereo+ devices:
        // average across channels per frame.
        if nChannels == 1 {
            guard let src = firstBuf.mData?.bindMemory(to: Float.self, capacity: framesInBuffer) else {
                return noErr
            }
            memcpy(scratch, src, framesInBuffer * bytesPerSample)
        } else if isNonInterleaved {
            // N buffers, one Float channel each. Sum then divide.
            for f in 0..<framesInBuffer {
                var sum: Float = 0
                for c in 0..<min(nChannels, listPtr.count) {
                    let buf = listPtr[c]
                    if let p = buf.mData?.bindMemory(to: Float.self, capacity: framesInBuffer) {
                        sum += p[f]
                    }
                }
                scratch[f] = sum / Float(nChannels)
            }
        } else {
            // Interleaved [c0 c1 c0 c1 ...]. Average per frame.
            guard let src = firstBuf.mData?.bindMemory(to: Float.self, capacity: framesInBuffer * nChannels) else {
                return noErr
            }
            let inv = 1.0 / Float(nChannels)
            for f in 0..<framesInBuffer {
                var sum: Float = 0
                let base = f * nChannels
                for c in 0..<nChannels {
                    sum += src[base + c]
                }
                scratch[f] = sum * inv
            }
        }

        // Debug-only sidetone: copy the just-downmixed mono frames into
        // the HAL output ring. Production paths never set sidetoneEnabled;
        // PR 5 (debug pane) introduces the real consumer of halOutput.
        #if DEBUG
        if sidetoneEnabled.load(ordering: .relaxed) {
            halOutput.writeMono(scratch, count: framesInBuffer)
        }
        #endif

        appendAndDrain(src: scratch, count: framesInBuffer)
        return noErr
    }

    // MARK: - Fallback: AVAudioEngine tap

    private func startFallback() throws {
        try installTap()
        try avEngine.start()
        let inLat = avEngine.inputNode.presentationLatency
        log.info("fallback tap started — sample rate \(self.sampleRate, privacy: .public) Hz, hop \(self.hop, privacy: .public), inputPresentationLatency=\(inLat * 1000, format: .fixed(precision: 1), privacy: .public)ms")
    }

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

        let tapBufferSize: AVAudioFrameCount = 512

        if needsConversion {
            let converter = AVAudioConverter(from: hwFormat, to: engineFormat)
            guard let converter else {
                throw pipelineError("could not construct AVAudioConverter")
            }
            let ratio = engineFormat.sampleRate / hwFormat.sampleRate
            let convertedCapacity = AVAudioFrameCount(Double(maxFramesPerSlice) * ratio + 64)
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

        log.info("hw input format: \(hwFormat, privacy: .public), scratch=\(self.scratchCapacity, privacy: .public) frames")
        inputNode.installTap(
            onBus: 0,
            bufferSize: tapBufferSize,
            format: hwFormat
        ) { [weak self] buffer, when in
            self?.handleInputBuffer(buffer, when: when)
        }
        log.info("tap installed on input bus 0 (buffer \(tapBufferSize, privacy: .public) frames, hop \(self.hop, privacy: .public))")
    }

    private func handleInputBuffer(
        _ buffer: AVAudioPCMBuffer,
        when _: AVAudioTime
    ) {
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
                let errDesc = error?.localizedDescription ?? "unknown"
                log.error("converter error: \(errDesc, privacy: .public)")
                return
            }
            guard let ch = converted.floatChannelData?[0] else { return }
            src = UnsafePointer(ch)
            frames = Int(converted.frameLength)
        } else {
            guard let ch = buffer.floatChannelData?[0] else { return }
            src = UnsafePointer(ch)
            frames = Int(buffer.frameLength)
        }

        appendAndDrain(src: src, count: frames)
    }

    // MARK: - Shared hot path (HAL + fallback both call this)

    /// Append `count` frames into the scratch buffer, then drain in
    /// hop-aligned chunks while there's enough fill. Any leftover
    /// frames (< hop) stay in the scratch for the next callback so YIN
    /// sees a continuous stream.
    private func appendAndDrain(src: UnsafePointer<Float>, count: Int) {
        guard let scratch = scratch else { return }
        guard let ptr = enginePtr else { return }

        // If the incoming chunk would overflow, drop the OLDEST frames.
        if scratchFill + count > scratchCapacity {
            let overflow = scratchFill + count - scratchCapacity
            if overflow < scratchFill {
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

        let dst = scratch.baseAddress!.advanced(by: scratchFill)
        for i in 0..<count {
            let y = src[i]
            dst[i] = y

            if wfFill == 0 {
                wfCurLo = y
                wfCurHi = y
            } else {
                if y < wfCurLo { wfCurLo = y }
                if y > wfCurHi { wfCurHi = y }
            }
            wfFill += 1
            if wfFill >= samplesPerBucket {
                wfLo[wfHead] = wfCurLo
                wfHi[wfHead] = wfCurHi
                wfHead = (wfHead + 1) % kWaveformBuckets
                wfFill = 0
            }
        }
        scratchFill += count

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

            // Read all five out-ports per the per-port shape table:
            //   pitch, breath, vibrato_rate, vibrato_depth → last-sample
            //   onset                                      → max-|x|
            let lastPitch = readLastSample(ptr: ptr, handle: pitchOut)
            let lastBreath = readLastSample(ptr: ptr, handle: breathOut)
            let lastVibratoRate = readLastSample(ptr: ptr, handle: vibratoRateOut)
            let lastVibratoDepth = readLastSample(ptr: ptr, handle: vibratoDepthOut)
            let onsetMag = readMaxAbs(ptr: ptr, handle: onsetOut)

            seq &+= 1
            let voiced = rms >= silenceRmsFloor && lastPitch > 0
            let publishedHz: Float = voiced ? lastPitch : .nan

            let snapshot = FeatureSnapshot(
                seq: seq,
                hz: publishedHz,
                onset: onsetMag,
                breath: lastBreath,
                vibratoRate: lastVibratoRate,
                vibratoDepth: lastVibratoDepth,
                discontinuity: pendingDiscontinuity
            )
            pendingDiscontinuity = false
            featureSlot.store(snapshot)

            // Publish a waveform snapshot too. Unwrap the ring into the
            // pre-allocated output arrays (oldest-bucket-first → newest)
            // so the UI just iterates 0..<N to draw left-to-right.
            for k in 0..<kWaveformBuckets {
                let src = (wfHead + k) % kWaveformBuckets
                wfLoOut[k] = wfLo[src]
                wfHiOut[k] = wfHi[src]
            }
            wfLoOut.withUnsafeBufferPointer { lo in
                wfHiOut.withUnsafeBufferPointer { hi in
                    if let lp = lo.baseAddress, let hp = hi.baseAddress {
                        waveformSlot.store(seq: seq, lo: lp, hi: hp)
                    }
                }
            }

            // Debug-pane tap. If the user (or in DEBUG, the hardcoded
            // selection below) has picked a (node, port), call
            // engine_read_port and copy the result into debugTapSlot.
            // engine_read_port is realtime-safe between process blocks —
            // string lookup happens once per hop, which the phase doc
            // explicitly accepts.
            publishDebugTap(ptr: ptr)

            // ~1 s cadence (writer is ~94 Hz). Print global min/max and
            // mean across the visible 1 s window, plus a coarse-decimated
            // envelope so we can see the shape in the log.
            if seq % 94 == 0 {
                var gLo: Float = wfLoOut[0]
                var gHi: Float = wfHiOut[0]
                var sumLo: Float = 0
                var sumHi: Float = 0
                for k in 0..<kWaveformBuckets {
                    if wfLoOut[k] < gLo { gLo = wfLoOut[k] }
                    if wfHiOut[k] > gHi { gHi = wfHiOut[k] }
                    sumLo += wfLoOut[k]
                    sumHi += wfHiOut[k]
                }
                let meanLo = sumLo / Float(kWaveformBuckets)
                let meanHi = sumHi / Float(kWaveformBuckets)
                // 10-point coarse decimation of the hi envelope so we
                // can see the shape (positive peaks per ~100 ms).
                var shapeHi = ""
                var shapeLo = ""
                let step = kWaveformBuckets / 10
                for k in 0..<10 {
                    shapeHi += String(format: "%+.3f ", wfHiOut[k * step])
                    shapeLo += String(format: "%+.3f ", wfLoOut[k * step])
                }
                log.info("""
                wf: gMin=\(gLo, format: .fixed(precision: 4), privacy: .public) gMax=\(gHi, format: .fixed(precision: 4), privacy: .public) \
                meanLo=\(meanLo, format: .fixed(precision: 4), privacy: .public) meanHi=\(meanHi, format: .fixed(precision: 4), privacy: .public)
                  hi: \(shapeHi, privacy: .public)
                  lo: \(shapeLo, privacy: .public)
                """)
            }

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

    /// Read the last sample of an out-port buffer. Use for
    /// sample-and-hold features (pitch, breath, vibrato rate/depth).
    /// Returns 0 on a failed FFI call rather than aborting the hop —
    /// a stale "0" is better than a missed publish across all five
    /// features.
    private func readLastSample(ptr: OpaquePointer, handle: UInt32) -> Float {
        var outPtr: UnsafePointer<Float>?
        var outLen: Int = 0
        let rc = engine_out_port(ptr, handle, &outPtr, &outLen)
        guard rc == GURUKUL_OK, let buf = outPtr, outLen > 0 else {
            return 0
        }
        return buf[outLen - 1]
    }

    /// Scan an out-port buffer for the maximum absolute value. Use for
    /// event-shaped features (onset emits a one-sample pulse and would
    /// be missed by a last-sample read; with hop=512 the pulse is at
    /// the last index only ~0.2% of the time).
    private func readMaxAbs(ptr: OpaquePointer, handle: UInt32) -> Float {
        var outPtr: UnsafePointer<Float>?
        var outLen: Int = 0
        let rc = engine_out_port(ptr, handle, &outPtr, &outLen)
        guard rc == GURUKUL_OK, let buf = outPtr, outLen > 0 else {
            return 0
        }
        var maxAbs: Float = 0
        for i in 0..<outLen {
            let v = abs(buf[i])
            if v > maxAbs { maxAbs = v }
        }
        return maxAbs
    }

    /// If the user (or, in DEBUG, the hardcoded dark selection) has
    /// chosen a (node, port), read its most-recent block via
    /// `engine_read_port` and copy into `debugTapSlot`. No-op when
    /// `debugSelectionSlot` is empty — the audio thread skips the FFI
    /// call entirely, which is the "no selection" steady state today.
    ///
    /// Must run on the audio thread, after `engine_process_block`
    /// returns in the same hop. `engine_read_port` does a string lookup
    /// per call; the phase doc explicitly accepts this cost.
    private func publishDebugTap(ptr: OpaquePointer) {
        // Read the current selection from debugSelectionSlot. The slot's
        // borrow() returns pointers directly into its internal storage —
        // no allocation, no string bridging. If no selection is set,
        // borrow() returns false and we skip the FFI call entirely.
        var nodeBase: UnsafePointer<UInt8>? = nil
        var portBase: UnsafePointer<UInt8>? = nil
        var typeTag: UInt8 = 0
        var monitorOn = false
        let has = debugSelectionSlot.borrow(
            nodeBase: &nodeBase,
            portBase: &portBase,
            typeTag: &typeTag,
            monitor: &monitorOn
        )
        guard has, let nodeP = nodeBase, let portP = portBase else {
            return
        }
        // The slot stores null-terminated UTF-8 buffers, so we can hand
        // the UInt8 pointers to engine_read_port as `char*` directly.
        let nodeCStr = UnsafeRawPointer(nodeP).assumingMemoryBound(to: CChar.self)
        let portCStr = UnsafeRawPointer(portP).assumingMemoryBound(to: CChar.self)
        var outPtr: UnsafePointer<Float>?
        var outLen: Int = 0
        let rc = engine_read_port(ptr, nodeCStr, portCStr, &outPtr, &outLen)
        guard rc == GURUKUL_OK, let src = outPtr, outLen > 0 else {
            return
        }
        debugTapSeq &+= 1
        debugTapSlot.store(
            seq: debugTapSeq,
            typeTag: typeTag,
            src: src,
            count: outLen
        )
        // Monitor route is wired in PR 5.3. For now, the flag is read
        // and ignored — keeps the data path stable so 5.3 only touches
        // the routing line.
        _ = monitorOn
    }

    // MARK: - HAL helpers

    private func defaultInputDeviceID() throws -> AudioDeviceID {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDefaultInputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain
        )
        var deviceID: AudioDeviceID = 0
        var size = UInt32(MemoryLayout<AudioDeviceID>.size)
        let status = AudioObjectGetPropertyData(
            AudioObjectID(kAudioObjectSystemObject),
            &address,
            0,
            nil,
            &size,
            &deviceID
        )
        try check(status, "AudioObjectGetPropertyData(DefaultInputDevice)")
        guard deviceID != kAudioObjectUnknown else {
            throw pipelineError("default input device is kAudioObjectUnknown")
        }
        return deviceID
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
            0,
            nil,
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

    private func asbdsDiffer(
        _ a: AudioStreamBasicDescription,
        _ b: AudioStreamBasicDescription
    ) -> Bool {
        return a.mSampleRate != b.mSampleRate
            || a.mChannelsPerFrame != b.mChannelsPerFrame
            || a.mFormatFlags != b.mFormatFlags
            || a.mBitsPerChannel != b.mBitsPerChannel
    }

    private func check(_ status: OSStatus, _ what: String) throws {
        if status != noErr {
            throw pipelineError("\(what) failed: OSStatus \(status)")
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
// The audio-thread callback (HAL IO proc → handleHALInput → appendAndDrain,
// or AVAudioEngine tap → handleInputBuffer → appendAndDrain) is on the hot
// path. It must not allocate, lock, or call Swift-runtime functions that can.
// Current state:
//
// - scratch buffer is pre-allocated in start(); reused across callbacks.
// - halBufferStorage + halBufferList are pre-allocated in installHALInput();
//   reused across callbacks.
// - AudioUnitRender is the intended RT-safe way to pull input from a HAL unit.
// - converter output buffer is pre-allocated and reused on the fallback path.
// - Per-hop: 1 engine_in_port + 1 engine_process_block + 5 engine_out_port
//   reads (4 last-sample + 1 max-scan-over-hop for onset). All pointer
//   indirections, all infallible-by-construction once handles resolve.
// - FeatureSlot.store is one .relaxed load + one .relaxed load + one plain
//   struct store (28 bytes) + one .release store on a UInt8 atomic.
//   Triple-buffered so writer is wait-free even under reader contention.
// - stopping.load is a single ManagedAtomic<Bool> load (relaxed).
// - log.error / log.info on the hot path fire only on rare branches (errors,
//   or every 200th HAL callback / 25th tap callback).
//
// Known fallback-path violation (carried from 1.4.6): AVAudioConverter is
// not strictly allocation-free across all hardware SR ratios. The HAL path
// avoids this — the unit's internal conversion does not touch our buffers.
