# Roadmap

Six stages, each shippable on its own. Each stage earns a product jump, not just a tech jump.

## Stage 1 — Tuner-plus

**What:** Real-time pitch, vibrato (rate + depth), onset, and breath detection with a rich, multi-lane visualisation — a Sa-anchored note dial plus a scrolling time graph of the live feature stream.

**Why it ships:** Even this alone is more useful than most consumer tuner apps. The *plus* is the visualisation: a scrolling pitch trace against the singer's scale, onset ticks, and breath spans show the *shape* of a phrase over time — where you attacked, where you breathed, how the pitch moved — not just a single live number. Stage 1 measures and shows; it does not yet interpret or prescribe (those are Stage 2 and Stage 5).

**Compute:** Trivial on any phone from the last 8 years. YIN + FFT runs in <1% CPU.

**Definition of done:** On-device iOS + Android builds. Practice-mode world runs. Full Tier-1/Tier-2 test suite per `TESTING.md` for pitch, vibrato, onset, breath modules.

### Stage 1 phasing

Stage 1 is big enough to need its own phase plan. The ordering is deliberate: back-of-rack (engine, DSP, tests) before front-of-rack (pixels, live mic, phone). Each phase has a checkable artefact; the product ships at 1.5, and 1.6 ports it to phone.

**Current phase: 1.7 — Android port + Stage-1 hardening.** Phases 1.0–1.6 (iOS phone port) complete: the core + Bevy head run on a physical iPhone with a native CoreAudio mic adapter. What remains of "ship Stage 1 on a phone" is the *other* phone — Android — plus a tail of operational hardening and one known pitch artefact (below). This line is the single source of truth for project status; don't duplicate it elsewhere.

**Phase 1.0 — Engine skeleton + inspection. ✓ done.** Rust engine crate (`Node` trait, graph runner, world loader, port-subscription API per `ARCHITECTURE.md`). Three trivial nodes — `SineSource`, `Passthrough`, `NullSink` — each their own crate. A `gurukul` CLI (`list-nodes`, `describe-node`, `validate`, `run`, `render`). A JSON Schema for the world file format, treated as the authoritative interface contract (the file format the editor will eventually read and write, not a debug dump). Moonrepo + Cargo for build orchestration. Authoring surface at this phase is text editor + schema + CLI; the visual editor is deferred. The CLI and schema are part of 1.0, not later polish — "1.0 works" is unfalsifiable without them.

**Phase 1.1 — Synth library + first test-mode world. ✓ done.** Synth node crates (`SynthSine`, `SynthVibratoSine`, `SynthPinkNoise`), a variadic `MixSum`, and an `AudioStatsSink` signal sanity check. Three test-mode worlds under `worlds/test/` (sine, vibrato, sine+pink) running end-to-end via `gurukul test`. Engine gained a `finish()` node hook and id validation; the `Node` trait shrank to `prepare`/`process`/`finish` with port and parameter declarations hoisted into the registry. `ParamSpec` carries a display unit. Tier-1 oracle loop is ready for the first real analyzer.

**Phase 1.2 — First analyzer end-to-end. ✓ done.** YIN pitch detector (`node-pitch-yin`), realtime-safe (zero alloc in `process()`, asserted by a `assert_no_alloc` test). Engine harness made realtime-safe too: `Engine::run_blocks` no longer allocates, with a companion no-alloc test at `engine/tests/no_alloc.rs`. Pitch error oracle (`node-pitch-error`) plus a `pitch × SNR` sweep test that asserts ≤10 cents median error for SNR ≥20 dB across a 5×6 grid; the sweep emits a CSV and a pass/fail grid as artifacts. CI runs the sweep on every push/PR and uploads both artifacts. Back-of-rack only — no pixels. From here an AI agent can author new analyzers against a working oracle loop.

**Phase 1.3 — Remaining Stage-1 analyzers. ✓ done.** Vibrato, onset, breath. Each lands with its paired synth and sweep on the same day. Still headless. All four detection primitives exist and are individually validated against synthetic oracles and Tier-2 impairments.

The three analyzers landed (`node-vibrato`, `node-onset`, `node-breath`) — each realtime-safe (`process()` does no allocation; asserted) and paired with a Tier-1 CSV/grid sweep run in CI. Tier-2 sweeps under impairments followed: `vibrato_snr_sweep` and `onset_snr_sweep` add pink-noise contamination across the 20–40 dB SNR band, and `breath_distractor_sweep` mixes a sustained sine tone under the breath bursts to check both false-positive rate and true-positive rate with a vowel-like distractor. All Tier-2 cells at the asserted operating points pass; the artifacts make the cliffs at lower SNR / louder distractor visible. Two of the four 1.2 follow-ups closed too (Tracer encoding removed; world schema is now registry-generated and rejects unknown node types and params at the schema layer). Two follow-ups remain — see below.

