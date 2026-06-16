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
// Vibrato-strength thresholds — all decisions in domain units, here only.
// ---------------------------------------------------------------------------

/// Depth below which we treat the pitch wobble as noise, not vibrato
/// (cents — the `vibrato_depth` feature is emitted in cents by `node-vibrato`,
/// which builds the contour as `1200 × log2(f)`). Typical sung vibrato is
/// ~20–50 cents peak-to-peak (≈ 0.2–0.5 st), so a 20-cent floor gives margin
/// against gentle ornamentation and pitch jitter.
const VIBRATO_DEPTH_FLOOR_CENTS: f32 = 20.0;

/// Depth at which the gate reaches 1.0 (cents). A ramp from 20 to 50 covers the
/// typical vibrato range; depth above 50 cents is unambiguously intentional
/// vibrato.
const VIBRATO_DEPTH_FULL_CENTS: f32 = 50.0;

/// Lower edge of the musical vibrato rate band (Hz). Below ~4 Hz the wobble
/// is too slow to be perceived as vibrato (more like a slow wavering).
const VIBRATO_RATE_LOW_ZERO: f32 = 3.5;

/// Rate at which the band reaches full weight on the low side (Hz).
const VIBRATO_RATE_LOW_FULL: f32 = 4.5;

/// Rate at which the band begins fading on the high side (Hz). Classical
/// vibrato rarely exceeds 7 Hz; anything faster starts to sound strained.
const VIBRATO_RATE_HIGH_FULL: f32 = 6.5;

/// Rate at which the band fades to zero on the high side (Hz).
const VIBRATO_RATE_HIGH_ZERO: f32 = 7.5;

use super::scene::{
    NormalizedBreathSpan, NormalizedGrooveLine, NormalizedOnsetTick, NormalizedPoint,
    NormalizedTracePoint, NormalizedTraceSegment, TimeGraphScene,
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
    let pitch_segments = graph
        .pitch_window
        .map(|pitch_window| {
            graph
                .trace_segments
                .iter()
                .filter_map(|segment| normalize_trace_segment(segment, time_window, pitch_window))
                .collect()
        })
        .unwrap_or_default();

    TimeGraphScene {
        pitch_segments,
        grooves,
        onset_ticks,
        breath_spans,
    }
}

fn normalize_trace_segment(
    segment: &TraceSegment,
    time_window: TimeWindow,
    pitch_window: PitchWindow,
) -> Option<NormalizedTraceSegment> {
    let points = segment
        .points
        .iter()
        .filter_map(|point| {
            Some(NormalizedTracePoint {
                point: NormalizedPoint {
                    x: normalize_time(point.t_ms, time_window)?,
                    y: normalize_pitch(point.pitch, pitch_window)?,
                },
                confidence: point.confidence,
                vibrato_strength: vibrato_strength(
                    point.vibrato_rate,
                    point.vibrato_depth,
                    point.confidence,
                ),
            })
        })
        .collect::<Vec<_>>();
    (!points.is_empty()).then_some(NormalizedTraceSegment { points })
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
    let span = window.end_ms.saturating_sub(window.start_ms);
    if span == 0 || t_ms < window.start_ms || t_ms > window.end_ms {
        return None;
    }
    Some((t_ms - window.start_ms) as f32 / span as f32)
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
    let span = window.end_ms.saturating_sub(window.start_ms);
    if span == 0 {
        return None;
    }
    Some((t_ms.saturating_sub(window.start_ms) as f32 / span as f32).clamp(0.0, 1.0))
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
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(8.0),
                max: PitchLog2(10.0),
            }),
            trace_segments: vec![TraceSegment {
                points: vec![
                    TracePoint {
                        t_ms: 10,
                        pitch: PitchLog2(8.0),
                        confidence: 0.2,
                        vibrato_rate: 0.0,
                        vibrato_depth: 0.0,
                    },
                    TracePoint {
                        t_ms: 60,
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
        let point = scene.pitch_segments[0].points[1].point;
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
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(8.0),
                max: PitchLog2(10.0),
            }),
            trace_segments: vec![TraceSegment {
                points: vec![TracePoint {
                    t_ms: 60,
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
        let xs: Vec<f32> = scene.pitch_segments[0]
            .points
            .iter()
            .map(|p| p.point.x)
            .collect();
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
}
