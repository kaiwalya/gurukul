import Foundation

/// A pure mapping from a fundamental frequency to a school-specific name.
/// Implementations are stateless — all configuration lives in the matching
/// context. The factory picks the right implementation for a given context.
protocol PitchNamer {
    /// Resolve `hz` (must be > 0 and finite) to a NamedPitch. Returns a
    /// silent NamedPitch (name "—", cents 0) for non-finite or non-positive
    /// inputs so call sites don't need to branch.
    func name(hz: Float) -> NamedPitch
}

/// "No signal" sentinel rendered uniformly across schools.
let silentPitch = NamedPitch(
    name: "—",
    cents: 0,
    register: .western(0),
    hz: 0
)
