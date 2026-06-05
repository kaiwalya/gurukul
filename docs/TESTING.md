# Testing

Gurukul uses **synthesis-as-oracle** testing: for each analysis module, generate signals with known ground-truth parameters, optionally apply realistic impairments, run the analyzer under test, and diff its output against the known ground truth.

This flips audio testing from *"does it sound right to me"* (subjective, slow, not CI-able) to *"does the detected value match the parameter I synthesized with"* (objective, fast, CI-able). It is the single highest-leverage decision for making AI-agent-driven development tractable in this domain.

## The test pyramid

### Tier 1 — Synthetic oracles

Milliseconds, thousands of cases. For each analysis module, write a synthesiser that is the *inverse* of what the module detects:

| Module | Synthesizer | Oracle |
|---|---|---|
| Pitch detector | Sine at f₀ | Detected pitch ≈ f₀ within N cents |
| Vibrato analyzer | Pitch modulated at rate R, depth D | Detected R, D within tolerance |
| Onset detector | Silence → gated tone at t=T | Detected onset within ±10ms of T |
| Formant tracker | Source-filter model with known F1/F2/F3 | Detected formants within M Hz |
| H1–H2 (phonation) | KLGLOTT88 / LF glottal model with known open quotient | Estimated OQ within tolerance |
| Breath detector | Pink noise burst at known position | Correct position and duration |
| Articulatory inversion | VocalTractLab / Pink Trombone with known tongue/jaw | Inverted state within tolerance (set-valued) |

Run in CI in seconds. Sweep parameter space — vibrato rates 3–8 Hz in 0.5 Hz steps, crossed with three depths and four fundamentals — and produce a pass/fail *grid* that tells you precisely where the detector breaks, not merely whether it broke.

### Tier 2 — Synthetic + realistic impairments

Seconds, hundreds of cases. The same synthesised signals, but run through realistic corruption:

- Additive noise (white, pink, brown, babble, HVAC recordings)
- Room impulse response convolution (MIT IR survey, or generated via `pyroomacoustics`)
- Bandlimiting (telephony 300–3400 Hz, phone-mic rolloff)
- Phone voice-processing artefacts (AGC, noise suppression — route through iOS/Android processing chains to generate these)
- Packet loss / clipping / quantisation to 8-bit

The test matrix becomes `{parameter sweep} × {impairment}`. The output is a **degradation curve**: *"vibrato detection holds to SNR=10 dB, collapses below that."* Regressions show up as curve shifts, not just binary flips.

### Tier 3 — Real recordings with partial ground truth

Minutes, tens of cases. A small curated set of real singing where *something* is known — a metronome click, a calibrated tone before the take, an annotated onset. Stays in the repo. Catches bugs that synthetic data will never catch — specifically the gap between *"model of a voice"* and *"voice."*

### Tier 4 — Human-in-the-loop golden takes

Hours, a handful of cases, rarely. Reference performances judged by a real coach with diagnosis recorded. Does not run in CI; runs before releases. Catches the *"numbers are right but the coaching is wrong"* bug class.

## The forward-model library

A dedicated module (likely `synth/` or `libs/synth/`) is the single source of truth for test data. Recommended structure:

```
synth/
├── sources/
│   ├── sine.py              pure tones
│   ├── glottal.py           LF / KLGLOTT88 glottal flow
│   ├── vocal_tract.py       Kelly-Lochbaum tube model
│   └── pink_trombone.py     articulatory toy model
├── modulators/
│   ├── vibrato.py           pitch modulation — configurable rate/depth/shape
│   ├── tremolo.py           amplitude modulation
│   ├── portamento.py        continuous pitch glides
│   └── crescendo.py         dynamic envelopes
├── impairments/
│   ├── noise.py             colored noise, babble, HVAC
│   ├── room.py              IR convolution
│   ├── mic.py               phone-mic frequency response + AGC
│   └── codec.py             AAC/Opus round-trips
└── scenarios/
    └── passaggio.py         composed difficult phrases for registration tests
```

Each generator:

- Is deterministic given a seed.
- Documents its parameter ranges.
- Emits `(audio, ground_truth_dict)`.

