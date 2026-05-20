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
    /// Shared pipeline instance, constructed once at app launch in
    /// `GurukulApp` and passed in so `SettingsView` and `ContentView`
    /// both target the same audio infrastructure.
    let pipeline: AudioPipeline

    @State private var status: String = "Idle"
    @State private var isRunning: Bool = false

    @State private var displayed: NamedPitch = silentPitch
    @State private var lastSeq: UInt32 = 0
    @State private var lastFreshAt: Date = .distantPast

    @State private var school: PitchSchool = .western
    @State private var westernCtx = WesternContext()

    /// Whether the inline debug pane is expanded. Defaults off so the
    /// pane is out-of-the-way for normal use — it's a developer
    /// affordance, not a primary feature.
    @State private var debugPaneOpen: Bool = false

    #if DEBUG
    /// Developer-only sidetone toggle. Routes the mic input through to
    /// the HAL output device — purely to verify end-to-end that the
    /// output IO proc fires. Off on every (re)start. Not shipped in
    /// release builds. See `HALOutput` and `AudioPipeline.setSidetoneEnabled`.
    @State private var sidetoneOn: Bool = false
    #endif

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

                #if DEBUG
                Toggle("Sidetone (debug)", isOn: $sidetoneOn)
                    .toggleStyle(.switch)
                    .controlSize(.small)
                    .disabled(!isRunning)
                    .help("Routes the mic input through the HAL output device. Developer aid; off on every start.")
                    .onChange(of: sidetoneOn) { _, newValue in
                        pipeline.setSidetoneEnabled(newValue)
                    }
                #endif

                Button(isRunning ? "Stop" : "Start") {
                    Task { await toggleRunning() }
                }
                .buttonStyle(.borderedProminent)
            }

            VStack(spacing: 6) {
                PitchClockView(
                    pitchClass: pitchClass(from: displayed),
                    noteName: displayed.hz > 0 ? displayed.name : "",
                    registerLabel: registerSuffix(displayed.register),
                    handTint: tintForCents(displayed.cents),
                    isVoiced: displayed.hz > 0
                )
                .frame(width: 240, height: 240)
                .opacity(dimAmount)
                .frame(maxWidth: .infinity)
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

            DisclosureGroup("Debug pane", isExpanded: $debugPaneOpen) {
                DebugPaneView(pipeline: pipeline)
                    .padding(.top, 4)
            }
            .font(.callout)

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
            #if DEBUG
            // Mirror the invariant in HALOutput / AudioPipeline.stop:
            // sidetone disengages on every (re)start. Keep the UI in sync.
            sidetoneOn = false
            #endif
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

    /// Convert a NamedPitch to a continuous pitch class in [0, 12).
    /// 0 = C, 1 = C#, …, 11 = B; fractional part is the cents offset.
    /// Returns 0 for unvoiced — the clock fades out so the value
    /// doesn't matter visually.
    private func pitchClass(from p: NamedPitch) -> Double {
        guard p.hz > 0 else { return 0 }
        let names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]
        let idx = names.firstIndex(of: p.name) ?? 0
        return Double(idx) + Double(p.cents) / 100.0
    }

    private func tintForCents(_ cents: Int) -> Color {
        let mag = abs(cents)
        if mag <= 5 { return .green }
        if mag <= 20 { return .primary }
        return .orange
    }
}

#Preview {
    ContentView(pipeline: AudioPipeline())
}
