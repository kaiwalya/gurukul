//! Vibrato analyzer: estimates `(rate_hz, depth_cents)` from an f0 contour.
//!
//! Consumes a Feature port carrying YIN-style f0 estimates (Hz, with `0.0` =
//! unvoiced sentinel). Maintains a ring buffer of the most recent samples'
//! worth of f0 values. Every `analysis_hop` blocks, runs one analysis on the
//! current window and updates zero-order-held estimates of vibrato rate and
//! depth. Outputs are Feature ports filled with the latest held value each
//! block.
//!
//! Algorithm: FFT-of-contour (robust to note steps and harmonic locking)
//!   1. Decimate the f0 ring by `decimation` (default 256× → 187.5 Hz contour
//!      rate at 48 kHz audio). Collapses redundant work with no information
//!      loss for vibrato frequencies ≪ contour_sr / 2.
//!   2. Walk the full decimated time grid. Voiced slots are converted to cents
//!      (1200 × log2(f)); unvoiced gaps are filled by linear interpolation
//!      between the nearest voiced neighbours (or held from the boundary if a
//!      neighbour is missing). This preserves uniform spacing so the FFT
//!      frequency axis remains correct. The voiced_frac guard (≥ 0.5) still
//!      rejects windows that are mostly unvoiced.
//!   3. Mean-centre → linear detrend (removes note glides / slow drift).
//!   4. Apply a Hann window; accumulate the actual window sum (used for
//!      physically-calibrated depth in step 7).
//!   5. Zero-pad to FFT_SIZE (512), execute a pre-planned real-FFT.
//!      Note: window_samples / decimation must be ≤ FFT_SIZE (512); with the
//!      default decimation=256 and window_samples=72000 the decimated length
//!      is 281 — comfortably under 512. Reduce window_samples or raise
//!      decimation if you need to lower the decimated count.
//!   6. Find the peak bin in the rate_min–rate_max band; parabolic interpolation
//!      on log-magnitude for sub-bin precision. Interpolated peak height is
//!      used for both depth and the dominance check.
//!   7. Depth (cents) = 2 × interp_peak_mag / window_sum.
//!      Robust to note steps inside the window (only the vibrato frequency's
//!      energy contributes, not the step amplitude). Uses the actual Hann
//!      window sum (not a fixed coherent-gain approximation) for correctness
//!      when the voiced window is shorter than FFT_SIZE.
//!   8. Reject if voiced_frac < 0.5, n_voiced < 32, no clear in-band peak
//!      (interpolated peak < 2× the median of in-band magnitudes excluding
//!      the peak bin's immediate neighbours).
//!
//! Realtime constraint: NO heap allocation in process()/analyse().
//! All scratch is pre-sized in new() and reused.

use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

/// Fixed FFT length. Must be a power of two. 512 gives bin resolution of
/// 187.5/512 ≈ 0.366 Hz at 187.5 Hz contour rate — more than adequate for
/// vibrato analysis. With parabolic interpolation resolution improves further.
const FFT_SIZE: usize = 512;

pub struct Vibrato {
    // Parameters resolved at construction.
    window_samples: usize,
    analysis_hop: usize,
    decimation: usize,
    rate_min_hz: f32,
    rate_max_hz: f32,

    // Set in prepare().
    sample_rate: f32,

    // Ring buffer of the most recent f0 values, sampled at the audio rate.
    ring: Vec<f32>,
    ring_write: usize,
    samples_since_analysis: usize,
    total_samples: usize,

    // Last held estimates (zero-order hold).
    held_rate: f32,
    held_depth: f32,

    // --- Scratch buffers, all pre-sized in new() ---
    /// Full decimated time grid (n_decim entries). Voiced slots hold cents;
    /// unvoiced slots are filled by linear interpolation in analyse().
    /// This ensures uniform spacing on the FFT input.
    grid: Vec<f32>,

    /// Detrended, Hann-windowed signal ready for the FFT (n_decim ≤ FFT_SIZE
    /// entries followed by zero-pad). Kept separate from `grid` so the
    /// detrending step can read the mean-centred values from `grid` while
    /// writing here.
    detrend: Vec<f32>,

    /// FFT input/output buffer: FFT_SIZE complex interleaved f32 [re, im, ...].
    /// Length = FFT_SIZE * 2.
    fft_buf: Vec<f32>,

    /// Magnitude scratch: FFT_SIZE / 2 + 1 bins (single-sided).
    fft_mag: Vec<f32>,

    /// Scratch for the in-band dominance check: a copy of the in-band magnitudes
    /// that can be partially sorted without allocating. Sized to the maximum
    /// possible band width (FFT_SIZE / 2 + 1 bins covers the whole spectrum).
    mag_scratch: Vec<f32>,

    /// Pre-computed twiddle factors for the radix-2 DIT FFT.
    /// Entry k = (cos(−2π k/FFT_SIZE), sin(−2π k/FFT_SIZE)).
    /// Length = FFT_SIZE / 2 (only the first half is needed by Cooley-Tukey).
    twiddle: Vec<(f32, f32)>,

    /// Bit-reversal permutation table for the radix-2 DIT FFT.
    /// Entry k = bit-reversed index of k. Length = FFT_SIZE.
    bit_rev: Vec<usize>,
}

