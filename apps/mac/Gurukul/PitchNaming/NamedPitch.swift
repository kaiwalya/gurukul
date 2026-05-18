import Foundation

/// A pitch resolved against a school's naming scheme. Cents are signed and in
/// the range [-50, +50] — the nearest named pitch is always the closer one.
struct NamedPitch: Equatable, Hashable {
    let name: String
    let cents: Int
    let register: Register
    let hz: Float
}
