import AVFoundation
import SwiftUI

/// Phase 1.4.5 skeleton view. There is no pitch display yet — pitch is
/// printed to stdout via `AudioPipeline`. The view exists only to host the
/// pipeline lifecycle and the mic-permission prompt; the visualiser arrives
/// in 1.4.6 and 1.4.7.
struct ContentView: View {
    @State private var pipeline = AudioPipeline()
    @State private var status: String = "Idle"

    var body: some View {
        VStack(spacing: 16) {
            Text("Gurukul")
                .font(.largeTitle)
            Text(status)
                .font(.body)
                .foregroundStyle(.secondary)
            Text("Pitch is printed to the Xcode console (View → Debug Area).")
                .font(.callout)
                .foregroundStyle(.tertiary)
                .multilineTextAlignment(.center)
        }
        .padding(40)
        .frame(minWidth: 360, minHeight: 200)
        .task {
            await startPipeline()
        }
    }

    private func startPipeline() async {
        let granted = await AVCaptureDevice.requestAccess(for: .audio)
        guard granted else {
            status = "Microphone permission denied."
            return
        }
        do {
            try pipeline.start()
            status = "Listening — sing or hum into the mic."
        } catch {
            status = "Failed to start: \(error.localizedDescription)"
        }
    }
}

#Preview {
    ContentView()
}
