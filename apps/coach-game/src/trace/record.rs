//! Trace record types — the on-disk JSONL schema.
//!
//! Each line of `ux.jsonl.gz` is one [`Record`]: a `{"f": <frame>, "k":
//! "<kind>", …}` object. The kinds and their payloads defined here *are* the
//! on-disk contract. These types are the *writer's* view; a reader (agent
//! `gzcat … | jq`, [`super::replay::load`]) parses the same shapes back.
//!
//! Everything here is plain serde data — no Bevy, no domain logic. The port
//! payloads (`FeatureSnapshot`, `CoachEvent`, `Command`) are embedded by
//! reference to their own `Serialize` impls (the `serde` feature on
//! `domain-ports`), so this module never restates a port type's shape.

use serde::Serialize;

use domain_ports::app_coach::{CoachEvent, Command, FeatureSnapshot};

/// Schema version of the `ux.jsonl` format. Bump on any
/// backward-incompatible change to a record shape so a reader can refuse a
/// trace it can't parse.
///
/// `2`: the `coach` channel carries port types (`FeatureSnapshot`, `f0_hz`
/// raw) rather than the head's lifted `Features` — replay serves reads verbatim
/// (the port-types rule; see [`TraceBuffer`](super::TraceBuffer)). A schema-1
/// trace is simply re-recorded.
///
/// `3`: the `input` channel records the **canonical** `bevy::window::WindowEvent`
/// stream (one reader, true cross-channel order) instead of the six *derived*
/// typed message channels. winit fans `WindowEvent` out into both the combined
/// stream and the typed channels; the typed channels are a lossy, order-scrambled
/// shadow, and crucially UI **picking** reads only the combined stream — so a
/// click recorded off the typed channels was invisible to replay. Recording the
/// upstream source and letting the app re-derive is the same rule schema 2
/// applied to the coach channel. The `InputRecord` set widened to the variants
/// that drive downstream state (hover enter/leave, touch, focus-lost). A
/// schema-≤2 trace is simply re-recorded.
///
/// `4`: the `poly` *output* channel records the pitch-trace polyline that a
/// future GPU-mesh renderer won't expose through the UI-node `geom` channel.
/// Two coord stages per segment: lane-local logical px (stage 1, where
/// "inside the lane?" is answerable) and physical-px AABB plus post-clip
/// drawn bounds (stage 2). Output-only — replay ignores `poly` records as
/// inputs but a re-run reproduces them bit-for-bit from the same `geom`
/// channel. A schema-≤3 trace is simply re-recorded.
pub const SCHEMA_VERSION: u32 = 4;

/// One line of the trace. Flattened so the JSON is `{"f":…,"k":"…",…payload}`
/// rather than a nested `{"f":…,"payload":{…}}` — friendlier to `jq`/grep.
/// (No `Debug`: it embeds the port's `Command`/`CoachEvent`, which the port
/// deliberately keeps `Debug`-free.)
#[derive(Serialize)]
pub struct Record {
    /// Bevy `FrameCount` when this record was written.
    pub f: u32,
    /// The record kind + its payload.
    #[serde(flatten)]
    pub body: Body,
}

/// The kind-tagged payload of a [`Record`]. `#[serde(tag = "k")]` writes the
/// kind into the `k` field alongside the payload fields.
#[derive(Serialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Body {
    /// Once, the first line: how to read everything below it.
    Run {
        schema: u32,
        app_version: &'static str,
        /// Window logical size at launch, `[width, height]`.
        window_logical: [f32; 2],
        scale_factor: f32,
        /// Wall-clock launch time, UTC, `YYYY-MM-DD HH:MM:SS`.
        wall_start: String,
        /// Trace directory name this run replays, replay runs only.
        #[serde(skip_serializing_if = "Option::is_none")]
        replay_of: Option<String>,
    },
    /// Every frame: the wall-time delta Bevy advanced the clock by.
    Frame { delta_s: f32 },
    /// On a Bevy input message this frame.
    Input(InputRecord),
    /// On an `AppState` transition.
    State { from: String, to: String },
    /// On a non-empty coach read this frame (what `drain_events` saw).
    Coach(CoachRead),
    /// On a `Command` sent to the coach.
    Cmd {
        #[serde(rename = "cmd")]
        command: Command,
    },
    /// On a per-entity geometry change after layout (or a despawn).
    Geom(GeomRecord),
    /// F10 pressed.
    Mark { marker: u32 },
}

