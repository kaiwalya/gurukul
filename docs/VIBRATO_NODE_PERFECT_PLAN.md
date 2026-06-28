# Perfecting the Vibrato Analyzer Node — Implementation Plan

Status: plan only (no code). Grounded against the repo as of this writing.

## Reality check (the repo is past the "spike" framing)

Several pieces the brief describes as a throwaway spike are **already
committed** and must be treated as the current baseline, not greenfield:

| Thing | Already present? | Where |
|---|---|---|
| Third `phase` output port | Yes (registered) | `dsp/node-vibrato/src/lib.rs:577-580`, `process()` fills `outputs[2]` at `:556` |
| `held_phase` raw `atan2` at peak bin | Yes, but RAW / un-extrapolated (marked `// SPIKE`) | `:491-493`, `:554-556` |
| `analysis_hop` default lowered | World overrides to **600** already | `dsp/worlds/coach.json:39`; node *default* is still 4800 at `:591` |
| `vibrato_phase` + `vibrato_t_ms` on the snapshot | Yes | `domain-ports/src/app_coach.rs:429-448` |
| Data-plane wiring of phase + back-dated stamp | Yes | `domain-adapters/app-coach/src/data_plane.rs:391-406` |
| Head-side back-dated band with PDC-aligned centerline | Yes (uses **smoothed pitch**, not `pitch−amp·cos φ` yet) | `apps/coach-game/src/widgets/time_graph/model.rs` |

So this pass is mostly **finishing + correcting** work already in flight, plus
the rename and the leakage fix. Three real code changes remain: (1) the
`depth → amplitude` rename, (2) replace the RAW held phase with a properly
**extrapolated** phase that references the same moment the band's x uses,
(3) the leakage/amplitude-bias fix. The hop default is already effectively
600 in the world; we only decide whether to move the *node default* too.

---

## 1. Output contract & the `depth → amplitude` rename

### New contract

The node outputs three Feature ports that reconstruct the wiggle:

```
f0_wiggle(t) = amplitude · cos(phase(t))      // amplitude in cents, phase in radians
```

so the head computes `centerline = pitch − amplitude·cos(phase)`.

- **`amplitude`** = the HALF-swing = the sinusoid amplitude `A` in
  `A·cos(phase)` (peak deviation from center), in cents. **Rename only, value
  semantics unchanged; NO arithmetic moves anywhere — no `/2`, no `·2` added or
  removed at the node formula or the band site.** Peak-to-peak full-swing =
  `2·amplitude`, derived at use sites if/when a site needs it.
- **`rate`** — unchanged (Hz).
- **`phase`** — radians; meaning tightened in §2.

> **Convention lock (do not get this backwards).** `amplitude` = half-swing = A.
> This is what the code ALREADY computes and consumes today — verified:
> - Node formula `2.0 * interp_peak_mag / window_sum` (`lib.rs:487`) already
>   yields the half-swing (70.1 ≈ 70 for a ±70-cent sine). It stays byte-for-byte.
> - Band half-height `(point.vibrato_depth / 1200.0) / octave_span`
>   (`model.rs:223`, **no `/2`**) already treats the value as the amplitude
>   directly. It stays byte-for-byte — just the field name changes.
>
> So the whole change is a pure identifier rename. If you find yourself adding or
> removing a factor of 2, STOP — that is wrong.

Document the convention in the node module doc (`lib.rs:1-41`): *amplitude is
the half-swing (sinusoid amplitude A); peak-to-peak = 2·amplitude.* The old
`depth` doc said "half its peak-to-peak range" (`app_coach.rs:423-427`) — the
same quantity, just a clearer name.

### Disk-format — **DECISION (relayed, pending your confirm): full rename top-to-bottom, including the on-disk field. Accept the replay break. Old recorded traces are disposable.**

> Provenance note: this resolution was relayed via the coordinator, not
> confirmed by you directly. It only *relaxes* the plan (accepts a break you own
> and can re-decide), so I've adopted it — but it abandons every existing
> recording, so veto here if that's not what you want.

The on-disk schema is `SidecarHop` in `dsp/audio-trace-format/src/lib.rs:12-20`,
field `vibrato_depth: f32`, with `Manifest.schema = 1` (`:25`). One concept,
one name, top to bottom — **no serde-rename split, no disk/in-memory mismatch.**

Resolution:

