/// Describes the signal type flowing through a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortType {
    Audio,
    Control,
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
