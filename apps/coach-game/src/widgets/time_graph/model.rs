//! Time-graph model: the pure domain → geometry projection.
//!
//! The only music-aware layer of the slice. Takes a [`SemanticGraph`]
//! (semantic pitch/time facts) and projects it into lane-local
//! normalized coordinates in `[0, 1]`. Plain Rust, no Bevy. After this
//! runs, music has been spent.

use crate::semantic_graph::{
    BreathSpan, GrooveLine, OnsetTick, PitchWindow, SemanticGraph, TimeWindow, TraceSegment,
};
use domain_ports::pitch::PitchLog2;

// ---------------------------------------------------------------------------
// Vibrato-strength thresholds — used by the vibrato_strength scalar function,
// which is test-only (the render path expresses rate/depth/confidence as
// independent visual channels and does not call vibrato_strength).
// ---------------------------------------------------------------------------

/// Depth below which we treat the pitch wobble as noise, not vibrato
/// (cents — the `vibrato_depth` feature is emitted in cents by `node-vibrato`,
/// which builds the contour as `1200 × log2(f)`). Typical sung vibrato is
/// ~20–50 cents peak-to-peak (≈ 0.2–0.5 st), so a 20-cent floor gives margin
/// against gentle ornamentation and pitch jitter.
#[cfg(test)]
const VIBRATO_DEPTH_FLOOR_CENTS: f32 = 20.0;

/// Depth at which the gate reaches 1.0 (cents). A ramp from 20 to 50 covers the
/// typical vibrato range; depth above 50 cents is unambiguously intentional
/// vibrato.
#[cfg(test)]
const VIBRATO_DEPTH_FULL_CENTS: f32 = 50.0;

/// Lower edge of the musical vibrato rate band (Hz). Below ~4 Hz the wobble
/// is too slow to be perceived as vibrato (more like a slow wavering).
#[cfg(test)]
const VIBRATO_RATE_LOW_ZERO: f32 = 3.5;

/// Rate at which the band reaches full weight on the low side (Hz).
#[cfg(test)]
const VIBRATO_RATE_LOW_FULL: f32 = 4.5;

/// Rate at which the band begins fading on the high side (Hz). Classical
/// vibrato rarely exceeds 7 Hz; anything faster starts to sound strained.
#[cfg(test)]
const VIBRATO_RATE_HIGH_FULL: f32 = 6.5;

/// Rate at which the band fades to zero on the high side (Hz).
#[cfg(test)]
const VIBRATO_RATE_HIGH_ZERO: f32 = 7.5;

/// Number of points in the symmetric moving-average window used to smooth the
/// per-point band half-heights. A 5-point window spans ~250 ms at 20 Hz and
/// kills single-hop spikes while tracking real depth changes quickly enough.
/// Chosen over a one-pole IIR because it has zero warm-up latency for the
/// initial points and needs no per-segment state.
const BAND_SMOOTH_WINDOW: usize = 5;

/// Number of points in the symmetric moving-average window used to smooth
/// the per-point band centre (mean pitch). Must span at least one full
/// vibrato cycle, or the "centerline" still wiggles with the pitch and the
/// band snakes along the trace instead of bracketing it. Trace points arrive
/// at ~13 ms/point, so a 5 Hz vibrato cycle (200 ms) is ~15 points and a slow
/// 2 Hz cycle (500 ms) is ~38 points. A 31-point window clears two full 5 Hz
/// cycles and most of a 2 Hz cycle, keeping the ribbon centre steady.
const BAND_CENTER_SMOOTH_WINDOW: usize = 31;

use super::scene::{
    NormalizedBreathSpan, NormalizedGrooveLine, NormalizedOnsetTick, PitchTracePoint,
    TimeGraphScene, VibratoBandPoint,
};

pub fn project_scene(graph: &SemanticGraph) -> TimeGraphScene {
    let Some(time_window) = graph.time_window else {
        return TimeGraphScene::default();
    };

    let onset_ticks = graph
        .onset_ticks
        .iter()
        .filter_map(|tick| normalize_onset_tick(*tick, time_window))
        .collect();
    let breath_spans = graph
        .breath_spans
        .iter()
        .filter_map(|span| normalize_breath_span(*span, time_window))
        .collect();
    let grooves = graph
        .pitch_window
        .map(|pitch_window| {
            graph
                .grooves
                .iter()
                .filter_map(|groove| normalize_groove(*groove, pitch_window))
                .collect()
        })
        .unwrap_or_default();
    let (pitch_segments, band_segments) = graph
        .pitch_window
        .map(|pitch_window| {
            let mut pitch_segs = Vec::new();
            let mut band_segs = Vec::new();
            for segment in &graph.trace_segments {
                let survivors = filter_in_window(segment, time_window, pitch_window);
                if survivors.is_empty() {
                    continue;
                }
                pitch_segs.push(project_pitch_segment(&survivors));
                band_segs.push(project_band_segment(&survivors, pitch_window));
            }
            (pitch_segs, band_segs)
        })
        .unwrap_or_default();

    TimeGraphScene {
        pitch_segments,
        band_segments,
        grooves,
        onset_ticks,
        breath_spans,
    }
}

/// Symmetric moving-average smoother. For each index `i` the output is the
/// mean of the window `[i - radius, i + radius]` clamped to the slice
/// bounds (edge points see a smaller window rather than zero-padding, so
/// they are not biased toward zero). `window` must be ≥ 1; a window of 1
/// is a no-op (returns input unchanged). Panics if `window` is 0.
fn smooth(values: &[f32], window: usize) -> Vec<f32> {
    let half_w = window / 2;
    let n = values.len();
    (0..n)
        .map(|i| {
            let lo = i.saturating_sub(half_w);
            let hi = (i + half_w + 1).min(n);
            let sum: f32 = values[lo..hi].iter().sum();
            sum / (hi - lo) as f32
        })
        .collect()
}

