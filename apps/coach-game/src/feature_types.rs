//! Plain feature read-model types shared by the head and semantic graph.

use domain_ports::app_coach::FeatureSnapshot;
use domain_ports::pitch::PitchLog2;

/// The game-side live features: the port's `FeatureSnapshot` with `f0`
/// already lifted out of raw Hz into a [`PitchLog2`] (`None` = unvoiced,
/// retiring the port's `f0_hz == 0.0` sentinel).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Features {
    /// Session-local producer sequence used to detect missing hops.
    pub hop_index: u64,
    /// The detected pitch, or `None` when the frame is unvoiced.
    pub pitch: Option<PitchLog2>,
    /// YIN periodicity confidence, `0.0..=1.0`.
    pub confidence: f32,
    /// Onset detector output (positive on attack).
    pub onset: f32,
    /// Breath / aspiration energy estimate.
    pub breath: f32,
    /// Vibrato rate in Hz over the recent window.
    pub vibrato_rate: f32,
    /// Vibrato depth in semitones.
    pub vibrato_depth: f32,
    /// Back-dated timestamp for vibrato features, in ms. See [`FeatureSnapshot::vibrato_t_ms`].
    #[serde(default)]
    pub vibrato_t_ms: u64,
    /// Snapshot timestamp in ms for time-axis placement.
    pub t_ms: u64,
}

impl From<FeatureSnapshot> for Features {
    fn from(snapshot: FeatureSnapshot) -> Self {
        Self {
            hop_index: snapshot.hop_index,
            pitch: (snapshot.f0_hz > 0.0).then(|| PitchLog2::from_hz(snapshot.f0_hz)),
            confidence: snapshot.confidence,
            onset: snapshot.onset,
            breath: snapshot.breath,
            vibrato_rate: snapshot.vibrato_rate,
            vibrato_depth: snapshot.vibrato_depth,
            vibrato_t_ms: snapshot.vibrato_t_ms,
            t_ms: snapshot.t_ms,
        }
    }
}
