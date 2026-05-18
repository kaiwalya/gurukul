import AVFoundation
import Combine
import OSLog
import SwiftUI

private let log = Logger(subsystem: "com.kaiwalya.Gurukul", category: "ContentView")

/// How long the display holds the last note before fading to "—".
private let silenceDimAfter: TimeInterval = 0.2
private let silenceClearAfter: TimeInterval = 0.5

/// Phase 1.4.6 view: big note + cents readout driven by a 30 Hz timer that
/// polls the audio pipeline's lock-free pitch slot.
struct ContentView: View {
    @State private var pipeline = AudioPipeline()
    @State private var status: String = "Idle"
    @State private var isRunning: Bool = false

    @State private var displayed: NamedPitch = silentPitch
    @State private var lastSeq: UInt32 = 0
    @State private var lastFreshAt: Date = .distantPast

    @State private var school: PitchSchool = .western
    @State private var westernCtx = WesternContext()

    private let tick = Timer.publish(every: 1.0 / 30.0, on: .main, in: .common).autoconnect()

    var body: some View {
        VStack(spacing: 24) {
            HStack {
                Picker("School", selection: $school) {
                    Text(PitchSchool.western.displayName).tag(PitchSchool.western)
                    // Indian schools are shaped in the type system but not
                    // shipped in 1.4.6 — no disabled menu items either.
                }
                .pickerStyle(.menu)
                .fixedSize()

                Spacer()

                Button(isRunning ? "Stop" : "Start") {
                    Task { await toggleRunning() }
                }
                .buttonStyle(.borderedProminent)
            }

            Spacer()

            VStack(spacing: 8) {
                noteText
                centsText
                hzText
            }

            Spacer()

            Text(status)
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding(32)
        .frame(minWidth: 480, minHeight: 320)
        .task {
            await startIfPermitted()
        }
        .onReceive(tick) { now in
            refreshDisplay(now: now)
        }
    }

    private var dimAmount: Double {
        let dt = Date().timeIntervalSince(lastFreshAt)
        if dt < silenceDimAfter { return 1.0 }
        if dt > silenceClearAfter { return 0.35 }
        let t = (dt - silenceDimAfter) / (silenceClearAfter - silenceDimAfter)
        return 1.0 - t * 0.65
    }

    @ViewBuilder private var noteText: some View {
        HStack(alignment: .firstTextBaseline, spacing: 4) {
            Text(displayed.name)
                .font(.system(size: 96, weight: .semibold, design: .rounded))
                .monospacedDigit()
            Text(registerSuffix(displayed.register))
                .font(.system(size: 40, weight: .regular, design: .rounded))
                .foregroundStyle(.secondary)
        }
        .opacity(dimAmount)
    }

    @ViewBuilder private var centsText: some View {
        if displayed.hz > 0 {
            Text(formatCents(displayed.cents))
                .font(.system(size: 28, weight: .medium, design: .rounded))
                .monospacedDigit()
                .foregroundStyle(tintForCents(displayed.cents))
                .opacity(dimAmount)
        } else {
            Text(" ")
                .font(.system(size: 28))
        }
    }

    @ViewBuilder private var hzText: some View {
        if displayed.hz > 0 {
            Text(String(format: "%.1f Hz", displayed.hz))
                .font(.system(size: 16, design: .rounded))
                .foregroundStyle(.tertiary)
                .monospacedDigit()
                .opacity(dimAmount)
        } else {
            Text(" ")
                .font(.system(size: 16))
        }
    }

    private func refreshDisplay(now: Date) {
        let (seq, hz) = pipeline.pitchSlot.load()
        if seq == lastSeq { return }
        lastSeq = seq

        let context = currentContext()
        let namer = PitchNamerFactory.namer(for: context)

        if hz.isFinite && hz > 0 {
            displayed = namer.name(hz: hz)
            lastFreshAt = now
        } else {
            // Block ran but no detection. Keep the last named pitch visible
            // so the user sees a brief hold; the dim curve handles the fade.
            if Date().timeIntervalSince(lastFreshAt) > silenceClearAfter {
                displayed = silentPitch
            }
        }
    }

    private func currentContext() -> PitchContext {
        switch school {
        case .western: return .western(westernCtx)
        case .hindustani, .karnatik: return .western(westernCtx) // not shipped yet
        }
    }

    private func toggleRunning() async {
        if isRunning {
            pipeline.stop()
            isRunning = false
            status = "Stopped"
        } else {
            await startIfPermitted()
        }
    }

    private func startIfPermitted() async {
        let granted = await AVCaptureDevice.requestAccess(for: .audio)
        guard granted else {
            log.error("microphone permission denied")
            status = "Microphone permission denied."
            return
        }
        do {
            try pipeline.start()
            isRunning = true
            status = "Listening — sing or hum into the mic."
        } catch {
            log.error("pipeline start failed: \(error.localizedDescription, privacy: .public)")
            status = "Failed to start: \(error.localizedDescription)"
        }
    }

    private func registerSuffix(_ register: Register) -> String {
        switch register {
        case .western(let octave):
            return displayed.hz > 0 ? "\(octave)" : ""
        case .indian(let octave):
            return octave.marker
        }
    }

    private func formatCents(_ cents: Int) -> String {
        if cents == 0 { return "in tune" }
        let sign = cents > 0 ? "+" : ""
        return "\(sign)\(cents)¢"
    }

    private func tintForCents(_ cents: Int) -> Color {
        let mag = abs(cents)
        if mag <= 5 { return .green }
        if mag <= 20 { return .primary }
        return .orange
    }
}

#Preview {
    ContentView()
}
