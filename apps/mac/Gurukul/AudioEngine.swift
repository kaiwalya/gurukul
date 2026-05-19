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
/// per-hop RMS energy gate, and publish the latest pitch into a lock-free
/// `PitchSlot` the UI polls at 30 Hz.
///
/// Project default is `MainActor`; the pipeline is explicitly
/// `nonisolated` because the audio render thread (HAL IO proc or tap
/// callback) cannot be on the main actor. SwiftUI touches `pitchSlot`
/// (lock-free) and the `start`/`stop` methods, which are quick and safe
/// to call from the main thread.
nonisolated final class AudioPipeline {
    /// Public, read-only handle the view polls every UI tick.
    let pitchSlot = PitchSlot()

    private var enginePtr: OpaquePointer?
    private var micHandle: UInt32 = GURUKUL_INVALID_PORT
    private var pitchHandle: UInt32 = GURUKUL_INVALID_PORT

    /// Sample rate the engine is built for. Both capture paths deliver
    /// frames at this rate (HAL via the unit's internal conversion;
    /// tap via AVAudioConverter or bypass).
    private let sampleRate: UInt32 = 48000

    /// Hop size that matches PitchYin's `hop` param in the world JSON.
    private let hop: Int = 512

    /// Per-hop RMS gate. YIN happily locks onto noise-floor content at
    /// any input level, so even mic-muted we'd see a stable bogus pitch.
    /// We publish the YIN value only when the hop's RMS is above this
    /// floor; below it, we publish NaN so the UI dims. -50 dBFS ≈
    /// 0.00316 linear.
    private let silenceRmsFloor: Float = 0.00316

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

    // MARK: - HAL state

    /// HAL output AudioUnit. Nil when the fallback path is in use.
    private var halUnit: AudioUnit?

    /// Backing storage for the IO proc's `AudioBufferList`. Sized to
    /// `maxFramesPerSlice` so an unexpected jump in `inNumberFrames`
    /// (route change, SR change) cannot overflow it.
    private var halBufferStorage: UnsafeMutableBufferPointer<Float>?

    /// Pre-built `AudioBufferList` we hand to `AudioUnitRender`. Has
    /// one buffer pointing at `halBufferStorage`. The `mDataByteSize`
    /// field is reset per-callback to match `inNumberFrames`.
    private var halBufferList: UnsafeMutableAudioBufferListPointer?

    /// M3: set BEFORE `AudioOutputUnitStop`. The IO proc checks this
    /// at entry and bails immediately if set, so a late callback that
    /// lands after Stop returns cannot touch torn-down state.
    private let stopping = ManagedAtomic<Bool>(false)

    /// Counts IO-proc callbacks for throttled logging on the HAL path.
    private var halCallbackCount: Int = 0

    /// Counts tap callbacks for throttled logging on the fallback path.
    private var tapCallbackCount: Int = 0

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

    /// The world driving the engine: one mic in-port, a YIN pitch
    /// tracker, one pitch out-port.
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
        allocateScratch(capacity: maxFramesPerSlice + hop)

        // Try HAL first. If anything along the chain fails, log and
        // fall back to the AVAudioEngine tap path. Failures are
        // expected on exotic devices (aggregate, non-Float32 native,
        // …); the fallback keeps the app usable.
        do {
            try installHALInput()
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
    }

    func stop() {
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
        if let ptr = enginePtr {
            engine_reset(ptr)
        }
        scratchFill = 0
        log.info("stopped")
    }

    deinit {
        if let ptr = enginePtr {
            engine_free(ptr)
        }
        if let scratch = scratch {
            scratch.deallocate()
        }
        if let halBufferStorage = halBufferStorage {
            halBufferStorage.deallocate()
        }
        if let halBufferList = halBufferList {
            halBufferList.unsafeMutablePointer.deallocate()
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

    // MARK: - HAL setup

    private func installHALInput() throws {
        // Component description: HAL output unit (used for both input
        // and output on macOS; the EnableIO flags below select input).
        var desc = AudioComponentDescription(
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_HALOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0
        )
        guard let component = AudioComponentFindNext(nil, &desc) else {
            throw pipelineError("AudioComponentFindNext failed")
        }

        var unit: AudioUnit?
        var status = AudioComponentInstanceNew(component, &unit)
        try check(status, "AudioComponentInstanceNew")
        guard let unit else {
            throw pipelineError("AudioComponentInstanceNew returned nil")
        }

        // EnableIO Input/1 = 1
        var enableInput: UInt32 = 1
        status = AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Input,
            1,
            &enableInput,
            UInt32(MemoryLayout<UInt32>.size)
        )
        try check(status, "SetProperty(EnableIO, Input, 1)")

        // EnableIO Output/0 = 0
        var disableOutput: UInt32 = 0
        status = AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Output,
            0,
            &disableOutput,
            UInt32(MemoryLayout<UInt32>.size)
        )
        try check(status, "SetProperty(EnableIO, Output, 0)")

        // CurrentDevice Global/0 = <default input device>
        var deviceID = try defaultInputDeviceID()
        halCurrentDeviceID = deviceID
        status = AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_CurrentDevice,
            kAudioUnitScope_Global,
            0,
            &deviceID,
            UInt32(MemoryLayout<AudioDeviceID>.size)
        )
        try check(status, "SetProperty(CurrentDevice)")

        // Capture native ASBD for diagnostics. Bus 1, Input scope = the
        // format coming FROM the device (before any internal conversion
        // the unit performs).
        var nativeASBD = AudioStreamBasicDescription()
        var asbdSize = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
        status = AudioUnitGetProperty(
            unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Input,
            1,
            &nativeASBD,
            &asbdSize
        )
        try check(status, "GetProperty(StreamFormat, Input, 1)")

        // StreamFormat Output/1 = clientASBD. NB: bus 1 is input from
        // device; element 1's *Output* scope is the format the unit
        // produces to our callback — that's the side we set, not Input.
        var clientASBD = makeClientASBD()
        status = AudioUnitSetProperty(
            unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            1,
            &clientASBD,
            UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
        )
        try check(status, "SetProperty(StreamFormat, Output, 1)")

        // MaximumFramesPerSlice Global/0 = maxFramesPerSlice. M2:
        // `inNumberFrames` on the IO proc is not pinned to the
        // device's current buffer frame size — it can jump on route
        // change. Set an explicit upper bound and size our buffer
        // list to match.
        var maxFrames: UInt32 = UInt32(maxFramesPerSlice)
        status = AudioUnitSetProperty(
            unit,
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
            &maxFrames,
            UInt32(MemoryLayout<UInt32>.size)
        )
        try check(status, "SetProperty(MaximumFramesPerSlice)")

        // Allocate the buffer list to maxFramesPerSlice. mDataByteSize
        // is reset per-callback inside the IO proc to match the actual
        // inNumberFrames; the underlying storage is fixed.
        //
        // Re-use existing buffers if present (device-swap path
        // re-enters installHALInput on the same pipeline, and we don't
        // want to leak the previous allocation).
        if halBufferStorage == nil {
            let backing = UnsafeMutableBufferPointer<Float>.allocate(capacity: maxFramesPerSlice)
            backing.initialize(repeating: 0)
            halBufferStorage = backing
        }
        if halBufferList == nil, let backing = halBufferStorage {
            let listPtr = AudioBufferList.allocate(maximumBuffers: 1)
            listPtr[0] = AudioBuffer(
                mNumberChannels: 1,
                mDataByteSize: UInt32(maxFramesPerSlice * MemoryLayout<Float>.size),
                mData: UnsafeMutableRawPointer(backing.baseAddress)
            )
            halBufferList = listPtr
        }

        // SetInputCallback Global/0 = (ioCallback, self)
        var callbackStruct = AURenderCallbackStruct(
            inputProc: Self.ioCallback,
            inputProcRefCon: Unmanaged.passUnretained(self).toOpaque()
        )
        status = AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_SetInputCallback,
            kAudioUnitScope_Global,
            0,
            &callbackStruct,
            UInt32(MemoryLayout<AURenderCallbackStruct>.size)
        )
        try check(status, "SetProperty(SetInputCallback)")

        status = AudioUnitInitialize(unit)
        try check(status, "AudioUnitInitialize")

        halUnit = unit

        let deviceName = deviceNameOrUnknown(for: deviceID)
        let nativeDesc = describe(nativeASBD)
        let clientDesc = describe(clientASBD)
        let conversion = asbdsDiffer(nativeASBD, clientASBD)
        log.info("""
        HAL engaged: device='\(deviceName, privacy: .public)' id=\(deviceID, privacy: .public)
          native=\(nativeDesc, privacy: .public)
          client=\(clientDesc, privacy: .public)
          internalConversion=\(conversion, privacy: .public)
          maxFramesPerSlice=\(self.maxFramesPerSlice, privacy: .public)
        """)
    }

    private func startHALUnit() throws {
        guard let unit = halUnit else {
            throw pipelineError("startHALUnit called without halUnit")
        }
        stopping.store(false, ordering: .relaxed)
        let status = AudioOutputUnitStart(unit)
        try check(status, "AudioOutputUnitStart")
    }

    private func stopHALUnit() {
        // M3: flip the flag before AudioOutputUnitStop so any late
        // callback that fires after Stop returns bails at the
        // entry-check rather than touching torn-down state.
        stopping.store(true, ordering: .relaxed)
        if let unit = halUnit {
            AudioOutputUnitStop(unit)
            AudioUnitUninitialize(unit)
            AudioComponentInstanceDispose(unit)
        }
        halUnit = nil
        halCallbackCount = 0
    }

    private func tearDownHALUnit() {
        // Called when HAL setup throws partway through. Some properties
        // may have been set; the unit may be allocated but not started.
        stopping.store(true, ordering: .relaxed)
        if let unit = halUnit {
            AudioUnitUninitialize(unit)
            AudioComponentInstanceDispose(unit)
        }
        halUnit = nil
        if let backing = halBufferStorage {
            backing.deallocate()
            halBufferStorage = nil
        }
        if let listPtr = halBufferList {
            listPtr.unsafeMutablePointer.deallocate()
            halBufferList = nil
        }
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
                log.info("capture path: AVAudioEngine tap (post-swap fallback)")
            } catch {
                log.error("fallback start also failed: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    /// Stop the unit, re-target it to `newID`, restart. Reuses the
    /// same AudioUnit instance — cheaper and less race-prone than
    /// re-creating from scratch.
    private func swapHALDevice(to newID: AudioDeviceID) throws {
        guard let unit = halUnit else {
            throw pipelineError("swapHALDevice called without halUnit")
        }
        // M3 mirror: flag so any in-flight IO proc bails before we
        // touch the unit. Cleared after restart succeeds.
        stopping.store(true, ordering: .relaxed)
        AudioOutputUnitStop(unit)

        var deviceID = newID
        let status = AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_CurrentDevice,
            kAudioUnitScope_Global,
            0,
            &deviceID,
            UInt32(MemoryLayout<AudioDeviceID>.size)
        )
        try check(status, "SetProperty(CurrentDevice) during swap")
        halCurrentDeviceID = newID

        stopping.store(false, ordering: .relaxed)
        let startStatus = AudioOutputUnitStart(unit)
        try check(startStatus, "AudioOutputUnitStart during swap")

        // Reset scratch — the device change is a discontinuity and we
        // don't want to feed a stitched-together hop into YIN.
        scratchFill = 0
        log.info("HAL device swap complete")
    }

    // MARK: - HAL IO proc

    /// C trampoline. Pulls the pipeline out of the refcon and
    /// dispatches to `handleHALInput`. Lives at file scope (via
    /// `static let`) so the closure can be `@convention(c)`.
    private static let ioCallback: AURenderCallback = { refcon, flags, ts, busNum, nFrames, _ in
        let pipeline = Unmanaged<AudioPipeline>.fromOpaque(refcon).takeUnretainedValue()
        return pipeline.handleHALInput(flags: flags, ts: ts, busNum: busNum, nFrames: nFrames)
    }

    private func handleHALInput(
        flags: UnsafeMutablePointer<AudioUnitRenderActionFlags>,
        ts: UnsafePointer<AudioTimeStamp>,
        busNum: UInt32,
        nFrames: UInt32
    ) -> OSStatus {
        // M3 entry-check.
        if stopping.load(ordering: .relaxed) {
            return noErr
        }

        // M2 defense: the unit shouldn't deliver more than we set, but
        // bail safely if it does instead of writing past our buffer.
        let frames = Int(nFrames)
        if frames > maxFramesPerSlice {
            log.error("HAL inNumberFrames=\(frames, privacy: .public) > max=\(self.maxFramesPerSlice, privacy: .public); skipping callback")
            return noErr
        }

        guard let unit = halUnit, let listPtr = halBufferList else {
            return noErr
        }

        // Re-arm mDataByteSize to the requested write size; the storage
        // capacity is fixed at maxFramesPerSlice.
        listPtr[0].mDataByteSize = UInt32(frames * MemoryLayout<Float>.size)

        let status = AudioUnitRender(unit, flags, ts, busNum, nFrames, listPtr.unsafeMutablePointer)
        if status != noErr {
            log.error("AudioUnitRender failed: \(status, privacy: .public)")
            return noErr
        }

        guard let raw = listPtr[0].mData else { return noErr }
        let src = raw.bindMemory(to: Float.self, capacity: frames)

        appendAndDrain(src: src, count: frames)

        halCallbackCount += 1
        // ~10 ms callbacks → every 200 ≈ 2 s.
        if halCallbackCount % 200 == 1 {
            var peak: Float = 0
            for i in 0..<frames {
                let v = abs(src[i])
                if v > peak { peak = v }
            }
            log.info("HAL #\(self.halCallbackCount, privacy: .public) frames=\(frames, privacy: .public) peak=\(peak, format: .fixed(precision: 4), privacy: .public) scratchFill=\(self.scratchFill, privacy: .public)")
        }
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
        tapCallbackCount += 1

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
            guard let ch = buffer.floatChannelData?[0] else { return }
            src = UnsafePointer(ch)
            frames = Int(buffer.frameLength)
        }

        appendAndDrain(src: src, count: frames)

        if tapCallbackCount % 25 == 1 {
            var peak: Float = 0
            for i in 0..<frames {
                let v = abs(src[i])
                if v > peak { peak = v }
            }
            log.info("tap #\(self.tapCallbackCount, privacy: .public) frames=\(frames, privacy: .public) peak=\(peak, format: .fixed(precision: 4), privacy: .public) scratchFill=\(self.scratchFill, privacy: .public)")
        }
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

        scratch.baseAddress!.advanced(by: scratchFill)
            .update(from: src, count: count)
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

            var outPtr: UnsafePointer<Float>?
            var outLen: Int = 0
            let rc3 = engine_out_port(ptr, pitchHandle, &outPtr, &outLen)
            guard rc3 == GURUKUL_OK, let pitchBuf = outPtr, outLen > 0 else {
                log.error("engine_out_port failed rc=\(rc3, privacy: .public)")
                return
            }
            let lastPitch = pitchBuf[outLen - 1]

            seq &+= 1
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

    // MARK: - HAL helpers

    private func makeClientASBD() -> AudioStreamBasicDescription {
        // Mono Float32 non-interleaved at the engine's sample rate.
        // mBytesPerFrame = mBytesPerPacket = sizeof(Float) — for
        // non-interleaved formats CoreAudio expects per-channel byte
        // counts here.
        let bytesPerSample = UInt32(MemoryLayout<Float>.size)
        return AudioStreamBasicDescription(
            mSampleRate: Float64(sampleRate),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat
                | kAudioFormatFlagIsPacked
                | kAudioFormatFlagIsNonInterleaved,
            mBytesPerPacket: bytesPerSample,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytesPerSample,
            mChannelsPerFrame: 1,
            mBitsPerChannel: 32,
            mReserved: 0
        )
    }

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
// - PitchSlot.store is a single ManagedAtomic<UInt64> store.
// - stopping.load is a single ManagedAtomic<Bool> load (relaxed).
// - log.error / log.info on the hot path fire only on rare branches (errors,
//   or every 200th HAL callback / 25th tap callback).
//
// Known fallback-path violation (carried from 1.4.6): AVAudioConverter is
// not strictly allocation-free across all hardware SR ratios. The HAL path
// avoids this — the unit's internal conversion does not touch our buffers.