**Phase 1.4 — Minimal live surface: Mac app. ✓ done.** First pixel drawn. A Bevy ECS app (`apps/coach-game`), CoreAudio HAL mic capture, the four analyzers running live, and a first visualiser surface: a Sa-anchored **note dial** tracking the live pitch as a needle against the configured scale (the singer can capture Sa from their own voice). Practice-mode world running against a real mic. The dial shows the *current instant*; the scrolling time-graph view of the feature stream over time (pitch trace, onset ticks, breath spans, vibrato annotation) is **Phase 1.5**, not here. The head reads the live feature stream through the `AppCoach` read model (snapshot resources), not via per-port entity bindings — a generic `PortBinding(path)` ECS abstraction was scoped here but proved unnecessary at four fixed signals, so it was dropped rather than built. **Phase 1.4.8** added audio-I/O preferences (input/output device + engine sample-rate pickers with engine rebuild) and an interactive debug pane (any node + port, with monitor route for audio-shaped ports).

Bevy is the *current* presentation bet, not a fixed one. The hexagonal split is the hedge: all business logic lives behind the `AppCoach` port (the `app-coach` adapter holds the session state machine, the tuning/scale model, the read-side resources), and the head is a thin, swappable presentation layer that only renders read-model snapshots and sends commands. If Bevy hits a wall on a target platform, the head is replaced without touching the domain. Keep heads thin and the port fat — that discipline is what makes the cross-platform plan (Phase 1.6) cheap.

**Phase 1.5 — Visualisation layer. ✓ done.** The thing that makes Stage 1 "tuner-*plus*" rather than "tuner": a scrolling, multi-lane time graph filling the InGame surface beside the dial, so the singer sees the *shape* of a phrase, not just the current instant. Lanes: a tall **pitch trace** (continuous absolute `PitchLog2`, y-window auto-zoomed from the last ~6 s of voiced history, brightness = confidence, thickened/tinted where stable vibrato is detected) over the configured tuning's full groove grid, with the current scale acting only as a lit/dim mask on those grooves; and a short **events lane** carrying onset ticks and breath spans. The head ring-buffers `FeatureSnapshot` history and a dumb `time_graph` widget renders a Bevy-free semantic graph model, mirroring the `note_dial` split. One port change is needed: the per-hop firehose is a single-slot snapshot (the dial only ever wants the latest), so a faithful scrolling trace — onset impulses included — needs a *drained* hop queue alongside it. The port grows a `drain_features` read backed by a lock-free `rtrb` ring on the data-plane worker; no DSP/analyzer change. This is what earns the "Stage 1 shippable" designation on Mac.

All five deliverables landed. The `time_graph` widget lives in `apps/coach-game/src/widgets/time_graph/` (Bevy-free `model.rs` projecting a normalized scene from the `semantic_graph.rs` domain model, `systems.rs` painting it), mirroring the `note_dial` split. Pitch trace carries auto-zoom (instant expansion, eased contraction), confidence→alpha brightness, and a coral vibrato tint gated on depth/rate/confidence; grooves render lit for scale degrees and dim otherwise. The events lane paints onset ticks and breath spans (spans clipped to the window, points dropped). The port grew `drain_features` (`domain-ports/src/app_coach.rs`) backed by an `rtrb` ring on the data-plane worker, with the head draining into `FeatureHistory` only while InGame. Covered by unit tests (vibrato gates, window math) and integration tests (`apps/coach-game/tests/time_graph_*.rs`).

Deliberately **not** interpretation. Reading the feature stream into plain-language statements ("fast vibrato suggests tension," "no pre-phrase breath suggests support problem") is the first *diagnostic* step and lives in **Stage 2**, alongside the spectral diagnostics it naturally groups with — it produces the diagnostic state vector that Stage 5's prescription consumes. Stage 1 stays pure instrumentation + display: it measures and shows, it does not opine.

**Phase 1.6 — Phone port (iOS). ✓ done.** Rust core recompiled for `aarch64-apple-ios`; the same Bevy head cross-compiles and renders the same UI on device (no per-platform UI rewrite). The hexagonal plan held with one exception worth recording: the live-audio plan assumed cpal's iOS backend would work as-is, but cpal's input capture was broken on real hardware (the true floor was `coreaudio-rs` calling `AudioUnitInitialize` inside its constructor, failing the first `EnableIO` with -10851). The fix was a native **`audio-apple`** adapter on raw objc2 CoreAudio FFI, behind the existing audio-capture port — the domain and head were untouched, exactly the escape hatch this phase was designed around. Proven on a physical iPhone 14 Pro. Also landed: bundle/plist scaffold, mic permission + AVAudioSession lifecycle, on-screen pause (touch exit), landscape lock, writable trace root on iOS, and an audio port-conformance battery any adapter runs.