impl Vibrato {
    pub fn new(
        window_samples: usize,
        analysis_hop: usize,
        decimation: usize,
        rate_min_hz: f32,
        rate_max_hz: f32,
    ) -> Self {
        let decimation = decimation.max(1);
        let decim_cap = window_samples / decimation;

        // Pre-compute twiddle factors: W_k = exp(-j * 2π * k / FFT_SIZE).
        let mut twiddle = Vec::with_capacity(FFT_SIZE / 2);
        for k in 0..FFT_SIZE / 2 {
            let angle = -2.0 * std::f32::consts::PI * k as f32 / FFT_SIZE as f32;
            twiddle.push((angle.cos(), angle.sin()));
        }

        // Pre-compute bit-reversal table.
        let log2_n = FFT_SIZE.ilog2() as usize;
        let mut bit_rev = Vec::with_capacity(FFT_SIZE);
        for k in 0..FFT_SIZE {
            let mut rev = 0usize;
            let mut x = k;
            for _ in 0..log2_n {
                rev = (rev << 1) | (x & 1);
                x >>= 1;
            }
            bit_rev.push(rev);
        }

        Self {
            window_samples,
            analysis_hop,
            decimation,
            rate_min_hz,
            rate_max_hz,
            sample_rate: 48000.0,
            ring: vec![0.0; window_samples],
            ring_write: 0,
            samples_since_analysis: 0,
            total_samples: 0,
            held_rate: 0.0,
            held_depth: 0.0,
            grid: vec![0.0; decim_cap],
            detrend: vec![0.0; decim_cap],
            // Interleaved complex: 2 f32 per bin.
            fft_buf: vec![0.0; FFT_SIZE * 2],
            // Single-sided magnitudes.
            fft_mag: vec![0.0; FFT_SIZE / 2 + 1],
            // In-band dominance scratch: worst case is the whole single-sided spectrum.
            mag_scratch: vec![0.0; FFT_SIZE / 2 + 1],
            twiddle,
            bit_rev,
        }
    }

    /// In-place radix-2 DIT FFT on the interleaved complex buffer `buf`
    /// (length = FFT_SIZE * 2: [re0, im0, re1, im1, ...]).
    ///
    /// Uses the pre-computed twiddle factors and bit-reversal table stored in
    /// `self.twiddle` and `self.bit_rev`. Zero heap allocation.
    fn fft_inplace(&mut self) {
        let n = FFT_SIZE;
        let buf = &mut self.fft_buf;

        // Bit-reversal permutation.
        for i in 0..n {
            let j = self.bit_rev[i];
            if i < j {
                buf.swap(2 * i, 2 * j);
                buf.swap(2 * i + 1, 2 * j + 1);
            }
        }

        // Cooley-Tukey butterfly stages.
        let mut half = 1usize;
        while half < n {
            let step = half * 2;
            let twiddle_stride = n / step; // maps butterfly index → twiddle table
            for k in (0..n).step_by(step) {
                for j in 0..half {
                    let tw = self.twiddle[j * twiddle_stride];
                    let u_re = buf[2 * (k + j)];
                    let u_im = buf[2 * (k + j) + 1];
                    let v_re = buf[2 * (k + j + half)];
                    let v_im = buf[2 * (k + j + half) + 1];
                    // t = twiddle * v
                    let t_re = tw.0 * v_re - tw.1 * v_im;
                    let t_im = tw.0 * v_im + tw.1 * v_re;
                    buf[2 * (k + j)] = u_re + t_re;
                    buf[2 * (k + j) + 1] = u_im + t_im;
                    buf[2 * (k + j + half)] = u_re - t_re;
                    buf[2 * (k + j + half) + 1] = u_im - t_im;
                }
            }
            half = step;
        }
    }

