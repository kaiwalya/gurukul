import SwiftUI

/// Compact one-line readout: rate (Hz) + depth (cents). Shows "—" when
/// either value is unset (Vibrato emits 0/0 on no-vibrato segments).
struct VibratoReadoutView: View {
    let rate: Float
    let depth: Float

    var body: some View {
        HStack(spacing: 8) {
            Text("Vibrato:")
                .font(.callout)
                .foregroundStyle(.secondary)
            if hasVibrato {
                Text(String(format: "%.1f Hz", rate))
                    .font(.callout.monospacedDigit())
                Text(",")
                    .foregroundStyle(.secondary)
                Text(String(format: "%d¢", Int(depth.rounded())))
                    .font(.callout.monospacedDigit())
            } else {
                Text("—")
                    .font(.callout.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            Spacer()
        }
    }

    private var hasVibrato: Bool {
        guard rate.isFinite, depth.isFinite else { return false }
        return rate > 0 && depth > 0
    }
}
