use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

/// YIN absolute-threshold pitch detector (de Cheveigné & Kawahara 2002).
///
/// Accumulates input samples in a ring buffer; every `hop` samples it runs one YIN
/// analysis on the most recent `window` samples and updates a zero-order-held f0
/// estimate. The `f0` output port is filled with the latest estimate each block.
/// Sentinel: `0.0` = unvoiced, positive float = estimated Hz.
pub struct PitchYin {
    // Parameters resolved at factory/prepare time.
    window: usize,
    hop: usize,
    fmin_hz: f32,
    fmax_hz: f32,
    threshold: f32,

    // Derived at prepare().
    sample_rate: f32,
    // User-facing acceptance bound: reported f0 must not exceed fmax_hz.
    tau_min: usize,
    tau_max: usize,
    // Internal search floor: dropped to at least sr/4000 so CMND has enough
    // pre-fundamental conditioning regardless of the user's fmax_hz.
    tau_search_min: usize,

    // Ring buffer: stores the most recent `window` samples.
    ring: Vec<f32>,
    // Write position within the ring (wraps at `window`).
    ring_write: usize,
    // How many samples have been written since the last analysis.
    samples_since_analysis: usize,
    // How many samples have been written total (used to know when first window is full).
    total_samples: usize,

    // Scratch buffers allocated in prepare().
    d: Vec<f64>,
    dprime: Vec<f64>,

    // Current held f0 estimate (ZOH).
    held_f0: f32,

    // Set true when the node is in an invalid configuration (e.g. block_size > window).
    unhealthy: bool,
}

/// Internal constant: if the best d' value after fallback argmin exceeds this, treat
/// as unvoiced. Not exposed as a parameter yet.
const UNVOICED_DPRIME_THRESHOLD: f64 = 0.8;

impl PitchYin {
    pub fn new(window: usize, hop: usize, fmin_hz: f32, fmax_hz: f32, threshold: f32) -> Self {
        // Nominal sr=48000 for sizing; prepare() will recompute with the actual rate.
        let sr = 48000.0f32;
        let half = window / 2;
        let tau_min = ((sr / fmax_hz).floor() as usize).clamp(1, half);
        let tau_max = ((sr / fmin_hz).ceil() as usize).clamp(1, half);
        let tau_min = tau_min.min(tau_max);
        // Drop the search floor to at least sr/4000 so CMND has ≥~10 pre-fundamental
        // samples of conditioning regardless of the user's fmax_hz setting.
        let tau_search_min = tau_min.min((sr / 4000.0).floor() as usize).max(2);
        let scratch_len = tau_max + 1;

        PitchYin {
            window,
            hop,
            fmin_hz,
            fmax_hz,
            threshold,
            sample_rate: sr,
            tau_min,
            tau_max,
            tau_search_min,
            ring: vec![0.0f32; window],
            ring_write: 0,
            samples_since_analysis: 0,
            total_samples: 0,
            d: vec![0.0f64; scratch_len],
            dprime: vec![1.0f64; scratch_len],
            held_f0: 0.0,
            unhealthy: false,
        }
    }