1. **Rename `SidecarHop.vibrato_depth → vibrato_amplitude`** (`:19`). This
   changes the JSONL line format; existing `.features.jsonl` traces will no
   longer deserialize. **Accepted** — old traces are disposable.
2. **Bump `Manifest.schema` 1 → 2** (`:25`). Its doc says "bump only on a
   breaking change"; a field rename is exactly that. The replay/diff path should
   reject `schema != 2` with a clear error so a stale trace fails loud, not weird.
3. **Rename `FeatureSnapshot.vibrato_depth → vibrato_amplitude`**
   (`app_coach.rs:427`), plain rename, **NO `#[serde(rename)]`** — the wire name
   follows the field name. (`#[serde(default)]` on `vibrato_t_ms` at `:447` is
   unrelated and stays.)
4. **Everything else renames freely** — port name, world JSON wiring,
   graph-output id, head structs, locals, captured-port strings. One name
   everywhere: `amplitude` / `vibrato_amplitude`. No mismatch to document.

Simpler than the back-compat alternative: no `serde(rename)`, no string
mismatch, no old-format-parsing test. Cost: pre-rename recordings are abandoned.

### The world-JSON port name (`depth` → `amplitude`) IS a versioned interface change

`dsp/worlds/coach.json:52` wires `vibrato_det.depth → vibrato_depth`. The node
registers an output port literally named `"depth"` (`lib.rs:574`). Per
`dsp/CLAUDE.md`, the world JSON Schema is the interface contract. Renaming the
**port** `depth → amplitude` means:

- `lib.rs:574` port name → `"amplitude"`.
- `coach.json:52` edge `from` → `vibrato_det.amplitude`. The graph-output id
  `vibrato_depth` (`:13,52`) **also renames → `vibrato_amplitude`** (full-rename
  decision). The coupled `resolve_out_port("vibrato_depth")` call in
  `data_plane.rs:468` and the captured-port string in `audio_trace.rs:113`
  rename in lockstep.
- `inspect.rs:132` matches the literal port name `"depth"` for `PortShape`
  classification — must add/replace with `"amplitude"`.

`coach.json` is `include_str!`-embedded (`pitch_world.rs`), so this is a
compile-time-checked rename; a missed edge fails at engine wiring, not silently.

### The separate synth node — **DECISION: leave `node-synth-vibrato-sine` alone.**

`dsp/node-synth-vibrato-sine/src/lib.rs` uses `vibrato_depth_cents` as a
**generator input** (`:9,77,91,100-107`) — "produce a tone wiggling ±N cents."
That is a different concept (commanded swing of a synthesizer) from the
analyzer's *measured* amplitude, and its param name appears in the world schema
as a SynthVibratoSine param (`world.schema.json:728`). Renaming it would churn
the schema and the synth tests for zero benefit to the analyzer contract. Keep
it. Note the divergence in the analyzer doc so future readers don't assume the
two `depth`s are the same axis.

---

## 2. Phase output — extrapolation & the PDC-coupling resolution (the crux)

### The defect today

`process()` fills `outputs[2]` with the **raw** `held_phase` =
`atan2(im, re)` at the peak bin (`lib.rs:491-493, 554-556`), recomputed only
when `analyse()` runs and then zero-order-held between analyses. Two problems:

1. **It is the phase at the window *center*, not "now".** The FFT phase
   describes the sinusoid at the temporal reference point of the windowed data.
2. **It is frozen between analyses** (ZOH). Even with `analysis_hop=600` the
   phase only updates every 12.5 ms and then sits still, so the reconstructed
   `cos(phase)` is a staircase, not a smooth phasor.

### The coupling we must resolve

There are TWO independent "back-datings" in the system, and they MUST converge
on the **same instant** or the `cos()` beats against the trace:

- **PDC / band-x back-dating** (head side): the band's x is placed at
  `vibrato_t_ms = t_ms − vibrato_latency_ms`, where `vibrato_latency_ms` comes
  from `engine.out_port_latency(...)` = the node's `declare_latency()`
  (`data_plane.rs:311-322`). `declare_latency()` returns
  `window_samples/2 + analysis_hop/2` (`lib.rs:498-504`) — i.e. the band is
  drawn at the **window-center moment**, ~0.8 s in the past.
- **Phase reference**: the raw FFT phase ALSO refers to the window-center
  moment (that is what a centered FFT measures).

**Key realization:** these two are *already the same moment* (window center).
So the correct design is **NOT** to extrapolate phase forward to "now". It is to
make the phase describe **exactly the window-center moment that `declare_latency`
already aligns the band to**, and let it advance smoothly between analyses at
the held rate so it isn't a staircase.

