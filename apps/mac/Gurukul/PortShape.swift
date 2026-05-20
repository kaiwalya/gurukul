import Foundation

/// What kind of data lives on a given engine port. Drives both the widget
/// choice in the debug pane and the audio-thread store path (audio-typed
/// ports can also be wired to the monitor route in PR 5.3).
///
/// rawValue stays stable across builds — it is what we publish through
/// `DebugSelectionSnapshot.typeTag` and `DebugTapSnapshot.typeTag` to the
/// audio thread, and across DebugTapSlot's lock-free boundary.
enum PortShape: UInt8 {
    /// Continuous audio samples — one float per frame, hop-many per
    /// block. Rendered as a waveform. Audition through the monitor route
    /// is allowed for this shape.
    case audio = 0
    /// One frequency reading (Hz) per hop, sample-and-hold over the
    /// block. Rendered as a numeric readout + small trace.
    case featureHz = 1
    /// Event-shaped: rare non-zero pulses (e.g. onset detector). Rendered
    /// as a tick row. Reader takes max-|x| over the block.
    case featureEvent = 2
    /// A control-type port: one slowly-changing scalar per hop. Rendered
    /// as a numeric readout + sparkline.
    case control = 3

    /// Default for unknown / unclassified ports. Safe — renders a generic
    /// numeric readout and never routes to monitor.
    static let unknown: PortShape = .control
}

/// Maps (nodeId, portName) → PortShape for the current world.
///
/// Where this lives: the cabinet (Swift) classifies the port shape today
/// because there is no FFI for it yet. When the engine grows real
/// port-shape metadata (Phase 1.5+ / ECS refactor), this table goes away
/// and the cabinet asks the engine. Until then, **adding a new node = one
/// row here**.
///
/// Wildcard matching: "*" as the port name matches any port on that node.
/// Used for nodes whose ports all share a shape.
enum PortShapeTable {
    /// Look up the shape for `(node, port)`. Falls back to
    /// `PortShape.unknown` for unknown combinations — safe (renders as
    /// control, never routes to monitor).
    static func shape(node: String, port: String) -> PortShape {
        // Exact match wins over wildcard.
        if let exact = exactMatches["\(node).\(port)"] {
            return exact
        }
        if let wildcard = wildcardMatches[node] {
            return wildcard
        }
        return .unknown
    }

    /// Exact-port classifications. Keep one row per port until the
    /// engine exposes shape metadata.
    private static let exactMatches: [String: PortShape] = [
        // node-pitch-yin: f0 is a held Hz reading per hop.
        "pitch_yin.f0": .featureHz,
        // node-onset: events is a pulse train.
        "onset_det.events": .featureEvent,
        // node-breath: amplitude envelope, slow-moving scalar.
        "breath_det.breath": .control,
        // node-vibrato: rate and depth are slow scalars.
        "vibrato_det.rate": .control,
        "vibrato_det.depth": .control,
    ]

    /// Whole-node defaults. None today — listed for shape only.
    private static let wildcardMatches: [String: PortShape] = [:]
}
