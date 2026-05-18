import Foundation

/// 12-TET namer. A4 (configurable) is the anchor; every other pitch is found
/// by counting semitones from A4 and rounding.
///
/// Cents are the signed residual times 100, clamped to [-50, +50] by the
/// rounding step. A perfectly-in-tune note returns `cents = 0`.
struct WesternNamer: PitchNamer {
    let context: WesternContext

    private static let names: [String] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"
    ]

    func name(hz: Float) -> NamedPitch {
        guard hz.isFinite, hz > 0 else { return silentPitch }

        // Semitones above A4 (can be negative).
        let semisFromA4 = 12.0 * log2f(hz / context.a4Hz)
        let rounded = semisFromA4.rounded()
        let cents = Int(((semisFromA4 - rounded) * 100).rounded())

        // MIDI note number for the rounded semitone. A4 is MIDI 69.
        let midi = Int(rounded) + 69
        let pitchClass = ((midi % 12) + 12) % 12
        let octave = (midi / 12) - 1  // MIDI octave convention (C-1 = 0).

        return NamedPitch(
            name: Self.names[pitchClass],
            cents: cents,
            register: .western(octave),
            hz: hz
        )
    }
}
