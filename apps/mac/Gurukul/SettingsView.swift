import OSLog
import SwiftUI

private let log = Logger(subsystem: "com.kaiwalya.Gurukul", category: "SettingsView")

/// Audio preferences pane. Opens via Cmd-, (the standard macOS Settings
/// shortcut). Three pickers: input device, output device, engine sample
/// rate. When a chosen device's native SR ≠ the engine SR, an inline
/// "Align engine to N kHz" button appears under the picker.
///
/// State model: this view owns no audio state. It reads/writes
/// `Prefs` for persistence and calls `pipeline.applySettings(...)` to
/// apply changes. The pipeline diffs against its current state and
/// performs the cheapest sufficient action (device swap vs full
/// rebuild).
struct SettingsView: View {
    let pipeline: AudioPipeline
    @ObservedObject var catalog: AudioDeviceCatalog

    @State private var settings: AudioSettings
    /// Last successfully applied settings. We track this so that when
    /// `applySettings` throws we can revert the picker UI rather than
    /// leaving the user looking at a value the pipeline rejected.
    @State private var lastApplied: AudioSettings
    @State private var lastError: String?
    /// Re-entrancy guard: setting `settings = lastApplied` on a failed
    /// apply would otherwise re-fire onChange and cascade.
    @State private var suppressNextApply: Bool = false

    init(pipeline: AudioPipeline, catalog: AudioDeviceCatalog, initial: AudioSettings) {
        self.pipeline = pipeline
        self.catalog = catalog
        self._settings = State(initialValue: initial)
        self._lastApplied = State(initialValue: initial)
    }

    var body: some View {
        Form {
            Section("Input") {
                inputPicker
                if let mismatch = inputRateMismatch {
                    alignButton(
                        message: "\(mismatch.deviceName) runs at \(mismatch.deviceRate.displayName); engine is at \(mismatch.engineRate.displayName).",
                        targetRate: mismatch.deviceRate
                    )
                }
            }
            Section("Output") {
                outputPicker
                if let mismatch = outputRateMismatch {
                    alignButton(
                        message: "\(mismatch.deviceName) runs at \(mismatch.deviceRate.displayName); engine is at \(mismatch.engineRate.displayName).",
                        targetRate: mismatch.deviceRate
                    )
                }
            }
            Section("Engine") {
                Picker("Sample rate", selection: $settings.sampleRate) {
                    ForEach(SupportedSampleRate.allCases) { rate in
                        Text(rate.displayName).tag(rate.rawValue)
                    }
                }
                Text("Engine, input, and output all run at the same rate — the cabinet does not resample.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            if let lastError {
                Section("Last error") {
                    Text(lastError)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                }
            }
        }
        .formStyle(.grouped)
        .padding()
        .frame(minWidth: 420, minHeight: 360)
        .onChange(of: settings) { _, newValue in
            apply(newValue)
        }
    }

    // MARK: - Pickers

    private var inputPicker: some View {
        Picker("Input device", selection: $settings.inputDeviceUID) {
            Text("System default").tag(String?.none)
            ForEach(catalog.inputs) { device in
                Text(label(for: device)).tag(Optional(device.uid))
            }
        }
    }

    private var outputPicker: some View {
        Picker("Output device", selection: $settings.outputDeviceUID) {
            Text("System default").tag(String?.none)
            ForEach(catalog.outputs) { device in
                Text(label(for: device)).tag(Optional(device.uid))
            }
        }
    }

    private func label(for device: AudioDeviceInfo) -> String {
        let rate = SupportedSampleRate.from(device.nominalSampleRate)?.displayName
            ?? "\(device.nominalSampleRate) Hz"
        return "\(device.name) — \(rate)"
    }

    // MARK: - Alignment affordance

    private struct RateMismatch {
        let deviceName: String
        let deviceRate: SupportedSampleRate
        let engineRate: SupportedSampleRate
    }

    private var inputRateMismatch: RateMismatch? {
        rateMismatch(uid: settings.inputDeviceUID, in: catalog.inputs)
    }

    private var outputRateMismatch: RateMismatch? {
        rateMismatch(uid: settings.outputDeviceUID, in: catalog.outputs)
    }

    private func rateMismatch(uid: String?, in devices: [AudioDeviceInfo]) -> RateMismatch? {
        guard let uid, let device = devices.first(where: { $0.uid == uid }) else {
            return nil
        }
        guard let deviceRate = SupportedSampleRate.from(device.nominalSampleRate),
              let engineRate = SupportedSampleRate.from(settings.sampleRate) else {
            return nil
        }
        guard deviceRate != engineRate else { return nil }
        return RateMismatch(
            deviceName: device.name,
            deviceRate: deviceRate,
            engineRate: engineRate
        )
    }

    private func alignButton(message: String, targetRate: SupportedSampleRate) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(message)
                .font(.callout)
                .foregroundStyle(.secondary)
            Button("Align engine to \(targetRate.displayName)") {
                var updated = settings
                updated.sampleRate = targetRate.rawValue
                settings = updated
                // The .onChange handler fires once for the combined
                // sampleRate update, which triggers exactly one rebuild.
            }
            .controlSize(.small)
        }
    }

    // MARK: - Apply

    private func apply(_ newValue: AudioSettings) {
        if suppressNextApply {
            suppressNextApply = false
            return
        }
        // Skip Prefs write + pipeline call when nothing changed.
        if newValue.change(from: lastApplied) == .none {
            return
        }
        do {
            try pipeline.applySettings(newValue)
            // Only persist after a successful apply — a failed config
            // shouldn't be the one that comes back on next launch.
            Prefs.saveAudioSettings(newValue)
            lastApplied = newValue
            lastError = nil
        } catch {
            lastError = error.localizedDescription
            log.error("applySettings failed: \(error.localizedDescription, privacy: .public)")
            // Revert the picker UI to the last-applied value so the
            // user doesn't see a stale selection that doesn't match
            // the running pipeline.
            suppressNextApply = true
            settings = lastApplied
        }
    }
}
