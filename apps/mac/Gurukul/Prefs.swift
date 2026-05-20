import Foundation

/// Thin UserDefaults wrapper for the cabinet's persisted preferences.
///
/// Single source of truth for keys used by `SettingsView` and read at
/// launch in `GurukulApp` to seed the initial `AudioSettings`. Keep
/// `AudioPipeline` ignorant of UserDefaults — pass `AudioSettings` in
/// explicitly so the pipeline stays testable.
///
/// Key naming convention: dotted, namespaced by feature area. Once a
/// key ships it must not be renamed (would strand users on old values).
enum Prefs {
    /// Persisted-key identifiers. String values are part of the on-disk
    /// schema — do NOT rename without a migration.
    enum Key {
        static let inputDeviceUID = "audio.inputDeviceUID"
        static let outputDeviceUID = "audio.outputDeviceUID"
        static let engineSampleRate = "audio.engineSampleRate"
    }

    /// Default values used when the user has never set a preference.
    static let defaultSampleRate: UInt32 = 48_000
    static let defaultBufferSize: UInt32 = 4096

    private static var store: UserDefaults { .standard }

    // MARK: - Audio

    /// Load the persisted audio settings. Missing values fall back to
    /// "follow system default device" / 48 kHz.
    static func loadAudioSettings() -> AudioSettings {
        let inputUID = store.string(forKey: Key.inputDeviceUID).flatMap {
            $0.isEmpty ? nil : $0
        }
        let outputUID = store.string(forKey: Key.outputDeviceUID).flatMap {
            $0.isEmpty ? nil : $0
        }
        let rawRate = store.object(forKey: Key.engineSampleRate) as? Int
        let rate = rawRate.flatMap { UInt32(exactly: $0) } ?? defaultSampleRate
        let validatedRate = SupportedSampleRate.from(rate)?.rawValue ?? defaultSampleRate
        return AudioSettings(
            inputDeviceUID: inputUID,
            outputDeviceUID: outputUID,
            sampleRate: validatedRate,
            bufferSize: defaultBufferSize
        )
    }

    /// Persist the given settings. `bufferSize` is not yet persisted.
    static func saveAudioSettings(_ settings: AudioSettings) {
        store.set(settings.inputDeviceUID ?? "", forKey: Key.inputDeviceUID)
        store.set(settings.outputDeviceUID ?? "", forKey: Key.outputDeviceUID)
        store.set(Int(settings.sampleRate), forKey: Key.engineSampleRate)
    }
}