**Phase 1.7 — Android port + Stage-1 hardening.** The remaining half of "ship Stage 1 on a phone." Three threads:
- **Android.** Repeat the iOS pattern for `aarch64-linux-android`: same head cross-compiled, a per-platform mic-session adapter behind the audio-capture port (Oboe/AAudio), informed by what the iOS port taught. The engine and analyzers do not change.
- **Operational hardening (iOS).** The device-only failure modes a desktop app never sees: `MediaServicesReset` recoverable rebuild (currently exits to `Error`), interruption / route-change recovery affordance, and surfacing mic errors *in-UI* rather than sitting silently in `Error`.
- **Octave-jump artefact.** A known pitch defect, reproducible on Mac via `--replay-audio` over `.scratch/repros/sim-jitter-16s.wav`. Measured: frame-to-frame jitter is fine (~3.7 cents) but the YIN track **octave-jumps** (~36 jumps in 16 s — period-doubling / wrong-multiple selection), which reads on screen as the needle leaping an octave. Not platform-specific; it's in the pitch pipeline (YIN octave selection / gating). (Originally filed as "jitter" — that was a misdiagnosis.)

### Phase 1.2 follow-ups — do alongside 1.3

These are coupled to 1.3's three-more-analyzers work; doing them earlier is premature and doing them later means reshaping multiple sweep tests instead of one.

- **Typed `Report` from `Node::finish()`.** Carried over from the 1.1 follow-up list. `finish()` still returns `Result<(), NodeError>`; the sweep tests reconstruct their reports out-of-band by reading port outputs directly. The four sweep tests (`pitch_sweep`, `vibrato_sweep`, `onset_sweep`, `breath_sweep`) all work fine with this pattern, so the urgency dropped — but the typed `Report` shape is still the right home for "pitch track / per-sample error / pass-fail cells" if a fifth sweep family appears. Defer until there is a concrete second consumer.
- **Float-sentinel `ParamSpec` cleanup.** `HashMap<String, f64>` for params has spawned sentinels (`gain_linear = NaN` for "unset", `Feature` port `0.0 = unvoiced`). Still outstanding; the natural moment is the next time an analyzer needs a non-numeric parameter (e.g. an enum mode or a path).
- ~~**Tracer node-id encoding.**~~ ✓ done (Phase 1.3 PR 5). Tracer ids are now plain `trace_N` strings; the CLI prints a `# trace_N = node.port` legend before run output. The engine's `__` reservation in node ids is gone.
- ~~**Registry-generated world schema.**~~ ✓ done (Phase 1.3 PR 6). `emit-schema` and `validate` share `build_world_schema()`, which emits a `oneOf` of per-node-type variants with `const` type strings and known-param `additionalProperties: false`. Worlds with unknown node types or unknown params now fail validation at the schema layer.

The `Phase 1.1 follow-up` originally on this list re: `SynthPinkNoise` seed mixing landed in 1.2 (`splitmix64` in `node-synth-pink-noise/src/lib.rs:6-9`).

**Deferred past Stage 1:** visual graph editor. `ARCHITECTURE.md` is explicit that the editor is a client of the introspection API, and the API should be stressed by real node work before a canvas is built on top of it. The Phase 1.0 CLI + Graphviz renderer + JSON Schema serve the "see what nodes exist and how they connect" need until then.

## Stage 2 — Spectral / phonation layer

**What:** Formants (F1–F4), H1–H2, spectral tilt, singer's-formant cluster energy (2.8–3.4 kHz), glottal inverse filter estimate. Registration classifier (chest / mix / head / falsetto) derived from formants + H1–H2 + pitch. Passaggio-transition detector. Tension / phonation-mode estimator.

Also the home for **rule-based interpretation of the Stage-1 features** — the plain-language read-out that turns the Stage-1 measurements into statements ("fast vibrato suggests tension," "no pre-phrase breath suggests support problem"). It groups here, not in Stage 1, because it is the same *kind* of work as the spectral diagnostics — measurement → diagnosis — and because the union of feature-rule and spectral diagnostics is the **diagnostic state vector** that Stage 5's prescription layer consumes. No LLM yet; these are deterministic rules over the feature stream and the spectral estimates.

**Why it ships:** This is the crossing from *measurement tool* to *diagnostic tool*. The product now tells a singer *what register they're in*, *whether they pushed chest voice too high into the passaggio*, and *that their vibrato just went tense* — diagnostic capability that no consumer app offers. It interprets; it does not yet prescribe (that is Stage 5).