    /// Run one analysis on the current window contents, writing results into
    /// `(self.held_rate, self.held_depth)`. Allocation-free: all scratch is
    /// pre-sized in new() and reused.
    fn analyse(&mut self) {
        let n_decim = self.window_samples / self.decimation;
        debug_assert!(
            n_decim <= FFT_SIZE,
            "window_samples/decimation ({n_decim}) must be ≤ FFT_SIZE ({FFT_SIZE}); \
             reduce window_samples or increase decimation"
        );

        // 1. Build the full decimated time grid in cents.
        //    Voiced slots are converted directly; unvoiced slots (f0 == 0) are
        //    filled by linear interpolation between the nearest voiced neighbours
        //    so the FFT sees a uniformly-spaced signal with correct time axis.
        //    Two passes: first collect raw cents (0.0 = unvoiced sentinel),
        //    then interpolate gaps.

        let mut n_voiced = 0usize;
        for k in 0..n_decim {
            let idx = (self.ring_write + k * self.decimation) % self.window_samples;
            let f = self.ring[idx];
            if f > 0.0 {
                self.grid[k] = 1200.0 * f.log2();
                n_voiced += 1;
            } else {
                self.grid[k] = 0.0; // sentinel
            }
        }

        let voiced_frac = n_voiced as f32 / n_decim as f32;
        if voiced_frac < 0.5 || n_voiced < 32 {
            self.held_rate = 0.0;
            self.held_depth = 0.0;
            return;
        }

        // Linear interpolation to fill unvoiced gaps.
        // Strategy: find the first voiced sample, back-fill head from it,
        // then forward-scan interpolating between each voiced pair,
        // then forward-fill any trailing tail.
        {
            // Find first and last voiced index.
            let first_voiced = (0..n_decim).find(|&k| self.grid[k] != 0.0);
            let last_voiced = (0..n_decim).rev().find(|&k| self.grid[k] != 0.0);
            let (first_v, last_v) = match (first_voiced, last_voiced) {
                (Some(f), Some(l)) => (f, l),
                _ => {
                    // No voiced samples at all (shouldn't reach here after voiced_frac guard).
                    self.held_rate = 0.0;
                    self.held_depth = 0.0;
                    return;
                }
            };

            // Back-fill head (hold first voiced value).
            let head_val = self.grid[first_v];
            for k in 0..first_v {
                self.grid[k] = head_val;
            }

            // Forward-fill tail (hold last voiced value).
            let tail_val = self.grid[last_v];
            for k in (last_v + 1)..n_decim {
                self.grid[k] = tail_val;
            }

            // Interpolate interior gaps between consecutive voiced anchors.
            let mut anchor = first_v;
            while anchor < last_v {
                // Find next voiced sample after anchor.
                let next = match (anchor + 1..=last_v).find(|&k| {
                    // Re-check against the original voiced status: a slot is
                    // voiced if it was set to a non-zero value before
                    // interpolation. After head/tail fill all slots in
                    // [first_v..=last_v] that were 0.0 sentinel are still
                    // 0.0 (we only wrote to head/tail outside that range
                    // above). Slots inside that were voiced are non-zero.
                    // But wait — we haven't written any interpolated values
                    // yet for the interior, so 0.0 still means unvoiced here.
                    self.grid[k] != 0.0 || k == last_v
                }) {
                    Some(n) => n,
                    None => break,
                };
                if next > anchor + 1 {
                    // Linear interpolation between grid[anchor] and grid[next].
                    let v0 = self.grid[anchor];
                    let v1 = self.grid[next];
                    let gap = (next - anchor) as f32;
                    for k in (anchor + 1)..next {
                        let t = (k - anchor) as f32 / gap;
                        self.grid[k] = v0 + t * (v1 - v0);
                    }
                }
                anchor = next;
            }
        }

        // 2. Mean-centre the grid in place.
        let mut sum = 0.0f32;
        for k in 0..n_decim {
            sum += self.grid[k];
        }
        let mean = sum / n_decim as f32;
        for k in 0..n_decim {
            self.grid[k] -= mean;
        }

        // 3. Linear detrend (remove note glide / slow drift).
        //    Fit y = b*i (slope only; mean already removed → intercept = 0).
        //    b = sum(i * c_i) / (sum(i^2) - n * mean_i^2)
        let n_f = n_decim as f32;
        let mean_i = (n_f - 1.0) * 0.5;
        let mut sum_ic = 0.0f32;
        let mut sum_i2 = 0.0f32;
        for (i, &c) in self.grid[..n_decim].iter().enumerate() {
            let fi = i as f32;
            sum_ic += fi * c;
            sum_i2 += fi * fi;
        }
        let denom = sum_i2 - n_f * mean_i * mean_i;
        let b = if denom.abs() > 1e-6 {
            sum_ic / denom
        } else {
            0.0
        };

        let mut sum_sq = 0.0f32;
        for (i, &c) in self.grid[..n_decim].iter().enumerate() {
            let d = c - b * (i as f32 - mean_i);
            self.detrend[i] = d;
            sum_sq += d * d;
        }

        // RMS guard — below 1 cent RMS we're chasing f32 roundoff.
        let rms = (sum_sq / n_decim as f32).sqrt();
        if rms < 1.0 {
            self.held_rate = 0.0;
            self.held_depth = 0.0;
            return;
        }

        // 4. Apply Hann window and zero-pad to FFT_SIZE.
        //    Accumulate the actual window sum for depth normalisation (step 7).
        //    Using the actual sum (rather than a fixed 0.5 coherent-gain
        //    approximation) keeps depth correct when n_decim < FFT_SIZE.
        for v in self.fft_buf.iter_mut() {
            *v = 0.0;
        }
        let mut window_sum = 0.0f32;
        for i in 0..n_decim {
            let hann =
                0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (n_decim - 1) as f32).cos());
            self.fft_buf[2 * i] = self.detrend[i] * hann;
            // imaginary part stays 0.0 (set by the zero-init above)
            window_sum += hann;
        }

        // 5. FFT.
        self.fft_inplace();

        // 6. Compute single-sided magnitudes.
        for bin in 0..=FFT_SIZE / 2 {
            let re = self.fft_buf[2 * bin];
            let im = self.fft_buf[2 * bin + 1];
            self.fft_mag[bin] = (re * re + im * im).sqrt();
        }

        // 7. Find peak bin in [rate_min_hz, rate_max_hz] band.
        //    Bin k corresponds to frequency k * contour_sr / FFT_SIZE.
        let contour_sr = self.sample_rate / self.decimation as f32;
        let bin_hz = contour_sr / FFT_SIZE as f32;
        let bin_min = (self.rate_min_hz / bin_hz).ceil() as usize;
        let bin_max = ((self.rate_max_hz / bin_hz).floor() as usize).min(FFT_SIZE / 2);

        if bin_max <= bin_min {
            self.held_rate = 0.0;
            self.held_depth = 0.0;
            return;
        }

        // Find integer peak bin index and value in band.
        let mut peak_bin = bin_min;
        let mut peak_mag = self.fft_mag[bin_min];
        for bin in (bin_min + 1)..=bin_max {
            if self.fft_mag[bin] > peak_mag {
                peak_mag = self.fft_mag[bin];
                peak_bin = bin;
            }
        }

        // 8. Parabolic interpolation on log-magnitude for sub-bin rate precision
        //    and interpolated peak height (used for depth and dominance check).
        //    Uses the 3-bin neighbourhood around peak_bin.
        let (refined_bin, interp_peak_mag) = if peak_bin > bin_min && peak_bin < bin_max {
            let mag_l = self.fft_mag[peak_bin - 1].max(1e-12);
            let mag_c = peak_mag.max(1e-12);
            let mag_r = self.fft_mag[peak_bin + 1].max(1e-12);
            let log_l = mag_l.ln();
            let log_c = mag_c.ln();
            let log_r = mag_r.ln();
            let denom_par = log_l - 2.0 * log_c + log_r;
            let delta = if denom_par.abs() > 1e-12 {
                // Clamp interpolation shift to ±0.5 bins.
                (0.5 * (log_l - log_r) / denom_par).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            // Interpolated peak height from parabola: exp(log_c - delta^2 * denom/2)
            // This is the log-parabola peak value at the fractional bin.
            let interp_log_peak = log_c - delta * delta * denom_par * 0.5;
            (peak_bin as f32 + delta, interp_log_peak.exp())
        } else {
            (peak_bin as f32, peak_mag)
        };

        // 9. Dominance check: the interpolated peak must exceed 2× the median
        //    of in-band magnitudes, EXCLUDING the peak bin and its immediate
        //    neighbours (±1 bin). Including the peak inflates the mean and
        //    weakens the reject; using median instead of mean is robust to the
        //    few large-magnitude harmonics that can appear in noisy windows.
        //
        //    Implementation: copy in-band magnitudes to mag_scratch (excluding
        //    peak ±1), find the median via partial sort up to the midpoint.
        //    No allocation — mag_scratch is pre-sized.
        let mut scratch_len = 0usize;
        for bin in bin_min..=bin_max {
            let dist = (bin as isize - peak_bin as isize).unsigned_abs();
            if dist > 1 {
                self.mag_scratch[scratch_len] = self.fft_mag[bin];
                scratch_len += 1;
            }
        }

        // Median of mag_scratch[..scratch_len] via partial selection sort.
        // Only sort up to the median index; O(n * n/2) but n ≤ ~26 bins so fine.
        let median_val = if scratch_len == 0 {
            0.0f32
        } else {
            let mid = scratch_len / 2;
            // Partial selection sort: bring the mid-th smallest to index mid.
            for i in 0..=mid {
                let mut min_idx = i;
                for j in (i + 1)..scratch_len {
                    if self.mag_scratch[j] < self.mag_scratch[min_idx] {
                        min_idx = j;
                    }
                }
                self.mag_scratch.swap(i, min_idx);
            }
            if scratch_len % 2 == 1 {
                self.mag_scratch[mid]
            } else {
                // Even length: average of mid-1 and mid (mid-1 already in place
                // from the sort).
                (self.mag_scratch[mid - 1] + self.mag_scratch[mid]) * 0.5
            }
        };

        // K=2: on pure noise all in-band bins have nearly equal magnitude, so
        // peak ≈ median and the ratio is ~1. A real vibrato peak is typically
        // 3-10× the surrounding median; 2× gives a comfortable margin while
        // accepting moderately noisy real-audio windows.
        if interp_peak_mag < 2.0 * median_val {
            self.held_rate = 0.0;
            self.held_depth = 0.0;
            return;
        }

        let rate = refined_bin * bin_hz;

        // 10. Depth from interpolated FFT bin magnitude.
        //    For a Hann-windowed real sinusoid of amplitude A, the single-sided
        //    FFT bin magnitude = A * window_sum / 2
        //    (window_sum = sum of Hann weights; the /2 is from the cos→exp split
        //    of the real sinusoid). Rearranged: A = 2 * peak_mag / window_sum.
        //    Using the actual window_sum (rather than n * 0.5) keeps depth
        //    correct when the windowed length differs from the nominal value.
        let depth = 2.0 * interp_peak_mag / window_sum;

        self.held_rate = rate;
        self.held_depth = depth;
    }
}