### The design

In `process()`, per output sample (or per block — block granularity is fine at
these rates), advance the held phase from where the last `analyse()` pinned it:

```
phase_now = held_phase_at_pin                                  // window-center phase from last analyse()
          + TAU * held_rate * (samples_since_pin) / sample_rate
```

where `samples_since_pin` counts audio samples since the `analyse()` that set
`held_phase`. This makes the **emitted** phase the window-center phase advanced
by however long ago that window center was — which, because the band is drawn
back-dated by exactly `declare_latency()` (the same window-center offset),
lands the phase and the band-x on the **same instant**. No separate
group-delay term is added in `process()`; the group delay is *already* expressed
once, via `declare_latency()`, and the head consumes it via PDC.

> Why not also add `+ group_delay_frames` here? Because that would double-count.
> The band's x is back-dated by the full `declare_latency()`. If we *also* rolled
> the phase forward to "now", the head would have to roll it *back* by the same
> latency to sample it at the back-dated x — a wash that just adds a place to get
> the sign wrong. Keep phase in the SAME frame the band-x lives in: window-center,
> advanced only to keep the phasor continuous between analyses.

Concretely:
- Add a counter `samples_since_pin` (or reuse `samples_since_analysis`, which is
  already maintained at `:540` and reset at `:547`) to measure advance since the
  pin. Note `samples_since_analysis` resets to 0 *at* the analyse call, so it is
  exactly "samples since the window-center phase was pinned" — reuse it directly.
- In `analyse()`, keep storing the raw window-center `atan2` into `held_phase`
  (`:493`) — that is the pin.
- In `process()`, replace the raw fill (`:556`) with the advanced value:
  `held_phase + TAU * held_rate * samples_since_analysis / sample_rate`,
  wrapped to `(−π, π]` (or leave unwrapped — head only uses `cos`, which is
  periodic; wrapping avoids f32 precision drift over long holds, so wrap).
- When vibrato is rejected (`held_rate = 0`), emit phase `0.0` (already the
  reset behavior). With `rate = 0` the advance term vanishes, so a held-but-stale
  phase won't crawl — good.

### Hop must be small enough (ties into §3)

The brief's spike proved the pin itself aliases when the hop is large: at
`analysis_hop=4800` (100 ms) a 5 Hz vibrato advances 180°/analysis, so
*successive pins* jump ±π and the held phase is a 2-value flip. At
`analysis_hop=600` (12.5 ms) each pin advances ~19°, smooth and monotonic. The
*intra-hop* advance term above smooths between pins, but the **pins themselves**
must not alias — hence §3 keeps the hop ≤ 600.

### Test

