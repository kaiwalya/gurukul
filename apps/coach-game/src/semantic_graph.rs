//! Bevy-free semantic model for the scrolling time graph.

use crate::feature_history::FeatureHistory;
use crate::feature_types::Features;
use domain_ports::app_coach::MusicInfo;
use domain_ports::pitch::{PitchLog2, PitchLog2Interval};
use domain_ports::tuning::{Tuning, ORIGIN};

const PITCH_PADDING_OCTAVES: f32 = 0.25;
const MIN_PITCH_SPAN_OCTAVES: f32 = 0.5;
/// Each window edge halves its remaining distance to the contraction
/// target over this long — expansion stays instant.
const CONTRACT_HALF_LIFE_MS: f32 = 1_000.0;
/// An edge this close to its target snaps onto it, giving the ease a fixed
/// point so downstream change-detection goes quiet between contractions.
const CONTRACT_SNAP_EPSILON_OCTAVES: f32 = 1e-3;
/// One easing step never integrates more elapsed time than this, so a
/// stalled feature stream resumes with a smooth step instead of a snap.
const MAX_EASE_STEP_MS: u64 = 250;
pub const BREATH_ACTIVE_THRESHOLD: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeWindow {
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PitchWindow {
    pub min: PitchLog2,
    pub max: PitchLog2,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TracePoint {
    pub t_ms: u64,
    pub pitch: PitchLog2,
    pub confidence: f32,
    pub vibrato_rate: f32,
    pub vibrato_depth: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraceSegment {
    pub points: Vec<TracePoint>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrooveLine {
    pub pitch: PitchLog2,
    pub slot: usize,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OnsetTick {
    pub t_ms: u64,
    pub strength: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BreathSpan {
    pub start_ms: u64,
    pub end_ms: u64,
    pub peak: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SemanticGraph {
    pub time_window: Option<TimeWindow>,
    pub pitch_window: Option<PitchWindow>,
    pub trace_segments: Vec<TraceSegment>,
    pub grooves: Vec<GrooveLine>,
    pub onset_ticks: Vec<OnsetTick>,
    pub breath_spans: Vec<BreathSpan>,
}

#[derive(Debug, Default)]
pub struct GraphProjector {
    pitch_window: Option<PitchWindow>,
    last_ms: Option<u64>,
}

impl GraphProjector {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn project(
        &mut self,
        history: &FeatureHistory,
        music: Option<&MusicInfo>,
    ) -> SemanticGraph {
        let time_window = history
            .time_bounds()
            .map(|(start_ms, end_ms)| TimeWindow { start_ms, end_ms });
        let newest_ms = time_window.map(|window| window.end_ms);
        let target = target_pitch_window(history);
        self.update_pitch_window(target, newest_ms);

        SemanticGraph {
            time_window,
            pitch_window: self.pitch_window,
            trace_segments: trace_segments(history),
            grooves: match (self.pitch_window, music) {
                (Some(window), Some(info)) => groove_lines(window, info),
                _ => Vec::new(),
            },
            onset_ticks: onset_ticks(history),
            breath_spans: breath_spans(history),
        }
    }

    fn update_pitch_window(&mut self, target: Option<PitchWindow>, newest_ms: Option<u64>) {
        let dt_ms = match (self.last_ms, newest_ms) {
            (Some(last), Some(now)) => now.saturating_sub(last).min(MAX_EASE_STEP_MS),
            _ => 0,
        };
        if newest_ms.is_some() {
            self.last_ms = newest_ms;
        }

        // No voiced pitch in history: hold the window where it is.
        let Some(target) = target else {
            return;
        };
        let Some(current) = self.pitch_window else {
            self.pitch_window = Some(target);
            return;
        };

        // A register jump (no overlap with the current window) refits
        // immediately.
        if target.min.0 > current.max.0 || target.max.0 < current.min.0 {
            self.pitch_window = Some(target);
            return;
        }

        // Each edge moves independently: expansion is instant, contraction
        // eases and snaps once the remainder is sub-visible.
        let alpha = 1.0 - 0.5_f32.powf(dt_ms as f32 / CONTRACT_HALF_LIFE_MS);
        let contract = |edge: f32, toward: f32| {
            let next = edge + (toward - edge) * alpha;
            if (toward - next).abs() < CONTRACT_SNAP_EPSILON_OCTAVES {
                toward
            } else {
                next
            }
        };
        self.pitch_window = Some(PitchWindow {
            min: PitchLog2(if target.min.0 < current.min.0 {
                target.min.0
            } else {
                contract(current.min.0, target.min.0)
            }),
            max: PitchLog2(if target.max.0 > current.max.0 {
                target.max.0
            } else {
                contract(current.max.0, target.max.0)
            }),
        });
    }
}

/// Fit every voiced pitch still in the history: the history's retention is
/// also the visible time width, so the window tracks exactly what's on
/// screen and contracts as extremes scroll off the left edge.
fn target_pitch_window(history: &FeatureHistory) -> Option<PitchWindow> {
    let mut voiced = history.iter().filter_map(|sample| sample.pitch);

    let first = voiced.next()?;
    let (mut min, mut max) = (first.0, first.0);
    for pitch in voiced {
        min = min.min(pitch.0);
        max = max.max(pitch.0);
    }

    min -= PITCH_PADDING_OCTAVES;
    max += PITCH_PADDING_OCTAVES;
    if max - min < MIN_PITCH_SPAN_OCTAVES {
        let center = (min + max) * 0.5;
        let half = MIN_PITCH_SPAN_OCTAVES * 0.5;
        min = center - half;
        max = center + half;
    }
    Some(PitchWindow {
        min: PitchLog2(min),
        max: PitchLog2(max),
    })
}

fn trace_segments(history: &FeatureHistory) -> Vec<TraceSegment> {
    let mut segments = Vec::new();
    let mut points = Vec::new();
    let mut previous: Option<&Features> = None;

    for sample in history.iter() {
        let contiguous =
            previous.is_some_and(|prev| sample.hop_index == prev.hop_index.wrapping_add(1));
        match sample.pitch {
            Some(pitch) if previous.is_none() || contiguous => points.push(TracePoint {
                t_ms: sample.t_ms,
                pitch,
                confidence: sample.confidence,
                vibrato_rate: sample.vibrato_rate,
                vibrato_depth: sample.vibrato_depth,
            }),
            Some(pitch) => {
                finish_segment(&mut segments, &mut points);
                points.push(TracePoint {
                    t_ms: sample.t_ms,
                    pitch,
                    confidence: sample.confidence,
                    vibrato_rate: sample.vibrato_rate,
                    vibrato_depth: sample.vibrato_depth,
                });
            }
            None => finish_segment(&mut segments, &mut points),
        }
        previous = Some(sample);
    }
    finish_segment(&mut segments, &mut points);
    segments
}

fn finish_segment(segments: &mut Vec<TraceSegment>, points: &mut Vec<TracePoint>) {
    if !points.is_empty() {
        segments.push(TraceSegment {
            points: std::mem::take(points),
        });
    }
}

fn groove_lines(window: PitchWindow, music: &MusicInfo) -> Vec<GrooveLine> {
    let scale = music.scale;
    let tuning = scale.tuning();
    let intervals = tuning.intervals();
    let active_slots = scale.intervals().degree_slots();
    let mut grooves = Vec::new();

    for slot in 0..tuning.len() {
        let base = ORIGIN + tuning.rotation() + intervals.cumulative_rotation_to(slot);
        let first_octave = ((window.min - base).0).ceil() as i32;
        let last_octave = ((window.max - base).0).floor() as i32;
        for octave in first_octave..=last_octave {
            grooves.push(GrooveLine {
                pitch: base + PitchLog2Interval::octaves(octave),
                slot,
                active: active_slots.contains(&(slot as u32)),
            });
        }
    }
    grooves.sort_by(|a, b| a.pitch.0.total_cmp(&b.pitch.0));
    grooves
}

fn onset_ticks(history: &FeatureHistory) -> Vec<OnsetTick> {
    history
        .iter()
        .filter(|sample| sample.onset > 0.0)
        .map(|sample| OnsetTick {
            t_ms: sample.t_ms,
            strength: sample.onset,
        })
        .collect()
}

fn breath_spans(history: &FeatureHistory) -> Vec<BreathSpan> {
    let mut spans = Vec::new();
    let mut active: Option<BreathSpan> = None;
    let mut previous: Option<&Features> = None;

    for sample in history.iter() {
        let contiguous =
            previous.is_some_and(|prev| sample.hop_index == prev.hop_index.wrapping_add(1));
        if previous.is_some() && !contiguous {
            if let Some(span) = active.take() {
                spans.push(span);
            }
        }

        if sample.breath >= BREATH_ACTIVE_THRESHOLD {
            match active.as_mut() {
                Some(span) => {
                    span.end_ms = sample.t_ms;
                    span.peak = span.peak.max(sample.breath);
                }
                None => {
                    active = Some(BreathSpan {
                        start_ms: sample.t_ms,
                        end_ms: sample.t_ms,
                        peak: sample.breath,
                    });
                }
            }
        } else if let Some(span) = active.take() {
            spans.push(span);
        }
        previous = Some(sample);
    }
    if let Some(span) = active {
        spans.push(span);
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::scale::{Scale, ScaleIntervals};
    use domain_ports::tuning::{TuningAbsolute, TuningKind};

    fn feature(hop: u64, t_ms: u64, pitch: Option<f32>) -> Features {
        Features {
            hop_index: hop,
            pitch: pitch.map(PitchLog2),
            confidence: 0.8,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_depth: 0.0,
            t_ms,
        }
    }

    fn music(kind: TuningKind, intervals: ScaleIntervals, octave: i32) -> MusicInfo {
        let tuning = TuningAbsolute::new(kind.intervals(), PitchLog2Interval(0.17));
        MusicInfo {
            scale: Scale::new(intervals, tuning.shift_up(0), octave),
        }
    }

    fn history(samples: impl IntoIterator<Item = Features>) -> FeatureHistory {
        let mut history = FeatureHistory::default();
        history.extend(samples);
        history
    }

    #[test]
    fn trace_breaks_at_silence_and_hop_gaps() {
        let history = history([
            feature(0, 0, Some(8.0)),
            feature(1, 10, Some(8.1)),
            feature(2, 20, None),
            feature(3, 30, Some(8.2)),
            feature(7, 40, Some(8.3)),
        ]);
        let graph = GraphProjector::default().project(&history, None);

        assert_eq!(
            graph
                .trace_segments
                .iter()
                .map(|segment| segment.points.len())
                .collect::<Vec<_>>(),
            vec![2, 1, 1]
        );
    }

    #[test]
    fn silence_holds_window_but_resumed_octave_jump_refits_immediately() {
        let mut projector = GraphProjector::default();
        let initial = history([feature(0, 0, Some(8.0))]);
        let initial_window = projector.project(&initial, None).pitch_window.unwrap();

        let silent = history([feature(1, 7_000, None)]);
        assert_eq!(
            projector.project(&silent, None).pitch_window,
            Some(initial_window)
        );

        let resumed = history([feature(0, 8_000, Some(9.0))]);
        let resumed_window = projector.project(&resumed, None).pitch_window.unwrap();
        assert!(resumed_window.min.0 > initial_window.max.0);
        assert!(resumed_window.min.0 < 9.0 && resumed_window.max.0 > 9.0);
    }

    #[test]
    fn contraction_waits_for_extremes_to_scroll_out_of_history() {
        let mut projector = GraphProjector::default();
        let broad = history([feature(0, 0, Some(8.0)), feature(1, 10, Some(9.0))]);
        let broad_window = projector.project(&broad, None).pitch_window.unwrap();

        // The extremes are still on screen: window holds.
        let recent = history([
            feature(0, 0, Some(8.0)),
            feature(1, 10, Some(9.0)),
            feature(2, 1_000, Some(8.5)),
        ]);
        assert_eq!(
            projector.project(&recent, None).pitch_window,
            Some(broad_window)
        );

        // Only the narrow note remains in history: window contracts.
        let aged = history([feature(3, 5_000, Some(8.5)), feature(4, 5_800, Some(8.5))]);
        let contracted = projector.project(&aged, None).pitch_window.unwrap();
        assert!(contracted.max.0 - contracted.min.0 < broad_window.max.0 - broad_window.min.0);
    }

    #[test]
    fn expansion_snaps_the_breached_edge_while_the_other_eases() {
        let mut projector = GraphProjector::default();
        let broad = history([feature(0, 0, Some(8.0)), feature(1, 10, Some(9.0))]);
        let broad_window = projector.project(&broad, None).pitch_window.unwrap();

        // A note slightly above the window top: max expands instantly,
        // min only eases a small step toward the new target.
        let higher = history([feature(3, 1_000, Some(9.3))]);
        let target_min = 9.3 - PITCH_PADDING_OCTAVES;
        let expanded = projector.project(&higher, None).pitch_window.unwrap();
        assert!(expanded.max.0 > broad_window.max.0);
        assert!(expanded.min.0 >= broad_window.min.0);
        assert!(expanded.min.0 < broad_window.min.0 + 0.3);
        assert!(expanded.min.0 < target_min);
    }

    #[test]
    fn expansion_below_the_window_snaps_the_min_edge() {
        let mut projector = GraphProjector::default();
        let broad = history([feature(0, 0, Some(8.0)), feature(1, 10, Some(9.0))]);
        let broad_window = projector.project(&broad, None).pitch_window.unwrap();

        // 7.6 padded overlaps the window bottom: min expands instantly.
        let lower = history([feature(3, 1_000, Some(7.6))]);
        let expanded = projector.project(&lower, None).pitch_window.unwrap();
        assert!(expanded.min.0 < broad_window.min.0);
        assert!(expanded.max.0 <= broad_window.max.0);
    }

    #[test]
    fn downward_register_jump_refits_immediately() {
        let mut projector = GraphProjector::default();
        let broad = history([feature(0, 0, Some(8.0)), feature(1, 10, Some(9.0))]);
        let broad_window = projector.project(&broad, None).pitch_window.unwrap();

        // 7.0 padded sits entirely below the window: full refit.
        let low = history([feature(3, 1_000, Some(7.0))]);
        let jumped = projector.project(&low, None).pitch_window.unwrap();
        assert!(jumped.max.0 < broad_window.min.0);
        assert!(jumped.min.0 < 7.0 && jumped.max.0 > 7.0);
    }

    #[test]
    fn contraction_is_framerate_independent() {
        let broad = || history([feature(0, 0, Some(8.0)), feature(1, 10, Some(9.0))]);

        let mut two_steps = GraphProjector::default();
        two_steps.project(&broad(), None);
        two_steps.project(&history([feature(2, 110, Some(8.5))]), None);
        let fine = two_steps
            .project(
                &history([feature(2, 110, Some(8.5)), feature(3, 210, Some(8.5))]),
                None,
            )
            .pitch_window
            .unwrap();

        let mut one_step = GraphProjector::default();
        one_step.project(&broad(), None);
        let coarse = one_step
            .project(&history([feature(2, 210, Some(8.5))]), None)
            .pitch_window
            .unwrap();

        assert!((fine.min.0 - coarse.min.0).abs() < 1e-4);
        assert!((fine.max.0 - coarse.max.0).abs() < 1e-4);
    }

    #[test]
    fn silence_does_not_inflate_the_next_easing_step() {
        let mut projector = GraphProjector::default();
        let broad = history([feature(0, 0, Some(8.0)), feature(1, 10, Some(9.0))]);
        let broad_window = projector.project(&broad, None).pitch_window.unwrap();
        let broad_span = broad_window.max.0 - broad_window.min.0;

        // Silence holds the window but must still advance the clock, so
        // the next easing step integrates 100ms, not 590ms.
        projector.project(&history([feature(2, 500, None)]), None);
        let eased = projector
            .project(&history([feature(3, 600, Some(8.5))]), None)
            .pitch_window
            .unwrap();
        assert!(eased.max.0 - eased.min.0 > broad_span - 0.1);
    }

    #[test]
    fn contraction_settles_exactly_on_the_target() {
        let mut projector = GraphProjector::default();
        projector.project(
            &history([feature(0, 0, Some(8.0)), feature(1, 10, Some(9.0))]),
            None,
        );

        let mut window = None;
        for i in 0..80u64 {
            let t = 1_000 + i * 250;
            window = projector
                .project(&history([feature(2 + i, t, Some(8.5))]), None)
                .pitch_window;
        }
        let settled = window.unwrap();
        let half = MIN_PITCH_SPAN_OCTAVES * 0.5;
        assert_eq!(settled.min.0, 8.5 - half);
        assert_eq!(settled.max.0, 8.5 + half);
    }

    #[test]
    fn contraction_eases_toward_target_instead_of_snapping() {
        let mut projector = GraphProjector::default();
        let broad = history([feature(0, 0, Some(8.0)), feature(1, 10, Some(9.0))]);
        let broad_window = projector.project(&broad, None).pitch_window.unwrap();
        let broad_span = broad_window.max.0 - broad_window.min.0;

        // Each projection step closes only part of the remaining gap.
        projector.project(&history([feature(2, 1_000, Some(8.5))]), None);
        let first = projector
            .project(
                &history([feature(2, 1_000, Some(8.5)), feature(3, 1_800, Some(8.5))]),
                None,
            )
            .pitch_window
            .unwrap();
        let second = projector
            .project(
                &history([
                    feature(2, 1_000, Some(8.5)),
                    feature(3, 1_800, Some(8.5)),
                    feature(4, 2_600, Some(8.5)),
                ]),
                None,
            )
            .pitch_window
            .unwrap();

        let first_span = first.max.0 - first.min.0;
        let second_span = second.max.0 - second.min.0;
        assert!(first_span < broad_span);
        assert!(second_span < first_span);
        // Neither step jumps straight to the minimum-span target.
        assert!(second_span > MIN_PITCH_SPAN_OCTAVES + 0.05);
    }

    #[test]
    fn groove_geometry_repeats_by_octave_and_ignores_scale_register() {
        let intervals = ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]);
        let low = music(TuningKind::HindustaniJust, intervals, 4);
        let high = music(TuningKind::HindustaniJust, intervals, 11);
        let window = PitchWindow {
            min: PitchLog2(7.2),
            max: PitchLog2(8.2),
        };

        let low_lines = groove_lines(window, &low);
        let high_lines = groove_lines(window, &high);
        assert_eq!(low_lines, high_lines);
        assert_eq!(low_lines.len(), 12);

        for line in &low_lines {
            let translated = GrooveLine {
                pitch: line.pitch + PitchLog2Interval::octaves(1),
                ..*line
            };
            assert!(
                ((translated.pitch - line.pitch).0 - 1.0).abs() < 1e-5,
                "expected an octave translation, got {:?}",
                translated.pitch - line.pitch
            );
        }
    }

    #[test]
    fn tuning_slot_count_and_spacing_survive_projection() {
        let intervals = ScaleIntervals::from_mask(1);
        let window = PitchWindow {
            min: PitchLog2(7.23),
            max: PitchLog2(8.23),
        };
        let tet = groove_lines(window, &music(TuningKind::TwelveTet, intervals, 8));
        let just = groove_lines(window, &music(TuningKind::HindustaniJust, intervals, 8));
        let shruti = groove_lines(window, &music(TuningKind::TwentyTwoShruti, intervals, 8));

        assert_eq!(tet.len(), 12);
        assert_eq!(just.len(), 12);
        assert_eq!(shruti.len(), 22);
        let tet_gaps: Vec<_> = tet
            .windows(2)
            .map(|pair| pair[1].pitch.0 - pair[0].pitch.0)
            .collect();
        let just_gaps: Vec<_> = just
            .windows(2)
            .map(|pair| pair[1].pitch.0 - pair[0].pitch.0)
            .collect();
        assert!(tet_gaps
            .windows(2)
            .all(|pair| (pair[0] - pair[1]).abs() < 1e-5));
        assert!(just_gaps
            .windows(2)
            .any(|pair| (pair[0] - pair[1]).abs() > 1e-3));
    }

    #[test]
    fn scale_mask_changes_highlights_not_positions() {
        let sparse = ScaleIntervals::from_mask(1);
        let bilawal = ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]);
        let window = PitchWindow {
            min: PitchLog2(7.23),
            max: PitchLog2(8.23),
        };
        let sparse_lines = groove_lines(window, &music(TuningKind::TwelveTet, sparse, 8));
        let bilawal_lines = groove_lines(window, &music(TuningKind::TwelveTet, bilawal, 8));

        assert_eq!(
            sparse_lines
                .iter()
                .map(|line| line.pitch)
                .collect::<Vec<_>>(),
            bilawal_lines
                .iter()
                .map(|line| line.pitch)
                .collect::<Vec<_>>()
        );
        assert_ne!(
            sparse_lines
                .iter()
                .map(|line| line.active)
                .collect::<Vec<_>>(),
            bilawal_lines
                .iter()
                .map(|line| line.active)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn onset_ticks_and_breath_spans_preserve_timestamps() {
        let mut samples = [
            feature(0, 10, None),
            feature(1, 20, None),
            feature(2, 30, None),
            feature(3, 40, None),
        ];
        samples[0].onset = 0.7;
        samples[1].breath = 1.0;
        samples[2].breath = 0.8;
        samples[3].onset = 0.4;
        let graph = GraphProjector::default().project(&history(samples), None);

        assert_eq!(
            graph.onset_ticks,
            vec![
                OnsetTick {
                    t_ms: 10,
                    strength: 0.7
                },
                OnsetTick {
                    t_ms: 40,
                    strength: 0.4
                }
            ]
        );
        assert_eq!(
            graph.breath_spans,
            vec![BreathSpan {
                start_ms: 20,
                end_ms: 30,
                peak: 1.0
            }]
        );
    }
}
