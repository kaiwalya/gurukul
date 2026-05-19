import SwiftUI

/// Chromatic pitch clock. Full revolution = one octave. C sits at 12
/// o'clock; the hand rotates clockwise through C#, D, D#, …, B and
/// wraps back to C.
///
/// The hand angle is driven by the absolute pitch class (semitone +
/// cents), so it sweeps smoothly *between* semitone tick marks when
/// the singer slides. The note label and octave live in the centre
/// of the face and snap (no smoothing) so transitions feel decisive,
/// while the hand position smooths with a one-pole filter (~100 ms
/// tau at 30 Hz UI tick).
///
/// When unvoiced, the hand fades out rather than parking at a stale
/// position.
struct PitchClockView: View {
    /// Pitch class as a continuous value in [0, 12). 0 = C, 1 = C#, …,
    /// 11 = B. Fractional part is the cents offset within that semitone.
    let pitchClass: Double
    /// Big label drawn in the centre (e.g. "C#").
    let noteName: String
    /// Smaller register suffix drawn under the note (e.g. "3" or a
    /// Hindustani saptak marker).
    let registerLabel: String
    /// Tint applied to the hand based on how in-tune the singer is.
    /// Passed in so the parent's logic stays the source of truth.
    let handTint: Color
    /// True when there's a real detected pitch.
    let isVoiced: Bool

    /// One-pole smoothed pitch class. Unwrapped (not modulo) so we can
    /// follow long glides without the hand snapping back across the
    /// face when crossing C.
    @State private var smoothedUnwrapped: Double = 0
    @State private var hasSeed: Bool = false

    /// ~100 ms time constant at 30 Hz UI tick.
    private let alpha: Double = 0.25

    private let noteNames = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"
    ]

    var body: some View {
        Canvas { ctx, size in
            let cx = size.width / 2
            let cy = size.height / 2
            let radius = min(cx, cy) - 4
            if radius <= 0 { return }

            // --- Face ring ---
            let ring = CGRect(
                x: cx - radius, y: cy - radius,
                width: radius * 2, height: radius * 2
            )
            ctx.stroke(
                Path(ellipseIn: ring),
                with: .color(.white.opacity(0.18)),
                lineWidth: 1.5
            )

            // --- Tick marks + labels at each semitone ---
            // Twelve o'clock = -π/2 in standard math angle (canvas
            // angles run CW from 3 o'clock when positive).
            for i in 0..<12 {
                let a = angleFor(pitchClass: Double(i))
                let isC = (i == 0)
                let tickLen: CGFloat = isC ? 10 : 6
                let lineWidth: CGFloat = isC ? 2 : 1
                let colour = isC ? Color.white.opacity(0.7) : Color.white.opacity(0.35)

                let outer = CGPoint(x: cx + cos(a) * radius, y: cy + sin(a) * radius)
                let inner = CGPoint(
                    x: cx + cos(a) * (radius - tickLen),
                    y: cy + sin(a) * (radius - tickLen)
                )
                var tickPath = Path()
                tickPath.move(to: outer)
                tickPath.addLine(to: inner)
                ctx.stroke(tickPath, with: .color(colour), lineWidth: lineWidth)

                // Label just inside the tick. Skip the accidentals so
                // the face doesn't get crowded — naturals only.
                let isNatural = ![1, 3, 6, 8, 10].contains(i)
                if isNatural {
                    let labelR = radius - tickLen - 12
                    let lp = CGPoint(x: cx + cos(a) * labelR, y: cy + sin(a) * labelR)
                    let text = Text(noteNames[i])
                        .font(.system(size: 11, weight: isC ? .semibold : .regular, design: .rounded))
                        .foregroundColor(isC ? .white.opacity(0.8) : .white.opacity(0.5))
                    ctx.draw(text, at: lp, anchor: .center)
                }
            }

            // --- Hand ---
            let displayClass = wrapMod12(smoothedUnwrapped)
            let handAngle = angleFor(pitchClass: displayClass)
            let handLen = radius - 18
            let tip = CGPoint(
                x: cx + cos(handAngle) * handLen,
                y: cy + sin(handAngle) * handLen
            )
            var hand = Path()
            hand.move(to: CGPoint(x: cx, y: cy))
            hand.addLine(to: tip)
            ctx.stroke(
                hand,
                with: .color(handTint),
                style: StrokeStyle(lineWidth: 2.5, lineCap: .round)
            )

            // Hub.
            let hub = CGRect(x: cx - 4, y: cy - 4, width: 8, height: 8)
            ctx.fill(Path(ellipseIn: hub), with: .color(handTint))

            // --- Centre label: big note name + small register ---
            if isVoiced {
                let nameOrigin = CGPoint(x: cx, y: cy + radius * 0.42)
                let nameText = Text(noteName)
                    .font(.system(size: max(18, radius * 0.32), weight: .semibold, design: .rounded))
                    .foregroundColor(.white)
                ctx.draw(nameText, at: nameOrigin, anchor: .center)

                if !registerLabel.isEmpty {
                    let regOrigin = CGPoint(x: cx, y: cy + radius * 0.72)
                    let regText = Text(registerLabel)
                        .font(.system(size: max(11, radius * 0.16), design: .rounded))
                        .foregroundColor(.white.opacity(0.6))
                    ctx.draw(regText, at: regOrigin, anchor: .center)
                }
            }
        }
        .opacity(isVoiced ? 1.0 : 0.25)
        .onChange(of: pitchClass) { _, newValue in
            updateSmoothed(target: newValue)
        }
        .onChange(of: isVoiced) { _, voiced in
            if voiced {
                smoothedUnwrapped = pitchClass
                hasSeed = true
            } else {
                hasSeed = false
            }
        }
    }

    /// Move `smoothedUnwrapped` toward `target`, taking the *shortest*
    /// angular path around the 12-semitone circle. Without the
    /// shortest-path step, sliding from B (11) to C (0) would walk all
    /// the way back through Bb, A, … instead of crossing the seam.
    private func updateSmoothed(target: Double) {
        if !hasSeed {
            smoothedUnwrapped = target
            hasSeed = true
            return
        }
        let current = smoothedUnwrapped
        let currentMod = wrapMod12(current)
        var delta = target - currentMod
        if delta > 6 { delta -= 12 }
        if delta < -6 { delta += 12 }
        smoothedUnwrapped = current + alpha * delta
    }

    private func wrapMod12(_ v: Double) -> Double {
        let m = v.truncatingRemainder(dividingBy: 12)
        return m < 0 ? m + 12 : m
    }

    /// 12 o'clock = pitch class 0 (C); CW through 11. SwiftUI/Canvas
    /// angle 0 is at 3 o'clock and grows clockwise downward, so 12
    /// o'clock is -π/2. Each semitone advances by 2π/12.
    private func angleFor(pitchClass: Double) -> CGFloat {
        let frac = wrapMod12(pitchClass) / 12.0
        let radians = -.pi / 2 + frac * 2 * .pi
        return CGFloat(radians)
    }
}