impl Node for Vibrato {
    fn prepare(&mut self, _id: &str, sample_rate: u32, _block_size: usize) {
        self.sample_rate = sample_rate as f32;
        self.ring_write = 0;
        self.samples_since_analysis = 0;
        self.total_samples = 0;
        self.held_rate = 0.0;
        self.held_depth = 0.0;
        for v in self.ring.iter_mut() {
            *v = 0.0;
        }
    }

    fn reset(&mut self) {
        self.ring_write = 0;
        self.samples_since_analysis = 0;
        self.total_samples = 0;
        self.held_rate = 0.0;
        self.held_depth = 0.0;
        for v in self.ring.iter_mut() {
            *v = 0.0;
        }
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if inputs.is_empty() || outputs.len() < 2 {
            return;
        }
        let f0_in = &inputs[0][..nframes];

        for &f in f0_in {
            self.ring[self.ring_write] = f;
            self.ring_write = (self.ring_write + 1) % self.window_samples;
            self.samples_since_analysis += 1;
            self.total_samples += 1;

            if self.samples_since_analysis >= self.analysis_hop
                && self.total_samples >= self.window_samples
            {
                self.analyse();
                self.samples_since_analysis = 0;
            }
        }

        // ZOH: fill outputs with the latest held estimates.
        outputs[0][..nframes].fill(self.held_rate);
        outputs[1][..nframes].fill(self.held_depth);
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "Vibrato",
        vec![PortSpec {
            name: "f0",
            ty: PortType::Feature,
        }],
        vec![
            PortSpec {
                name: "rate",
                ty: PortType::Feature,
            },
            PortSpec {
                name: "depth",
                ty: PortType::Feature,
            },
        ],
        vec![
            ParamSpec {
                name: "window_samples",
                default: 72000.0, // 1.5 s at 48 kHz — enough for ~7-10 vibrato cycles at 5 Hz.
                min: 4096.0,
                max: 192000.0,
                unit: "samples",
            },
            ParamSpec {
                name: "analysis_hop",
                default: 4800.0, // 0.1 s at 48 kHz.
                min: 256.0,
                max: 48000.0,
                unit: "samples",
            },
            ParamSpec {
                name: "decimation",
                // YIN's default hop is 512; decimating by 256 gives a 187.5 Hz
                // contour rate at 48 kHz audio — well above Nyquist for any
                // realistic vibrato (≤ 20 Hz) while collapsing FFT work by 256×.
                // Constraint: window_samples / decimation must be ≤ FFT_SIZE (512).
                // With defaults (72000 / 256 = 281) this is satisfied with margin.
                default: 256.0,
                min: 1.0,
                max: 4096.0,
                unit: "samples",
            },
            ParamSpec {
                name: "rate_min_hz",
                // Lower edge of the FFT vibrato-search band. 4 Hz is the
                // practical lower bound for human vibrato; lower values let
                // slow note-transition drift (detrend residual) pollute the
                // band and capture sub-vibrato artifacts.
                default: 4.0,
                min: 0.5,
                max: 20.0,
                unit: "Hz",
            },
            ParamSpec {
                name: "rate_max_hz",
                // Upper vibrato rate bound. Defines the FFT search band's
                // upper edge. 10 Hz covers fast vibrato; the FFT method is
                // not susceptible to harmonic locking so a wider band is safe.
                default: 10.0,
                min: 1.0,
                max: 30.0,
                unit: "Hz",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let window_samples = *params.get("window_samples").unwrap_or(&72000.0) as usize;
            let analysis_hop = *params.get("analysis_hop").unwrap_or(&4800.0) as usize;
            let decimation = *params.get("decimation").unwrap_or(&256.0) as usize;
            let rate_min_hz = *params.get("rate_min_hz").unwrap_or(&4.0) as f32;
            let rate_max_hz = *params.get("rate_max_hz").unwrap_or(&10.0) as f32;
            Box::new(Vibrato::new(
                window_samples,
                analysis_hop,
                decimation,
                rate_min_hz,
                rate_max_hz,
            )) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesise an f0 contour for `seconds` of audio at `sr` Hz with vibrato
    /// of `rate_hz` and `depth_cents`. Returns one f0 value per audio sample
    /// (ZOH-equivalent — matches how YIN fills its f0 port).
    fn synth_f0_contour(
        sr: u32,
        seconds: f32,
        carrier_hz: f32,
        rate_hz: f32,
        depth_cents: f32,
    ) -> Vec<f32> {
        let n = (sr as f32 * seconds) as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            // f(t) = carrier * 2^((depth/1200) * sin(2π rate t))
            let inst = carrier_hz
                * 2.0f32.powf((depth_cents / 1200.0) * (std::f32::consts::TAU * rate_hz * t).sin());
            out.push(inst);
        }
        out
    }

    fn run_through_blocks(node: &mut Vibrato, contour: &[f32], block_size: usize) -> (f32, f32) {
        let mut last_rate = 0.0f32;
        let mut last_depth = 0.0f32;
        for chunk in contour.chunks(block_size) {
            let nframes = chunk.len();
            let mut rate_out = vec![0.0f32; nframes];
            let mut depth_out = vec![0.0f32; nframes];
            {
                let mut outs: Vec<&mut [f32]> =
                    vec![rate_out.as_mut_slice(), depth_out.as_mut_slice()];
                node.process(&[chunk], &mut outs, nframes);
            }
            last_rate = rate_out[nframes - 1];
            last_depth = depth_out[nframes - 1];
        }
        (last_rate, last_depth)
    }

    #[test]
    fn recovers_5hz_50cent_vibrato() {
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 4.0, 10.0);
        node.prepare("test", sr, 512);

        let contour = synth_f0_contour(sr, 2.5, 440.0, 5.0, 50.0);
        let (rate, depth) = run_through_blocks(&mut node, &contour, 512);

        // Rate within 0.5 Hz, depth within 10 cents.
        assert!((rate - 5.0).abs() < 0.5, "rate {rate} should be ~5.0 Hz");
        assert!(
            (depth - 50.0).abs() < 10.0,
            "depth {depth} should be ~50 cents"
        );
    }

    #[test]
    fn recovers_6hz_30cent_vibrato() {
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 4.0, 10.0);
        node.prepare("test", sr, 512);

        let contour = synth_f0_contour(sr, 2.5, 220.0, 6.0, 30.0);
        let (rate, depth) = run_through_blocks(&mut node, &contour, 512);

        assert!((rate - 6.0).abs() < 0.5, "rate {rate} should be ~6.0 Hz");
        assert!(
            (depth - 30.0).abs() < 10.0,
            "depth {depth} should be ~30 cents"
        );
    }

