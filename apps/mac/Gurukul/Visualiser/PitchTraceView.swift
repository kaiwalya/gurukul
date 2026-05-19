import SwiftUI

/// Scrolling pitch trace with onset ticks layered on top.
///
/// Y-axis is **MIDI note number** (log-pitch), so an octave occupies a
/// fixed vertical span regardless of where it sits. Range is fixed at
/// C2 (MIDI 36) through C6 (MIDI 84) — four octaves, covers bass
/// through soprano. Voiced samples below or above range are clipped to
/// the band edge (rare for sung input; the band is generous).
///
/// Two ring buffers:
///   - `midis` — N points of MIDI numbers, `nan` for unvoiced (drawn
///     as gaps via `move`).
///   - `onsets` — N points of onset magnitude, drawn as full-height
///     vertical ticks at the same x-position.
///
/// One ring write per `FeatureSnapshot.seq` change. With a 30 Hz UI
/// tick and a 5 s window, capacity is 150.
struct PitchTraceView: View {
    let snapshot: FeatureSnapshot

    /// Visible MIDI range. C2 = 36, C6 = 84.
    private static let midiMin: Float = 36
    private static let midiMax: Float = 84

    private static let capacity: Int = 150

    @State private var midis: [Float] = Array(repeating: .nan, count: PitchTraceView.capacity)
    @State private var onsets: [Float] = Array(repeating: 0, count: PitchTraceView.capacity)
    @State private var head: Int = 0
    @State private var lastSeq: UInt32 = 0

    var body: some View {
        Canvas { ctx, size in
            drawGrid(ctx: ctx, size: size)
            drawTrace(ctx: ctx, size: size)
            drawOnsets(ctx: ctx, size: size)
        }
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(Color.gray.opacity(0.08))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .stroke(Color.gray.opacity(0.2), lineWidth: 1)
        )
        .onChange(of: snapshot.seq) { _, _ in
            ingest(snapshot)
        }
    }

    private func ingest(_ s: FeatureSnapshot) {
        if s.discontinuity {
            midis = Array(repeating: .nan, count: Self.capacity)
            onsets = Array(repeating: 0, count: Self.capacity)
            head = 0
            lastSeq = s.seq
            return
        }
        let val: Float
        if s.hz.isFinite && s.hz > 0 {
            // MIDI = 69 + 12 * log2(hz / 440).
            val = 69 + 12 * log2f(s.hz / 440)
        } else {
            val = .nan
        }
        midis[head] = val
        onsets[head] = s.onset
        head = (head + 1) % Self.capacity
        lastSeq = s.seq
    }

    private func yFor(midi: Float, in size: CGSize) -> CGFloat {
        let clipped = max(Self.midiMin, min(Self.midiMax, midi))
        let t = (clipped - Self.midiMin) / (Self.midiMax - Self.midiMin)
        // y axis grows downward; high pitch sits at top.
        return size.height * CGFloat(1 - t)
    }

    private func drawGrid(ctx: GraphicsContext, size: CGSize) {
        // Octave gridlines + labels (C2, C3, C4, C5, C6).
        let octaves: [(midi: Float, label: String)] = [
            (36, "C2"), (48, "C3"), (60, "C4"), (72, "C5"), (84, "C6"),
        ]
        for (midi, label) in octaves {
            let y = yFor(midi: midi, in: size)
            // Middle C (C4) gets a slightly brighter line.
            let opacity: Double = midi == 60 ? 0.35 : 0.15
            var line = Path()
            line.move(to: CGPoint(x: 0, y: y))
            line.addLine(to: CGPoint(x: size.width, y: y))
            ctx.stroke(line, with: .color(.secondary.opacity(opacity)), lineWidth: 1)

            let text = Text(label)
                .font(.caption2.monospacedDigit())
                .foregroundStyle(.secondary)
            let resolved = ctx.resolve(text)
            let textSize = resolved.measure(in: CGSize(width: 30, height: 14))
            ctx.draw(
                resolved,
                at: CGPoint(x: 4 + textSize.width / 2, y: y - textSize.height / 2 - 1),
                anchor: .center
            )
        }

        // Subtle semitone hint at each interior octave — every 12 MIDI
        // steps is the strong line; we leave inter-octave space empty
        // to keep the surface readable.
    }

    private func drawTrace(ctx: GraphicsContext, size: CGSize) {
        let n = Self.capacity
        let stepX = size.width / CGFloat(n - 1)

        var path = Path()
        var hasPrev = false

        for i in 0..<n {
            let idx = (head + i) % n
            let v = midis[idx]
            let x = CGFloat(i) * stepX

            if v.isNaN {
                hasPrev = false
                continue
            }
            let y = yFor(midi: v, in: size)
            let pt = CGPoint(x: x, y: y)
            if hasPrev {
                path.addLine(to: pt)
            } else {
                path.move(to: pt)
                hasPrev = true
            }
        }
        ctx.stroke(
            path,
            with: .color(.accentColor),
            style: StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round)
        )
    }

    private func drawOnsets(ctx: GraphicsContext, size: CGSize) {
        let n = Self.capacity
        let stepX = size.width / CGFloat(n - 1)
        for i in 0..<n {
            let idx = (head + i) % n
            let mag = onsets[idx]
            guard mag > 0 else { continue }
            let x = CGFloat(i) * stepX
            var tick = Path()
            tick.move(to: CGPoint(x: x, y: 0))
            tick.addLine(to: CGPoint(x: x, y: size.height))
            ctx.stroke(tick, with: .color(.orange.opacity(0.7)), lineWidth: 1.5)
        }
    }
}
