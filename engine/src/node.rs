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
}

/// The core node contract. Every processing unit in the graph implements this.
pub trait Node: Send {
    /// Returns (inputs, outputs) port declarations.
    fn declare_ports(&self) -> (Vec<PortSpec>, Vec<PortSpec>);
    fn declare_parameters(&self) -> Vec<ParamSpec>;
    fn prepare(&mut self, id: &str, sample_rate: u32, block_size: usize);
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize);
}