Replay the pure 5 Hz / 70-cent vibrato WAV (the brief's spike fixture) through
`--replay-audio`, read the UX trace JSONL, and assert the emitted phase is a
**monotonic rotating phasor**: `unwrap(phase)` increases ~`TAU·5` rad/s within
tolerance, with no ±π flips. Add a unit test in `lib.rs` that feeds the synth
contour and checks `phase` advances by ≈ `TAU·rate·hop/sr` between consecutive
analysis outputs (mod aliasing), and that `pitch − amplitude·cos(phase)` has
<25% residual wiggle vs the raw contour (the brief measured 77% cancellation;
target ≥70%).

---

## 3. `analysis_hop` change + cost

### Decision: lower the **node default** from 4800 to **600**, matching what the world already sets.

`coach.json:39` already passes `analysis_hop: 600`, so the live coach is
already at 12.5 ms. The node *default* (`lib.rs:591`) is still 4800, which is
misleading and a trap for any new world. Lower the default to 600 so the node's
self-documented behavior matches production.

**World override now redundant.** Once the node default is 600, the explicit
`"params": { "analysis_hop": 600 }` in `coach.json:37-39` matches the default
and can be dropped. **DECISION: keep it as explicit documentation** — it makes
the coach's intent self-evident at the world level and guards against a future
default change silently altering the live coach. (Dropping it is equally valid;
noted so the implementer doesn't treat it as a required edit either way.)

- **Phase-samples per cycle at slowest vibrato:** at the band floor
  `rate_min_hz = 4 Hz` (`:614`), hop 600 → `4800/600 / 4 ... ` i.e.
  `(sr/hop)/rate = (48000/600)/4 = 20` analyses per cycle. Even at a
  hypothetical 2 Hz that is 40/cycle. Comfortably ≥ 4. 600 is justified.
- **Cost:** `analyse()` runs `sr/hop = 80×/s` instead of `10×/s` — **8×** more
  often. One `analyse()` = one 512-pt real FFT + O(n_decim≈281) bookkeeping,
  allocation-free (`lib.rs:40-41`). 80 FFTs/s of size 512 is trivial (sub-1%
  of a core). Confirmed acceptable.
- **Anything assuming the old hop?** `declare_latency()` =
  `window/2 + hop/2` (`:503`): with hop 600 the hop term is 300 frames (6.25 ms)
  vs 2400 (50 ms) — *lowers* total latency slightly, which only tightens band
  alignment. The latency-rounding unit test
  (`data_plane.rs:536-551`) hard-codes 38400 frames (window 72000 + hop 4800
  /2). **That test will break** and must be updated to the hop-600 value
  (`72000/2 + 600/2 = 36300` → 756 ms). The node's own
  `declare_latency_equals_window_half_plus_hop_half` test (`lib.rs:691-697`)
  constructs `Vibrato::new(72000, 4800, ...)` explicitly and asserts 38400 —
  that one can keep its explicit 4800 (it tests the formula, not the default),
  but if we change the default we should add/adjust a test asserting the new
  default.

---

## 4. Amplitude accuracy — the leakage/scalloping fix

### Symptom (from the brief, verified)

On real YIN-derived contours the node reads ~26% **low** (63 vs 85 cents). On a
clean synthetic sine the formula is exact (70.1 vs 70.0). So it is not a formula
error — it is **scalloping loss + spectral leakage** when the vibrato frequency
falls between FFT bins (non-integer number of cycles in the window).

### DSP reasoning

The amplitude estimator (`lib.rs:480-487`) reads a single (parabolically
interpolated) peak bin magnitude and inverts the Hann coherent gain
(`A = 2·peak_mag/window_sum`). For a tone exactly on a bin center, a Hann
window puts *all* coherent energy in that bin and the inversion is exact. For a
tone at a fractional bin offset, the Hann main lobe **spreads energy into the
neighbor bins**; the single peak bin under-reads by the *scalloping loss*. Worst
case (half-bin offset) for a Hann window the peak-bin amplitude loss is ≈ −1.4 dB
≈ 15%, and broader leakage pushes the observed real-signal miss to ~26% once
detrend residual and non-stationarity are added.

Parabolic interpolation on log-magnitude (`:403-420`) already recovers the
*frequency* well but only **partially** recovers the peak *height* — it fits a
parabola to 3 samples of a Gaussian-ish lobe, which underestimates a Hann lobe's
true peak.

### Approach — coherent energy recovery across the main lobe

Replace "single interpolated peak magnitude" with **main-lobe energy
summation**: the Hann window's coherent energy for a sinusoid is distributed
across the peak bin ± a small neighborhood. Sum the *complex* contributions (or
the power) over `peak_bin ± K` and normalize by the window's known energy so the
recovered amplitude is offset-invariant.

Two candidate formulations (pick during implementation, test decides):

- **(A) Power sum (energy recovery).** `A = 2·sqrt(Σ_{k=peak−K..peak+K} mag[k]²)
  · c / window_sum`, where the sum captures the leaked main-lobe power and `c`
  is a Hann-specific normalization so that the on-bin case returns exactly the
  current value. K = 1 or 2 (Hann main lobe is ~4 bins wide; ±2 captures
  >99% of the lobe). Must subtract an in-band noise/median floor per bin
  (reuse the median already computed at `:445-466`) so broadband energy isn't
  counted as signal.
- **(B) Scalloping-loss correction factor.** Keep the single-peak read but
  multiply by a correction derived from the *fractional bin offset* `delta`
  already computed at `:411-420`: `A_corrected = A_peak / hann_lobe_gain(delta)`,
  where `hann_lobe_gain(delta)` is the analytic Hann main-lobe amplitude at
  fractional offset `delta` (closed form, ≈ `sinc`-like, =1 at delta=0). This is
  cheaper (no extra summation) and **provably preserves the clean case** because
  `delta→0 ⇒ gain→1 ⇒ A unchanged`.

**Recommendation: start with (B).** It is surgical, analytic, allocation-free,
and guaranteed to keep the exact clean-signal result (the 70.1≈70.0 invariant)
because at zero offset the correction is identity. (A) is the fallback if (B)
under-corrects real signals (since real-signal loss is partly leakage beyond the
single bin, not pure scalloping — if so, blend: energy-sum with median-floor
subtraction).

### Out of scope (per brief): hop-to-hop amplitude RIPPLE

We fix the *magnitude bias* (DC offset of the estimate), NOT the temporal
wobble of the estimate as the vibrato frequency drifts across bin boundaries
between analyses. That stability work is deferred (see §7).

### Test (define the bar)

New unit test `amplitude_accurate_at_fractional_bin_offset`:

- Synthesize a clean vibrato contour whose rate lands at a **half-bin offset**
  (choose `rate_hz` so `rate/bin_hz` ends in `.5`; `bin_hz = (sr/decim)/512 =
  187.5/512 ≈ 0.366 Hz`, so e.g. pick a rate ≈ `bin_hz·(N+0.5)` in-band).
- Known amplitude (e.g. 70 cents). Assert recovered amplitude error **< 5%**
  at the worst-case (half-bin) offset, and **< 1%** at zero offset (the existing
  exactness invariant — add an explicit on-bin assertion so the fix can't regress
  it).
- Keep the existing `recovers_5hz_50cent` / `recovers_6hz_30cent` tests
  (`lib.rs:699-730`) green; tighten their tolerance from ±10 to ±5 cents once
  the fix lands, as a regression guard.
- Real-contour test `real_contour_recovers_true_vibrato_rate` (`:974-1008`):
  loosen-then-retighten the depth band assertion if (B) shifts it; the truth
  there is ~8–11 cents so the band [5,40] has room.

---

## 5. Ordered file-by-file change list

Do in this order (interface first, then producer, then consumers, then tests).

**Phase A — analyzer node (`dsp/node-vibrato/src/lib.rs`)**
1. Rename in-memory `held_depth → held_amplitude` (fields `:71,150,...` and all
   assigns `:240,348,385,473,489,512,524`).
2. Rename output port `"depth" → "amplitude"` (`:574`); update module doc
   (`:1-41`, esp. step 7 at `:31` and `:480-487`) to state amplitude = half-swing.
3. Phase: replace raw fill at `:556` with the advanced expression (§2). Remove
   the `// SPIKE` comments `:554-556`. Keep `held_phase` pin in `analyse()`.
4. Amplitude fix (§4, approach B): apply Hann scalloping correction at `:487`.
5. Lower `analysis_hop` default `4800 → 600` (`:591` and the closure default
   `:632`). Adjust the latency test if it relies on the default.
6. Update/extend unit tests `:647-1009` per §2/§4 test plans.

**Phase B — world + schema (interface contract)**
7. `dsp/worlds/coach.json`: `vibrato_det.depth → vibrato_det.amplitude` (`:52`)
   AND graph-output id `vibrato_depth → vibrato_amplitude` (`:13,52`). Optionally
   drop the now-redundant `analysis_hop: 600` override (`:37-39`) — recommend
   keeping it as explicit documentation (§3).
8. `dsp/schema/world.schema.json`: regenerate if it enumerates Vibrato ports
   (verify — the shown block only carries the *synth* params; if the Vibrato
   node type lists ports, update `depth → amplitude`). Treat as versioned
   interface change per `dsp/CLAUDE.md`.

**Phase C — port + adapters (full rename, no serde split)**
9. `domain-ports/src/app_coach.rs:427`: rename field
   `vibrato_depth → vibrato_amplitude`, **plain rename, no `#[serde(rename)]`**;
   update the doc `:423-427`.
10. `dsp/audio-trace-format/src/lib.rs`: rename `SidecarHop.vibrato_depth →
    vibrato_amplitude` (`:19`) AND bump `Manifest.schema` 1 → 2 (`:25`). Add the
    replay-side `schema != 2` reject (§1). Accept the old-trace break.
11. `domain-adapters/app-coach/src/data_plane.rs`: rename locals/handles
    `vibrato_depth → vibrato_amplitude` (`:316-318,392,403,415,445,468-470,481,525`)
    INCLUDING the `resolve_out_port("vibrato_depth")` string → `"vibrato_amplitude"`
    (graph-output id renamed in step 7). Fix the latency test value (§3).
12. `domain-adapters/app-coach/src/inspect.rs:132`: port-name match
    `"depth" → "amplitude"`.
13. `domain-adapters/app-coach/src/audio_recorder.rs:383,461,562` and
    `dsp/bench/src/audio_trace.rs:114,124,136,179`: rename all `vibrato_depth →
    vibrato_amplitude` (the `SidecarHop` field renamed in step 10). The captured
    port-name string `"vibrato_depth"` at `audio_trace.rs:113` → `"vibrato_amplitude"`
    (matches the renamed graph-output id).

**Phase D — head (coach-game)**
14. `apps/coach-game/src/feature_types.rs:24,43`: `vibrato_depth → vibrato_amplitude`.
    Update doc `:23`.
15. `apps/coach-game/src/feature_history.rs`, `semantic_graph.rs`, `game/mod.rs`,
    `trace/replay/load.rs`, `widgets/time_graph/model.rs` + `scene.rs`: rename
    all `vibrato_depth` references. In `model.rs` the band half-height math
    (`:217-223`) reads `point.vibrato_depth / 1200` — rename only; the
    centerline rewrite to `pitch − amp·cos(phase)` is a SEPARATE pass (§7).
16. `apps/coach-game/tests/*`: rename across the 8 test files that reference
    `vibrato_depth` (grep list). `trace_replay_roundtrip.rs` now round-trips the
    NEW field name + `schema = 2`; any committed OLD-format fixture must be
    regenerated or deleted (old traces are disposable).

**Phase E — synth node**
17. No change to `dsp/node-synth-vibrato-sine` (§1 decision). Add one comment if
    helpful distinguishing its `vibrato_depth_cents` (generator input) from the
    analyzer's `amplitude` (measurement).

---

## 6. Test plan

| Test | Where | Asserts |
|---|---|---|
| Clean on-bin amplitude exactness | `node-vibrato` unit | recovered amp within 1% of 70c (guards the §4 invariant) |
| Half-bin amplitude accuracy | `node-vibrato` unit (new) | error < 5% at worst-case fractional offset |
| Phase rotates (synthetic) | `node-vibrato` unit (new) | `unwrap(phase)` advances ≈ `TAU·rate·hop/sr`/analysis, no ±π flips |
| Reconstruction residual | `node-vibrato` unit (new) | `pitch − amp·cos(phase)` cancels ≥70% of wiggle |
| Existing rate/depth recovery | `node-vibrato` (`:699-730`) | tighten depth tol to ±5c |
| Real-contour | `node-vibrato` (`:974-1008`) | rate 4–6.5 Hz, amp 5–40c still pass |
| No-alloc | `node-vibrato/tests/no_alloc.rs` | new phase/amp math allocates nothing |
| Sweeps | `vibrato_sweep.rs`, `vibrato_snr_sweep.rs` | re-run; update any depth-named expectations |
| Latency rounding | `data_plane.rs:536-551` | UPDATE to hop-600 latency |
| Phase end-to-end | `--replay-audio` on 5 Hz/70c WAV + trace JSONL read | monotonic phasor; band aligns to trace (manual/integration) |
| Trace round-trip | `coach-game/tests/trace_replay_roundtrip.rs` | NEW `vibrato_amplitude` field + `schema = 2` round-trips; regenerate/delete any old-format fixture |

Gate: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
`cargo test --workspace --release` all clean (release per project rule —
debug sweeps are too slow).

---

## 7. Deferred / escalate back to you

- **Head-side centerline rewrite** (`model.rs`) to use
  `centerline = pitch − amplitude·cos(phase)` instead of the current
  PDC-aligned **smoothed-pitch** centerline (`:300-350`). The brief says this is
  a separate later pass. This pass only RENAMES in `model.rs`; it does not change
  the centerline algorithm. **Flag:** once phase is a clean phasor (this pass),
  the head can drop the smoothing entirely — but that is the next pass.
- **Hop-to-hop amplitude ripple** (scalloping *stability* over time) —
  explicitly out of scope; we fix bias not wobble.
- **Provenance flag (needs your direct OK):** the two resolved decisions below
  came via the coordinator, not from you directly. Both only *relax* the plan,
  so I adopted them — but please veto if either is wrong:
  1. **Full rename incl. disk** — `vibrato_depth → vibrato_amplitude` everywhere
     (disk field, `Manifest.schema 1→2`, wiring, code). **Old recorded traces
     are abandoned** (they won't replay). Reversible only by re-recording.
  2. **Node default hop 4800 → 600** — makes the node default match the live
     coach; world override kept as documentation.
