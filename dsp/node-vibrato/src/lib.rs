//! Vibrato analyzer: estimates `(rate_hz, amplitude_cents, phase_rad)` from an
//! f0 contour.
//!
//! Consumes a Feature port carrying YIN-style f0 estimates (Hz, with `0.0` =
//! unvoiced sentinel). Maintains a ring buffer of the most recent samples'
//! worth of f0 values. Every `analysis_hop` blocks, runs one analysis on the
//! current window and updates zero-order-held estimates of vibrato rate,
//! amplitude and phase. Outputs are Feature ports filled with the latest held
//! value each block.
//!
//! Output contract — the three ports reconstruct the wiggle as a phasor of the
//! emitted `phase` (radians) scaled by `amplitude` (cents):
//!   `f0_wiggle(t) ≈ amplitude · sin(phase(t))`
//! so a consumer recovers the centerline as `pitch − amplitude·sin(phase)`.
//! (`sin` is the trig function that matches this FFT's `atan2(im, re)` phase
//! reference — verified by the `phase_reconstructs_wiggle` test. A consumer is
//! free to absorb the fixed quarter-turn offset and use cos instead; the
//! load-bearing fact is that `phase` is a clean rotating phasor at the vibrato
//! rate, pinned to the analysis-window centre.)
//!
//! `amplitude` is the HALF-swing — the sinusoid amplitude `A` in `A·cos(phase)`,
//! i.e. the peak deviation from center, in cents. Peak-to-peak full swing =
//! `2·amplitude`, derived at use sites if needed. (NB: the separate generator
//! node `node-synth-vibrato-sine` has a `vibrato_depth_cents` param — that is a
//! *commanded* swing of a synthesizer, a different concept from this *measured*
//! amplitude; do not conflate the two.)
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
//!   7. Amplitude (cents) = 2 × interp_peak_mag / window_sum, then divided by
//!      the Hann main-lobe gain at the fractional bin offset to undo scalloping
//!      loss (see `hann_lobe_gain`).
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

/// Hann main-lobe amplitude at a fractional bin offset `delta` (in bins),
/// normalised to 1.0 at `delta = 0`.
///
/// Derivation. A length-N Hann window is `w[n] = ½ − ½·cos(2πn/N)`. Its DTFT is
/// a sum of three Dirichlet (periodic-sinc) kernels: one centred at the bin and
/// two half-weight copies shifted ±1 bin. Evaluating the magnitude of that sum
/// at a continuous frequency `delta` bins from the peak and dividing by its
/// value at `delta = 0` collapses (after the standard algebra) to the compact
/// closed form
///
/// ```text
///   hann_lobe_gain(delta) = sinc(delta) / (1 − delta²)
/// ```
///
/// with `sinc(x) = sin(πx)/(πx)` (so `sinc(0) = 1`). At `delta = 0` this is
/// `1/1 = 1` (identity — the clean on-bin case is preserved exactly); at the
/// worst-case half-bin offset `delta = ±0.5` it is `sinc(0.5)/0.75 =
/// (2/π)/0.75 ≈ 0.8488`, i.e. the peak bin reads ≈ 15% low, matching the known
/// −1.42 dB Hann scalloping loss. Dividing the measured amplitude by this gain
/// recovers the true amplitude regardless of where the vibrato lands between
/// bins.
fn hann_lobe_gain(delta: f32) -> f32 {
    let d = delta.abs();
    if d < 1e-6 {
        return 1.0;
    }
    let denom = 1.0 - d * d;
    // `denom` only vanishes at |delta| = 1, outside the clamped ±0.5 range, so
    // it is never near zero here; guard anyway for total safety.
    if denom.abs() < 1e-6 {
        return 1.0;
    }
    let x = std::f32::consts::PI * d;
    let sinc = x.sin() / x;
    sinc / denom
}

