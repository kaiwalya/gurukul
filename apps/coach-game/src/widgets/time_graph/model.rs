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
                vibrato_rate: point.vibrato_rate,
                vibrato_depth: point.vibrato_depth,
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
                        vibrato_rate: 5.0,
                        vibrato_depth: 0.2,
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
}
