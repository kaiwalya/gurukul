# Plan: Band Step B — bind the band's coordinate transforms into a chain

## Why this exists (the spine)
We already hit the bug this plan is meant to make impossible. For PDC alignment we
back-dated the band's **time** (x) by ~0.80 s — and forgot to back-date its
**centre pitch** (y) to that same moment. On a pitch ramp the band's x sat 0.80 s
in the past while its y reflected the present pitch, so the band detached from the
trace. We fixed it (`sample_center_at_x`), but the lesson is the design's reason to
exist:

> **A band step that moves x MUST move y in the same step.** When the band's x is
> back-dated, its centre y must be sampled at that same back-dated moment. x and y
> are ONE inseparable operation — never two edits in two places.

So the **primary design criterion** is **coupling-safety**, not "maximize
toggleable steps." Structure the band projection so the final `x` and the final
`center_y` are written *together, exactly once, by the same step* — making it
structurally impossible for a future edit to move time without moving pitch. The
named/ordered/toggleable chain is the *means*; coupling-safety is the *end*.
Toggleability is secondary.

This is a **pure refactor — identical pixels.** Behaviour is unchanged; only the
shape changes so the bug can't recur.

## The rule that drives the shape: y is written exactly once
The subtle failure mode to avoid: smoothing the centre into the series early, then
*overwriting* it during back-date. That double-writes `center_y` — the early write
looks meaningful but is dead, and a future "fix" to y could land in the wrong
place, silently. That is the x-without-matching-y trap, just relocated.

**Therefore: the smoothed centerline is an INTERMEDIATE, never a written
`center_y`.** It is computed and carried as context; the *only* step that writes
`center_y` into the series is the back-date step, which writes `x` and `center_y`
as one pair. No step writes a `center_y` that a later step overwrites.

## What a "step" is (the lens)
A band step is a **unit of coupled coordinate transformation on the band series**:
coordinates that must move as one are emitted together by one step, never exposed
to a half-edit. That definition — not toggleability — justifies the chain. The
back-date is the canonical example, and it alone earns the abstraction (so the
"is the chain empty?" worry dissolves: the back-date is the real, meaningful step
— the one we forgot half of).

## The shape
Each step reads the shared survivor context (the Step-A drift-safe
`&[SurvivingPoint]`, read-only) plus the band series so far, and returns the next
band series. Survivors are context, not the flowing value, so a step that needs raw
survivor data is first-class, not trapped in a "seed". The smoothed centerline that
the back-date samples rides in the context (computed once, up front), never in the
series.

```rust
/// Read-only context every band step sees: the Step-A drift-safe survivor list
/// plus precomputed intermediates. The smoothed centerline lives HERE, not as a
/// written center_y in the series — so only the back-date step ever writes the
/// final center_y, paired with the final x.
struct BandCtx<'a> {
    survivors: &'a [SurvivingPoint],
    smoothed_center: &'a [f32], // intermediate; the back-date step SAMPLES this
    point_xs: &'a [f32],        // survivors' nx, the interpolation axis
}

/// One named band transform: a unit of COUPLED coordinate change. Reads context
/// + the series so far; returns the next series. Coordinates that must move
/// together (x and center_y under back-date) are emitted together by one step.
struct BandStep {
    name: &'static str,
    enabled: bool,
    apply: fn(ctx: &BandCtx, series: Vec<VibratoBandPoint>) -> Vec<VibratoBandPoint>,
}

fn band_chain() -> [BandStep; N] { /* ordered list below */ }

fn project_band_segment(survivors: &[SurvivingPoint], _pw: PitchWindow) -> Vec<VibratoBandPoint> {
    let smoothed_center = smooth(&survivors.iter().map(|s| s.ny).collect::<Vec<_>>(),
                                 BAND_CENTER_SMOOTH_WINDOW);
    let point_xs: Vec<f32> = survivors.iter().map(|s| s.nx).collect();
    let ctx = BandCtx { survivors, smoothed_center: &smoothed_center, point_xs: &point_xs };

    // Seed: x = nx, half_height = 0, center_y = 0 (UNSET — the back-date step is
    // the sole writer of the final center_y), confidence passthrough.
    let mut series = seed_band_series(survivors);
    for step in band_chain() {
        if step.enabled {
            series = (step.apply)(&ctx, series);
        }
    }
    series
}
```

- **`fn` pointer, not a trait.** No per-step state; each step is a pure, testable
  function; `name` makes a step greppable.
- **Drift invariant preserved.** Every step reads the same `survivors` /
  `point_xs` from `filter_in_window`. No re-filtering → index spaces can't drift
  (Step-A trap stays closed).
- **The centerline as context, not series,** is what enforces single-write y: a
  step physically cannot write `center_y` early because the smoothed values aren't
  in the series for it to write.

