# Vibrato on the pitch trace — UX spec

*Product-level spec for how vibrato is shown on the scrolling pitch trace. The
**what** and **why**, not the implementation.* For where the pitch trace lives
in the product, see [`VISION.md`](VISION.md) ("Pitch stability"). For the
musical signals, see [`MUSIC_MODEL.md`](MUSIC_MODEL.md).

## The signal

Vibrato is a periodic wobble of pitch. Three numbers describe it: **rate** (how
fast, Hz), **depth** (how wide the swing, in semitones), and **regularity** (how
steady rate and depth stay). What a singer is trying to feel is *width of swing*
— an even, controlled oscillation of a particular size.

A single derived scalar, **vibrato strength** ∈ [0,1], gates whether a stretch
counts as real vibrato: it combines a depth gate, a rate band (centred on the
musical vibrato range, fading outside it), and the pitch reading's confidence —
so a shaky, low-confidence wobble never registers as vibrato.

## The principle: encode swing as geometry, not colour

The core decision: **vibrato is a width, so show it as a width.** The trace's
*shape* already carries the wobble; the visualization should make the *size* of
that wobble legible, not recolour the line.

This rejects the earlier coral colour-tint (line warms toward red over vibrato).
Colour competes with the other things colour should carry, doesn't communicate
*how wide* the swing is, and turns a continuous physical quantity into a hue the
singer has to decode. Depth is geometry; render it as geometry.

## The design: one band the trace lives inside

Show vibrato as **a single translucent band centred on the trace, its height
equal to the measured vibrato depth** — the trace wiggles *inside* the band.

- **One band, not two rails.** Drawing a line above and below the oscillation
  produces three competing wavy lines and teaches the wrong lesson — "stay
  between these." A filled band teaches "fill this smoothly." Good vibrato reads
  at a glance: the trace evenly kisses both edges of its band.
- **The band is the singer's *own* measured swing** (descriptive), not a target
  to hit. It brackets the excursion the singer actually produced, so they see
  *where* their vibrato is steady and *how wide* it is — feedback, not a target
  rail. (A prescriptive "fill this reference depth" mode is a deliberate future
  variant — see Open question.)
- **The band fades with vibrato strength.** No vibrato → no band (the trace is a
  plain line). Stronger, steadier vibrato → a more present band. The band never
  appears over a stretch that didn't qualify as vibrato.

## Confidence is a separate cue

Confidence — how sure the pitch reading is — is the one signal with no natural
shape, so it gets **opacity**: the trace itself is faint where the reading is
uncertain and solid where it's confident. This is independent of the vibrato
band. The two cues never collide: vibrato owns the *band* (and its height),
confidence owns the *trace's opacity*.

## What the singer should be able to read at a glance

| They see | They learn |
| --- | --- |
| A band appears around the trace | "I'm producing vibrato here" |
| The band is tall / short | "My vibrato is wide / narrow" |
| The trace evenly fills the band | "My vibrato is even" — the goal |
| The trace is lopsided in the band | "My swing is uneven" |
| The trace is faint | "The app isn't sure of my pitch here" (low confidence) |
| No band, solid line | Clean sustained tone, no vibrato |

## Non-goals (for this feature)

- **No numeric readout** of rate/depth on the trace itself. The band is
  glanceable; numbers belong elsewhere if at all.
- **No prescription.** This spec covers *showing* vibrato, not coaching it
  ("imagine the note spinning forward"). Prescription is a separate concern —
  see [`VISION.md`](VISION.md) ("Beyond detection: prescription").
- **No rate visualization** beyond what the wobble's own shape conveys. The band
  encodes depth; rate is legible from the oscillation itself.

## Open question (deferred)

**Descriptive vs prescriptive band.** This spec defines the band as the singer's
*own measured* depth (descriptive). A future variant could make the band a
*fixed reference* depth the singer tries to fill (prescriptive). These teach
different things and shouldn't be conflated; the prescriptive mode is deferred
until there's a target-depth curriculum to drive it.

---

*Provenance: the band approach is the ui-designer recommendation from the Phase
1.5/1.6 trace work, captured here after it was built once and then lost when the
trace renderer was rewritten. The coral colour-tint currently shipping is the
approach this spec supersedes.*
