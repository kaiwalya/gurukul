# Research notes

Context and references for the voice-modelling and articulatory-synthesis fields that gurukul builds on.

## Voice synthesis / TTS frontier

The field moved through three generations in about five years:

1. **Mel-spectrogram + vocoder** (Tacotron2 + WaveNet/HiFi-GAN, ~2018–2020). Two-stage, brittle prosody.
2. **Neural codec + autoregressive LM over audio tokens** (VALL-E, 2023). Phase shift: treat speech as a sequence of discrete codec tokens (EnCodec / SoundStream / DAC), do zero-shot cloning from a 3-second reference. Every current *"clone my voice"* product descends from this.
3. **Diffusion / flow-matching over continuous latents + LM-style control** (2024–2026). Where the frontier actually is now.

The live frontier, concretely:

- **End-to-end speech-language models.** Single model reasoning in audio tokens — GPT-4o voice mode, Moshi (Kyutai, open), Sesame CSM, HumeAI TADA. Handle turn-taking, back-channels, interruption, laughing-while-talking as native behaviours. Sub-300 ms latency because text is skipped as an intermediate.
- **Flow matching / rectified flow vocoders.** Matcha-TTS, F5-TTS, E2 TTS. Few ODE steps instead of many diffusion steps — real-time quality without autoregressive-LM compute cost. F5-TTS is the current open-weights favourite for quality-per-parameter.
- **Paralinguistics as first-class signal.** `[laugh]`, `[sigh]`, `[whisper]` tags (Chatterbox Turbo) are the crude version. The frontier is *continuous* emotion/style conditioning — reference clip conditioning plus natural-language style prompts that the model actually follows (Qwen CustomVoice, TADA).
- **Ultra-long-form coherence.** Keeping persona, pacing, prosody stable across 10+ minutes. Most models are good for ~60 s before character drift shows up. TADA's 700 s claim is a frontier number.
- **Speaker disentanglement.** Separating *identity* from *accent*, *emotion*, *age*, *health*. Research goal: factored representation with independently controllable knobs. Still aspirational.
- **Real-time streaming synthesis** with sub-150 ms first-token latency and stable prosody. Hard because good prosody wants lookahead; streaming denies it.
- **Multilingual + code-switching in a single utterance** with correct mid-sentence phonetic and prosodic shift.
- **Watermarking and detection.** AudioSeal (Meta), SynthID-audio (Google). Policy pressure pushing hard.

Short summary: quality is basically solved for short read-aloud speech. The frontier is everything *around* it — conversation, long-form stability, controllability, latency, provenance.

## Articulatory / physics-based synthesis — the other frontier

This is the lineage that matters most for gurukul.

### Classical approach

- **Articulatory synthesis** goes back to the 1960s (Kelly-Lochbaum tube models). Represent the vocal tract as connected acoustic tubes with time-varying cross-sectional areas; simulate airflow; get speech out.
- **VocalTractLab (Birkholz)** is the reference modern implementation — full 3D biomechanical model of lips, jaw, tongue, velum, glottis.
- **Parameters are physical.** Tongue tip position (x, y), tongue body shape, jaw opening, lip rounding, velum opening, glottal tension, subglottal pressure. ~20–30 parameters instead of thousands of latent dimensions.
- **Never won commercially** because (a) articulatory inversion is ambiguous — multiple tongue positions produce similar acoustics, and (b) hand-tuned physics never beat data-driven neural models on raw quality.

### Why it's getting interesting again

- **Articulatory data is finally available.** Real-time MRI speech corpora (USC rtMRI — 83 speakers, full-head MRI at 83 fps), EMA (electromagnetic articulography), ultrasound tongue imaging. A decade ago this was dozens of subjects; now it's hundreds.
- **Neural articulatory inversion is tractable.** Deep models can map acoustics → articulatory trajectories with reasonable accuracy (SPIRE, Articulatory-WavLM). Gives a *controllable middle representation*: phoneme → articulation → acoustics, with articulation as the layer a human can reason about and edit.
- **Differentiable vocal-tract models.** If you can backprop through the physics simulator, you can train neural acoustic and physical models jointly. Early work (DDSP-style for voice, Pink Trombone as a toy) suggests this is the path to models that are *interpretable* and *controllable by physics*.
- **Medical applications drive it.** Speech therapy, post-laryngectomy voice restoration, cleft palate simulation, surgical planning. These fields need *what-if* control that black-box neural TTS cannot provide.
- **Singing voice and emotion.** Articulation is how most expressive nuance works — vowel shading, tongue-root retraction for dark timbre, breathy vs pressed phonation. Articulatory control surfaces give knobs that "emotion intensity scalar" never will.

### Research directions to watch

1. **Hybrid models.** Neural acoustic backbone conditioned on a compact articulatory code from MRI/EMA data. Neural-quality audio with physics-level controllability. Active research direction.
2. **Real-time differentiable vocal-tract synthesis** good enough to actually use. Pink Trombone is a toy; a production-grade version doesn't exist yet.
3. **Articulatory latent spaces as interpretable style vectors.** Factor "speaker embedding" into {vocal tract length, glottal characteristics, articulatory habits}.
4. **Cross-speaker articulatory mapping.** Given one speaker's articulation, synthesise in another speaker's voice with the same gestures.
5. **Glottal-source modelling with neural vocoders.** Most "voice quality" lives in the glottal excitation (breathiness, creak, strain, tension). Models explicitly modelling glottal source + filter are making a comeback because they're controllable and low-data.

## Why articulatory framing fits singing coaching

Voice coaching is the *ideal* domain for the articulatory/physics frontier, because:

- The student *wants* to understand the physical mechanism, unlike a TTS user who just wants the audio.
- Feedback is naturally about *gestures*, not *acoustics* — "relax your jaw" is articulatory.
- Inversion ambiguity (many configurations → similar audio) matters less, because pedagogy narrows the hypothesis space — high-frequency failure modes are known (tongue retraction, jaw tension, laryngeal elevation).
- Errors are non-catastrophic — a wrong diagnosis gets corrected next lesson, vs a TTS hallucination shipped to production.

## Where to watch the field

- **Kyutai** — Moshi, open speech-LMs.
- **Meta FAIR audio** — AudioSeal, Voicebox (the paper), SeamlessM4T.
- **Microsoft** — NaturalSpeech series.
- **Peter Birkholz's lab** — VocalTractLab, physics-based articulatory synthesis.
- **Shrikanth Narayanan's group at USC** — rtMRI corpora, articulatory inversion.
- **Interspeech proceedings** (ISCA) — general venue for articulatory work.

## Key references for the coaching-specific layer

For the pedagogy corpus that will back Stage 5 (see `ROADMAP.md`):

- **Estill Voice Model** — structured taxonomy of vocal qualities mapped to laryngeal and articulatory configurations. Good for rule-based diagnostics.
- **Complete Vocal Technique (CVT)** — Cathrine Sadolin's framework. Clear vocal-mode taxonomy.
- **Bel canto pedagogy literature** — historical imagery ("spin the note," "forward placement") that maps onto measurable acoustic/articulatory targets.
- **Voice science textbooks** — Titze's *Principles of Voice Production*, Sundberg's *The Science of the Singing Voice*.

## One-sentence synthesis

Acoustic-side TTS is in a late-stage arms race; articulatory/physics-side modelling is in an early-stage renaissance; the interesting bet for the next five years is **hybrid models using articulation as a controllable middle layer between intent and sound**, which happens to be the exact representation a singing coach works in.
