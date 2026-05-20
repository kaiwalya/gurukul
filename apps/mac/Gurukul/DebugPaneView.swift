import Combine
import SwiftUI

/// Inline debug pane that lets the developer pick any (node, port) in
/// the live engine and see its most-recent block rendered with a
/// shape-appropriate widget. The first interactive consumer of the
/// port-subscription pattern — Phase 1.5+ rules, future editors, and
/// any game UI inherit the same shape.
///
/// State model: the view owns the picker selection in `@State`. On any
/// change it republishes `DebugSelectionSlot` so the audio thread starts
/// tapping the new port. On engine rebuild / device swap, the pipeline
/// clears the slot from the audio side; the view re-reads the
/// snapshot and finds `len == 0` ⇒ shows the "no selection" placeholder.
struct DebugPaneView: View {
    let pipeline: AudioPipeline

    /// 30 Hz UI tick driving `tap` and the engine introspection refresh
    /// when this pane is visible.
    private let tick = Timer.publish(every: 1.0 / 30.0, on: .main, in: .common).autoconnect()

    @State private var nodeIds: [String] = []
    @State private var portNames: [String] = []
    /// nil = "no selection" — the picker's first row, equivalent to
    /// clearing `DebugSelectionSlot`.
    @State private var selectedNode: String? = nil
    @State private var selectedPort: String? = nil

    @State private var tap: DebugTapSnapshot = .empty
    @State private var lastTapSeq: UInt32 = 0
    /// UI-side generation counter; bumped on every selection republish
    /// so the audio thread sees a strictly-increasing value.
    @State private var generation: UInt32 = 0

    /// Monitor toggle — routes audio-shaped port samples into the HAL
    /// output ring so the developer can hear what they're inspecting.
    /// Off on every appearance; the pipeline also force-clears it on
    /// engine reset / device swap (via DebugSelectionSlot.clear).
    @State private var monitorOn: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Inspect port")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Refresh") {
                    refreshNodeList()
                }
                .controlSize(.small)
            }

            HStack {
                Picker("Node", selection: $selectedNode) {
                    Text("—").tag(String?.none)
                    ForEach(nodeIds, id: \.self) { id in
                        Text(id).tag(Optional(id))
                    }
                }
                .pickerStyle(.menu)
                .frame(maxWidth: 200)
                .onChange(of: selectedNode) { _, newNode in
                    onNodeChanged(newNode)
                }

                Picker("Port", selection: $selectedPort) {
                    Text("—").tag(String?.none)
                    ForEach(portNames, id: \.self) { name in
                        Text(name).tag(Optional(name))
                    }
                }
                .pickerStyle(.menu)
                .frame(maxWidth: 200)
                .disabled(selectedNode == nil)
                .onChange(of: selectedPort) { _, _ in
                    publishSelection()
                }

                Spacer()

                // Monitor toggle. Only meaningful for audio-shaped ports
                // (writeMono ignores feature/control values anyway).
                // Hidden — not just disabled — for non-audio shapes so
                // the toggle's mere presence doesn't suggest it would do
                // anything on a numeric readout.
                if currentShape == .audio {
                    Toggle("Monitor", isOn: $monitorOn)
                        .toggleStyle(.switch)
                        .controlSize(.small)
                        .onChange(of: monitorOn) { _, _ in
                            publishSelection()
                        }
                }
            }

            body(for: currentShape)
                .frame(minHeight: 40)
        }
        .padding(12)
        .background(
            RoundedRectangle(cornerRadius: 8)
                .fill(Color.secondary.opacity(0.06))
        )
        .onAppear {
            refreshNodeList()
        }
        .onReceive(tick) { _ in
            refreshTap()
        }
    }

    // MARK: - Body widgets

    /// Shape we should render with. nil ⇒ no selection — show a hint.
    private var currentShape: PortShape? {
        guard let node = selectedNode, let port = selectedPort else { return nil }
        return PortShapeTable.shape(node: node, port: port)
    }

    @ViewBuilder
    private func body(for shape: PortShape?) -> some View {
        if let shape {
            switch shape {
            case .audio:
                DebugAudioBlockView(tap: tap)
                    .frame(height: 80)
            case .featureHz:
                FeatureHzReadout(tap: tap)
            case .featureEvent:
                FeatureEventTicks(tap: tap)
            case .control:
                ControlReadout(tap: tap)
            }
        } else {
            Text("Pick a node and port to inspect.")
                .font(.callout)
                .foregroundStyle(.secondary)
        }
    }

    // MARK: - Engine introspection

    private func refreshNodeList() {
        nodeIds = pipeline.nodeIds()
        // If the previously-selected node is no longer in the engine
        // (post-rebuild edge case), drop the selection.
        if let node = selectedNode, !nodeIds.contains(node) {
            selectedNode = nil
            selectedPort = nil
            portNames = []
            publishSelection()
        }
    }

    private func onNodeChanged(_ newNode: String?) {
        if let newNode {
            portNames = pipeline.outPortNames(for: newNode)
        } else {
            portNames = []
        }
        // The previously-selected port likely doesn't exist on the new
        // node. Clear it; user picks again.
        selectedPort = nil
        publishSelection()
    }

    private func publishSelection() {
        generation &+= 1
        let node = selectedNode ?? ""
        let port = selectedPort ?? ""
        let typeTag: UInt8
        if node.isEmpty || port.isEmpty {
            typeTag = PortShape.unknown.rawValue
        } else {
            typeTag = PortShapeTable.shape(node: node, port: port).rawValue
        }
        // Only an audio-shaped port may engage monitor. Defensively
        // gate here so a stale `monitorOn = true` from a previous audio
        // selection can't sneak through when the user picks a
        // non-audio port.
        let allowMonitor = (typeTag == PortShape.audio.rawValue) && monitorOn
        pipeline.debugSelectionSlot.store(
            generation: generation,
            nodeId: node,
            port: port,
            typeTag: typeTag,
            monitor: allowMonitor
        )
    }

    private func refreshTap() {
        let next = pipeline.debugTapSlot.load()
        if next.seq == lastTapSeq { return }
        lastTapSeq = next.seq
        tap = next
    }
}