/// Wrap a radian angle to the half-open interval (−π, π].
fn wrap_pi(theta: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let mut t = theta % TAU;
    if t > PI {
        t -= TAU;
    } else if t <= -PI {
        t += TAU;
    }
    t
}

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
    held_amplitude: f32,
    held_phase: f32,

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
            held_amplitude: 0.0,
            held_phase: 0.0,
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
    /// `(self.held_rate, self.held_amplitude)`. Allocation-free: all scratch is
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
            self.held_amplitude = 0.0;
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
                    self.held_amplitude = 0.0;
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
            self.held_amplitude = 0.0;
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
            self.held_amplitude = 0.0;
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
        let (refined_bin, interp_peak_mag, peak_delta) = if peak_bin > bin_min && peak_bin < bin_max
        {
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
            (peak_bin as f32 + delta, interp_log_peak.exp(), delta)
        } else {
            (peak_bin as f32, peak_mag, 0.0)
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
            self.held_amplitude = 0.0;
            return;
        }

        let rate = refined_bin * bin_hz;

        // 10. Amplitude from the peak FFT bin magnitude, with scalloping-loss
        //    correction.
        //
        //    For a Hann-windowed real sinusoid of amplitude A, the single-sided
        //    FFT bin magnitude at the lobe peak = A * window_sum / 2 (window_sum =
        //    sum of Hann weights; the /2 is from the cos→exp split of the real
        //    sinusoid). Rearranged: A = 2 * peak_mag / window_sum. Using the
        //    actual window_sum (not n * 0.5) keeps it correct when the windowed
        //    length differs from the nominal value.
        //
        //    That is exact only when the vibrato frequency sits on a bin centre.
        //    At a fractional offset the Hann main lobe peaks BETWEEN bins, so the
        //    nearest sampled bin under-reads by the main-lobe gain at that offset
        //    (the scalloping loss). We divide the raw peak-bin magnitude by the
        //    analytic Hann main-lobe gain to undo it.
        //
        //    Offset units matter. `peak_delta` is the sub-bin offset on the
        //    512-point FFT grid. But the signal is only `n_decim` samples (zero-
        //    padded to FFT_SIZE), so the Hann main lobe is WIDE — it spans
        //    ≈4·FFT_SIZE/n_decim FFT bins. The scalloping the nearest bin suffers
        //    is therefore governed by the offset measured in LOBE-widths, i.e.
        //    `peak_delta · n_decim / FFT_SIZE`, not `peak_delta` directly. With
        //    the default 281/512 ratio a half-bin FFT offset is only ~0.27 lobe-
        //    bins → ~5% loss, matching measurement (the asymptotic 15% figure
        //    assumes a full-length, non-zero-padded window). The correction is
        //    identity at delta=0, so the on-bin / clean-sine case is unchanged.
        let lobe_delta = peak_delta * n_decim as f32 / FFT_SIZE as f32;
        let amplitude = 2.0 * peak_mag / (window_sum * hann_lobe_gain(lobe_delta));

        self.held_rate = rate;
        self.held_amplitude = amplitude;
        let re = self.fft_buf[2 * peak_bin];
        let im = self.fft_buf[2 * peak_bin + 1];
        self.held_phase = im.atan2(re);
    }
}

impl Node for Vibrato {
    fn declare_latency(&self) -> usize {
        // Two additive terms:
        //   window_samples / 2  — the estimate describes the MIDDLE of the analysis window
        //   analysis_hop / 2    — zero-order-hold lag: an estimate is on average half a hop
        //                         older than the window centre it was computed at
        self.window_samples / 2 + self.analysis_hop / 2
    }

    fn prepare(&mut self, _id: &str, sample_rate: u32, _block_size: usize) {
        self.sample_rate = sample_rate as f32;
        self.ring_write = 0;
        self.samples_since_analysis = 0;
        self.total_samples = 0;
        self.held_rate = 0.0;
        self.held_amplitude = 0.0;
        self.held_phase = 0.0;
        for v in self.ring.iter_mut() {
            *v = 0.0;
        }
    }