**Compute:** Still easy. LPC formant tracking is microseconds per frame; neural formant trackers (DeepFormants class) are ~1–3 MB, milliseconds per inference on CPU. No NPU required.

**Definition of done:** Registration classifier demonstrated on a test corpus of real singing with coach-labelled ground truth. Clinical-adjacent usefulness confirmed by a domain expert.

## Stage 3 — Reference alignment

**What:** DTW alignment of student take against reference (another singer's take, or the student's own past take). Multi-feature diff: pitch, timing, vibrato, spectral, onset. Overlay UI.

**Why it ships:** Converts the product from a monitor into a *practice tool*. "Here is your take vs the target, with per-dimension differences" is actionable feedback. This is also the first stage at which longitudinal progress tracking becomes visible to the user.

**Compute:** DTW is O(n²); for a 30-second phrase at ~100 frames/s that's <1 s on phone CPU. Neural embeddings (HuBERT-small, WavLM-base, ~95 MB) for style similarity run at ~10 ms per second of audio on an NPU.

**Definition of done:** Student can record a take, align against a reference, and see clear per-dimension deltas. Longitudinal view: same phrase across weeks.

## Stage 4 — Articulatory inversion

**What:** Neural model estimating tongue/jaw/velum articulatory state from audio, optionally fused with front-camera lip/jaw tracking. Diagnostic layer on top: "tongue root retracted on high notes," "jaw clenched through consonants," "velum lowered on oral vowels."

**Why it ships:** This is the frontier and the defensible differentiator. Detection of articulatory problems the singer cannot hear or feel directly is the core of what a human coach provides and what no current software provides.

**Compute:** Research-grade models are 100–500 MB. Quantised INT8 and distilled to ~50M params, they run at 2–4× realtime on a 2024+ flagship NPU. Realtime isn't required — a 2-second lag to report articulation is fine. Front-camera lip/jaw via MediaPipe FaceMesh is essentially free.

**Definition of done:** Inversion accuracy competitive with published baselines on standard articulatory corpora (rtMRI, EMA). At least three articulatory diagnostics (tongue-root retraction, jaw tension, velum position) integrated into the coaching flow.

## Stage 5 — Coaching layer

**What:** LLM-backed cue generator mapping diagnostic state → pedagogical intervention in the student's frame. Curriculum engine that prioritises *what to tell the student this session* given their level and longitudinal profile. Safety guardrails that detect pressed phonation / fatigue markers and stop the lesson.

**Why it ships:** This is the product. Everything before it is instrumentation. This stage turns instrumentation into *coaching*.

**Compute:** LLM can be cloud-based. Only the derived diagnostic state vector crosses the network — never raw audio. On-device small models (Gemini Nano, Apple Intelligence, Phi-3-mini) as an offline fallback.

**Definition of done:** A curated pedagogy corpus (Estill, CVT, bel canto literature) is integrated. Coaching quality is judged *subjectively useful* by at least three working voice coaches. Safety guardrails demonstrated on failure-mode recordings (fatigue, pressed phonation, vocal strain).

## Stage 6 — Optional BLE hardware peripherals

**What:** Watch app for haptic timing/pitch cues, breath-mechanics sensing via accelerometer, HR/HRV as tension/fatigue proxy. Optional throat microphone. Optional chin ultrasound puck for direct tongue imaging. Optional airflow sensor.

**Why it ships:** Pro / advanced / clinical tier. Expands the product's analytical ceiling and its price envelope. None of it is a dependency for stages 1–5.

**Compute:** Watch is a peripheral, not a compute target. All analysis continues to run on the phone.

**Definition of done:** Watch app ships. At least one BLE accessory (throat mic is the easiest) integrated end-to-end.

## What to build first

The immediate path in order of dependency:

1. Stand up the engine + plugin-graph runtime in minimal form per `ARCHITECTURE.md`. Start with the text-format graph and an in-process runner. No UI.
2. Build the `synth/` library per `TESTING.md`. First sweep: pitch × SNR.
3. Implement the first analyzer nodes as plugins: pitch (YIN or CREPE), vibrato, onset, breath. Each lands with its paired synthesiser and test sweep on the same day.
4. Practice-mode world: mic → pitch + vibrato + onset + breath → visualiser. Ship an iOS/Android prototype.
5. Grow from there.

## What to explicitly defer

- **Visual graph editor.** Useful eventually; expensive now. JSON/YAML world files suffice for stages 1–4.
- **Third-party plugin SDK.** Stabilise the `Node` contract internally first. External SDK only once there's a reason to expose it.
- **Microcontroller / embedded deployment.** Phone is the compute target.
- **Music-theory-aware features** (scale detection, interval training, ear-training games). Adjacent product category; don't dilute the coaching wedge.
