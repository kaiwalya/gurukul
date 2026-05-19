import AVFoundation
import Combine
import OSLog
import SwiftUI

private let log = Logger(subsystem: "com.kaiwalya.Gurukul", category: "ContentView")

/// How long the display holds the last note before fading to "—".
private let silenceDimAfter: TimeInterval = 0.2
private let silenceClearAfter: TimeInterval = 0.5

/// Phase 1.4.7 view: big note + cents readout (from 1.4.6) and a four-up
/// visualiser panel (pitch trace + onset ticks + breath strip + vibrato
/// readout) below, all driven by a 30 Hz timer polling the pipeline's
/// lock-free `FeatureSlot`.
struct ContentView: View {
    @State private var pipeline = AudioPipeline()
    @State private var status: String = "Idle"
    @State private var isRunning: Bool = false

    @State private var displayed: NamedPitch = silentPitch
    @State private var lastSeq: UInt32 = 0
    @State private var lastFreshAt: Date = .distantPast

    @State private var school: PitchSchool = .western
    @State private var westernCtx = WesternContext()

    /// Latest coherent snapshot of all four features, refreshed every UI
    /// tick. Passed by reference (via @State) to the visualiser subviews.
    @State private var snapshot: FeatureSnapshot = .empty

    /// Latest waveform snapshot — independent slot, also refreshed each
    /// UI tick. Lets us eyeball the raw input next to the derived pitch.
    @State private var waveform: WaveformSnapshot = .empty

    private let tick = Timer.publish(every: 1.0 / 30.0, on: .main, in: .common).autoconnect()

    var body: some View {
        VStack(spacing: 16) {
            HStack {
                Picker("School", selection: $school) {
                    Text(PitchSchool.western.displayName).tag(PitchSchool.western)
                }
                .pickerStyle(.menu)
                .fixedSize()

                Spacer()

                Button(isRunning ? "Stop" : "Start") {
                    Task { await toggleRunning() }
                }
                .buttonStyle(.borderedProminent)
            }

            VStack(spacing: 4) {
                noteText
                centsText
                hzText
            }

            WaveformView(waveform: waveform)
                .frame(maxWidth: .infinity, minHeight: 80)

            PitchTraceView(snapshot: snapshot)
                .frame(maxWidth: .infinity, minHeight: 260)

            BreathStripView(breath: snapshot.breath)
                .frame(maxWidth: .infinity, minHeight: 18)

            VibratoReadoutView(
                rate: snapshot.vibratoRate,
                depth: snapshot.vibratoDepth
            )

            Spacer()

            Text(status)
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding(24)
        .frame(minWidth: 520, minHeight: 780)
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
                .font(.system(size: 80, weight: .semibold, design: .rounded))
                .monospacedDigit()
            Text(registerSuffix(displayed.register))
                .font(.system(size: 32, weight: .regular, design: .rounded))
                .foregroundStyle(.secondary)
        }
        .opacity(dimAmount)
    }

    @ViewBuilder private var centsText: some View {
        if displayed.hz > 0 {
            Text(formatCents(displayed.cents))
                .font(.system(size: 24, weight: .medium, design: .rounded))
                .monospacedDigit()
                .foregroundStyle(tintForCents(displayed.cents))
                .opacity(dimAmount)
        } else {
            Text(" ")
                .font(.system(size: 24))
        }
    }

    @ViewBuilder private var hzText: some View {
        if displayed.hz > 0 {
            Text(String(format: "%.1f Hz", displayed.hz))
                .font(.system(size: 14, design: .rounded))
                .foregroundStyle(.tertiary)
                .monospacedDigit()
                .opacity(dimAmount)
        } else {
            Text(" ")
                .font(.system(size: 14))
        }
    }

    private func refreshDisplay(now: Date) {
        // Waveform refreshes every tick regardless of feature seq —
        // it's a separate slot and we want the scrolling envelope to
        // look continuous even on idle frames.
        waveform = pipeline.waveformSlot.load()

        let next = pipeline.featureSlot.load()
        if next.seq == lastSeq { return }
        lastSeq = next.seq
        snapshot = next

        let context = currentContext()
        let namer = PitchNamerFactory.namer(for: context)
        let hz = next.hz
        if hz.isFinite && hz > 0 {
            displayed = namer.name(hz: hz)
            lastFreshAt = now
        } else {
            if Date().timeIntervalSince(lastFreshAt) > silenceClearAfter {
                displayed = silentPitch
            }
        }
    }

    private func currentContext() -> PitchContext {
        switch school {
        case .western: return .western(westernCtx)
        case .hindustani, .karnatik: return .western(westernCtx)
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
