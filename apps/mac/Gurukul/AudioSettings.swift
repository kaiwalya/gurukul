import Foundation

/// User-controllable audio I/O settings the cabinet applies as a unit.
///
/// `AudioPipeline.applySettings(_:)` consumes one of these and decides
/// whether the change requires a full engine rebuild (sample rate
/// changed), a device hot-swap (input or output UID changed), or no
/// action (settings equal current state).
///
/// `bufferSize` is plumbed for future use; PR 4 keeps it at the current
/// hardcoded value, but having the field here means a later PR can
/// expose it in `SettingsView` without an API break.
struct AudioSettings: Equatable {
    /// Persistent device identifier (`kAudioDevicePropertyDeviceUID`) of
    /// the user-selected input device. `nil` means "follow system
    /// default input." Empty string is treated as `nil`.
    var inputDeviceUID: String?

    /// Same for the output device.
    var outputDeviceUID: String?

    /// Engine sample rate in Hz. The input device, output device, and
    /// engine all run at this rate — the cabinet does not resample on
    /// either side. Picking a rate the current devices can't honour
    /// triggers a UI alignment prompt in `SettingsView`.
    var sampleRate: UInt32

    /// Maximum frames per audio IO callback. Used to size scratch
    /// buffers. SR-independent (4096 covers ~85 ms at 48 kHz, larger
    /// margin at 96 kHz). Not user-exposed in PR 4.
    var bufferSize: UInt32

    /// What changed between two settings values. Drives the rebuild
    /// decision in `AudioPipeline.applySettings`.
    enum Change {
        /// No change required.
        case none
        /// Input and/or output device UID changed; sample rate same.
        /// Cabinet stops the affected HAL path and re-engages the new
        /// device, no engine rebuild.
        case deviceOnly
        /// Sample rate changed (with or without device change). Full
        /// engine teardown, `engine_free`, `engine_build` at new rate,
        /// re-resolve handles, restart all HAL paths.
        case sampleRateChanged
    }

    /// Diff against `other` to decide the cheapest sufficient change.
    func change(from other: AudioSettings) -> Change {
        if sampleRate != other.sampleRate {
            return .sampleRateChanged
        }
        if inputDeviceUID != other.inputDeviceUID
            || outputDeviceUID != other.outputDeviceUID {
            return .deviceOnly
        }
        return .none
    }
}

/// Supported engine sample rates. The cabinet refuses to build at any
/// other rate (no resampling on input or output).
enum SupportedSampleRate: UInt32, CaseIterable, Identifiable {
    case sr44_1k = 44_100
    case sr48k = 48_000
    case sr96k = 96_000

    var id: UInt32 { rawValue }

    /// Display string for the picker, e.g. "48 kHz" or "44.1 kHz".
    var displayName: String {
        switch self {
        case .sr44_1k: return "44.1 kHz"
        case .sr48k: return "48 kHz"
        case .sr96k: return "96 kHz"
        }
    }

    /// Map an arbitrary integer (e.g. a CoreAudio nominal rate, rounded
    /// from Double) to a supported case, or nil if unsupported.
    static func from(_ raw: UInt32) -> SupportedSampleRate? {
        SupportedSampleRate(rawValue: raw)
    }
}