## The ordered chain (existing math, repartitioned)
| # | Step | Reads | Writes into series |
|---|------|-------|--------------------|
| 1 | `half_height_derive` | survivor `raw_half_height` (`smooth`, window 5) | `half_height` |
| 2 | `pdc_align` ⭐ | ctx `smoothed_center`, `point_xs`, survivor `vibrato_nx` | **`x` and `center_y` as ONE pair** (x-fallback resolved here) |

The centre-smooth (window 9) is no longer a chain step that writes the series — it
is a one-shot intermediate computed into `BandCtx`. The chain shrinks to two steps,
and that's the point: the coupled coordinate step (`pdc_align`) is the spine, and y
is written exactly once.

`half_height` (step 1) is independent of `x`/`y`, so it's safe as its own step. It
could run before or after `pdc_align`; keep it first for readability.

### `pdc_align` — the coordinate-binding step
It writes `x` and `center_y` from a single tuple per point — one `match`, both
coordinates:

```
// per point i:
match survivors[i].vibrato_nx {
    Some(vx) => (vx, sample_center_at_x(ctx.point_xs, ctx.smoothed_center, vx)),
    None     => (survivors[i].nx, ctx.smoothed_center[i]),   // x-fallback, centre at own time
}
```

There is no code path where `x` moves and `center_y` doesn't — they're one tuple.
To reintroduce the original bug you'd have to delete the y half of a tuple you're
already editing: visibly wrong, not a silent omission in another function. This is
exactly today's math, so the output series is **bit-for-bit identical** for any
input.

## Test-first plan
Pixels are identical → headline is an **equivalence** test (write FIRST against a
snapshot of today's output), backed by the **coupling** test that pins the spine.

### New test 1 — `band_chain_matches_legacy_projection` (equivalence)
Capture today's output BEFORE refactoring on a representative graph: a multi-point
segment (≥ 20 points) with a centre wobble, varying `vibrato_depth`, and back-dated
`vibrato_t_ms` (some points before the window start, to hit the x-fallback path).
Record `band_segments[0]` as expected literals. After refactoring, assert the
chained projection reproduces it within `1e-6` per field.

### Coupling guard — `band_center_y_back_dated_on_rising_ramp` (existing, stays green)
This existing test already guards the coupling: on a rising ramp it asserts
`center_y` tracks the BACK-DATED (lower) pitch, not the present pitch — i.e.
`center_y` equals the smoothed centerline sampled at the band's OWN back-dated `x`.
After the refactor it must stay green **because the coupling is now structural**
(`pdc_align` writes the pair). Note this in the test comment: it is the regression
guard for the original x-without-y bug, now backstopped by the single-write shape.
- **Bite check:** if `pdc_align` is mutated to move `x` but leave `center_y` at the
  unshifted `smoothed_center[i]`, this test must fail.

### Existing band tests — stay GREEN, assertions UNCHANGED
| Test | Status |
|------|--------|
| `band_half_height_known_depth_projects_correctly` | unchanged |
| `band_half_height_zero_for_zero_depth_point` | unchanged |
| `band_half_height_nonzero_when_depth_nonzero_but_strength_zero` | unchanged |
| `band_center_y_tracks_mean_not_instantaneous_pitch` | unchanged |
| `band_center_y_back_dated_on_rising_ramp` | unchanged (now also the coupling guard) |
| `band_center_samples_correct_pitch_when_leading_points_dropped` (drift guard) | unchanged — reconstructs expected centre from the shared survivor curve, preserved by the refactor |

If any existing test needs an assertion change, the refactor is not pure — that's a
bug in the refactor, not a reason to edit the test.

Run `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
`cargo test -p coach-game --release` clean.

## Files that change
| File | Change |
|------|--------|
| `apps/coach-game/src/widgets/time_graph/model.rs` | Add `BandCtx`, `BandStep`, `band_chain()`, `seed_band_series`; split `project_band_segment` so the smoothed centerline is a one-shot intermediate in `BandCtx` and `pdc_align` is the sole writer of `(x, center_y)`; add the equivalence test; annotate the ramp test as the coupling guard. |
| *(none else)* | `scene.rs`, `systems.rs`, the glue, `VibratoBandPoint`, `filter_in_window` — untouched. |

## Out of scope
- **The scalloping / flat-rail fix — deferred, and it does NOT belong in the head.**
  The flat-rail shape ("steady rails over a held vibrato") is a property of the
  DATA: if the rails should be steady, that's the vibrato engine node emitting a
  steady depth envelope, so the eventual fix lives **upstream in `dsp/node-vibrato`**,
  not in `model.rs`. The band renderer faithfully renders whatever depth the engine
  hands it. Do NOT design a head-side flattening transform here.
- **Runtime UI toggles** (`enabled` is compile-time only).
- **Breath as its own series** (named in Step A only to prove the shape generalizes).
- Any change to PDC back-date *math*, the shared `filter_in_window`, `scene.rs`, or
  `systems.rs`.
