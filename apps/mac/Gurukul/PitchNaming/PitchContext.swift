import Foundation

/// A school of pitch naming. Western 12-TET assigns absolute names (C, C#, D…).
/// Indian classical schools name relative to a tonic (Sa) the singer chooses.
///
/// New schools added later should land here as additional cases plus a matching
/// `PitchNamer` implementation; nothing else needs to change.
enum PitchSchool: String, CaseIterable, Identifiable, Hashable {
    case western
    case hindustani
    case karnatik

    var id: String { rawValue }

    var displayName: String {
        switch self {
        case .western: return "Western"
        case .hindustani: return "Hindustani"
        case .karnatik: return "Karnatik"
        }
    }
}

/// Western 12-TET tuning anchor. `a4Hz` is exposed for a future calibration
/// UI; the picker is deferred. No tonic — names are absolute.
struct WesternContext: Equatable, Hashable {
    var a4Hz: Float = 440.0
}

/// Indian classical context — the singer's tonic (Sa frequency in Hz) plus
/// which school's syllable set to use. Hindustani and Karnatik differ in
/// komal / tivra naming conventions; the namer dispatches on `school`.
struct IndianContext: Equatable, Hashable {
    var tonicHz: Float
    var school: IndianSchoolVariant
}

enum IndianSchoolVariant: Equatable, Hashable {
    case hindustani
    case karnatik
}

/// The active pitch-naming context. Carries exactly the data the chosen
/// namer needs — Western has no tonic, Indian has no `a4Hz`.
enum PitchContext: Equatable, Hashable {
    case western(WesternContext)
    case indian(IndianContext)

    var school: PitchSchool {
        switch self {
        case .western: return .western
        case .indian(let ctx):
            switch ctx.school {
            case .hindustani: return .hindustani
            case .karnatik: return .karnatik
            }
        }
    }
}
