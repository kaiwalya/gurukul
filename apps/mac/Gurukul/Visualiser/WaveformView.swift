import SwiftUI

/// Min/max-envelope waveform of the last ~1 s of mic audio. Each x-pixel
/// shows the [min, max] range of the samples that fell into that bucket;
/// silence shows as a thin centerline, loud audio fills more of the
/// vertical range.
///
/// Auto-scales the vertical range to the loudest bucket on screen so
/// quiet input is still visible. Soft clipping ceiling at 1.0 keeps the
/// rendering bounded when input is hot.
struct WaveformView: View {
    let waveform: WaveformSnapshot

    var body: some View {
        Canvas { ctx, size in
            // Background.
            let bgRect = CGRect(origin: .zero, size: size)
            ctx.fill(Path(bgRect), with: .color(.black.opacity(0.18)))

            // Centerline.
            var center = Path()
            center.move(to: CGPoint(x: 0, y: size.height / 2))
            center.addLine(to: CGPoint(x: size.width, y: size.height / 2))
            ctx.stroke(center, with: .color(.white.opacity(0.12)), lineWidth: 1)

            let buckets = waveform.buckets
            if buckets.isEmpty { return }

            // Auto-scale: largest absolute deflection on screen, floored
            // at a small value so we don't divide by ~0 and amplify pure
            // silence into a wall of noise.
            var peak: Float = 0.02
            for b in buckets {
                let m = max(abs(b.lo), abs(b.hi))
                if m > peak { peak = m }
            }
            if peak > 1.0 { peak = 1.0 }

            let bucketWidth = size.width / CGFloat(buckets.count)
            let halfH = size.height / 2

            // Build a single filled polygon: walk left-to-right along
            // the max envelope, then right-to-left along the min
            // envelope, then close. Renders as a smooth filled shape
            // regardless of bucket count.
            var envelope = Path()
            for (i, b) in buckets.enumerated() {
                let x = CGFloat(i) * bucketWidth + bucketWidth / 2
                let y = halfH - CGFloat(b.hi / peak) * halfH * 0.95
                if i == 0 {
                    envelope.move(to: CGPoint(x: x, y: y))
                } else {
                    envelope.addLine(to: CGPoint(x: x, y: y))
                }
            }
            for b in buckets.reversed().enumerated() {
                let i = buckets.count - 1 - b.offset
                let x = CGFloat(i) * bucketWidth + bucketWidth / 2
                let y = halfH - CGFloat(b.element.lo / peak) * halfH * 0.95
                envelope.addLine(to: CGPoint(x: x, y: y))
            }
            envelope.closeSubpath()
            ctx.fill(envelope, with: .color(.cyan.opacity(0.55)))
            ctx.stroke(envelope, with: .color(.cyan.opacity(0.95)), lineWidth: 1)

            // Scale hint (top-right).
            let label = String(format: "peak %.2f", peak)
            let text = Text(label)
                .font(.system(size: 10, design: .rounded))
                .foregroundColor(.white.opacity(0.45))
            ctx.draw(text, at: CGPoint(x: size.width - 6, y: 10), anchor: .topTrailing)
        }
    }
}
