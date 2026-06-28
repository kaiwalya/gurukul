//! Ordered feature history for the scrolling graph.

use crate::feature_types::Features;
use std::collections::VecDeque;

/// Retention is also the graph's visible time width: everything held here
/// is drawn, so `semantic_graph` fits the pitch window to all of it.
pub const DEFAULT_HISTORY_MS: u64 = 20_000;
pub const DEFAULT_HISTORY_CAPACITY: usize = 4_096;

#[derive(Debug)]
pub struct FeatureHistory {
    samples: VecDeque<Features>,
    window_ms: u64,
    capacity: usize,
    max_seen_t_ms: Option<u64>,
}

impl Default for FeatureHistory {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY_MS, DEFAULT_HISTORY_CAPACITY)
    }
}

impl FeatureHistory {
    pub fn new(window_ms: u64, capacity: usize) -> Self {
        Self {
            samples: VecDeque::new(),
            window_ms,
            capacity: capacity.max(1),
            max_seen_t_ms: None,
        }
    }

    pub fn push(&mut self, sample: Features) {
        self.max_seen_t_ms = Some(
            self.max_seen_t_ms
                .map_or(sample.t_ms, |max| max.max(sample.t_ms)),
        );
        self.samples.push_back(sample);
        self.evict();
    }

    pub fn extend(&mut self, samples: impl IntoIterator<Item = Features>) {
        for sample in samples {
            self.push(sample);
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.max_seen_t_ms = None;
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Features> {
        self.samples.iter()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let newest = self.max_seen_t_ms?;
        Some((newest.saturating_sub(self.window_ms), newest))
    }

    /// The fixed retention/display width in milliseconds.
    pub fn window_ms(&self) -> u64 {
        self.window_ms
    }

    fn evict(&mut self) {
        let Some(newest_ms) = self.max_seen_t_ms else {
            return;
        };
        let cutoff = newest_ms.saturating_sub(self.window_ms);
        self.samples.retain(|sample| sample.t_ms >= cutoff);
        while self.samples.len() > self.capacity {
            self.samples.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::pitch::PitchLog2;

    fn sample(hop_index: u64, t_ms: u64) -> Features {
        Features {
            hop_index,
            pitch: Some(PitchLog2(8.0)),
            confidence: 1.0,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_amplitude: 0.0,
            vibrato_phase: 0.0,
            vibrato_t_ms: t_ms,
            t_ms,
        }
    }

    #[test]
    fn retains_time_window_across_different_cadences() {
        for cadence_ms in [10, 40] {
            let mut history = FeatureHistory::new(100, 100);
            for (hop, t_ms) in (0..=200).step_by(cadence_ms).enumerate() {
                history.push(sample(hop as u64, t_ms));
            }

            let oldest = history.iter().next().unwrap().t_ms;
            assert!(oldest >= 100);
            assert!(oldest < 100 + cadence_ms as u64);
            assert_eq!(history.iter().next_back().unwrap().t_ms, 200);
        }
    }

    #[test]
    fn repeated_timestamps_are_preserved() {
        let mut history = FeatureHistory::new(100, 100);
        history.extend((0..4).map(|hop| sample(hop, 7)));

        assert_eq!(history.len(), 4);
        assert_eq!(
            history
                .iter()
                .map(|sample| sample.hop_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn safety_cap_bounds_bad_or_stalled_timestamps() {
        let mut history = FeatureHistory::new(100, 3);
        history.extend((0..8).map(|hop| sample(hop, 7)));

        assert_eq!(history.len(), 3);
        assert_eq!(
            history
                .iter()
                .map(|sample| sample.hop_index)
                .collect::<Vec<_>>(),
            vec![5, 6, 7]
        );
    }

    #[test]
    fn late_older_timestamp_does_not_move_window_backwards() {
        let mut history = FeatureHistory::new(100, 10);
        history.push(sample(0, 100));
        history.push(sample(1, 180));
        history.push(sample(2, 120));

        assert_eq!(history.time_bounds(), Some((80, 180)));
        assert_eq!(
            history.iter().map(|sample| sample.t_ms).collect::<Vec<_>>(),
            vec![100, 180, 120]
        );
    }
}