    /// Run one YIN analysis on the current ring buffer contents and update `held_f0`.
    fn analyse(&mut self) {
        let window = self.window;
        let tau_min = self.tau_min;
        let tau_search_min = self.tau_search_min;
        let tau_max = self.tau_max;

        // Clear scratch buffers before each analysis (no reallocation).
        self.d.fill(0.0);
        self.dprime.fill(1.0);

        // --- Step 1: difference function ---
        // d[tau] = sum_{j=0..window-tau} (x[j] - x[j+tau])^2
        // Computed for tau in [1..=tau_max]; tau=0 is unused (d'[0]=1 by definition).
        // The loop variable `tau` is used both as the d[] index and as a ring-buffer
        // offset, so a range loop is the clearest form here.
        #[allow(clippy::needless_range_loop)]
        for tau in 1..=tau_max {
            let mut acc = 0.0f64;
            for j in 0..(window - tau) {
                let a = self.ring[(self.ring_write + j) % window] as f64;
                let b = self.ring[(self.ring_write + j + tau) % window] as f64;
                let diff = a - b;
                acc += diff * diff;
            }
            self.d[tau] = acc;
        }

        // --- Step 2: cumulative mean normalised difference ---
        // d'[0] = 1 (by definition)
        // d'[tau] = tau * d[tau] / sum_{k=1..=tau} d[k]
        // The running sum starts from tau=1 so that by the time we reach tau_search_min
        // the denominator includes enough pre-fundamental samples to suppress octave dips.
        self.dprime[0] = 1.0;

        let mut running_sum = 0.0f64;
        for tau in 1..=tau_max {
            running_sum += self.d[tau];
            if running_sum > 0.0 {
                self.dprime[tau] = tau as f64 * self.d[tau] / running_sum;
            } else {
                // All d values so far are 0 → silence.
                self.dprime[tau] = 1.0;
            }
        }

        // --- Step 3: absolute threshold search ---
        // The search range uses tau_search_min (the lowered internal floor) so CMND has
        // enough conditioning context. After picking a tau we apply the user-facing
        // tau_min as an acceptance bound: if the winner is below tau_min the reported
        // frequency would exceed fmax_hz, so we emit unvoiced.
        let threshold = self.threshold as f64;
        let mut chosen_tau: Option<usize> = None;

        for tau in tau_search_min..=tau_max {
            if self.dprime[tau] < threshold {
                let prev = if tau > tau_search_min {
                    self.dprime[tau - 1]
                } else {
                    f64::INFINITY
                };
                let next = if tau < tau_max {
                    self.dprime[tau + 1]
                } else {
                    f64::INFINITY
                };
                if self.dprime[tau] < prev && self.dprime[tau] <= next {
                    chosen_tau = Some(tau);
                    break;
                }
            }
        }

        // Fallback: global argmin of d' over [tau_search_min, tau_max].
        let dprime = &self.dprime;
        let chosen_tau = chosen_tau.unwrap_or_else(|| {
            (tau_search_min..=tau_max)
                .min_by(|&a, &b| dprime[a].partial_cmp(&dprime[b]).unwrap())
                .unwrap_or(tau_search_min)
        });

        // If the best d' is too high, treat as unvoiced.
        if dprime[chosen_tau] > UNVOICED_DPRIME_THRESHOLD {
            self.held_f0 = 0.0;
            return;
        }

        // Enforce the user-facing fmax_hz bound: a tau below tau_min means the
        // candidate frequency exceeds fmax_hz. Reject it as unvoiced.
        if chosen_tau < tau_min {
            self.held_f0 = 0.0;
            return;
        }

        // If the chosen tau is pinned to the fmin floor, it's almost certainly
        // the argmin fallback "giving up" — the actual periodic content is
        // outside [fmin..fmax] or there's no periodic content at all. Without
        // this guard, near-silent / non-tonal input gets reported as the fmin
        // frequency, which displays as a fake low note.
        if chosen_tau >= tau_max {
            self.held_f0 = 0.0;
            return;
        }

        // --- Step 4: parabolic interpolation ---
        let tau_hat = parabolic_interp(dprime, chosen_tau, tau_search_min, tau_max);

        // --- Step 5: f0 ---
        self.held_f0 = (self.sample_rate / tau_hat as f32).max(0.0);
    }
}

/// Parabolic interpolation around `tau` in the `dprime` slice.
/// Returns the interpolated sub-sample tau.
fn parabolic_interp(dprime: &[f64], tau: usize, tau_min: usize, tau_max: usize) -> f64 {
    if tau <= tau_min || tau >= tau_max {
        return tau as f64;
    }
    let y_m1 = dprime[tau - 1];
    let y_0 = dprime[tau];
    let y_p1 = dprime[tau + 1];
    let denom = y_m1 - 2.0 * y_0 + y_p1;
    if denom.abs() < 1e-12 {
        return tau as f64;
    }
    let offset = 0.5 * (y_m1 - y_p1) / denom;
    // Clamp offset to [-1, 1] to guard against pathological shapes.
    tau as f64 + offset.clamp(-1.0, 1.0)
}

