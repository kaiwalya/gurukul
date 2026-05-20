import Combine
import CoreAudio
import Foundation
import OSLog

private let log = Logger(subsystem: "com.kaiwalya.Gurukul", category: "AudioDeviceCatalog")

/// A single CoreAudio device entry as exposed by the preferences pane.
struct AudioDeviceInfo: Identifiable, Hashable {
    /// AudioDeviceID — runtime-only, can change across boots. Use `uid`
    /// for persistence and lookup.
    let id: AudioDeviceID
    /// Persistent device identifier (`kAudioDevicePropertyDeviceUID`).
    /// Stable across boots and reconnects. Empty if CoreAudio refuses
    /// to provide one (rare; degenerate devices).
    let uid: String
    /// Human-readable name (`kAudioObjectPropertyName`).
    let name: String
    /// Nominal sample rate the device is currently configured at, in Hz.
    /// May differ from the engine's rate; the preferences pane offers
    /// an "Align engine" affordance in that case.
    let nominalSampleRate: UInt32
    /// True if this device exposes the requested scope (input or output)
    /// — i.e. it has at least one channel on that side. Devices that
    /// fail this filter are excluded from the picker entirely.
    let hasRequestedScope: Bool
}

/// Live list of input and output devices on the system, observable from
/// SwiftUI. Tracks `kAudioHardwarePropertyDevices` changes so the
/// picker refreshes when a device is plugged or unplugged.
///
/// Lives on the main actor — UI consumers (pickers) read it directly,
/// and CoreAudio's property-listener callback hops back to main to
/// publish the refresh.
@MainActor
final class AudioDeviceCatalog: ObservableObject {
    @Published private(set) var inputs: [AudioDeviceInfo] = []
    @Published private(set) var outputs: [AudioDeviceInfo] = []

    private var listenerInstalled = false

    init() {
        refresh()
        installDeviceListListener()
    }

    deinit {
        // Best-effort: removePropertyListener captures self via the
        // opaque pointer, but the listener uses unretained references so
        // there's no retain cycle. CoreAudio cleans up on process exit
        // if we miss this in deinit.
        if listenerInstalled {
            var address = Self.deviceListAddress
            AudioObjectRemovePropertyListener(
                AudioObjectID(kAudioObjectSystemObject),
                &address,
                Self.deviceListChanged,
                Unmanaged.passUnretained(self).toOpaque()
            )
        }
    }

    // MARK: - Enumeration