/// Interpolate `values` (indexed by point, on the `xs` time axis) at an
/// arbitrary query position `query_x`.
///
/// WHY: the vibrato band's x is back-dated to `vibrato_t_ms` so the band
/// slides left to align with the pitch trace. The center-y must share the
/// same back-dated time — otherwise on a fast pitch ramp the band x is 0.8 s
/// in the past while the center-y reflects the present pitch. This helper
/// converts `vibrato_x` (normalized time of `vibrato_t_ms`) back to a
/// fractional index into `smoothed_center` so both coordinates reference the
/// same moment in the audio.
///
/// `xs` must be monotonically non-decreasing (guaranteed by construction: the
/// points are ordered by `t_ms` and `normalize_time` is monotone). If
/// `query_x` is outside `[xs[0], xs[last]]` the nearest endpoint value is
/// returned (clamp, no extrapolation).
fn sample_center_at_x(xs: &[f32], values: &[f32], query_x: f32) -> f32 {
    debug_assert_eq!(xs.len(), values.len());
    let n = xs.len();
    if n == 0 {
        return 0.0;
    }
    if n == 1 || query_x <= xs[0] {
        return values[0];
    }
    if query_x >= xs[n - 1] {
        return values[n - 1];
    }
    // Binary search for the segment [lo, lo+1] that straddles query_x.
    let lo = xs.partition_point(|&x| x <= query_x).saturating_sub(1);
    let hi = (lo + 1).min(n - 1);
    let x_lo = xs[lo];
    let x_hi = xs[hi];
    let span = x_hi - x_lo;
    if span <= 0.0 {
        return values[lo];
    }
    let t = (query_x - x_lo) / span;
    values[lo] + t * (values[hi] - values[lo])
}

/// A raw trace point that survived both the time and pitch window filters.
/// Carries the original point data plus its pre-computed normalized coordinates.
/// The shared post-filter list is the single source of truth for both
/// `project_pitch_segment` and `project_band_segment` — they must consume the
/// same `Vec<SurvivingPoint>` so their index spaces cannot drift.
struct SurvivingPoint {
    /// Normalized x in `[0, 1]` (time).
    nx: f32,
    /// Normalized y in `[0, 1]` (pitch).
    ny: f32,
    /// Normalized x for the back-dated vibrato band position, or `None` when
    /// `vibrato_t_ms` falls outside the visible window.
    vibrato_nx: Option<f32>,
    /// Raw (unsmoothed) band half-height in normalized-y units.
    raw_half_height: f32,
    /// Pitch detection confidence, passed through to both series.
    confidence: f32,
}

/// Build the shared post-filter point list: points that survive both the time
/// window and pitch window filters. Both projections consume this list so their
/// index spaces are identical — independent re-filtering would cause the band's
/// back-date interpolation to sample the wrong pitch when leading points are
/// dropped.
fn filter_in_window(
    segment: &TraceSegment,
    time_window: TimeWindow,
    pitch_window: PitchWindow,
) -> Vec<SurvivingPoint> {
    let octave_span = pitch_window.max.0 - pitch_window.min.0;
    segment
        .points
        .iter()
        .filter_map(|point| {
            let nx = normalize_time(point.t_ms, time_window)?;
            let ny = normalize_pitch(point.pitch, pitch_window)?;
            // Half-height = pure normalized depth: the band rails wrap the actual
            // peak-to-peak swing of the trace. Rate is already visible in the
            // wiggle itself; confidence drives opacity. Strength is NOT applied
            // here — it would double-count information already expressed through
            // the other visual channels.
            let raw_half_height = if octave_span > 0.0 {
                (point.vibrato_depth / 1200.0) / octave_span
            } else {
                0.0
            };
            // Vibrato band x uses the back-dated timestamp so the band slides
            // forward ~0.80s to align with the pitch trace. `None` when the
            // vibrato timestamp falls outside the window (band point hidden).
            let vibrato_nx = normalize_time(point.vibrato_t_ms, time_window);
            Some(SurvivingPoint {
                nx,
                ny,
                vibrato_nx,
                raw_half_height,
                confidence: point.confidence,
            })
        })
        .collect()
}

/// Project the shared filtered point list into the pitch-trace series.
/// Pure mapping: no smoothing, no band logic.
fn project_pitch_segment(survivors: &[SurvivingPoint]) -> Vec<PitchTracePoint> {
    survivors
        .iter()
        .map(|sp| PitchTracePoint {
            x: sp.nx,
            y: sp.ny,
            confidence: sp.confidence,
        })
        .collect()
}

/// Read-only context every band step sees.
/// `smoothed_center` is an INTERMEDIATE — only `pdc_align` writes it into
/// the series, paired with the final `x`, so `center_y` is written exactly once.
struct BandCtx<'a> {
    survivors: &'a [SurvivingPoint],
    smoothed_center: &'a [f32],
    point_xs: &'a [f32],
}

/// One named band transform. Reads context + the series so far; returns the
/// next series. Coordinates that must move together (x and center_y) are
/// emitted together by one step.
struct BandStep {
    /// Human-readable label for debugging / future tracing.
    #[allow(dead_code)]
    name: &'static str,
    enabled: bool,
    apply: fn(ctx: &BandCtx, series: Vec<VibratoBandPoint>) -> Vec<VibratoBandPoint>,
}

/// Seed the band series: x = survivor nx, half_height = 0.0, center_y = 0.0
/// (UNSET — pdc_align is the sole writer), confidence = survivor confidence.
fn seed_band_series(survivors: &[SurvivingPoint]) -> Vec<VibratoBandPoint> {
    survivors
        .iter()
        .map(|sp| VibratoBandPoint {
            x: sp.nx,
            center_y: 0.0,
            half_height: 0.0,
            confidence: sp.confidence,
        })
        .collect()
}

/// Step 1: smooth raw half-heights (window 5) and write them into the series.
fn half_height_derive(ctx: &BandCtx, mut series: Vec<VibratoBandPoint>) -> Vec<VibratoBandPoint> {
    let raw: Vec<f32> = ctx.survivors.iter().map(|sp| sp.raw_half_height).collect();
    let smoothed = smooth(&raw, BAND_SMOOTH_WINDOW);
    for (pt, hh) in series.iter_mut().zip(smoothed) {
        pt.half_height = hh;
    }
    series
}

/// Step 2 ⭐ — the coordinate-binding step.
/// Writes `x` AND `center_y` as one pair per point; these two coordinates
/// are inseparable (x is back-dated; center_y must be sampled at that same
/// back-dated moment). This is the sole writer of both fields.
fn pdc_align(ctx: &BandCtx, mut series: Vec<VibratoBandPoint>) -> Vec<VibratoBandPoint> {
    for (i, pt) in series.iter_mut().enumerate() {
        let (x, center_y) = match ctx.survivors[i].vibrato_nx {
            Some(vx) => (
                vx,
                sample_center_at_x(ctx.point_xs, ctx.smoothed_center, vx),
            ),
            None => (ctx.survivors[i].nx, ctx.smoothed_center[i]),
        };
        pt.x = x;
        pt.center_y = center_y;
    }
    series
}