impl Node for PitchYin {
    fn prepare(&mut self, _id: &str, sample_rate: u32, block_size: usize) {
        let window = self.window;
        let hop = self.hop;

        self.sample_rate = sample_rate as f32;

        // block_size > window is pathological — mark unhealthy and return cleanly.
        if block_size > window {
            self.unhealthy = true;
            return;
        }
        self.unhealthy = false;

        // Tau bounds: derived from fmin/fmax, clamped to [1, window/2].
        let half = window / 2;
        let sr = sample_rate as f32;
        let tau_min = ((sr / self.fmax_hz).floor() as usize).clamp(1, half);
        let tau_max = ((sr / self.fmin_hz).ceil() as usize).clamp(1, half);
        let tau_min = tau_min.min(tau_max);
        // Drop the search floor to at least sr/4000 so CMND has ≥~10 pre-fundamental
        // samples of conditioning regardless of the user's fmax_hz setting.
        let tau_search_min = tau_min.min((sr / 4000.0).floor() as usize).max(2);

        self.tau_min = tau_min;
        self.tau_max = tau_max;
        self.tau_search_min = tau_search_min;
        self.hop = hop;

        // (Re-)allocate ring buffer and scratch buffers.
        self.ring = vec![0.0f32; window];
        self.ring_write = 0;
        self.samples_since_analysis = 0;
        self.total_samples = 0;
        self.held_f0 = 0.0;

        let scratch_len = tau_max + 1;
        self.d = vec![0.0f64; scratch_len];
        self.dprime = vec![1.0f64; scratch_len];
    }

