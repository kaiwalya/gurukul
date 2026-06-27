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

/// Number of points in the symmetric moving-average window used to smooth the
/// per-point band half-heights. A 5-point window spans ~250 ms at 20 Hz and
/// kills single-hop spikes while tracking real depth changes quickly enough.
/// Chosen over a one-pole IIR because it has zero warm-up latency for the
/// initial points and needs no per-segment state.
const BAND_SMOOTH_WINDOW: usize = 5;

/// Number of points in the symmetric moving-average window used to smooth
/// the per-point band centre (mean pitch). Wider than [`BAND_SMOOTH_WINDOW`]
/// so one full vibrato cycle is averaged out and the ribbon rails stay
/// steady: at ~5.5 Hz vibrato and 50 ms/point a full cycle is ~4 points;
/// a 9-point window covers ~1 full cycle plus margin on each side.
const BAND_CENTER_SMOOTH_WINDOW: usize = 9;

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

fn normalize_trace_segment(
    segment: &TraceSegment,
    time_window: TimeWindow,
    pitch_window: PitchWindow,
) -> Option<NormalizedTraceSegment> {
    let octave_span = pitch_window.max.0 - pitch_window.min.0;

    // Build parallel lists of raw (unsmoothed) band half-heights and raw
    // normalized-y values alongside the partial points.
    let mut raw_half_heights: Vec<f32> = Vec::with_capacity(segment.points.len());
    let mut raw_ys: Vec<f32> = Vec::with_capacity(segment.points.len());
    let points = segment
        .points
        .iter()
        .filter_map(|point| {
            let strength =
                vibrato_strength(point.vibrato_rate, point.vibrato_depth, point.confidence);
            // Half-height = pure normalized depth: the band rails wrap the actual
            // peak-to-peak swing of the trace. Rate is already visible in the
            // wiggle itself; confidence drives opacity (see `apply_mesh_band`).
            // Strength is NOT applied here — it would double-count information
            // already expressed through the other two visual channels.
            let raw_hh = if octave_span > 0.0 {
                (point.vibrato_depth / 1200.0) / octave_span
            } else {
                0.0
            };
            let nx = normalize_time(point.t_ms, time_window)?;
            let ny = normalize_pitch(point.pitch, pitch_window)?;
            raw_half_heights.push(raw_hh);
            raw_ys.push(ny);
            Some(NormalizedTracePoint {
                point: NormalizedPoint { x: nx, y: ny },
                confidence: point.confidence,
                vibrato_strength: strength,
                band_half_height: 0.0, // filled in below after smoothing
                band_center_y: 0.0,    // filled in below after smoothing
            })
        })
        .collect::<Vec<_>>();

    if points.is_empty() {
        return None;
    }

    // Smooth the raw half-heights with the narrower window (kills single-hop
    // spikes while tracking real depth changes quickly).
    let smoothed_hh = smooth(&raw_half_heights, BAND_SMOOTH_WINDOW);
    // Smooth the raw y values with the wider window so one full vibrato cycle
    // is averaged out and the ribbon's centre rails stay steady.
    let smoothed_center = smooth(&raw_ys, BAND_CENTER_SMOOTH_WINDOW);

    let points = points
        .into_iter()
        .zip(smoothed_hh)
        .zip(smoothed_center)
        .map(|((mut tp, hh), cy)| {
            tp.band_half_height = hh;
            tp.band_center_y = cy;
            tp
        })
        .collect();

    Some(NormalizedTraceSegment { points })
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
                span_ms: 100,
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
        let hh = scene.pitch_segments[0].points[0].band_half_height;
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
        let tp_list = &scene.pitch_segments[0].points;

        // The window is 9 points (odd). With 12 input points, indices 4..=7
        // see a full symmetric 9-point window. An odd window over alternating
        // ±delta values has a worst-case 5:4 split, so the mean deviates from
        // 0.5 by at most delta/9 = 0.15/9 ≈ 0.0167. The key property is that
        // this is much tighter than the raw instantaneous deviation of ±0.15.
        // Allow ±0.02 (slightly above the theoretical max) for fp rounding.
        for (i, tp) in tp_list.iter().enumerate() {
            if i >= 4 && i <= 7 {
                assert!(
                    (tp.band_center_y - 0.5).abs() < 0.02,
                    "interior point {i}: band_center_y expected ~0.5, got {}",
                    tp.band_center_y
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
        let hh = scene.pitch_segments[0].points[0].band_half_height;
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
        let hh = scene.pitch_segments[0].points[0].band_half_height;
        assert!(
            (hh - 0.05).abs() < 1e-4,
            "depth-driven height must be nonzero even when strength=0, got {hh}"
        );
    }
}