fn band_chain() -> [BandStep; 2] {
    [
        BandStep {
            name: "half_height_derive",
            enabled: true,
            apply: half_height_derive,
        },
        BandStep {
            name: "pdc_align",
            enabled: true,
            apply: pdc_align,
        },
    ]
}

/// Project the shared filtered point list into the vibrato-band series.
/// Owns the band transform: smoothing, back-date interpolation, x-fallback.
///
/// The transform is expressed as a named chain so each step's responsibility
/// is explicit. The coupling-safety invariant: `x` and `center_y` are written
/// together, exactly once, by the `pdc_align` step. `smoothed_center` is an
/// intermediate that lives only in `BandCtx`; it never enters the series early.
///
/// Correction #5: the x-fallback (`vibrato_nx.unwrap_or(nx)`) is resolved
/// inside `pdc_align` so `VibratoBandPoint.x` is final. The render system reads
/// a plain `x` — it must not re-derive the fallback.
fn project_band_segment(
    survivors: &[SurvivingPoint],
    pitch_window: PitchWindow,
) -> Vec<VibratoBandPoint> {
    let _ = pitch_window; // octave_span already spent in filter_in_window

    let smoothed_center: Vec<f32> = {
        let raw_ys: Vec<f32> = survivors.iter().map(|sp| sp.ny).collect();
        smooth(&raw_ys, BAND_CENTER_SMOOTH_WINDOW)
    };
    let point_xs: Vec<f32> = survivors.iter().map(|sp| sp.nx).collect();

    let ctx = BandCtx {
        survivors,
        smoothed_center: &smoothed_center,
        point_xs: &point_xs,
    };

    let mut series = seed_band_series(survivors);
    for step in band_chain() {
        if step.enabled {
            series = (step.apply)(&ctx, series);
        }
    }
    series
}

fn normalize_groove(groove: GrooveLine, pitch_window: PitchWindow) -> Option<NormalizedGrooveLine> {
    Some(NormalizedGrooveLine {
        y: normalize_pitch(groove.pitch, pitch_window)?,
        slot: groove.slot,
        active: groove.active,
    })
}

fn normalize_onset_tick(tick: OnsetTick, time_window: TimeWindow) -> Option<NormalizedOnsetTick> {
    Some(NormalizedOnsetTick {
        x: normalize_time(tick.t_ms, time_window)?,
        strength: tick.strength,
    })
}

fn normalize_breath_span(
    span: BreathSpan,
    time_window: TimeWindow,
) -> Option<NormalizedBreathSpan> {
    // A span covers an interval, so a span straddling the window edge is
    // clipped to the edge (it occupies visible time) — unlike point-like
    // features, which drop. Drop only a span that lies *entirely* outside
    // the window: no overlap with [start, end] means nothing to show.
    if span.end_ms < time_window.start_ms || span.start_ms > time_window.end_ms {
        return None;
    }
    Some(NormalizedBreathSpan {
        x0: clamp_time(span.start_ms, time_window)?,
        x1: clamp_time(span.end_ms, time_window)?,
        peak: span.peak,
    })
}

/// Map a time onto the window as a `[0, 1]` fraction, **dropping** points
/// outside the window rather than clamping them to the edge. The in/out
/// decision is the domain question "is this instant within the visible time
/// window?" — answered in milliseconds, on the domain side, before any
/// pixels exist (see `ARCHITECTURE.md`, "a domain decision is made in
/// domain units"). Clamping instead piled out-of-window points on the lane
/// edge, and the `windows(2)` trace painter then drew spurious segments to
/// the pile — the on-screen "lines everywhere" defect. `None` means *not
/// shown*: either a degenerate (zero-span) window or a point outside it.
fn normalize_time(t_ms: u64, window: TimeWindow) -> Option<f32> {
    // Divide by the *fixed* retention span, not `end_ms - start_ms`. Early in
    // a session (or after silence) the buffer holds less than `span_ms` of
    // data, so `end_ms - start_ms` is smaller than the window and the few
    // seconds present get stretched across the full width — a "zoom-out" that
    // relaxes only once the buffer fills. Anchoring on `span_ms` keeps the
    // pixels-per-second constant from the first frame: "now" sits at x = 1.0
    // and older data marches left at a fixed rate.
    let span = window.span_ms;
    if span == 0 || t_ms < window.start_ms || t_ms > window.end_ms {
        return None;
    }
    // Anchor "now" (`end_ms`) at x = 1.0 and measure age backwards from it, so
    // the live edge is pinned to the right from the very first frame and older
    // samples sit at a fixed fraction of `span_ms` to its left.
    let age_ms = window.end_ms - t_ms;
    Some(1.0 - age_ms as f32 / span as f32)
}

/// Map a pitch onto the window as a `[0, 1]` fraction, **dropping** pitches
/// outside the window rather than clamping. Same rule as [`normalize_time`]:
/// the keep/drop decision is made in `PitchLog2`, not in normalized space.
fn normalize_pitch(pitch: PitchLog2, window: PitchWindow) -> Option<f32> {
    let span = window.max.0 - window.min.0;
    if span <= 0.0 || pitch.0 < window.min.0 || pitch.0 > window.max.0 {
        return None;
    }
    Some((pitch.0 - window.min.0) / span)
}