    fn reset(&mut self) {
        for v in self.ring.iter_mut() {
            *v = 0.0;
        }
        self.ring_write = 0;
        self.samples_since_analysis = 0;
        self.total_samples = 0;
        self.held_f0 = 0.0;
        self.d.fill(0.0);
        self.dprime.fill(1.0);
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        let out = match outputs.first_mut() {
            Some(o) => o,
            None => return,
        };

        if self.unhealthy {
            out[..nframes].fill(0.0);
            return;
        }

        let input = match inputs.first() {
            Some(s) => s,
            None => {
                out[..nframes].fill(0.0);
                return;
            }
        };

        let window = self.window;
        let hop = self.hop;

        for &sample in &input[..nframes] {
            // Write sample into ring buffer.
            self.ring[self.ring_write] = sample;
            self.ring_write = (self.ring_write + 1) % window;
            self.total_samples += 1;
            self.samples_since_analysis += 1;

            // Run analysis every `hop` samples, once the ring buffer is full.
            if self.samples_since_analysis >= hop && self.total_samples >= window {
                self.samples_since_analysis = 0;
                self.analyse();
            }
        }

        // ZOH: fill entire output block with current estimate.
        out[..nframes].fill(self.held_f0);
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "PitchYin",
        vec![PortSpec {
            name: "audio_in",
            ty: PortType::Audio,
        }],
        vec![PortSpec {
            name: "f0",
            ty: PortType::Feature,
        }],
        vec![
            ParamSpec {
                name: "window",
                default: 2048.0,
                min: 256.0,
                max: 8192.0,
                unit: "samples",
            },
            ParamSpec {
                name: "hop",
                default: 512.0,
                min: 32.0,
                max: 4096.0,
                unit: "samples",
            },
            ParamSpec {
                name: "fmin_hz",
                default: 50.0,
                min: 20.0,
                max: 500.0,
                unit: "Hz",
            },
            ParamSpec {
                name: "fmax_hz",
                default: 2000.0,
                min: 500.0,
                max: 8000.0,
                unit: "Hz",
            },
            ParamSpec {
                name: "threshold",
                default: 0.1,
                min: 0.01,
                max: 0.5,
                unit: "",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let window = params.get("window").copied().unwrap_or(2048.0) as usize;
            let hop = params.get("hop").copied().unwrap_or(512.0) as usize;
            let fmin_hz = params.get("fmin_hz").copied().unwrap_or(50.0) as f32;
            let fmax_hz = params.get("fmax_hz").copied().unwrap_or(2000.0) as f32;
            let threshold = params.get("threshold").copied().unwrap_or(0.1) as f32;
            Box::new(PitchYin::new(window, hop, fmin_hz, fmax_hz, threshold)) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48000;
    const BLOCK_SIZE: usize = 512;

    /// Build a PitchYin node with the given fmax_hz and call prepare().
    fn make_node(fmin_hz: f32, fmax_hz: f32) -> PitchYin {
        let mut node = PitchYin::new(2048, 512, fmin_hz, fmax_hz, 0.1);
        node.prepare("test", SR, BLOCK_SIZE);
        node
    }

    /// Generate a pure sine wave of `n_samples` samples at `freq` Hz and `amplitude`.
    fn sine(freq: f64, amplitude: f32, n_samples: usize) -> Vec<f32> {
        (0..n_samples)
            .map(|i| {
                (amplitude as f64
                    * (2.0 * std::f64::consts::PI * freq * i as f64 / SR as f64).sin())
                    as f32
            })
            .collect()
    }

    /// Feed `signal` to `node` in BLOCK_SIZE chunks; return the last emitted f0 value.
    fn run_and_last_f0(node: &mut PitchYin, signal: &[f32]) -> f32 {
        let mut last_f0 = 0.0f32;
        let mut out_buf = vec![0.0f32; BLOCK_SIZE];
        let n_blocks = signal.len().div_ceil(BLOCK_SIZE);
        for b in 0..n_blocks {
            let start = b * BLOCK_SIZE;
            let end = (start + BLOCK_SIZE).min(signal.len());
            let nframes = end - start;
            let block = &signal[start..end];
            out_buf[..nframes].fill(0.0);
            node.process(&[block], &mut [&mut out_buf[..nframes]], nframes);
            last_f0 = out_buf[nframes - 1];
        }
        last_f0
    }

    #[test]
    fn sine_440_within_1hz() {
        // 8 windows worth of audio to get several estimates.
        let signal = sine(440.0, 0.5, 2048 * 8);
        let mut node = make_node(50.0, 2000.0);
        let f0 = run_and_last_f0(&mut node, &signal);
        assert!(
            (f0 - 440.0).abs() < 1.0,
            "expected f0 near 440 Hz, got {f0}"
        );
    }

    #[test]
    fn sine_441_3_parabolic_within_0_3hz() {
        // 4 windows of audio at 441.3 Hz. Integer tau alone gives >1 Hz error at this
        // freq/sr combination; parabolic interpolation must push it under 0.3 Hz.
        let signal = sine(441.3, 0.5, 2048 * 4);
        let mut node = make_node(50.0, 2000.0);
        let f0 = run_and_last_f0(&mut node, &signal);
        assert!(
            (f0 as f64 - 441.3).abs() < 0.3,
            "expected f0 within 0.3 Hz of 441.3, got {f0}"
        );
    }

    #[test]
    fn silence_is_unvoiced() {
        let signal = vec![0.0f32; 4096];
        let mut node = make_node(50.0, 2000.0);
        let f0 = run_and_last_f0(&mut node, &signal);
        assert_eq!(f0, 0.0, "expected unvoiced (0.0) for silence, got {f0}");
    }

    #[test]
    fn low_e_82hz_within_1hz() {
        // Low E string: ~82 Hz. Several windows.
        let signal = sine(82.0, 0.5, 2048 * 8);
        let mut node = make_node(50.0, 2000.0);
        let f0 = run_and_last_f0(&mut node, &signal);
        assert!((f0 - 82.0).abs() < 1.0, "expected f0 near 82 Hz, got {f0}");
    }

    #[test]
    fn high_a6_1760hz_within_2hz() {
        // A6 = 1760 Hz. tau ≈ 27.3 at sr=48000, tau_min=24 at fmax_hz=2000.
        // tau_search_min is dropped to floor(48000/4000)=12 internally so CMND has
        // enough pre-fundamental conditioning to suppress the octave-down dip at tau≈54.
        let signal = sine(1760.0, 0.5, 2048 * 8);
        let mut node = make_node(50.0, 2000.0);
        let f0 = run_and_last_f0(&mut node, &signal);
        assert!(
            (f0 - 1760.0).abs() < 2.0,
            "expected f0 within 2 Hz of 1760, got {f0}"
        );
    }

    #[test]
    fn near_ceiling_1975hz_within_3hz() {
        // 1975 Hz sits just below the default fmax_hz=2000 ceiling.
        // tau ≈ 24.3 at sr=48000. Verifies the node reports correctly near its upper
        // bound and that the tau_min acceptance check does not false-reject it.
        let signal = sine(1975.0, 0.5, 2048 * 8);
        let mut node = make_node(50.0, 2000.0);
        let f0 = run_and_last_f0(&mut node, &signal);
        assert!(
            (f0 as f64 - 1975.0).abs() < 3.0,
            "expected f0 within 3 Hz of 1975, got {f0}"
        );
    }
}
