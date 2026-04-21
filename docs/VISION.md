# Vision

## What gurukul is

A singing coach that diagnoses *mechanism*, not just *result*. The product delivers real-time and post-hoc feedback on the physical, articulatory, and phonatory choices a singer is making — the things a human coach would notice and name — and translates them into actionable pedagogical cues.

## What a coach actually does

A coach diagnoses across many mostly-independent axes, often simultaneously:

- **Pitch accuracy and intonation** — on the note, and if not, flat vs sharp (they mean different things).
- **Pitch stability** — vibrato rate, depth, regularity; whether it's intentional or a tension tremor.
- **Timing / rhythmic placement** — ahead, behind, on the beat; phrase arc.
- **Breath management** — where you breathe, how deeply, whether support collapses mid-phrase.
- **Registration** — chest / mixed / head / falsetto, and the *bridge* (passaggio) transitions. This is where most amateurs struggle most.
- **Resonance and placement** — "forward" vs "back," oral/nasal balance, singer's-formant ring around 2.8–3.4 kHz.
- **Vowel shape** — whether vowels hold their identity through registers or collapse under pressure.
- **Onset type** — breathy, aspirate, glottal, balanced. The first 50ms of a note reveals a lot.
- **Phonation mode** — pressed, balanced, breathy, flow phonation. Driven by glottal adduction.
- **Articulation** — consonant clarity without disrupting legato.
- **Dynamic control** — crescendo/decrescendo without pitch drift.
- **Tension markers** — jaw tension, tongue-root retraction, laryngeal elevation.

Existing "singing apps" (SingSharp, Vanido, Yousician-singing) address at most the first two of these. Gurukul's wedge is to address the rest.

## Beyond detection: prescription

Detection alone is not coaching. A coach also:

1. **Prescribes an intervention** in the student's frame: *"imagine the note spinning forward,"* *"yawn-sigh before the phrase,"* *"think /i/ on the /a/."* This requires a pedagogy corpus and a mapping from measured state → appropriate cue for *this* singer's level.
2. **Prioritises ruthlessly.** Telling a beginner about their 5Hz vibrato when they can't sustain a phrase is malpractice. Requires a curriculum / progression model.
3. **Tracks longitudinally.** Real progress happens across weeks. "Your passaggio has gotten 40% smoother this month, compare week 1 vs now" is more valuable than any single-session metric.
4. **Protects the voice.** Detects pressed phonation, prolonged high-SPL singing, signs of fatigue — and stops the lesson. This is a genuine product feature, not a disclaimer.

## Product shape

**Primary device:** a phone (iOS and Android). Modern flagship phones (2023+) have ample compute for the full analysis stack including on-device articulatory inversion; see `ROADMAP.md` and `ARCHITECTURE.md`.

**Peripheral:** an optional watch (Apple Watch, Wear OS) as a body-sensor and haptic-output device — breath mechanics via accelerometer, HR/HRV as a tension/fatigue proxy, haptic pitch/timing cues, glanceable lesson state. **Not** a compute target.

**Optional pro hardware:** BLE accessories — throat mic, chin ultrasound puck, airflow sensor. Not required; natural upgrade path for advanced/clinical users.

**Not targeted:** microcontroller/embedded builds as the primary product. The product runs on a phone.

## What success looks like

- A beginner can identify and correct tongue retraction without a human coach present.
- A serious amateur can practice passaggio transitions with phrase-level diagnostic feedback and measurable weekly progress.
- A working coach can author lessons as data (see `ARCHITECTURE.md` — the world editor) and deploy them to students.
- The analysis layer is accurate enough that clinical speech-voice therapists would find it useful adjacent to their existing workflow.

## Non-goals

- **Not a generic TTS or voice-cloning product.** Voicebox and ElevenLabs cover that. Gurukul is analysis + pedagogy.
- **Not a performance-grade real-time effects processor.** Effects may appear as post-processing for student recordings, but the product is diagnostic, not productive.
- **Not a karaoke game.** Gamification may show up in the UI, but the product's core is pedagogical, not entertainment.
