/// Describes the signal type flowing through a port.
///
/// All three variants share the same physical buffer representation: `&[f32]` / `&mut [f32]` of
/// length `block_size`. The variant is a semantic tag that documents intent and will be used by
/// future tooling (e.g. visual editor, schema validation) to enforce connection type rules.
///
/// # `Feature` sentinel convention (provisional until Phase 1.3)
///
/// Feature ports carry per-sample analyzer outputs (e.g. pitch estimate in Hz). Within a block
/// buffer each sample holds one estimate:
/// - `0.0` means "unvoiced / no estimate this sample".
/// - Positive values carry the meaningful measurement (e.g. Hz for pitch).
///
/// **Do not use `NaN`** — sentinels are finite so callers can branch without `is_nan()` checks.
///
/// This convention is documented here because Phase 1.2 intentionally defers sparse and
/// timestamped Feature port designs; the zero-sentinel is a temporary stand-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortType {
    Audio,
    Control,
    /// Carries analyzer outputs (e.g. pitch estimate in Hz). See the enum-level doc for the
    /// sentinel convention.
    Feature,
}

#[derive(Debug, Clone)]
pub struct PortSpec {
    pub name: &'static str,
    pub ty: PortType,
}

#[derive(Debug, Clone)]
pub struct ParamSpec {
    pub name: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
    /// Free-form unit tag, e.g. "Hz", "cents", "s", "dB", "count", "" for dimensionless.
    pub unit: &'static str,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NodeError(pub String);

/// The core node contract. Every processing unit in the graph implements this.
pub trait Node: Send {
    fn prepare(&mut self, id: &str, sample_rate: u32, block_size: usize);
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize);

    /// Called once after the last block. Default impl is a no-op.
    fn finish(&mut self) -> Result<(), NodeError> {
        Ok(())
    }
}