/// One [`bevy::window::WindowEvent`] variant, reduced to the fields that matter
/// for replay / debugging. We capture our *own* small shapes rather than
/// re-serializing Bevy's input structs, so the trace format does not depend on
/// Bevy's `serialize` feature and stays readable.
///
/// **No window entity is recorded.** Entity ids don't survive the trace
/// boundary (a fresh run mints fresh ids); the driver remaps every replayed
/// window reference to the live `PrimaryWindow` at injection. Recording the raw
/// id would be a number that means nothing on replay.
///
/// The variant set is the subset of `WindowEvent` that drives downstream app
/// state — pointer position/buttons/wheel (picking → `Interaction`), hover
/// enter/leave (picking hover state), touch (iOS picking), keys, focus-lost
/// (clears `ButtonInput`), and window resize/scale. Lifecycle/IME/file-drop
/// variants are not replay-relevant and are dropped.
#[derive(Debug, Serialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum InputRecord {
    Key {
        /// `KeyCode` `Debug` form (e.g. `"F10"`, `"Escape"`).
        key: String,
        /// `"pressed"` / `"released"`.
        state: &'static str,
        repeat: bool,
    },
    /// All-windows keyboard focus lost — Bevy uses it to clear held keys.
    KeyboardFocusLost,
    MouseButton {
        /// `MouseButton` `Debug` form (e.g. `"Left"`).
        button: String,
        state: &'static str,
    },
    Cursor {
        /// Logical-pixel position `[x, y]` — the `WindowEvent::CursorMoved`
        /// position verbatim, which winit emits in logical px (physical ÷ scale
        /// factor). On replay the driver re-derives physical (× the recorded
        /// scale) when it mirrors winit's `set_physical_cursor_position`.
        pos: [f32; 2],
    },
    /// Cursor entered the window — picking starts tracking it.
    CursorEntered,
    /// Cursor left the window — picking drops hover state.
    CursorLeft,
    Wheel {
        /// `MouseScrollUnit` `Debug` form.
        unit: String,
        x: f32,
        y: f32,
    },
    Touch {
        /// `TouchPhase` `Debug` form (`"Started"`/`"Moved"`/`"Ended"`/`"Canceled"`).
        phase: String,
        /// Logical-pixel position `[x, y]` — `TouchInput.position` verbatim, which
        /// winit converts to logical before emitting (like `CursorMoved`).
        pos: [f32; 2],
        /// Finger id, so multi-touch sequences replay distinctly.
        id: u64,
    },
    Resize {
        /// New logical size `[width, height]`.
        size: [f32; 2],
    },
    ScaleFactor {
        scale_factor: f64,
    },
}

/// What `drain_events` read from the coach this frame. Only written when at
/// least one of the three is non-empty (a quiet frame writes nothing), so the
/// frame-batching jitter the plan cares about is visible: a frame that drained
/// several snapshots writes one `Coach` record carrying all of them, a frame
/// that drained nothing writes none.
#[derive(Serialize)]
pub struct CoachRead {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<CoachEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<FeatureSnapshot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub drained: Vec<FeatureSnapshot>,
}

impl CoachRead {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.latest.is_none() && self.drained.is_empty()
    }
}

/// Per-entity geometry after layout. Keyed by `path` (widget-`Name` ancestry,
/// not `Entity`, so run-to-run diffs survive). All pixel fields are *physical*
/// and recorded together with `scale_factor`, so a reader derives logical and
/// a frame-confusion bug shows up as data (a rect exactly 2× off at scale 2).
#[derive(Debug, Serialize)]
pub struct GeomRecord {
    /// Widget path: `Name` ancestry joined with `/`, plus a sibling index for
    /// nameless or repeated nodes (e.g. `time_graph/lane/trace_layer/body.3`).
    pub path: String,
    /// Raw entity id, supplementary only — does not survive replay.
    pub entity: u64,
    /// Physical size `[width, height]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_px: Option<[f32; 2]>,
    /// Global axis-aligned rect in physical px `[min_x, min_y, max_x, max_y]`,
    /// accounting for rotation (the four transformed corners' bounding box).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rect_px: Option<[f32; 4]>,
    /// Clip rect in physical px `[min_x, min_y, max_x, max_y]`, if the node is
    /// under a clip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip_px: Option<[f32; 4]>,
    /// Rotation in radians (0 for an unrotated node).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rot: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale_factor: Option<f32>,
    /// `true` when this entity vanished since the previous frame — the
    /// despawn-fight bug class is "something disappeared that shouldn't have".
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub gone: bool,
}