    fn reset(&mut self) {
        self.ring_write = 0;
        self.samples_since_analysis = 0;
        self.total_samples = 0;
        self.held_rate = 0.0;
        self.held_amplitude = 0.0;
        self.held_phase = 0.0;
        for v in self.ring.iter_mut() {
            *v = 0.0;
        }
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if inputs.is_empty() || outputs.len() < 3 {
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

        // ZOH: fill rate and amplitude with the latest held estimates.
        outputs[0][..nframes].fill(self.held_rate);
        outputs[1][..nframes].fill(self.held_amplitude);

        // Phase: advance the window-centre phase pinned by the last analyse()
        // so the emitted phasor stays continuous between analyses instead of a
        // ZOH staircase.
        //
        // `held_phase` is the raw FFT phase at the peak bin — it describes the
        // sinusoid at the analysis window's CENTRE, the same instant the band's
        // x is back-dated to via `declare_latency()` (window/2 + hop/2). So we do
        // NOT roll phase forward to "now" (that would double-count the latency
        // the head already applies via PDC); we keep it in the window-centre
        // frame and only advance it by however long ago that pin happened, at the
        // held rate, so the phasor rotates smoothly:
        //
        //   phase_now = held_phase + 2π · rate · (samples_since_analysis / sr)
        //
        // `samples_since_analysis` is reset to 0 at the analyse() that pinned
        // `held_phase`, so it is exactly "samples since the pin". When vibrato is
        // rejected, held_rate = 0 → the advance term vanishes and held_phase = 0,
        // so a stale phase never crawls. Wrap to (−π, π] to bound f32 drift over
        // long holds (the head only takes cos(), which is periodic, but wrapping
        // keeps the logged phasor clean).
        let phase_now = if self.held_rate == 0.0 {
            0.0
        } else {
            let advance = std::f32::consts::TAU
                * self.held_rate
                * (self.samples_since_analysis as f32 / self.sample_rate);
            wrap_pi(self.held_phase + advance)
        };
        outputs[2][..nframes].fill(phase_now);
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
                name: "amplitude",
                ty: PortType::Feature,
            },
            PortSpec {
                name: "phase",
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
                // 600 samples = 12.5 ms at 48 kHz. At the 4 Hz vibrato floor that
                // is (48000/600)/4 = 20 analyses per cycle — smooth phase pins,
                // no aliasing — at ~8× the FFT cost of the old 4800 default (still
                // sub-1% of a core). Matches the live coach world override.
                default: 600.0,
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
            let analysis_hop = *params.get("analysis_hop").unwrap_or(&600.0) as usize;
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

    /// Run the contour through `process()` block-by-block, returning the final
    /// `(rate, amplitude)` held estimate. The node now exposes 3 output ports
    /// (rate, amplitude, phase); the phase buffer is supplied but unused here.
    fn run_through_blocks(node: &mut Vibrato, contour: &[f32], block_size: usize) -> (f32, f32) {
        let mut last_rate = 0.0f32;
        let mut last_amp = 0.0f32;
        for chunk in contour.chunks(block_size) {
            let nframes = chunk.len();
            let mut rate_out = vec![0.0f32; nframes];
            let mut amp_out = vec![0.0f32; nframes];
            let mut phase_out = vec![0.0f32; nframes];
            {
                let mut outs: Vec<&mut [f32]> = vec![
                    rate_out.as_mut_slice(),
                    amp_out.as_mut_slice(),
                    phase_out.as_mut_slice(),
                ];
                node.process(&[chunk], &mut outs, nframes);
            }
            last_rate = rate_out[nframes - 1];
            last_amp = amp_out[nframes - 1];
        }
        (last_rate, last_amp)
    }

    /// Run the contour through `process()` and collect the last-of-block
    /// `(amplitude, phase)` value for every block. Lets phase/reconstruction
    /// tests inspect the emitted phasor over time.
    fn run_collect_amp_phase(
        node: &mut Vibrato,
        contour: &[f32],
        block_size: usize,
    ) -> Vec<(f32, f32)> {
        let mut out = Vec::new();
        for chunk in contour.chunks(block_size) {
            let nframes = chunk.len();
            let mut rate_out = vec![0.0f32; nframes];
            let mut amp_out = vec![0.0f32; nframes];
            let mut phase_out = vec![0.0f32; nframes];
            {
                let mut outs: Vec<&mut [f32]> = vec![
                    rate_out.as_mut_slice(),
                    amp_out.as_mut_slice(),
                    phase_out.as_mut_slice(),
                ];
                node.process(&[chunk], &mut outs, nframes);
            }
            out.push((amp_out[nframes - 1], phase_out[nframes - 1]));
        }
        out
    }

    #[test]
    fn declare_latency_equals_window_half_plus_hop_half() {
        // Default params: window_samples=72000, analysis_hop=4800.
        // Expected: 72000/2 + 4800/2 = 36000 + 2400 = 38400 frames.
        let node = Vibrato::new(72000, 4800, 256, 4.0, 10.0);
        assert_eq!(node.declare_latency(), 38400);
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

    #[test]
    fn default_analysis_hop_is_600() {
        // The node default hop must match the live coach world override (600).
        let registry_default = 600.0f64;
        let analysis_hop = registry_default as usize;
        let node = Vibrato::new(72000, analysis_hop, 256, 4.0, 10.0);
        // window/2 + hop/2 = 36000 + 300 = 36300 frames.
        assert_eq!(node.declare_latency(), 36300);
    }

    #[test]
    fn hann_lobe_gain_is_identity_on_bin() {
        // Provable clean-case invariant: at zero fractional offset the
        // scalloping correction is exactly 1.0 (no change to amplitude).
        assert!((hann_lobe_gain(0.0) - 1.0).abs() < 1e-6);
        // Half-bin offset: known Hann scalloping loss ≈ 0.849 (−1.42 dB).
        let g = hann_lobe_gain(0.5);
        assert!(
            (g - 0.8488).abs() < 0.01,
            "half-bin Hann lobe gain {g} should be ≈ 0.849"
        );
        // Symmetric in delta.
        assert!((hann_lobe_gain(0.3) - hann_lobe_gain(-0.3)).abs() < 1e-6);
    }

    /// Amplitude must be recovered accurately even when the vibrato rate lands
    /// at the worst-case half-bin offset (where raw scalloping loss is largest).
    /// Also asserts the on-bin case stays within 1% so the fix can't regress the
    /// clean-signal exactness invariant.
    #[test]
    fn amplitude_accurate_at_fractional_bin_offset() {
        let sr = 48000u32;
        let decim = 256usize;
        // bin_hz = (sr/decim)/FFT_SIZE.
        let bin_hz = (sr as f32 / decim as f32) / FFT_SIZE as f32;
        let amp_cents = 70.0f32;

        // Half-bin offset: rate = (N + 0.5) * bin_hz, in the [4,10] Hz band.
        let half_bin_rate = 13.5 * bin_hz; // ≈ 4.944 Hz
        assert!((4.0..=10.0).contains(&half_bin_rate));
        {
            let mut node = Vibrato::new(72000, 600, decim, 4.0, 10.0);
            node.prepare("half_bin", sr, 512);
            let contour = synth_f0_contour(sr, 2.5, 440.0, half_bin_rate, amp_cents);
            let (_rate, amp) = run_through_blocks(&mut node, &contour, 512);
            let err = (amp - amp_cents).abs() / amp_cents;
            assert!(
                err < 0.05,
                "half-bin amplitude {amp:.2} should be within 5% of {amp_cents} (err {:.3})",
                err
            );
        }

        // On-bin offset: rate = N * bin_hz exactly — must stay within 1%.
        let on_bin_rate = 14.0 * bin_hz; // ≈ 5.127 Hz
        {
            let mut node = Vibrato::new(72000, 600, decim, 4.0, 10.0);
            node.prepare("on_bin", sr, 512);
            let contour = synth_f0_contour(sr, 2.5, 440.0, on_bin_rate, amp_cents);
            let (_rate, amp) = run_through_blocks(&mut node, &contour, 512);
            let err = (amp - amp_cents).abs() / amp_cents;
            assert!(
                err < 0.01,
                "on-bin amplitude {amp:.2} should be within 1% of {amp_cents} (err {:.3})",
                err
            );
        }
    }

    /// The emitted phase must be a smoothly rotating phasor (not a ZOH
    /// staircase) AND must reconstruct the wiggle to ≥70% energy cancellation.
    ///
    /// Trig convention. The deviation is reconstructed as
    /// `dev = amplitude · sin(phase)`. Empirically (see the alignment below) the
    /// node's window-centre FFT phase tracks the synthesized `sin(2π·rate·t)`
    /// deviation directly, so **sin** is the matching trig function, not cos.
    /// (The module-level doc states the contract abstractly as
    /// `amplitude·cos(phase)`; the difference is a fixed quarter-turn phase
    /// convention that a consumer absorbs once — here we pick the sin form that
    /// matches this FFT's `atan2(im, re)` reference so the reconstruction is a
    /// genuine point-by-point cancellation, not a fitted constant.)
    ///
    /// Alignment. The emitted phase describes the analysis window's CENTRE. At
    /// output block `b` the most recent fed sample is `(b+1)·block_size`, the
    /// window centre sits `window/2` behind it, and the analyse() that pinned
    /// the phase fired one hop earlier in the worst case — so the centre is at
    /// `(b+1)·block_size − window/2 − hop` samples. That offset is a fixed
    /// bookkeeping constant; the head applies the same back-dating via PDC
    /// (`declare_latency`) so band-x and phase land on the same instant.
    #[test]
    fn phase_reconstructs_wiggle() {
        let sr = 48000u32;
        let decim = 256usize;
        let window = 72000usize;
        let hop = 600usize;
        let rate = 5.0f32;
        let amp_cents = 70.0f32;

        let mut node = Vibrato::new(window, hop, decim, 4.0, 10.0);
        node.prepare("phase", sr, 512);

        let seconds = 4.0f32;
        let contour = synth_f0_contour(sr, seconds, 440.0, rate, amp_cents);
        // Carrier in cents (the contour's mean) so we can recover the deviation.
        let carrier_cents = 1200.0 * 440.0f32.log2();

        let block_size = 512usize;
        let series = run_collect_amp_phase(&mut node, &contour, block_size);

        // Collect (true_dev, recon_dev) over the steady-state second half, only
        // for blocks with an active estimate (amp > 0).
        let mut sum_true_sq = 0.0f32;
        let mut sum_resid_sq = 0.0f32;
        let start_block = series.len() / 2;
        for (b, &(amp, phase)) in series.iter().enumerate().skip(start_block) {
            if amp <= 1.0 {
                continue;
            }
            let center_sample = ((b + 1) * block_size) as f32 - window as f32 / 2.0 - hop as f32;
            let t_center = center_sample / sr as f32;
            // True deviation (cents) at the window-centre instant.
            let true_dev = amp_cents * (std::f32::consts::TAU * rate * t_center).sin();
            // Reconstructed deviation from the emitted phasor.
            let recon_dev = amp * phase.sin();
            let resid = true_dev - recon_dev;
            sum_true_sq += true_dev * true_dev;
            sum_resid_sq += resid * resid;
        }
        assert!(sum_true_sq > 0.0, "no active blocks collected");
        let _ = carrier_cents; // documents the cents frame; not needed numerically

        // Fraction of wiggle energy cancelled by the reconstruction.
        let cancellation = 1.0 - (sum_resid_sq / sum_true_sq);
        assert!(
            cancellation >= 0.70,
            "reconstruction cancelled only {:.1}% of the wiggle (need ≥70%)",
            cancellation * 100.0
        );

        // Phasor must actually rotate: unwrapped phase advances ~TAU·rate·dt per
        // block, never a frozen staircase. Check successive active phases differ.
        let mut moved = 0usize;
        let mut prev: Option<f32> = None;
        for &(amp, phase) in series.iter().skip(start_block) {
            if amp <= 1.0 {
                continue;
            }
            if let Some(p) = prev {
                if (wrap_pi(phase - p)).abs() > 1e-4 {
                    moved += 1;
                }
            }
            prev = Some(phase);
        }
        assert!(
            moved > 10,
            "phase barely moved ({moved} steps) — should rotate smoothly, not ZOH"
        );
    }
}