    #[test]
    fn flat_contour_yields_zero_outputs() {
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 4.0, 10.0);
        node.prepare("test", sr, 512);

        // Constant 440 Hz, no vibrato.
        let contour = vec![440.0f32; sr as usize * 2];
        let (rate, depth) = run_through_blocks(&mut node, &contour, 512);

        // Depth should be ~0 cents; FFT dominance check should reject and return 0.
        assert!(
            depth.abs() < 5.0,
            "depth {depth} should be ~0 for flat contour"
        );
        assert_eq!(rate, 0.0, "rate should be 0 when there's no modulation");
    }

    #[test]
    fn all_unvoiced_yields_zero() {
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 4.0, 10.0);
        node.prepare("test", sr, 512);

        let contour = vec![0.0f32; sr as usize * 2];
        let (rate, depth) = run_through_blocks(&mut node, &contour, 512);

        assert_eq!(rate, 0.0);
        assert_eq!(depth, 0.0);
    }

    #[test]
    fn note_step_depth_is_detrended() {
        // A 150-cent linear pitch glide across the 1.5 s window (440 → 564 Hz)
        // superimposed with 5 Hz / 40 cent vibrato. Without detrend the peak-
        // to-peak range of the centred contour is dominated by the glide
        // (~75 cents half-range) rather than the vibrato (40 cents). With
        // linear detrend the slope is removed and depth recovers ~40 cents.
        //
        // With the FFT algorithm, depth is read from the vibrato bin magnitude,
        // not peak-to-peak, so the glide has minimal effect regardless.
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 4.0, 10.0);
        node.prepare("test", sr, 512);

        let n = 72000usize * 2; // 3 s, so the last window is fully glide-overlapping
        let mut contour = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            // Carrier glides from 440 Hz (+0 c) to 564 Hz (+430 c) over 3 s.
            let glide_cents = 143.0_f32 * t;
            let vib_cents = 40.0_f32 * (std::f32::consts::TAU * 5.0 * t).sin();
            let f = 440.0_f32 * 2.0f32.powf((glide_cents + vib_cents) / 1200.0);
            contour.push(f);
        }

        let (rate, depth) = run_through_blocks(&mut node, &contour, 512);

        assert!(
            (rate - 5.0).abs() < 0.7,
            "rate {rate} Hz should be ~5 Hz across a pitch glide"
        );
        // FFT bin depth: should reflect vibrato amplitude (~40 cents).
        assert!(
            depth < 60.0,
            "depth {depth} cents should be ~40 (glide removed by detrend), not much larger"
        );
        assert!(
            depth > 15.0,
            "depth {depth} cents should be at least 15 (vibrato present)"
        );
    }

    #[test]
    fn jittered_contour_still_recovers_rate() {
        // 5 Hz / 40-cent vibrato with YIN-like tracker-error spikes: ~30 % of
        // decimated blocks carry a ±100-cent error. The FFT method concentrates
        // the vibrato energy in a single bin and is more robust to broadband
        // jitter than the autocorr picker.
        //
        // Fixed-seed LCG — deterministic, no rand crate.
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 4.0, 10.0);
        node.prepare("test", sr, 512);

        let seconds = 2.5f32;
        let n = (sr as f32 * seconds) as usize;
        let mut contour = Vec::with_capacity(n);

        let mut lcg: u32 = 0xDEAD_BEEF;
        let decimation = 256usize;
        for blk in 0..n / decimation {
            let t_blk = blk as f32 * decimation as f32 / sr as f32;
            lcg = lcg.wrapping_mul(1664525).wrapping_add(1013904223);
            let spike_cents = if (lcg >> 16) < 20000 {
                100.0f32
            } else {
                0.0f32
            };
            for j in 0..decimation {
                let t = t_blk + j as f32 / sr as f32;
                let vib = 40.0_f32 / 1200.0 * (std::f32::consts::TAU * 5.0 * t).sin();
                let f = 440.0_f32 * 2.0f32.powf(vib + spike_cents / 1200.0);
                contour.push(f);
            }
        }

        let (rate, _depth) = run_through_blocks(&mut node, &contour, 512);

        assert!(
            (rate - 5.0).abs() < 0.7,
            "rate {rate} Hz should be ~5 Hz even with tracker-error spikes"
        );
    }

    /// A window with a 30%-voiced gap punched into the middle must still recover
    /// the correct rate. With the old pack-and-pretend approach the voiced samples
    /// are compressed in time, biasing the rate upward. With the gridded approach
    /// (linear interpolation over gaps) the time axis is preserved.
    ///
    /// Fail-before-fix proof: run `cargo test -p node-vibrato --release
    /// unvoiced_gap_does_not_bias_rate` on the pack-and-pretend code; rate will
    /// be biased to ~7+ Hz. With the grid+interpolation fix it recovers ~5 Hz.
    #[test]
    fn unvoiced_gap_does_not_bias_rate() {
        // 5 Hz / 40-cent vibrato on a 440 Hz carrier for 2.5 s.
        // A contiguous block of ~30% of the window (≈ 85 out of 281 decimated
        // slots, centred in the window) is zeroed to simulate an unvoiced gap.
        let sr = 48000u32;
        let decimation = 256usize;
        let mut node = Vibrato::new(72000, 4800, decimation, 4.0, 10.0);
        node.prepare("gap_test", sr, 512);

        let n = (sr as f32 * 2.5) as usize;
        let mut contour = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let inst =
                440.0f32 * 2.0f32.powf(40.0 / 1200.0 * (std::f32::consts::TAU * 5.0 * t).sin());
            contour.push(inst);
        }

        // Zero out a 30%-wide gap in the middle of the window at the decimated
        // level. The window is 72000 samples = 281 decimated slots.
        // Gap: decim slots 90..174 (84 slots ≈ 30%). In audio samples that is
        // slots 90*256..174*256.
        let gap_start = 90 * decimation;
        let gap_end = 174 * decimation;
        for s in gap_start..gap_end.min(contour.len()) {
            contour[s] = 0.0;
        }

        let (rate, _depth) = run_through_blocks(&mut node, &contour, 512);

        // With correct gridded interpolation the rate should stay ~5 Hz.
        // The pack-and-pretend code would compress time and report ~7+ Hz.
        assert!(
            (rate - 5.0).abs() < 0.7,
            "rate {rate} Hz should be ~5 Hz with a 30% unvoiced gap (gridded interp preserves time)"
        );
    }

    /// A contour with no periodic vibrato in the 4-8 Hz band (broadband random
    /// jitter only) must be rejected → rate=0, depth=0. This tests the
    /// dominance check: the peak-to-median ratio stays below 2× when the
    /// spectrum is flat.
    ///
    /// Construction: deterministic LCG noise in cents added to a constant
    /// 440 Hz carrier. No sinusoidal modulation at any frequency.
    #[test]
    fn noisy_no_vibrato_contour_rejects() {
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 4.0, 10.0);
        node.prepare("noise_test", sr, 512);

        let n = sr as usize * 2;
        let mut contour = Vec::with_capacity(n);
        let decimation = 256usize;
        let mut lcg: u32 = 0xC0FFEE42;
        // Each decimated block gets a ±5 cent random offset — enough RMS to
        // pass the 1-cent guard but no coherent periodicity.
        for _ in 0..n / decimation {
            lcg = lcg.wrapping_mul(1664525).wrapping_add(1013904223);
            // Map to [-5, +5] cents.
            let noise_cents = ((lcg >> 16) as f32 / 65535.0 - 0.5) * 10.0;
            let f = 440.0_f32 * 2.0f32.powf(noise_cents / 1200.0);
            for _ in 0..decimation {
                contour.push(f);
            }
        }

        let (rate, depth) = run_through_blocks(&mut node, &contour, 512);

        assert_eq!(
            rate, 0.0,
            "rate {rate} should be 0 for broadband noise (no vibrato)"
        );
        assert_eq!(
            depth, 0.0,
            "depth {depth} should be 0 for broadband noise (no vibrato)"
        );
    }

    /// 281-sample decimated f0 contour (Hz) extracted from sa-re-ga-ma-pa.wav,
    /// 0.80–2.30 s. Ground truth (Python FFT-of-contour): rate ≈ 4.3–5.0 Hz,
    /// depth ≈ 8–11 cents. The old autocorr picker locks to ~10.42 Hz (2× harmonic).
    ///
    /// Feed strategy: decimation=256, sample_rate=48000 → contour_sr=187.5 Hz.
    /// Each of the 281 Hz values is repeated 256× to form a 71936-sample
    /// audio-rate signal. window_samples=71936 so exactly one window fills.
    const REAL_CONTOUR_F0_HZ: [f32; 281] = [
        121.519, 121.683, 121.888, 122.385, 122.925, 123.601, 124.281, 124.970, 125.411, 125.585,
        125.760, 125.936, 125.984, 125.984, 125.836, 125.660, 125.654, 125.654, 125.654, 125.691,
        125.866, 126.101, 126.454, 126.810, 127.167, 127.424, 127.604, 127.785, 127.966, 128.148,
        128.330, 128.172, 128.000, 128.000, 127.937, 127.576, 127.215, 126.858, 126.575, 126.398,
        126.221, 126.045, 125.984, 125.984, 125.984, 125.984, 126.143, 126.323, 126.679, 127.060,
        127.601, 128.048, 128.230, 128.412, 128.595, 128.686, 128.686, 128.686, 128.686, 128.686,
        128.686, 128.686, 128.686, 128.506, 128.323, 128.141, 128.000, 128.000, 127.937, 127.756,
        127.660, 127.660, 127.553, 127.373, 127.193, 127.014, 126.984, 126.984, 126.814, 126.662,
        126.841, 126.984, 126.984, 126.984, 126.984, 127.062, 127.242, 127.321, 127.321, 127.321,
        127.321, 127.465, 127.645, 127.494, 127.321, 127.321, 127.350, 127.530, 127.609, 127.429,
        127.321, 127.321, 127.321, 127.321, 127.321, 127.321, 127.184, 127.005, 127.143, 127.322,
        127.502, 127.683, 127.864, 128.045, 128.227, 128.410, 128.593, 128.686, 128.686, 128.686,
        128.686, 128.686, 128.686, 128.530, 128.347, 128.165, 127.983, 127.802, 127.660, 127.660,
        127.720, 127.902, 128.000, 128.000, 128.105, 128.287, 128.470, 128.653, 128.837, 129.021,
        129.206, 129.380, 129.380, 129.380, 129.380, 129.324, 129.139, 129.111, 129.296, 129.380,
        129.380, 129.257, 129.072, 129.178, 129.363, 129.380, 129.380, 129.380, 129.380, 129.380,
        129.380, 129.380, 129.452, 129.639, 129.730, 129.730, 129.612, 129.426, 129.241, 129.056,
        128.872, 128.688, 128.869, 129.011, 128.827, 128.730, 128.914, 129.098, 129.283, 129.558,
        129.931, 130.194, 130.382, 130.435, 130.435, 130.593, 130.783, 130.973, 131.148, 131.148,
        131.148, 131.148, 131.148, 131.148, 131.148, 131.148, 131.148, 131.148, 131.148, 131.148,
        130.842, 130.463, 130.261, 130.081, 130.081, 130.081, 130.081, 130.136, 130.324, 130.512,
        130.701, 130.891, 131.081, 131.272, 131.463, 131.655, 131.847, 132.041, 132.231, 132.231,
        132.231, 132.231, 132.231, 132.231, 132.526, 133.310, 134.613, 136.443, 137.796, 138.854,
        139.449, 139.881, 140.697, 141.575, 142.474, 143.353, 144.042, 144.630, 144.862, 145.176,
        145.645, 146.006, 146.243, 146.481, 146.719, 146.958, 147.197, 147.239, 147.239, 147.013,
        146.773, 146.535, 146.297, 146.060, 145.824, 145.589, 145.354, 145.120, 144.760, 144.297,
        143.990, 143.760, 143.352, 142.897, 142.651, 142.426, 142.201, 142.046, 142.271, 142.495,
        142.721, 143.037, 143.493, 143.831, 144.061, 144.144, 144.144, 144.319, 144.550, 144.578,
        144.578,
    ];

    #[test]
    fn real_contour_recovers_true_vibrato_rate() {
        // Feed the real decimated contour through the node.
        // decimation=256, sample_rate=48000 → contour_sr=187.5 Hz (correct).
        // Each of the 281 Hz values is repeated 256× to form an audio-rate
        // signal; window_samples=281*256=71936 so exactly one window fills.
        // analysis_hop=71936 so analysis fires as soon as the window is full.
        const DECIM: usize = 256;
        const N_CONTOUR: usize = 281;
        const WIN: usize = N_CONTOUR * DECIM; // 71936
        let sr = 48000u32;
        let mut node = Vibrato::new(WIN, WIN, DECIM, 4.0, 10.0);
        node.prepare("real_contour_test", sr, 512);

        // Build audio-rate signal: repeat each decimated value DECIM times.
        let audio: Vec<f32> = REAL_CONTOUR_F0_HZ
            .iter()
            .flat_map(|&f| std::iter::repeat(f).take(DECIM))
            .collect();

        let (rate, depth) = run_through_blocks(&mut node, &audio, 512);

        // Ground truth (Python FFT-of-contour): rate ≈ 4.3 Hz on this window,
        // depth ≈ 8–11 cents. Old autocorr picker reports ~10.42 Hz (2× harmonic).
        // Band [4.0, 6.5] covers the Python-verified truth (4.28 Hz on this slice)
        // and excludes the harmonic lock at ~10 Hz.
        assert!(
            (4.0..=6.5).contains(&rate),
            "rate {rate:.3} Hz should be in 4.0–6.5 Hz (harmonic lock = ~10 Hz, truth = ~4.3 Hz)"
        );
        assert!(
            (5.0..=40.0).contains(&depth),
            "depth {depth:.1} cents should be in 5–40 cents (truth ≈ 8–11 cents)"
        );
    }
}