// MARK: - Per-shape widgets

private struct FeatureHzReadout: View {
    let tap: DebugTapSnapshot

    var body: some View {
        HStack(spacing: 12) {
            Text("Hz")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(displayedValue)
                .font(.system(size: 28, weight: .medium, design: .rounded))
                .monospacedDigit()
                .foregroundStyle(isVoiced ? .primary : .secondary)
            Spacer()
        }
    }

    /// f0 ports hold the last-detected pitch across the block. Take the
    /// last sample (matches readLastSample's contract for sample-and-hold
    /// features). 0 = unvoiced this hop.
    private var lastValue: Float {
        guard tap.len > 0 else { return 0 }
        return tap.samples[Int(tap.len) - 1]
    }

    private var isVoiced: Bool { lastValue > 0 }

    private var displayedValue: String {
        if !isVoiced { return "—" }
        return String(format: "%.1f", lastValue)
    }
}

private struct FeatureEventTicks: View {
    let tap: DebugTapSnapshot

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text("max |x|")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Text(String(format: "%.3f", magnitude))
                    .font(.system(size: 14, design: .rounded))
                    .monospacedDigit()
                Spacer()
            }
            // A simple bar: width tracks the magnitude (clamped 0..1).
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    Rectangle()
                        .fill(Color.secondary.opacity(0.15))
                    Rectangle()
                        .fill(magnitude > 0.05 ? Color.accentColor : .secondary)
                        .frame(width: geo.size.width * CGFloat(min(magnitude, 1)))
                }
            }
            .frame(height: 10)
            .clipShape(RoundedRectangle(cornerRadius: 3))
        }
    }

    /// Max-|x| over the block, matching readMaxAbs's event-port contract.
    private var magnitude: Float {
        guard tap.len > 0 else { return 0 }
        var m: Float = 0
        for i in 0..<Int(tap.len) {
            let v = abs(tap.samples[i])
            if v > m { m = v }
        }
        return m
    }
}

/// One block (up to kDebugTapMaxSamples = 512 floats) drawn as a min/max
/// envelope. Unlike WaveformView, this is a single block — no horizontal
/// scrolling, no per-bucket aggregation. Auto-scales to the loudest
/// sample so quiet signals are still legible.
private struct DebugAudioBlockView: View {
    let tap: DebugTapSnapshot

    var body: some View {
        Canvas { ctx, size in
            let bgRect = CGRect(origin: .zero, size: size)
            ctx.fill(Path(bgRect), with: .color(.black.opacity(0.18)))

            var center = Path()
            center.move(to: CGPoint(x: 0, y: size.height / 2))
            center.addLine(to: CGPoint(x: size.width, y: size.height / 2))
            ctx.stroke(center, with: .color(.white.opacity(0.12)), lineWidth: 1)

            let len = Int(tap.len)
            if len == 0 { return }

            var peak: Float = 0.02
            for i in 0..<len {
                let v = abs(tap.samples[i])
                if v > peak { peak = v }
            }
            if peak > 1.0 { peak = 1.0 }

            let halfH = size.height / 2
            let xPerSample = size.width / CGFloat(len)

            var line = Path()
            for i in 0..<len {
                let x = CGFloat(i) * xPerSample
                let y = halfH - CGFloat(tap.samples[i] / peak) * halfH * 0.95
                if i == 0 {
                    line.move(to: CGPoint(x: x, y: y))
                } else {
                    line.addLine(to: CGPoint(x: x, y: y))
                }
            }
            ctx.stroke(line, with: .color(.accentColor), lineWidth: 1.2)
        }
        .clipShape(RoundedRectangle(cornerRadius: 4))
    }
}

private struct ControlReadout: View {
    let tap: DebugTapSnapshot

    var body: some View {
        HStack(spacing: 12) {
            Text("value")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(String(format: "%.4f", lastValue))
                .font(.system(size: 22, weight: .medium, design: .rounded))
                .monospacedDigit()
            Spacer()
        }
    }

    private var lastValue: Float {
        guard tap.len > 0 else { return 0 }
        return tap.samples[Int(tap.len) - 1]
    }
}
