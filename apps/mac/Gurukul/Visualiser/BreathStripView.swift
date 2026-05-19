import SwiftUI

/// Thin horizontal strip whose fill amount tracks the breath-detector
/// gate. The raw gate is binary (0 / 1) per hop; we smooth it with a
/// short exponential filter so the eye sees a fill animation instead
/// of a flicker.
struct BreathStripView: View {
    let breath: Float

    @State private var smoothed: Double = 0

    /// ~50 ms time constant at 30 Hz UI tick.
    private let alpha: Double = 0.4

    var body: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                RoundedRectangle(cornerRadius: 4, style: .continuous)
                    .fill(Color.gray.opacity(0.15))
                RoundedRectangle(cornerRadius: 4, style: .continuous)
                    .fill(Color.blue.opacity(0.6))
                    .frame(width: geo.size.width * smoothed)
            }
            .overlay(
                HStack {
                    Text("breath")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .padding(.leading, 6)
                    Spacer()
                }
            )
        }
        .onChange(of: breath) { _, new in
            let target = Double(max(0, min(1, new)))
            smoothed = smoothed + alpha * (target - smoothed)
        }
    }
}