The synthesis library is itself the hardest thing to get right — a bad synthesiser gives you a falsely-passing test suite. Validate early against published voice-science papers' figures or against a human singing coach.

## Integration with the engine architecture

Because the product uses a plugin-graph engine (see `ARCHITECTURE.md`), tests
are natural. The unit is a **triple**: `(input, world, expectation)`.

- **input** — a source of samples for the world's boundary in-ports: a
  recorded WAV, a generator world (`SynthSine`, impairment chain), or a
  constant. The cabinet drives the in-ports; there is no `FileSource` node.
- **world** — the graph under test, *unmodified*. The live `coach.json` is
  tested by the same JSON the app ships, not a test-rigged copy. Throwaway
  micro-worlds can be inlined as a JSON string.
- **expectation** — an ordinary Rust `assert!` over the captured wires.
  Expectations live in code, not in the graph: there is no `OracleSink` /
  `AssertNear` / `PassFailReporter` node. This keeps the expectation
  language unbounded (any predicate you can write) without growing a JSON
  schema, and `cargo test` is the runner — discovery, filtering, CI for free.

The harness is the `Bench` type in `dsp/bench` (`Bench::mount(world)` or
`Bench::new(inline_json)` → `.bind(port, source)` → `.capture(wire)` →
`.run(duration)` → `Captured`). A synthesiser node and an analyzer node still
share the same `Node` interface — in opposite directions — and the agent
writing a new analyzer writes the paired synthesiser *and* a `#[test]` that
benches it on the same day. Real-recording cases are `#[ignore]`-gated (the
WAVs live under a git-ignored `test_data/`).

## Why this works for AI-driven development

Three properties compound:

1. **The agent can run the full loop.** Write code → synthesise test signals → run detector → diff against ground truth → localise failure → fix. No human in the loop, no subjective judgment, no ambiguity about pass/fail.
2. **Failures are interpretable.** *"Vibrato detection fails at rate=7.5 Hz, depth=30 cents, SNR=15 dB"* is a crisp bug. The agent can read the parameters and often reason about why (7.5 Hz is near the analysis window's resolution limit).
3. **Regressions are visible as curve shifts.** When a change moves the *"vibrato accuracy vs SNR"* curve down, the test suite reports *which region of parameter space got worse*. Far more useful than binary pass/fail.

## Things to watch out for

- **Don't test the synthesiser with itself.** If the synthesiser generates sinusoidal vibrato and the detector assumes sinusoidal vibrato, the test proves nothing. Include triangle / square / noisy / asymmetric modulation shapes — real human vibrato is slightly irregular in rate, model that.
- **Impairments must be realistic.** White noise is easy; a real phone-mic environment has *correlated* noise (HVAC hum, babble), *nonlinear* processing (AGC, NS), and *time-varying* characteristics. A test suite that only covers AWGN passes in CI and fails in the field.
- **Include pathological cases.** 3 Hz vibrato with 100-cent depth (a wobble, not singing). 9 Hz vibrato with 5-cent depth (a tremor). Fundamental near the vibrato rate of a lower note. These catch *class-of-bug* failures, not parameter-range failures.
- **Ground truth for articulatory inversion is set-valued.** Multiple tongue positions produce near-identical audio. Tests must accept any of a set of acceptable inversions, not a single value.
- **Loopback hardware testing.** If and when hardware peripherals enter the picture, a signal-out → signal-in loopback on the device gives real integration testing — catches timing, buffer, and clock-drift bugs that pure-software tests miss.

## First-step plan

1. Add `synth/` with three generators: sine, vibrato'd sine, and sine + pink noise.
2. Wire a test harness that exercises the full analysis pipeline end-to-end (source → features → oracle comparator), not an algorithm in isolation. File-based mock at the driver boundary is fine.
3. First sweep: `pitch ∈ {82, 220, 440, 880, 1760} Hz × SNR ∈ {∞, 40, 30, 20, 10, 0} dB`. Assert detected pitch within ±10 cents above SNR=20 dB. Emit a CSV of results.
4. CI runs the sweep on every PR, fails on regression beyond tolerance, uploads the result grid as an artefact.
5. From there, add vibrato, onset, formant tests as those analyzers come online. Every new analysis module gets a synthesiser and a sweep the same day.
