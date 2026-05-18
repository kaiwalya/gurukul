import Foundation

/// Dispatches a `PitchContext` to the right `PitchNamer` implementation.
/// Only Western is wired in PR 1.4.6; Indian variants will land alongside
/// their namer implementations.
enum PitchNamerFactory {
    static func namer(for context: PitchContext) -> PitchNamer {
        switch context {
        case .western(let ctx):
            return WesternNamer(context: ctx)
        case .indian:
            // Not yet implemented — the picker doesn't ship this option in
            // 1.4.6. If we somehow get here, return the Western fallback so
            // the UI keeps moving instead of crashing.
            return WesternNamer(context: WesternContext())
        }
    }
}