/// Compute a [0, 1] vibrato-tint signal from the raw analyzer outputs.
///
/// Three gates are multiplied together:
/// - `depth_gate`: ramps 0 → 1 between [`VIBRATO_DEPTH_FLOOR_CENTS`] and
///   [`VIBRATO_DEPTH_FULL_CENTS`] (cents). Below the floor the signal is
///   just noise.
/// - `rate_band`: 1 inside the musical vibrato band (~4.5–6.5 Hz), ramping
///   to 0 outside it. Prevents slow waver or fast flutter from tinting.
/// - `confidence`: a low-confidence detection cannot produce a visible tint.
///
/// Non-finite inputs (NaN / ±inf) are treated as 0 so strength is always a
/// clean [0, 1] value. Intentionally instantaneous — no temporal windowing
/// (that would be Stage-2 interpretation, explicitly deferred).
///
/// Not used by production projection code (the render path expresses strength
/// via the three visual channels independently); kept for the scalar unit tests
/// that guard the gate and rate-band math.
#[cfg(test)]
fn vibrato_strength(rate_hz: f32, depth_cents: f32, confidence: f32) -> f32 {
    let depth_gate = ((depth_cents - VIBRATO_DEPTH_FLOOR_CENTS)
        / (VIBRATO_DEPTH_FULL_CENTS - VIBRATO_DEPTH_FLOOR_CENTS))
        .clamp(0.0, 1.0);

    let rate_band = if !(VIBRATO_RATE_LOW_ZERO..=VIBRATO_RATE_HIGH_ZERO).contains(&rate_hz) {
        0.0
    } else if rate_hz < VIBRATO_RATE_LOW_FULL {
        (rate_hz - VIBRATO_RATE_LOW_ZERO) / (VIBRATO_RATE_LOW_FULL - VIBRATO_RATE_LOW_ZERO)
    } else if rate_hz > VIBRATO_RATE_HIGH_FULL {
        (VIBRATO_RATE_HIGH_ZERO - rate_hz) / (VIBRATO_RATE_HIGH_ZERO - VIBRATO_RATE_HIGH_FULL)
    } else {
        1.0
    };

    let v = (depth_gate * rate_band * confidence).clamp(0.0, 1.0);
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// Map a time onto the window as a fraction, **clamping** to `[0, 1]`
/// instead of dropping. This is the right policy *only* for a span that
/// covers an interval of time (a breath span): if it straddles the window
/// edge it genuinely occupies visible time and should be clipped to the
/// edge, not dropped. Point-like features (trace points, onset ticks) use
/// [`normalize_time`] and drop instead. `None` only for a degenerate
/// window. See [`normalize_breath_span`] for the entirely-outside case.
fn clamp_time(t_ms: u64, window: TimeWindow) -> Option<f32> {
    let span = window.span_ms;
    if span == 0 {
        return None;
    }
    // Same fixed-span, now-pinned-right basis as `normalize_time`; clamps
    // instead of dropping so a span straddling the left edge is clipped to it.
    let age_ms = window.end_ms.saturating_sub(t_ms);
    Some((1.0 - age_ms as f32 / span as f32).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_graph::{
        BreathSpan, GrooveLine, OnsetTick, PitchWindow, SemanticGraph, TimeWindow, TracePoint,
        TraceSegment,
    };

    #[test]
    fn project_scene_normalizes_times_pitches_and_events() {
        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 10,
                end_ms: 110,
                span_ms: 100,
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(8.0),
                max: PitchLog2(10.0),
            }),
            trace_segments: vec![TraceSegment {
                points: vec![
                    TracePoint {
                        t_ms: 10,
                        vibrato_t_ms: 10,
                        pitch: PitchLog2(8.0),
                        confidence: 0.2,
                        vibrato_rate: 0.0,
                        vibrato_depth: 0.0,
                    },
                    TracePoint {
                        t_ms: 60,
                        vibrato_t_ms: 60,
                        pitch: PitchLog2(9.0),
                        confidence: 0.8,
                        vibrato_rate: 5.5,
                        vibrato_depth: 60.0,
                    },
                ],
            }],
            grooves: vec![GrooveLine {
                pitch: PitchLog2(9.0),
                slot: 3,
                active: true,
            }],
            onset_ticks: vec![OnsetTick {
                t_ms: 35,
                strength: 0.9,
            }],
            breath_spans: vec![BreathSpan {
                start_ms: 20,
                end_ms: 80,
                peak: 0.7,
            }],
        };

        let scene = project_scene(&graph);
        assert_eq!(scene.pitch_segments.len(), 1);
        assert_eq!(scene.grooves.len(), 1);
        assert_eq!(scene.onset_ticks.len(), 1);
        assert_eq!(scene.breath_spans.len(), 1);
        let point = &scene.pitch_segments[0][1];
        assert!((point.x - 0.5).abs() < 1e-5);
        assert!((point.y - 0.5).abs() < 1e-5);
        assert!((scene.grooves[0].y - 0.5).abs() < 1e-5);
        assert!((scene.onset_ticks[0].x - 0.25).abs() < 1e-5);
        assert!((scene.breath_spans[0].x0 - 0.10).abs() < 1e-5);
        assert!((scene.breath_spans[0].x1 - 0.70).abs() < 1e-5);
    }

    fn trace_point(t_ms: u64, pitch: PitchLog2) -> TracePoint {
        TracePoint {
            t_ms,
            vibrato_t_ms: t_ms,
            pitch,
            confidence: 0.8,
            vibrato_rate: 0.0,
            vibrato_depth: 0.0,
        }
    }

    /// A graph with both windows populated and one of every event, so each
    /// degenerate-window test below can knock out exactly one input.
    fn full_graph() -> SemanticGraph {
        SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 10,
                end_ms: 110,
                span_ms: 100,
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(8.0),
                max: PitchLog2(10.0),
            }),
            trace_segments: vec![TraceSegment {
                points: vec![TracePoint {
                    t_ms: 60,
                    vibrato_t_ms: 60,
                    pitch: PitchLog2(9.0),
                    confidence: 0.8,
                    vibrato_rate: 0.0,
                    vibrato_depth: 0.0,
                }],
            }],
            grooves: vec![GrooveLine {
                pitch: PitchLog2(9.0),
                slot: 3,
                active: true,
            }],
            onset_ticks: vec![OnsetTick {
                t_ms: 35,
                strength: 0.9,
            }],
            breath_spans: vec![BreathSpan {
                start_ms: 20,
                end_ms: 80,
                peak: 0.7,
            }],
        }
    }

    #[test]
    fn out_of_window_trace_points_are_dropped_not_clamped() {
        // The defect-4 guard. A segment with points straddling the window:
        // two before the start, two inside. The pre-window points must be
        // *dropped* — not clamped to x=0 — or the `windows(2)` painter draws
        // spurious segments from the piled-up edge points to the live ones.
        // We assert the *consequence on screen* (which points survive and
        // where), not merely that the code clamps, per the layer-1 rule that
        // a pure test must check the spec is right, not just self-consistent.
        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 100,
                end_ms: 200,
                span_ms: 100,
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(8.0),
                max: PitchLog2(10.0),
            }),
            trace_segments: vec![TraceSegment {
                points: vec![
                    trace_point(10, PitchLog2(9.0)),  // before window → drop
                    trace_point(50, PitchLog2(9.0)),  // before window → drop
                    trace_point(150, PitchLog2(9.0)), // inside → x = 0.5
                    trace_point(200, PitchLog2(9.0)), // at end → x = 1.0
                ],
            }],
            grooves: vec![],
            onset_ticks: vec![],
            breath_spans: vec![],
        };
        let scene = project_scene(&graph);
        assert_eq!(
            scene.pitch_segments.len(),
            1,
            "segment survives via its in-window tail"
        );
        let xs: Vec<f32> = scene.pitch_segments[0].iter().map(|p| p.x).collect();
        assert_eq!(xs.len(), 2, "only the two in-window points survive");
        assert!((xs[0] - 0.5).abs() < 1e-5, "150ms → 0.5, got {}", xs[0]);
        assert!((xs[1] - 1.0).abs() < 1e-5, "200ms → 1.0, got {}", xs[1]);
        // The bug's signature: NO point clamped to x = 0.0 (the dropped pile).
        assert!(
            xs.iter().all(|&x| x > 0.0),
            "no out-of-window point may survive clamped to the edge, got {xs:?}"
        );
    }

    #[test]
    fn breath_span_straddling_the_window_edge_is_clipped_not_dropped() {
        // The counterpart to the drop rule: a span covers an interval, so
        // one straddling the start is *clipped* to the edge (it occupies
        // visible time), unlike the point-like features which drop.
        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 100,
                end_ms: 200,
                span_ms: 100,
            }),
            breath_spans: vec![
                BreathSpan {
                    start_ms: 50, // before window
                    end_ms: 150,  // inside → clip to [0.0, 0.5]
                    peak: 0.7,
                },
                BreathSpan {
                    start_ms: 0, // entirely before the window → drop
                    end_ms: 80,
                    peak: 0.7,
                },
            ],
            ..full_graph()
        };
        let scene = project_scene(&graph);
        assert_eq!(scene.breath_spans.len(), 1, "the disjoint span is dropped");
        let span = scene.breath_spans[0];
        assert!(
            (span.x0 - 0.0).abs() < 1e-5,
            "start clipped to edge, got {}",
            span.x0
        );
        assert!(
            (span.x1 - 0.5).abs() < 1e-5,
            "end at 150ms → 0.5, got {}",
            span.x1
        );
    }

    #[test]
    fn no_time_window_yields_empty_scene() {
        // The whole projection is time-anchored: with no time window there
        // is no horizontal axis, so nothing renders — not even grooves,
        // which are vertical (the early return short-circuits before them).
        let graph = SemanticGraph {
            time_window: None,
            ..full_graph()
        };
        assert_eq!(project_scene(&graph), TimeGraphScene::default());
    }

    #[test]
    fn no_pitch_window_drops_grooves_and_segments_but_keeps_events() {
        // Grooves and pitch segments need a vertical (pitch) axis; onset
        // ticks and breath spans are time-only and survive without one.
        let graph = SemanticGraph {
            pitch_window: None,
            ..full_graph()
        };
        let scene = project_scene(&graph);
        assert!(scene.grooves.is_empty(), "no pitch axis → no grooves");
        assert!(
            scene.pitch_segments.is_empty(),
            "no pitch axis → no trace segments"
        );
        assert_eq!(scene.onset_ticks.len(), 1, "time-only events survive");
        assert_eq!(scene.breath_spans.len(), 1, "time-only events survive");
    }

    #[test]
    fn zero_span_time_window_drops_every_event() {
        // A collapsed time window (start == end) has no horizontal extent;
        // every per-event normalize_time returns None and filters out. The
        // time window is still `Some`, so we pass the early return and prove
        // the per-event guard, not the short-circuit.
        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 50,
                end_ms: 50,
                span_ms: 0,
            }),
            ..full_graph()
        };
        let scene = project_scene(&graph);
        assert!(scene.pitch_segments.is_empty());
        assert!(scene.onset_ticks.is_empty());
        assert!(scene.breath_spans.is_empty());
        // Grooves are pitch-only, unaffected by the collapsed time axis.
        assert_eq!(scene.grooves.len(), 1);
    }

    #[test]
    fn non_positive_pitch_span_drops_grooves_and_segments() {
        // An inverted or zero pitch window (max <= min) has no vertical
        // extent; normalize_pitch returns None for grooves and trace points.
        let graph = SemanticGraph {
            pitch_window: Some(PitchWindow {
                min: PitchLog2(9.0),
                max: PitchLog2(9.0),
            }),
            ..full_graph()
        };
        let scene = project_scene(&graph);
        assert!(scene.grooves.is_empty(), "zero pitch span → no grooves");
        assert!(
            scene.pitch_segments.is_empty(),
            "zero pitch span → no trace segments"
        );
        // Time-only events are unaffected.
        assert_eq!(scene.onset_ticks.len(), 1);
        assert_eq!(scene.breath_spans.len(), 1);
    }

    // --- vibrato_strength scalar tests ---
    //
    // Shorthand: FULL depth = 60 cents (above VIBRATO_DEPTH_FULL_CENTS = 50),
    // band-centre rate = 5.5 Hz, high confidence = 0.9.  Each group of
    // assertions uses these "high" values for the factors under test so exactly
    // one axis is varied at a time.

    // Interior: band-centre rate, depth above full-gate, high confidence.
    #[test]
    fn vibrato_strength_at_band_centre_with_good_depth_is_near_one() {
        let s = vibrato_strength(5.5, 60.0, 0.9);
        assert!(s > 0.85, "expected ~1, got {s}");
    }

    // Rate band — exact boundary values.
    #[test]
    fn vibrato_strength_rate_band_edges() {
        let depth = 60.0;
        let conf = 1.0;

        // At the hard zero edges the result must be exactly 0.
        assert_eq!(vibrato_strength(3.5, depth, conf), 0.0, "rate=3.5 → 0");
        assert_eq!(vibrato_strength(7.5, depth, conf), 0.0, "rate=7.5 → 0");

        // At the full-weight edges the result must be exactly 1 (conf=1, depth=full).
        assert!(
            (vibrato_strength(4.5, depth, conf) - 1.0).abs() < 1e-5,
            "rate=4.5 → 1"
        );
        assert!(
            (vibrato_strength(6.5, depth, conf) - 1.0).abs() < 1e-5,
            "rate=6.5 → 1"
        );

        // Midpoint of the low ramp: 4.0 Hz is halfway between 3.5 and 4.5 → ~0.5.
        let mid_low = vibrato_strength(4.0, depth, conf);
        assert!(
            (mid_low - 0.5).abs() < 0.02,
            "rate=4.0 (mid low-ramp) → ~0.5, got {mid_low}"
        );
    }

    // Depth gate — exact boundary values.
    #[test]
    fn vibrato_strength_depth_gate_edges() {
        let rate = 5.5;
        let conf = 1.0;

        // At or below the floor the gate is 0.
        assert_eq!(
            vibrato_strength(rate, VIBRATO_DEPTH_FLOOR_CENTS, conf),
            0.0,
            "depth=floor → 0"
        );

        // At or above the full threshold the gate is 1 → result equals conf.
        assert!(
            (vibrato_strength(rate, VIBRATO_DEPTH_FULL_CENTS, conf) - 1.0).abs() < 1e-5,
            "depth=full → 1"
        );

        // Midpoint of the depth ramp → ~0.5.
        let mid_depth = (VIBRATO_DEPTH_FLOOR_CENTS + VIBRATO_DEPTH_FULL_CENTS) * 0.5;
        let s = vibrato_strength(rate, mid_depth, conf);
        assert!((s - 0.5).abs() < 0.02, "depth midpoint → ~0.5, got {s}");
    }

    // Independent-zero gates: each factor alone drives strength to ~0.

    #[test]
    fn vibrato_strength_off_band_rate_is_zero() {
        // 9 Hz is strictly outside the band (> 7.5).
        assert_eq!(vibrato_strength(9.0, 60.0, 0.9), 0.0);
    }

    #[test]
    fn vibrato_strength_sub_floor_depth_is_zero() {
        // 5 cents is below the 20-cent floor.
        assert_eq!(vibrato_strength(5.5, 5.0, 0.9), 0.0);
    }

    #[test]
    fn vibrato_strength_near_zero_confidence_is_near_zero() {
        let s = vibrato_strength(5.5, 60.0, 0.02);
        assert!(s < 0.05, "near-zero confidence → ~0, got {s}");
    }

    // NaN guard: NaN in any input must not propagate to the output.
    // rate=NaN: `contains` returns false → rate_band=0 → product=0, already
    // finite; the guard is still exercised via depth/confidence paths.
    // depth=NaN and confidence=NaN both propagate NaN into the product;
    // the `is_finite` guard catches them and returns 0.
    #[test]
    fn vibrato_strength_nan_inputs_yield_zero() {
        assert_eq!(vibrato_strength(f32::NAN, 60.0, 0.9), 0.0);
        assert_eq!(vibrato_strength(5.5, f32::NAN, 0.9), 0.0);
        assert_eq!(vibrato_strength(5.5, 60.0, f32::NAN), 0.0);
    }

    /// A point with a known depth in cents should project to a predictable
    /// `band_half_height`. Arithmetic (by hand):
    ///   pitch_window span = 10.0 - 8.0 = 2.0 octaves
    ///   depth_cents = 120.0
    ///   raw_hh = (120.0 / 1200.0) / 2.0 = 0.1 / 2.0 = 0.05
    ///   strength is NOT applied to height (confidence drives opacity instead)
    ///   single-point segment → smoother window = 1 → smoothed = 0.05
    #[test]
    fn band_half_height_known_depth_projects_correctly() {
        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 0,
                end_ms: 100,
                span_ms: 100,
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(8.0),
                max: PitchLog2(10.0),
            }),
            trace_segments: vec![TraceSegment {
                points: vec![TracePoint {
                    t_ms: 50,
                    vibrato_t_ms: 50,
                    pitch: PitchLog2(9.0),
                    confidence: 1.0,
                    vibrato_rate: 5.5,    // band centre
                    vibrato_depth: 120.0, // well above full gate
                }],
            }],
            grooves: vec![],
            onset_ticks: vec![],
            breath_spans: vec![],
        };
        let scene = project_scene(&graph);
        let hh = scene.band_segments[0][0].half_height;
        assert!((hh - 0.05).abs() < 1e-4, "expected 0.05, got {hh}");
    }

    /// The band centre should be the smoothed mean of the raw pitch, not the
    /// instantaneous pitch. Feed a segment whose raw pitch alternates ±delta
    /// around 0.5 for enough points to fill the `BAND_CENTER_SMOOTH_WINDOW`.
    /// Interior points should have `band_center_y ≈ 0.5` within a tight
    /// tolerance (the symmetric window averages out the alternation exactly).
    #[test]
    fn band_center_y_tracks_mean_not_instantaneous_pitch() {
        // Alternate ±0.1 around 0.5 for 12 points — more than the 9-point
        // window so at least some interior points see a full window.
        let n_points = 12usize;
        let pitch_window_min = 8.0_f64;
        let pitch_window_max = 10.0_f64;
        let span = pitch_window_max - pitch_window_min; // 2.0

        // Convert 0.5 normalized ± delta_norm back to log2 Hz.
        // normalized y = (pitch - min) / span  →  pitch = y * span + min
        // We want y values alternating 0.5+delta_norm and 0.5-delta_norm.
        let delta_norm = 0.15_f64; // large enough to be clearly visible in assertion

        let points: Vec<_> = (0..n_points)
            .map(|i| {
                let sign = if i % 2 == 0 { 1.0_f64 } else { -1.0_f64 };
                let ny = 0.5 + sign * delta_norm;
                let pitch_log2 = ny * span + pitch_window_min;
                TracePoint {
                    t_ms: (i as u64) * 50, // 50 ms per point
                    vibrato_t_ms: (i as u64) * 50,
                    pitch: PitchLog2(pitch_log2 as f32),
                    confidence: 0.0, // vibrato_strength=0, so band is zero
                    vibrato_rate: 0.0,
                    vibrato_depth: 0.0,
                }
            })
            .collect();

        let time_end_ms = (n_points as u64 - 1) * 50;
        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 0,
                end_ms: time_end_ms,
                span_ms: time_end_ms,
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(pitch_window_min as f32),
                max: PitchLog2(pitch_window_max as f32),
            }),
            trace_segments: vec![TraceSegment { points }],
            grooves: vec![],
            onset_ticks: vec![],
            breath_spans: vec![],
        };

        let scene = project_scene(&graph);
        let bp_list = &scene.band_segments[0];

        // The window is 9 points (odd). With 12 input points, indices 4..=7
        // see a full symmetric 9-point window. An odd window over alternating
        // ±delta values has a worst-case 5:4 split, so the mean deviates from
        // 0.5 by at most delta/9 = 0.15/9 ≈ 0.0167. The key property is that
        // this is much tighter than the raw instantaneous deviation of ±0.15.
        // Allow ±0.02 (slightly above the theoretical max) for fp rounding.
        for (i, bp) in bp_list.iter().enumerate() {
            if i >= 4 && i <= 7 {
                assert!(
                    (bp.center_y - 0.5).abs() < 0.02,
                    "interior point {i}: center_y expected ~0.5, got {}",
                    bp.center_y
                );
            }
        }
    }

    /// A point with zero vibrato DEPTH must yield `band_half_height = 0`.
    /// (Band height is pure depth; strength is not a factor.)
    #[test]
    fn band_half_height_zero_for_zero_depth_point() {
        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 0,
                end_ms: 100,
                span_ms: 100,
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(8.0),
                max: PitchLog2(10.0),
            }),
            trace_segments: vec![TraceSegment {
                points: vec![TracePoint {
                    t_ms: 50,
                    vibrato_t_ms: 50,
                    pitch: PitchLog2(9.0),
                    confidence: 0.9,
                    vibrato_rate: 0.0,
                    vibrato_depth: 0.0, // zero depth → zero height
                }],
            }],
            grooves: vec![],
            onset_ticks: vec![],
            breath_spans: vec![],
        };
        let scene = project_scene(&graph);
        let hh = scene.band_segments[0][0].half_height;
        assert_eq!(hh, 0.0, "zero-depth point must have zero band_half_height");
    }

    /// A point with nonzero depth but zero vibrato strength (off-band rate)
    /// must yield a NONZERO `band_half_height` — proving strength no longer
    /// gates height.
    #[test]
    fn band_half_height_nonzero_when_depth_nonzero_but_strength_zero() {
        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 0,
                end_ms: 100,
                span_ms: 100,
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(8.0),
                max: PitchLog2(10.0),
            }),
            trace_segments: vec![TraceSegment {
                points: vec![TracePoint {
                    t_ms: 50,
                    vibrato_t_ms: 50,
                    pitch: PitchLog2(9.0),
                    confidence: 0.9,
                    vibrato_rate: 0.0,    // off-band rate → strength = 0
                    vibrato_depth: 120.0, // 120 cents → raw_hh = 0.05
                }],
            }],
            grooves: vec![],
            onset_ticks: vec![],
            breath_spans: vec![],
        };
        let scene = project_scene(&graph);
        let hh = scene.band_segments[0][0].half_height;
        assert!(
            (hh - 0.05).abs() < 1e-4,
            "depth-driven height must be nonzero even when strength=0, got {hh}"
        );
    }

    /// Regression guard for the PDC vertical-alignment bug: on a rising pitch
    /// ramp, `band_center_y` must track the BACK-DATED pitch (lower), not the
    /// current pitch (higher). Before the fix, band_center_y reflected the
    /// point's own time while band_x was back-dated, causing the band to float
    /// above the trace during ascent.
    ///
    /// Setup: a long linear pitch ramp from y=0.2 to y=0.8 (normalized).
    /// Each point's `vibrato_t_ms` is exactly 5 steps behind its own `t_ms`
    /// (i.e. the band x is back-dated by 5 hops = 250 ms at 50 ms/hop).
    ///
    /// The ramp must be long enough that interior points have a FULL symmetric
    /// centerline-smoothing window ([`BAND_CENTER_SMOOTH_WINDOW`]) on both
    /// sides — otherwise the average is dragged toward the segment mean by the
    /// edges and the back-date shift is washed out. On a pure linear ramp a
    /// symmetric average returns the center value unchanged, so the shift is
    /// purely the back-date:
    /// - The unshifted smoothed pitch ≈ current normalized y at i.
    /// - The back-dated smoothed pitch ≈ normalized y at i-5.
    /// - So band_center_y should be ~slope × 5 below point.y.
    ///
    /// We assert: band_center_y[i] < point.y[i] - 0.10 for well-interior points.
    #[test]
    fn band_center_y_back_dated_on_rising_ramp() {
        // Enough points that the test indices have a full smoothing window each
        // side: half-window (BAND_CENTER_SMOOTH_WINDOW/2) margin + the span we
        // probe. 60 points comfortably clears a 31-point window.
        let n_points = 60usize;
        let hop_ms = 50u64;
        let back_date_hops = 5usize; // vibrato_t_ms is 5 hops behind t_ms
        let back_date_ms = back_date_hops as u64 * hop_ms;

        let pitch_window_min = 8.0_f64;
        let pitch_window_max = 10.0_f64;
        let span = pitch_window_max - pitch_window_min; // 2.0

        // Linear ramp: normalized y goes from 0.2 to 0.8.
        let y_start = 0.2_f64;
        let y_end = 0.8_f64;
        let slope_per_hop = (y_end - y_start) / (n_points as f64 - 1.0);

        let points: Vec<_> = (0..n_points)
            .map(|i| {
                let ny = y_start + i as f64 * slope_per_hop;
                let pitch_log2 = ny * span + pitch_window_min;
                // vibrato_t_ms is back-dated; clamp to 0 so we don't go negative.
                let t_ms = i as u64 * hop_ms;
                let vibrato_t_ms = t_ms.saturating_sub(back_date_ms);
                TracePoint {
                    t_ms,
                    vibrato_t_ms,
                    pitch: PitchLog2(pitch_log2 as f32),
                    confidence: 0.0,
                    vibrato_rate: 0.0,
                    vibrato_depth: 0.0,
                }
            })
            .collect();

        let time_end_ms = (n_points as u64 - 1) * hop_ms;
        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 0,
                end_ms: time_end_ms,
                span_ms: time_end_ms,
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(pitch_window_min as f32),
                max: PitchLog2(pitch_window_max as f32),
            }),
            trace_segments: vec![TraceSegment { points }],
            grooves: vec![],
            onset_ticks: vec![],
            breath_spans: vec![],
        };

        let scene = project_scene(&graph);
        let pitch_list = &scene.pitch_segments[0];
        let band_list = &scene.band_segments[0];

        // Check well-interior points: far enough from both edges that the
        // centerline smoothing window is fully symmetric (so on a linear ramp
        // it returns the center value unchanged) AND the back-dated sample is
        // interior. With 60 points and a 31-point window, the middle band
        // (i=28..=32) clears the half-window margin (~15) on both sides, so
        // band.x differs from pitch.x and the interpolation returns the pitch
        // from 5 hops ago.
        // On a pure linear ramp a symmetric average returns the centre value
        // unchanged, so the whole shift is the back-date: ~slope × back_date_hops.
        // Assert the centre is below the current pitch by close to that shift
        // (tolerance covers float + interpolation rounding), rather than a magic
        // absolute threshold that assumes a particular ramp steepness.
        let expected_shift = slope_per_hop as f32 * back_date_hops as f32;
        for i in 28..=32 {
            let current_y = pitch_list[i].y;
            let center = band_list[i].center_y;
            let actual_shift = current_y - center;
            assert!(
                (actual_shift - expected_shift).abs() < 0.01,
                "point {i}: centre is {actual_shift:.4} below pitch \
                 (center_y={center:.4}, pitch y={current_y:.4}); \
                 expected back-date shift ~{expected_shift:.4}"
            );
        }
    }

    /// Drift guard for the pitch/band series split.
    ///
    /// The band's `band_center_y` is sampled from the smoothed pitch array by a
    /// shared post-filter index. When the band becomes its own series, pitch and
    /// band must share ONE filtered point list — if they filter independently,
    /// dropped leading points shift the band's index space and the back-dated
    /// centre samples the WRONG pitch.
    ///
    /// We force the trap: a rising ramp whose *leading* points fall before the
    /// window start (so they are dropped), with the rest in-window. A surviving
    /// interior point's `band_center_y` must equal the *trace's own y* at that
    /// point's back-dated time — i.e. the centre must track the trace shifted by
    /// exactly `back_date_ms`, regardless of how many leading points were
    /// dropped. If the index spaces drift, this lands on the wrong pitch and the
    /// assertion fails.
    #[test]
    fn band_center_samples_correct_pitch_when_leading_points_dropped() {
        // Long enough that the surviving points include a well-interior band
        // (full symmetric centerline window each side). The centerline smoother
        // ([`BAND_CENTER_SMOOTH_WINDOW`]) drags edge points toward the segment
        // mean, so the rising-ramp sanity below is only meaningful where the
        // window is fully symmetric; 80 points clears a 31-point window with
        // margin even after the leading drop.
        let n_points = 80usize;
        let hop_ms = 50u64;
        let back_date_hops = 4usize;
        let back_date_ms = back_date_hops as u64 * hop_ms;

        let pitch_min = 8.0_f64;
        let pitch_max = 10.0_f64;
        let span = pitch_max - pitch_min;

        // Linear ramp in normalized y from 0.15 to 0.85 across all points.
        let y_start = 0.15_f64;
        let y_end = 0.85_f64;
        let slope_per_hop = (y_end - y_start) / (n_points as f64 - 1.0);

        let points: Vec<_> = (0..n_points)
            .map(|i| {
                let ny = y_start + i as f64 * slope_per_hop;
                let pitch_log2 = ny * span + pitch_min;
                let t_ms = i as u64 * hop_ms;
                let vibrato_t_ms = t_ms.saturating_sub(back_date_ms);
                TracePoint {
                    t_ms,
                    vibrato_t_ms,
                    pitch: PitchLog2(pitch_log2 as f32),
                    confidence: 1.0,
                    vibrato_rate: 5.5,
                    vibrato_depth: 60.0,
                }
            })
            .collect();

        // Window starts at hop 8 → the first 8 points (t=0..350ms) are DROPPED
        // by the time filter. The band index space must reflect ONLY survivors.
        let window_start_hop = 8u64;
        let window_start_ms = window_start_hop * hop_ms;
        let time_end_ms = (n_points as u64 - 1) * hop_ms;

        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: window_start_ms,
                end_ms: time_end_ms,
                span_ms: time_end_ms - window_start_ms,
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(pitch_min as f32),
                max: PitchLog2(pitch_max as f32),
            }),
            trace_segments: vec![TraceSegment { points }],
            grooves: vec![],
            onset_ticks: vec![],
            breath_spans: vec![],
        };

        let scene = project_scene(&graph);
        let pitch_list = &scene.pitch_segments[0];
        let band_list = &scene.band_segments[0];
        assert!(
            pitch_list.len() >= 8,
            "expected the in-window survivors only"
        );
        assert_eq!(
            pitch_list.len(),
            band_list.len(),
            "pitch and band must have the same number of survivors"
        );

        // The no-drift invariant, stated WITHOUT assuming anything about the
        // dropped leading points: the band centre must equal the *survivors'*
        // own trace curve sampled at the band's back-dated x. We rebuild the
        // survivors' (x, y) curve from the projected pitch points themselves and
        // re-derive what the centre SHOULD be — if the band had instead indexed
        // against an un-filtered list, its centre would land on a different
        // pitch and this equality breaks.
        let surv_xs: Vec<f32> = pitch_list.iter().map(|tp| tp.x).collect();
        let surv_ys: Vec<f32> = pitch_list.iter().map(|tp| tp.y).collect();
        // Smooth the survivors' y the same way the band centre is smoothed, so
        // we compare like with like (the centre is a smoothed sample).
        let surv_smoothed = smooth(&surv_ys, BAND_CENTER_SMOOTH_WINDOW);

        // A back-dated band point has band.x != pitch.x for the same index.
        // We detect "non-fallback" points by checking whether band.x differs
        // from the pitch's own x (a fallback point would have the same x).
        let mut checked = 0;
        for (i, (tp, bp)) in pitch_list.iter().zip(band_list.iter()).enumerate() {
            // Skip points where vibrato_x was None (fallback: band.x == pitch.x).
            // These are early-session points where back-dating overshot the start.
            if (bp.x - tp.x).abs() < 1e-6 && i < back_date_hops {
                continue;
            }
            let vx = bp.x;
            // Expected centre = the survivors' own smoothed curve sampled at the
            // band's back-dated x. Same helper the model uses internally.
            let expected = sample_center_at_x(&surv_xs, &surv_smoothed, vx);
            assert!(
                (bp.center_y - expected).abs() < 1e-4,
                "band centre {:.5} must equal the survivors' curve sampled at \
                 band.x={vx:.4} (={expected:.5}); a mismatch means the band \
                 indexed a different (un-shared) point list — drift.",
                bp.center_y
            );
            checked += 1;
        }
        assert!(checked >= 4, "test must exercise several back-dated points");

        // And the behavioural sanity: on a RISING ramp the back-dated centre is
        // never ABOVE the point's own y (it samples older = lower pitch). This
        // holds only where the centerline window is fully symmetric — at the
        // segment edges the smoother drags the centre toward the segment mean,
        // which can lift an early centre above its (lower) local trace y. So
        // restrict the check to well-interior survivors.
        let half_win = BAND_CENTER_SMOOTH_WINDOW / 2;
        let n = pitch_list.len();
        for (i, (tp, bp)) in pitch_list.iter().zip(band_list.iter()).enumerate() {
            if i < half_win || i + half_win >= n {
                continue; // edge: smoothing not symmetric
            }
            // Only check points where back-dating actually moved the x left.
            if (bp.x - tp.x).abs() > 1e-6 {
                assert!(
                    bp.center_y <= tp.y + 1e-4,
                    "rising ramp: centre {:.4} must not sit above trace y {:.4}",
                    bp.center_y,
                    tp.y
                );
            }
        }
    }
}