    /// Re-read the device list from CoreAudio. Cheap — called at init
    /// and on every device-list-changed event.
    func refresh() {
        let allIDs = allDeviceIDs()
        var ins: [AudioDeviceInfo] = []
        var outs: [AudioDeviceInfo] = []
        for id in allIDs {
            let inputInfo = deviceInfo(id: id, scope: kAudioObjectPropertyScopeInput)
            if inputInfo.hasRequestedScope {
                ins.append(inputInfo)
            }
            let outputInfo = deviceInfo(id: id, scope: kAudioObjectPropertyScopeOutput)
            if outputInfo.hasRequestedScope {
                outs.append(outputInfo)
            }
        }
        ins.sort { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        outs.sort { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        inputs = ins
        outputs = outs
        log.debug(
            "device list refreshed — inputs=\(ins.count, privacy: .public), outputs=\(outs.count, privacy: .public)"
        )
    }

    /// Look up a device by persistent UID. Returns nil if the device is
    /// not currently present.
    func device(uid: String, scope: ScopeFilter) -> AudioDeviceInfo? {
        switch scope {
        case .input: return inputs.first { $0.uid == uid }
        case .output: return outputs.first { $0.uid == uid }
        }
    }

    enum ScopeFilter { case input, output }

    // MARK: - CoreAudio helpers

    private func allDeviceIDs() -> [AudioDeviceID] {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain
        )
        var size: UInt32 = 0
        let sizeStatus = AudioObjectGetPropertyDataSize(
            AudioObjectID(kAudioObjectSystemObject),
            &address,
            0, nil,
            &size
        )
        if sizeStatus != noErr || size == 0 {
            return []
        }
        let count = Int(size) / MemoryLayout<AudioDeviceID>.size
        var ids = [AudioDeviceID](repeating: 0, count: count)
        let status = ids.withUnsafeMutableBufferPointer { buf -> OSStatus in
            AudioObjectGetPropertyData(
                AudioObjectID(kAudioObjectSystemObject),
                &address,
                0, nil,
                &size,
                buf.baseAddress!
            )
        }
        guard status == noErr else { return [] }
        return ids
    }

    private func deviceInfo(id: AudioDeviceID, scope: AudioObjectPropertyScope) -> AudioDeviceInfo {
        let uid = stringProperty(
            id: id,
            selector: kAudioDevicePropertyDeviceUID,
            scope: kAudioObjectPropertyScopeGlobal
        ) ?? ""
        let name = stringProperty(
            id: id,
            selector: kAudioObjectPropertyName,
            scope: kAudioObjectPropertyScopeGlobal
        ) ?? "<unknown>"
        let nominalRate = doubleProperty(
            id: id,
            selector: kAudioDevicePropertyNominalSampleRate,
            scope: kAudioObjectPropertyScopeGlobal
        ).map { UInt32($0.rounded()) } ?? 0
        let hasScope = channelCount(id: id, scope: scope) > 0
        return AudioDeviceInfo(
            id: id,
            uid: uid,
            name: name,
            nominalSampleRate: nominalRate,
            hasRequestedScope: hasScope
        )
    }

    private func channelCount(id: AudioDeviceID, scope: AudioObjectPropertyScope) -> Int {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyStreamConfiguration,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain
        )
        var size: UInt32 = 0
        let sizeStatus = AudioObjectGetPropertyDataSize(id, &address, 0, nil, &size)
        if sizeStatus != noErr || size == 0 { return 0 }
        let buffer = UnsafeMutableRawPointer.allocate(byteCount: Int(size), alignment: 16)
        defer { buffer.deallocate() }
        let status = AudioObjectGetPropertyData(id, &address, 0, nil, &size, buffer)
        guard status == noErr else { return 0 }
        let list = buffer.assumingMemoryBound(to: AudioBufferList.self)
        let ablPtr = UnsafeMutableAudioBufferListPointer(list)
        var total = 0
        for i in 0..<ablPtr.count {
            total += Int(ablPtr[i].mNumberChannels)
        }
        return total
    }

    private func stringProperty(
        id: AudioObjectID,
        selector: AudioObjectPropertySelector,
        scope: AudioObjectPropertyScope
    ) -> String? {
        var address = AudioObjectPropertyAddress(
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain
        )
        var cf: Unmanaged<CFString>?
        var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
        let status = AudioObjectGetPropertyData(id, &address, 0, nil, &size, &cf)
        guard status == noErr, let value = cf?.takeRetainedValue() else {
            return nil
        }
        return value as String
    }

    private func doubleProperty(
        id: AudioObjectID,
        selector: AudioObjectPropertySelector,
        scope: AudioObjectPropertyScope
    ) -> Double? {
        var address = AudioObjectPropertyAddress(
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain
        )
        var value: Double = 0
        var size = UInt32(MemoryLayout<Double>.size)
        let status = AudioObjectGetPropertyData(id, &address, 0, nil, &size, &value)
        guard status == noErr else { return nil }
        return value
    }

    // MARK: - Device-list-changed listener

    // CoreAudio's property-listener APIs are nonisolated and must be
    // callable from the deinit (also nonisolated). Marking these
    // explicitly nonisolated keeps Swift 6's actor checker happy.
    nonisolated(unsafe) private static var deviceListAddress = AudioObjectPropertyAddress(
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain
    )

    nonisolated private static let deviceListChanged: AudioObjectPropertyListenerProc = {
        _, _, _, refcon in
        guard let refcon else { return noErr }
        // Hop to main actor so @Published updates are SwiftUI-safe and
        // we can call back into the catalog without worrying about which
        // thread CoreAudio chose.
        let unmanaged = Unmanaged<AudioDeviceCatalog>.fromOpaque(refcon)
        Task { @MainActor in
            unmanaged.takeUnretainedValue().refresh()
        }
        return noErr
    }

    private func installDeviceListListener() {
        guard !listenerInstalled else { return }
        let status = AudioObjectAddPropertyListener(
            AudioObjectID(kAudioObjectSystemObject),
            &Self.deviceListAddress,
            Self.deviceListChanged,
            Unmanaged.passUnretained(self).toOpaque()
        )
        if status == noErr {
            listenerInstalled = true
            log.debug("device-list-changed listener installed")
        } else {
            log.error("failed to install device-list-changed listener: OSStatus \(status, privacy: .public)")
        }
    }
}
