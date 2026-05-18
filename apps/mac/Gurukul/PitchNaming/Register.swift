import Foundation

/// Which octave / register a named pitch sits in. Typed per school so the two
/// schemes never get mixed up at the call site.
enum Register: Equatable, Hashable {
    /// 12-TET octave number (the 4 in "A4"). Middle C is `western(4)`.
    case western(Int)

    /// Indian classical octave — mandra (low), madhya (middle), taara (high).
    case indian(IndianOctave)
}

enum IndianOctave: String, Equatable, Hashable {
    case mandra
    case madhya
    case taara

    /// Conventional short marker rendered alongside a syllable. Mandra Sa is
    /// typically written with a dot below; madhya is unmarked; taara with a
    /// dot above. We use simple ASCII markers for now; richer typography
    /// arrives with the visualiser.
    var marker: String {
        switch self {
        case .mandra: return "."
        case .madhya: return ""
        case .taara: return "'"
        }
    }
}
