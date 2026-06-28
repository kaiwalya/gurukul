# Plan: split the vibrato band into its own data series

## Goal (Step A only — pure refactor, identical pixels)
Stop the band and the pitch trace from sharing one struct. Make each a **separate
data series** with its own shape and its own mesh — like enable/disable-able chart
series. This is the groundwork; the band's internal *transform chain* (smooth /
back-date / flatten-rails) is **Step B, deferred** — explicitly NOT in this change.

User's framing: "the canvas should be built up in layers — one render path for pitch, one
for vibrato banding (later breath, etc.). We don't want one struct that keeps growing;
instead multiple meshes getting composited, like enabling/disabling data series in a chart."

## What's already true (don't rebuild it)
The RENDER layer is already two independent series:
- `apply_mesh_trace` (pitch) → its own `TraceMeshEntity` / `TraceMeshHandles`, z=0.2.
- `apply_mesh_band` (vibrato) → its own `BandMeshEntity` / `BandMeshHandles`, z=0.15.
Two meshes, two systems, different z-layers. Good — leave this structure.

## The actual problem: fused DATA
Both render systems read the SAME `live.pitch_segments`, and the per-point struct
`NormalizedTracePoint` (scene.rs) carries BOTH trace fields and band fields:
```rust
NormalizedTracePoint {
    point,              // ← trace series
    confidence,         // ← used by both
    vibrato_strength,   // ← band series
    band_half_height,   // ← band series
    band_center_y,      // ← band series
    vibrato_x,          // ← band series
}
```
`apply_mesh_band` reaches into a *trace* point to pull band fields. Every new feature
(breath…) would widen this struct again. That is the smell to kill.

## Target shape
Two parallel series in the live scene, each its own shape:

```rust
// scene.rs
struct PitchTracePoint { x: f32, y: f32, confidence: f32 }      // the raw trace, nothing else
struct VibratoBandPoint { x: f32, center_y: f32, half_height: f32, confidence: f32 }

struct TimeGraphLiveSceneRes {
    pitch_segments: Vec<Vec<PitchTracePoint>>,   // series 1
    band_segments:  Vec<Vec<VibratoBandPoint>>,  // series 2  ← NEW, was fused in
    onset_ticks, breath_spans,                   // unchanged
}
```
- `apply_mesh_trace` reads `pitch_segments` (PitchTracePoint) — only x/y/confidence.
- `apply_mesh_band` reads `band_segments` (VibratoBandPoint) — only band fields.
- Neither reaches into the other's shape.
- Adding breath later = add `breath_segments` + a system. Touch nothing existing.

## Where the band series is produced  (architect corrections folded in)
`model.rs::normalize_trace_segment` today does TWO jobs: project raw pitch AND derive the
band (smoothing + back-dating inline). Split into a SHARED filter + two projections.

**Correction #2 — the shared post-filter point list (the important one).** The band's
center-y is sampled from the PITCH's smoothed array by shared index `i`
(`smoothed_center[i]` / `point_xs`, built from `smooth(&raw_ys, 9)`). Both arrays are in
the **post-filter** index space — the points that survived `normalize_time` AND
`normalize_pitch` both returning `Some`. If pitch and band each re-run that filter
independently, their index spaces can DRIFT and the back-date samples the wrong point —
silently wrong pixels. So the filtered list must be ONE shared input:
- `filter_in_window(segment, time_window, pitch_window) -> Vec<SurvivingPoint>` — private
  helper; the single source of the post-filter point list (each carries raw + `nx`/`ny`).
- `project_pitch_segment(&[SurvivingPoint]) -> Vec<PitchTracePoint>` — raw projection only.
- `project_band_segment(&[SurvivingPoint]) -> Vec<VibratoBandPoint>` — owns smoothing +
  back-dating (the `smooth`, `sample_center_at_x`, `vibrato_x`, half-height code moves here
  unchanged). **Correction #3:** signature is series-in / series-out (whole `Vec`), because
  the back-date interpolation is a whole-series transform, not a per-point map — this is
  also the shape Step B's chain must compose.

Both consume the SAME filtered list, so their index spaces cannot drift.

**Correction #4 — drop the dead field.** The render path does NOT read `vibrato_strength`
(alpha = confidence, height = depth). `VibratoBandPoint` must NOT carry it; don't port a
field nothing reads.

**Correction #5 — move the x-fallback pair together.** `apply_mesh_band` currently does
`vibrato_x.unwrap_or(point.x)` (systems.rs ~753) and the model does `None =>
smoothed_center[i]`. These are a matched pair. Both move into `project_band_segment` so
`VibratoBandPoint.x` and `.center_y` are ALREADY resolved (fallback applied). The render
system reads a final `x` — it must not re-derive the fallback.

## Resource shape  (Correction #1)
Keep ONE `TimeGraphLiveSceneRes` with parallel sub-fields `pitch_segments` /
`band_segments`. Do NOT split into two resources: pitch and band scroll at the SAME cadence
(both normalized against the rolling time window, both written unconditionally by the same
glue), so separate resources gain no independent change-detection — and "split by feature"
is exactly what the scene.rs cadence doctrine forbids ("split by repaint cadence, not
feature type"). A future per-series toggle lives at the render layer (skip the system /
clear the mesh), not in a resource split.

## Invariant: identical pixels
This is a refactor. The band geometry math (PDC back-date, center sampling, half-height
overshoot, confidence alpha) is MOVED, not changed — and the shared-filter rule above is
what KEEPS it identical. Verification: a replay screenshot of `highlighter_test.wav` at the
ramp (~t=9.5s) must look the same as before the refactor.

## Out of scope (Step B, deferred — do NOT do now)
- The transform chain (smooth/back-date/flatten-rails as named composable steps).
- Runtime toggles for series or transforms.
- The scalloping / flat-rail highlighter fix.
- Breath as its own series (named only to prove the shape generalizes).

## Files that change
- `apps/coach-game/src/widgets/time_graph/scene.rs` — new `PitchTracePoint` /
  `VibratoBandPoint`; add `band_segments` to `TimeGraphLiveSceneRes`; retire
  `NormalizedTracePoint`'s band fields (or replace the struct).
- `apps/coach-game/src/widgets/time_graph/model.rs` — split projection into
  pitch + band; emit both series.
- `apps/coach-game/src/widgets/time_graph/systems.rs` — `apply_mesh_trace` reads
  `pitch_segments`; `apply_mesh_band` reads `band_segments`.
- The glue that distributes the projected scene into the live resource
  (`game/time_graph.rs` per the scene.rs comment) — populate both series.
- Tests referencing `NormalizedTracePoint` band fields — update to the new shapes.

## Test plan
- Existing model tests adapted to the split functions (pitch projection vs band
  projection), including the ramp regression `band_center_y_back_dated_on_rising_ramp`
  (now a band-projection test).
- `cargo fmt` / `cargo clippy --workspace -D warnings` / `cargo test -p coach-game --release`.
- Replay screenshot diff at the ramp — visually unchanged.
