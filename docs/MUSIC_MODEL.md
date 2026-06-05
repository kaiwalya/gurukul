# Music model

The musical configuration of a session decomposes into **four
independent axes** that drive a dial made of **five concentric
layers**. They are independent by design: changing one should never
force changes in any other. This document is the canonical statement
of the model and the reasoning behind the split.

Source-of-truth split:

- *Why* the model is shaped this way → this doc.
- *What* the Rust types look like → `domain-ports/src/music.rs` (the
  state + view layer), with the head-side resources in
  `apps/coach-game/src/state.rs`. `music.rs`'s own module docs are the
  authoritative prose on the affine point/vector model and the gauge law;
  this doc explains *why* that machinery exists and how it serves the
  dial.

If you're about to change `AppSettings`, `SongTonality`, the music
model in `domain-ports/src/music.rs`, the dial widget, or anything
dial-rendering, **read this doc first**. The
abstractions here are subtler than they look and the first time you
try to write a "what does each slot get labelled?" function you will
get it wrong if you haven't internalised the rings model.

## The dial as five concentric rings

The dial visualises one octave on a log-frequency circle. North = 12
o'clock. Think of it as five layers stacked at the same position,
each independently configurable:

```
            N (top)
        ┌───┴───┐
       /         \
      │  ┌─────┐  │   1. Compass body — the circle. North fixed.
      │ /       \ │   2. Needle — live, continuous log(f) % 2.
      ││  needle ││   3. Tuning ring — 12 tick marks at tuning positions.
      │ \       / │   4. Scale ring — mask: which ticks are lit.
      │  └─────┘  │   5. Label ring — note names, separately rotatable.
       \         /
        └───┬───┘
            S
```

1. **Compass body (fixed):** the circle itself. North = 12 o'clock =
   **the tonic (Sa)** — the dial anchors on the song tonic, so whatever
   key Sa is on renders at the top. Pure geometry.
2. **Needle:** live pitch, displayed **relative to Sa**. **Continuous** —
   it glides smoothly to wherever the voice actually is on the
   log-frequency circle, landing *between* the ticks when the singer
   slides (meend / glissando), never snapping. The model-side fold is
   `tuning_view::octave_position(f0, sa_hz)` (the live `f0` against Sa's
   resolved Hz, a fraction in `[0, 1)`); the dial turns that into an angle
   by `× TAU`. *Tuning-independent.* (Already in code, working.)
3. **Tuning ring:** 12 faint tick marks at log-frequency positions
   set by the **tuning system**. 12-TET → evenly spaced.
   Hindustani Just → uneven. **No labels, no highlights — just where
   the slots sit.** These ticks are the discrete *targets* the
   continuous needle glides between.
4. **Scale ring:** of those tuning slots, which ones are "in the
   scale". A mask on the tuning ring; renders as lit vs dim ticks.
   Derived from the `Tonality` (tonic key + scale widths).
5. **Label ring:** carries the note names. The geometry already put
   the tonic at north, so the ring **doesn't rotate** — its only job is
   to choose *which vocabulary* lands on each slot (movable Sargam vs
   absolute Western note-names). This is the layer that distinguishes
   Western from Hindustani — not the geometry.

The widget itself stays dumb: it takes pre-computed slot angles,
labels, and highlights, and renders them. All musical semantics live
above.

## The label ring's lock mode is the whole game

The single most important conceptual move from earlier design
discussion: **the Western/Hindustani difference is not two different
models. It's one model with two values of "what vocabulary does the
label ring paint?"**

> **Note — revised from an earlier draft.** An earlier version of this
> doc made the *geometry* carry the cultural split: north = C always,
> and the label ring *rotated* to drag Sa to the top for Hindustani
> while leaving C on top for Western. The dial now anchors **north on
> the tonic for everyone** (see "What's in code today" and the worked
> example). That is strictly cleaner: a Western singer in "D Major" is
> putting D as their tonic, so D belongs at north for them too. The
> Western/Hindustani difference collapses entirely into *what the labels
> say* — it never touches geometry. The lock-mode story below is rewritten
> around that.

**Geometry (all cultures): the tonic is at north.** Change the song
tonic from C to D → the whole dial re-anchors so D is at the top.
Nothing about this depends on the note system; it is pure
tonic-relative geometry (the needle and ticks fold against Sa's Hz).

**The label ring's only job is vocabulary.** Two values:

- **Sargam (movable-do):** paint "Sa" at north, "Re, Ga, …" clockwise.
  The same labels appear at the same dial positions no matter which key
  the tonic is on — because the *names* are tonic-relative and the
  *geometry* already put the tonic at north. "Sa" is always at the top
  because Sa is always the tonic and the tonic is always at north.
- **Western (fixed note-names):** paint the absolute note-name of the
  tonic at north ("D" for D Major), then the rest clockwise. The names
  are absolute-pitch, but they sit on the same tonic-anchored geometry —
  so the tonic's name lands at north, and changing key re-letters the
  ring without rotating the scale highlights.

Same machinery underneath; same geometry. The only difference is the
*vocabulary* the labels carry. **Once we get this abstraction right, the
"Western vs Hindustani" distinction stops needing special cases
elsewhere in the codebase** — it lives entirely in the label ring.

A `LabelRing` is therefore:

```rust
LabelRing {
    labels: Vec<Option<String>>,  // 12 entries for chromatic
    lock: LockMode,
}

enum LockMode {
    RelativeToTonic { tonic_label_index: usize },   // Sargam: 0 (Sa)
    AbsoluteHz { label_index: usize, hz: f32 },     // Western: idx of "A", 440.0
}
```

The lock mode now selects **which name lands on each slot**, not how far
to rotate a ring — the geometry already anchors the tonic at north
(above). `RelativeToTonic` reads names off a tonic-relative table (slot 0
of the scale = "Sa"); `AbsoluteHz` reads them off an absolute-pitch table
(the slot whose Hz is 440 = "A", and the rest follow). Both then render
on the same tonic-anchored dial. *Not yet built* — see "What's deferred"
below. This is a simplification of an earlier sketch that had the label
ring carry a rotation; with tonic-at-north geometry, no rotation is
needed.

## The four axes

### 1+2. Tuning (slot geometry)

> **Note:** earlier drafts split this into two axes — "tuning
> reference" (a bare Hz) and "tuning system" (slot geometry). They are
> now modelled as **one domain** with three inputs, because Just
> intonation is not rotationally symmetric and therefore needs to know
> *which slot* the reference Hz pegs — a fact the two-axis split could
> not express. See "Why the two spaces, and why `root` is needed" below.

The tuning domain answers exactly one question: **given the inputs,
where do the N slots sit in Hz?** It carries **no names and no
conventions** — names ("A", "Safed-6") are the note-system layer
(axis 3), which renders at display time and never reaches this domain.

**Two coordinate spaces — keep them apart.** The single source of bugs
here is that two different "which note" indices look identical (both
small integers). They are not the same space:

- **Key space** — a physical key of the instrument. What the
  harmonium player and singer *speak*: "Safed-1", "Kaali-1". Fixed to
  the instrument. Modelled by **`InstrumentKey`** (a *point*) and
  **`InstrumentKeyInterval`** (its *vector* — a signed distance in keys).
- **Slot space** — an index into the tuning's computed slot array,
  where slot 0 is the tuning's root note and the rest fan upward.
  What the tuning *math* works in. Slot space is a plain **array index**,
  not a point/vector space of its own — the bridge from a key to its slot
  (and then its Hz) is `tuning_view`, never arithmetic on the index.

(The genuinely affine sibling of key space is **scale space**, introduced
under axis 4 — degrees counted in *notes* rather than keys. The full
point/vector picture is in "The affine model" below; for the tuning
domain, key space and the slot-array index are what matter.)

```rust
/// A position on the keyboard line — a POINT in the affine model.
/// Name-free and `Copy` (so it crosses the FFI boundary): the *name*
/// ("Safed-1" vs "C") is derived head-side from `offset` by the note
/// system, never stored here.
struct InstrumentKey { offset: f32 }
// the one licensed constructor stamps the C-origin gauge:
//   harmonium_key(0.0) -> { offset: 0.0 } == Safed-1 / C, octave 0

/// A signed distance between two keys — the VECTOR of key space.
struct InstrumentKeyInterval(pub f32);
```

**`offset` is one number on the whole multi-octave line — and it is an
`f32`, not an integer.** A key is a position on a *continuous* line: the
whole keys are the keyboard's fixed positions, but a sliding voice sits
*between* them (offset `1.4`), which is exactly the slide the dial's
needle traces. `InstrumentKey` is therefore `PartialEq` but **not** `Eq`
(no total order on floats); nothing hashes it.

**`InstrumentKey` carries no `octave_size` and no methods.** This is the
key change from earlier drafts. A point doesn't own the keyboard's
keys-per-octave — that's a fact about the *tuning*, needed only when you
fold a position into the repeating slot pattern. So there is **no**
`fold()` and **no** `octave()` on the key. The only operations a point
exposes are the affine ones: subtract two keys to get an
`InstrumentKeyInterval`, or move a key by an interval. **The fold/octave
`divmod` moved into [`tuning_view`](#state-vs-view--two-layers)**, where
it runs against the tuning's slot count `N`, on the *delta* `key − root`
— never on an absolute offset. Why that matters is "The gauge law" just
below.

### The affine model: point vs vector, and the gauge law

The deepest organising idea in the code (and the thing the rings model
quietly relies on) is that pitch has **affine structure**, and the types
encode it. There are two affine point/vector spaces plus a terminal Hz
output:

- **Key space** — `InstrumentKey` is the *point*, `InstrumentKeyInterval`
  the *vector*. `InstrumentKey − InstrumentKey = InstrumentKeyInterval`
  (Sa→Re = 2 keys); `InstrumentKey + InstrumentKeyInterval = InstrumentKey`.
- **Scale space** (axis 4) — `ScaleNote` is the *point*,
  `ScaleNoteInterval` the *vector*, counted in *notes* (Sa→Re = 1 note).
- **Hz** is the terminal output of `tuning_view::hz` — a plain `f32`, not
  a point type; its "interval" is a multiplicative ratio.

The algebra is encoded as operator impls, and the **gauge law** is
encoded as the operators that *deliberately do not exist*:

```text
InstrumentKey  − InstrumentKey         = InstrumentKeyInterval   // the only path to Hz
InstrumentKey  + InstrumentKeyInterval = InstrumentKey           // place a degree on the keyboard
Interval       ± Interval              = Interval                // compose distances
InstrumentKey  + InstrumentKey         → DOES NOT COMPILE        // adding two gauges is nonsense
```

**The law: no logic may depend on a gauge.** Key space is affine — its
*origin* ("offset 0 = C") is a pure labelling choice, a gauge. Shift
every offset (keys, roots, tonics) by the same constant and **nothing
observable changes** (verified: moving the A=440 root from offset 9 to
21, and the tonic 0→12, left the resolved Hz table byte-for-byte
identical). So nothing may branch on an *absolute* `InstrumentKey`, only
on a **difference** (`InstrumentKeyInterval`), because a difference is
what survives a gauge shift: `(a+c) − (b+c) = a − b`. The type system
enforces it — an `InstrumentKey` has no arithmetic of its own, so the
*only* way to reach a slot or an octave is to first subtract to an
interval and then divide by the period `N` in `tuning_view`. The
octave-wrap bug (deriving the octave from two separately
gauge-dependent quantities that don't cancel) becomes
**unrepresentable**.

This is why `fold()`/`octave()` are gone: they read an *absolute*
offset, which the gauge law forbids. The licensed gauge pick happens at
exactly one door — the constructor `harmonium_key` (C-origin). Naming
that constructor *is* choosing the chart; pick one for the whole system
and never mix, since the cancellation only works when both operands
share a gauge.

**Two phases: construct once, then read frozen data.**

*Construction* takes three transient inputs:

1. **`root_note_hz: f32`** — the anchor frequency. A bare number; the
   "A=" in "A=440 Hz" is a *name*, supplied by the note system, not
   stored here.
2. **A `TuningKind`**, picked from an extensible set. Each kind
   declares its own slot count `N` and the rule for spacing the slots.
   Today:
   - **12-TET** — `N = 12`, slots 100 cents apart.
   - **Hindustani Just** — `N = 12`, 5-limit ratios (1/1, 16/15, 9/8,
     6/5, 5/4, 4/3, 45/32, 3/2, 8/5, 5/3, 9/5, 15/8). Pa lands at
     exactly 3/2 (slightly sharper than 12-TET G); shuddha Ga at 5/4
     (~14 cents flatter than 12-TET E).
3. **`root: InstrumentKey`** — the physical key the instrument was
   *tuned from*. The kind's ratios fan out from this key's note. **No
   key is privileged by the domain** — the caller supplies it. The
   familiar "A=440" convention is the *head* passing the key it will
   name "A"; the tuning domain never knows it as "A".

The `TuningKind` is a **pure shape generator** — it builds the N
frequencies of its fixed pattern *upward from the root note*, with the
root at slot 0:

```rust
enum TuningKind { TwelveTet, HindustaniJust }

impl TuningKind {
    /// The N frequencies of this kind's fixed pattern, built from the
    /// root note `root_hz` at slot 0. 12-TET: each slot ×2^(1/12) from
    /// the last. Just: `root_hz` × each 5-limit ratio. Length == N.
    /// `self` by value — a `TuningKind` is `Copy`.
    fn shape(self, root_hz: f32) -> Vec<f32> { /* ... */ }
}
```

The head hands the coach a flat, `Copy` **`TuningSpec`** carrying those
three inputs (`root_note_hz`, `kind`, `root`); the coach runs
`Tuning::new(spec)` — **one argument** — on receipt:

```rust
struct TuningSpec { root_note_hz: f32, kind: TuningKind, root: InstrumentKey }

/// A frozen tuning. OPAQUE: its fields are private; the only code that
/// sees inside is `tuning_view` (a child module). Everyone else holds a
/// `Tuning` and calls `tuning_view::hz` / `Tuning::n`.
struct Tuning {
    /// **One octave** of slot frequencies (linear Hz) in **slot space**:
    /// `slots_linear[0] == root_note_hz`, the rest fan upward by the
    /// kind's pattern for exactly N slots. `len() == N`. The pattern
    /// repeats every octave, so a key an octave up is `× 2`; the array
    /// never stores more than one octave (see `tuning_view::hz`). Feeds
    /// the dial's tuning ring (layer 3).
    slots_linear: Vec<f32>,
    /// `log2` of each `slots_linear` entry, frozen alongside it — the
    /// *pitch-linear* view of the same slots. An octave is `+1.0` here,
    /// and a fractional key between two slots is a plain weighted average
    /// of two of these followed by one `exp2`: this is the math that
    /// makes the slide land on a real Hz (see `tuning_view::hz`).
    slots_log2: Vec<f32>,
    /// The physical key (**key space**) that slot 0 sits on — the key
    /// the instrument was tuned from. The bridge between the two spaces:
    /// every `tuning_view` computation starts by subtracting it.
    root: InstrumentKey,
}

impl Tuning {
    fn new(spec: TuningSpec) -> Tuning {
        // slot 0 IS the root note; the kind fans upward from it. The two
        // slot arrays are filled here together and NOWHERE else, so they
        // cannot drift — one constructor fills both.
        let slots_linear = spec.kind.shape(spec.root_note_hz);
        let slots_log2 = slots_linear.iter().map(|hz| hz.log2()).collect();
        Tuning { slots_linear, slots_log2, root: spec.root }
    }
    /// The slot count N. Representation-neutral (both arrays are len N),
    /// so it stays public — the adapter reads it for `well_formed`.
    fn n(&self) -> usize { self.slots_linear.len() }
}
```

**Why two slot arrays, and why opaque.** The struct keeps both `slots_linear`
(Hz) and `slots_log2` (their base-2 logs). Storing a derived shadow
would normally invite drift — but both are written *once, together* in
the single constructor and never again (private fields, no setter), so
there is still exactly one source of truth. Opacity is what licenses the
redundancy: because only `tuning_view` sees inside, the view picks
whichever representation is cheaper per job (linear for a slot's target
Hz, log for interpolating the slide) with no outside coupling, and
callers never learn the slots are stored twice. They hold a `Tuning` and
ask `tuning_view::hz` / `Tuning::n`.

The *reading* of a `Tuning` — key→slot, slot→Hz, the slide interpolation
— lives in the View layer below, not on the struct. The struct owns only
its birth (`new`) and its data.

`root_note_hz` and the kind are absorbed into the slot arrays; the
`root` key survives because it bridges key space to slot space. A
`Tuning` is a *read* value computed inside the coach — not a flat FFI
command payload — so its `Vec`s and kind-defined length are unproblematic.
(`InstrumentKey` and the `TuningSpec` it travels in *are* flat/`Copy` and
do cross the wire; the constructed `Tuning` does not.)

**Why the two spaces, and why `root` is needed even in 12-TET.** 12-TET
is rotationally symmetric: the *set* of 12 frequencies is identical no
matter which physical key you tuned from, so `root` there is almost
pure labelling. Just intonation is **not** symmetric — its slots are
unequal ratios fanning out from the root note. "440 Hz" alone is then
ambiguous: 440 Hz at *which* key? You must say which physical key the
ratios fan out from. So `root` is genuinely load-bearing for Just and
near-vestigial for 12-TET — which is why it is a required input rather
than a 12-TET-only convenience. Without it, you would have 12 floating
frequencies and no idea which harmonium key each one belongs to.

**Why N is owned by the `TuningKind`.** The slot count is a property of
the *algorithm*, not a global constant. 12-TET and Hindustani Just
both emit 12; a future Carnatic 22-shruti kind emits 22. Letting the
kind declare `N` (and reading it back as `Tuning::n`) keeps the domain
N-agnostic without building the microtonal case now — the extra kinds
are deferred, but the shape already admits them.

**Why it's its own domain (independent of axis 3):** A Sargam user can
sing in 12-TET (many do, especially keyboard-trained); a Western user
can experiment with Just. The vocabulary used to *name* notes is
unrelated to where the slots *land*. Tuning is acoustic; naming is
language.

### 3. Note system (vocabulary)

> **Status: designed, not built.** This axis is *deferred*. The head is
> currently **vocabulary-free** — it stores no note-naming scheme and
> invents no labels. The sections below describe the intended model so
> the label layer lands coherently; until then, nothing in code names a
> slot or a tonic. The HUD shows the [math view](#what-s-in-code-today)
> (degrees / keys / Hz) instead. An earlier pass *did* implement a
> `NoteSystem` enum (`tonic_label`, `scale_name`, `AppSettings.note_system`)
> head-side; it was removed precisely because it duplicated the model's
> vocabulary outside `music.rs` and drifted. Reintroduce it only as the
> real `LabelRing`/`LockMode` layer (see [What's deferred](#whats-deferred)).

The labels used to talk about slots. Pure presentation; no effect on
math, geometry, or audio.

- **Western** — C, C♯, D, E♭, E, F, F♯, G, A♭, A, B♭, B.
- **Sargam Latin** — Sa, Re, Ga, Ma, Pa, Dha, Ni (plus komal/tivra
  forms; see below for why this list is shorter than 12).
- **Sargam Devanagari** — सा, रे, ग, म, प, ध, नि.
- Will be stored as: a head-side label-layer choice (shape TBD — see
  the `LabelRing`/`LockMode` sketch below), **not** on `AppSettings`
  today.

**Why it's its own axis:** Vocabulary is orthogonal to acoustics. The
same 12-TET slot at 440 Hz is "A" in Western or some other label in
Sargam depending on the tonic. The user picks which system they're
fluent in.

#### Western has one table; Sargam has two

A note system has to do **two** rendering jobs, and Western happens
to use the same vocabulary for both — which is what masks the asymmetry
and made earlier passes of this model fall over.

**Job A — labelling the dial label ring.** What's written on each
position around the dial. This is the "what's at the top of the
circle" job. Driven by the label ring's lock mode (see above).

**Job B — naming the tonic in HUDs and pickers.** When the user is
asked "which key are you in today?" / when the HUD shows the
current session's tonic, what string do we display? This is an
*absolute* job — it points at a specific chromatic slot.

**Western collapses both jobs into one absolute-pitch table:**

| Slot | Job A (dial label) | Job B (tonic name) |
|---|---|---|
| 0 | C | C |
| 1 | C♯ | C♯ |
| 2 | D | D |
| ... | ... | ... |

A Western singer announces "I'm singing D Major" — the tonic name "D"
is just the slot label for slot 2. One vocabulary, two jobs.

**Sargam needs two distinct tables:**

| Slot | Job A (dial label, tonic-relative) | Job B (tonic name, absolute harmonium position) |
|---|---|---|
| 0 | Sa | Safed-1 |
| 1 | re (komal Re) | Kaali-1 |
| 2 | Re | Safed-2 |
| 3 | ga (komal Ga) | Kaali-2 |
| 4 | Ga | Safed-3 |
| 5 | Ma | Safed-4 |
| 6 | Ma' (tivra) | Kaali-3 |
| 7 | Pa | Safed-5 |
| 8 | dha (komal Dha) | Kaali-4 |
| 9 | Dha | Safed-6 |
| 10 | ni (komal Ni) | Kaali-5 |
| 11 | Ni | Safed-7 |

Note what each column does:

- **Job A (Sa, Re, Ga, …)** is *tonic-relative*. "Sa" is always the
  tonic, never a specific chromatic slot — the position of "Sa" on
  the dial depends entirely on where the tonic key is, because the
  label ring is locked to the tonic (`RelativeToTonic`). A Hindustani
  singer never says "I'm singing in Sa" — that would be like a
  Western singer saying "I'm singing in 1" — because Sa is *defined*
  to be the tonic. Saying "Sa" tells you nothing about which
  harmonium key the singer is using.
- **Job B (Safed-1, Kaali-1, …)** is *absolute*. These name physical
  harmonium key positions: Safed = white key, Kaali = black key, the
  number counts within each colour across an octave. "Kaali-1" is
  *always* the first black key — slot 1 — no matter what the singer
  is doing. A Hindustani singer announces "I'm singing Kaali-1
  Bilawal" the way a Western singer announces "I'm singing D Major":
  *here is where I put my Sa today, here is the scale I'm using.*

This is why the HUD badge would read "Kaali-1 Bilawal" for a Sargam
user with the tonic on key 1, and "C♯ Major" for a Western user with
the same tonic key, *once the label layer ships*. Same data, two
different absolute-naming vocabularies. The dial face — when the label
ring is rendered, layer 5 — would meanwhile show "Sa" at the top in
both cases (Sargam), or "C" at the top with the highlight rotated to
slot 1 (Western). **Today**, with the head vocabulary-free, the HUD
shows the math view (`deg 0 2 4 5 7 9 11` / `key …` / `Hz …`) — the
same data with no names attached.

#### Where this collides with the `LabelRing` abstraction

The dial-label-ring lock mode (Job A) and the tonic-naming
vocabulary (Job B) are **independent choices** that happen to be
bundled per note system because culturally they go together.
Cleanest possible factoring:

```rust
NoteSystem {
    dial_labels: Vec<Option<String>>,           // Job A
    lock_mode: LockMode,                        // Job A (rotation)
    tonic_naming: Vec<&'static str>,            // Job B (per-slot absolute)
}
```

For v1 we don't need to expose this as configurable — bundling per
note system enum variant is fine. But the model recognises that
"label-ring vocabulary" and "tonic-naming vocabulary" are separable
concerns that just happen to ship together. A future "Western
movable-do" mode would mean reusing Western's tonic-naming with
Sargam's lock mode + label set.

### 4. Tonality (per-song selection)

The singer's per-song choice, layered on a [`Tuning`](#12-tuning-slot-geometry):
*of the tuning's N slots, which one is home, and which ones are in the
song?* Two values, one type, because they're meaningless separately:

```rust
struct Tonality {
    /// The key the singer calls home (Sa) — a POINT, the scale-space
    /// origin. **Key space** — the same space as the tuning's `root`
    /// (what the singer says: "Kaali-1"). This is the *song* root — the
    /// second of the two roots, distinct from the tuning's `root` (the
    /// key the instrument was tuned from). Both are `InstrumentKey`s on
    /// the same keyboard: one is where the tuner anchored, one is where
    /// the singer puts Sa.
    tonic: InstrumentKey,

    /// The scale's shape as **key-widths between successive notes** —
    /// `InstrumentKeyInterval`s (vectors in key space), walking up from
    /// the tonic. `[2,2,1,2,2,2,1]` for Bilawal/Major: each entry is how
    /// many *keys* (semitones) that note-step spans. These are *gaps*,
    /// not notes: the tonic (Sa) is implicit at the start, `widths[0]`
    /// is the Sa→Re width (2), and so on. So the number of widths (to the
    /// terminator) equals the number of notes, and they sum to N. **This
    /// array IS the scale-space → key-space conversion table.**
    ///
    /// Fixed-capacity (`MAX_SCALE_NOTES`) and **0-terminated**: read
    /// widths until the first `InstrumentKeyInterval(0.0)`. A `0` width is
    /// never musically valid (two notes on one key), so the sentinel is
    /// self-validating. This keeps `Tonality` flat and `Copy` so it can
    /// cross the FFI command boundary directly (unlike `Tuning`, which is
    /// coach-internal).
    ///
    /// **Invariant: every width is a whole number** (`2.0`, `1.0`, …).
    /// The element type is `InstrumentKeyInterval` (`f32`) only so a scale
    /// shares the affine vocabulary with the continuous key line; the
    /// fractional freedom is for the *live slide*, never an authored
    /// scale. Consumers needing an integer slot index (the dial mask)
    /// round, relying on this invariant.
    widths: [InstrumentKeyInterval; MAX_SCALE_NOTES],
}
```

`MAX_SCALE_NOTES` is a named `pub const = 32` (not a bare literal): a
fixed cap that keeps `Tonality` flat and `Copy`; 32 covers any 12- or
22-slot scale with room. `Tonality` is `PartialEq` but **not** `Eq` — it
holds `f32` widths and an `f32`-offset tonic.

`widths` is *tonic-relative* — it describes the scale's shape starting
at the tonic. `tonic` then says which physical key that shape is planted
on. The two are genuinely separate: `widths` is the scale's **shape**,
`tonic` is **where you plant it**.

**The two spaces meet here.** Key space and scale space (axis 4's
degree-count line) are deliberately parallel-but-distinct types, so a
note-distance can never be silently spent as a key-distance. They touch
on exactly one type — `Tonality` — and exactly one method,
`Tonality::key_of(ScaleNote) -> InstrumentKey`, which sums the first *d*
widths up from the tonic (`tonic + Σ widths[0..d]`). That sum *is* the
scale-space → key-space conversion: 1 note ⇒ its key-width (the first
Bilawal step, 1 note ⇒ 2 keys), plus the `+tonic` injection that lands
it on the keyboard. `ScaleNote(0)` (Sa) has an empty sum, recovering the
tonic. The reading methods are `widths()` (the slice up to the
terminator), `note_count()` (= `widths().len()`, the scale-space period),
`key_of()`, and `well_formed(n)`.

**Well-formedness invariant — widths sum to N.** Walking the widths from
the tonic must traverse exactly one octave and land back on the tonic.
So the widths (up to the terminator) must sum to the tuning's slot count
`N`. Sum < N never closes the octave; sum > N wraps past it. This is the
whole invariant — no interior `0` (`Tonality::new` rejects that by
construction) and no separate per-step bound (sum == N with non-negative
widths already caps any single width at N).

Because `N` comes from the *tuning* (`Tuning::n`), not from `Tonality`
alone, `well_formed` is a **method that takes `n`** and is called at the
**join point** — where a `Tonality` meets a `Tuning` (the coach
accepting the config). The same 7-note scale is valid on a 12-slot
tuning and invalid on a 22-slot one.

```rust
impl Tonality {
    fn well_formed(&self, n: u8) -> bool {
        // Widths are whole and small → they sum *exactly* in f32 (no
        // rounding below 2^24), so `== n` needs no epsilon.
        self.widths().iter().map(|w| w.0).sum::<f32>() == n as f32
    }
}
```

**Failure policy (today): construct-time guard.** Only code builds
`Tonality` right now — there is no per-song picker producing arbitrary
input — so an interior `0` or over-long list panics in
`Tonality::new`'s `debug_assert!`, and a bad sum is caught by
`debug_assert!(t.well_formed(n))` at the join. This graduates to a
runtime **reject** (log + keep the prior/default `Tonality`, matching
the coach's silent-no-op convention for illegal commands) when the
picker lands and untrusted input can reach it.

**Why it's its own axis:** These change per song. Singer's choice,
not harmonium-maker's. "I'll sing Vande Mataram in D Yaman" → tonic
= D, scale = Yaman. "I'll sing Yesterday in F Major" → tonic = F,
scale = Major. The tuning and vocabulary haven't moved.

**Why tonic is an `InstrumentKey`, not a note name like "D":** Because
"D" is a Western name. The same data needs to render as "D" for a
Western user and "Kaali-1" (or whichever harmonium position the key
corresponds to) for a Sargam user. Storing the *key* (a bare `offset`)
and resolving the *name* at render time via the note system's tonic-
naming table (Job B above) keeps the storage neutral to vocabulary
choice.

**The in-scale mask is a head-side render projection** (see "The mask
is a head-side projection" below). Given a `Tonality` and the tuning's
`root`, the mask (`[bool; N]` — which slots are in the song) is derived
by walking the `widths()` from the tonic. The mask is **slot-indexed**:
slot 0 is the tuning's root key, so it zips directly with the tick-angle
table (also slot-indexed). The walk starts at the tonic's **slot
index** — the gauge-clean delta `(tonic − root).0.round() % N`, *not* the
absolute offset — and adds each (rounded) width modulo `N`, marking every
visited slot. (The round is exact: tonic and widths are whole by the
`Tonality` invariant; only the live slide is fractional. Subtracting
`root` first is the gauge law: the lit set must depend only on the
*difference* tonic − root, never on an absolute key — see "The affine
model".) It is **tuning-independent** — the lit slot set is the same
integer pattern regardless of whether the tuning is 12-TET or Just (the
tuning only changes where each slot is *drawn*, not which is lit). So the
head computes it directly from the `Tonality` it holds; it does not cross
the port and the coach does not compute it. It feeds the dial's scale
ring (layer 4).

**Worked: what frequency is "Sa"?** The trap is that the `widths` play
*no part* in finding Sa — Sa is the tonic, before any width. The widths
only locate Re, Ga, … So the path to Sa's Hz is the short one. Example:
a 12-key harmonium tuned from A in octave 1 (`root = harmonium_key(21.0)`),
the singer puts Sa on the D just below it (`tonic = harmonium_key(14.0)`):

1. **Key → delta** (subtract the gauge first, the only path to Hz):
   `delta = tonic − root = InstrumentKeyInterval(14.0 − 21.0) = −7`. Sa
   sits seven 12-TET steps *below* the A we tuned from.
2. **Delta → Hz** (slot and octave from the *same* delta, via one
   divmod): slot = `(−7).rem_euclid(12) = 5`, octave =
   `(−7).div_euclid(12) = −1`, so `exp2(slots_log2[5] + (−1)) =
   440 × 2^(5/12) × 2^(−1) ≈ 293.7 Hz` (D one octave below the A). That's
   Sa. (A *whole* key like this reduces to the old
   `slots_linear[5] × 2^(−1)` exactly; the log form is what lets a
   *fractional* key interpolate — see "the slide" below.)

A general scale degree `d` *does* use the widths — `Tonality::key_of`
places it on the keyboard first (`tonic + Σ widths[0..d]`), then
`tuning_view::hz` resolves that key through the same delta-from-root
path: it computes `delta = key − root` and folds slot *and* octave from
that one delta. Degree 0 (Sa) has an empty sum (`key_of(ScaleNote(0)) ==
tonic`), recovering the trace above. Crucially the octave comes from the
*full* delta, not from folding the degree into the root's octave — that
keeps the scale ascending instead of wrapping (the octave-wrap bug we
fixed).

## State vs View — two layers

Everything above (§1+2, §4) is the **state layer**: the *memory* —
what's stored, what crosses the FFI wire. `InstrumentKey`, `Tuning`,
`Tonality`, `TuningKind`. It is deliberately dumb. It holds bytes —
arrays, offsets, a tonic — and knows how to *be born* (`Tuning::new`,
`Tonality::new`, the well-formed check) but nothing about how to be
*read*. The state doesn't know it's a number line; it's just the data.

On top of that sits the **View layer**: the functions that impose a
**number-line interpretation** on the state. "Give me key 14", "fold to
one octave", "walk three scale degrees up from Sa", "which keys are
lit" — these are *operations* on the state, where the integers acquire
a coordinate meaning. The View reads the state but is not the state.

The pipeline of lines, and the map between each:

```
 Tonal line  ──(tonality lens)──▶  Instrument line  ──(tuning)──▶  Hz line
   degree d                          abs key                       frequency
```

Each arrow is a View function. There is no third coordinate *system* —
the Tonal line is the Instrument line seen through an offset (to Sa) and
a scale filter; the Hz line is the Instrument line seen through the
tuning. Only the Hz↔Instrument map touches real frequencies and
arithmetic; the rest is index math.

**A View is a module of free functions** — a named bag of pure
functions that take the state as plain parameters and return a number.
No instance, no borrow, no cached state: state in, number out. (In
Rust, `mod tuning_view { pub fn hz(..) .. }`, called as
`tuning_view::hz(&t, key)`. A module — not an empty struct — is the
idiomatic Rust way to group pure functions: it confines the conversions
to one named place per line, the same disambiguation `InstrumentKey`
gives key-vs-slot, without a never-instantiated phantom type. It is a
*module*, not a `TuningView` struct, everywhere.)

Only **one** View is genuinely shared coach-side: `tuning_view`, the
Hz↔key map, because it reads a `Tuning` (which only the coach holds).
The scale-line projection that *would* have been a `tonality_view` does
**not** live here — see "The mask is a head-side projection" below.

```rust
/// The Hz↔key map — the only place real frequencies and key↔slot
/// arithmetic live. Reads a `Tuning`, never owns it.
mod tuning_view {
    /// Where a frequency sits WITHIN one octave, as a fraction in
    /// `[0, 1)` — the log-frequency fold, `log2(hz / ref_hz) mod 1`.
    /// `ref_hz` maps to 0. The model-side truth about "position around
    /// the octave circle"; a dial turns it into an angle by `× TAU`.
    /// Tuning-independent — it places ANY Hz, a slot's target *or* a
    /// live sliding voice between slots. THIS is what the continuous
    /// needle (layer 2) and the slot-angle geometry (layer 3) are built
    /// from. (Replaces the deleted `slot_of`.)
    pub fn octave_position(hz: f32, ref_hz: f32) -> f32 {
        if hz <= 0.0 { return 0.0; }
        (hz / ref_hz).log2().rem_euclid(1.0)
    }

    /// Hz of any key, at any octave, relative to the root — INCLUDING a
    /// fractional key (the slide), interpolated between the two whole
    /// keys bracketing it. The interpolation is a plain **average in
    /// pitch**: log-frequency of the slot at/below the key, lerped with
    /// the next slot up by the fractional part, exponentiated once.
    /// Equal steps in `offset` become equal steps in *cents* — how the
    /// ear hears a glide; a straight average of frequencies would read
    /// sharp. Working from `slots_log2` makes this a literal weighted
    /// average of two stored numbers (no `log` at call time); the octave
    /// wrap (slot 11 → slot 0 an octave up) falls out for free.
    ///
    /// A WHOLE key has `frac == 0`, so this reduces to
    /// `slots_linear[d mod N] × 2^(d div N)` — the old behaviour exactly.
    pub fn hz(t: &Tuning, key: InstrumentKey) -> f32 {
        let d = (key - t.root).0;            // the gauge-invariant delta
        let floor = d.floor();
        let frac = d - floor;
        let lo = slot_log2(t, floor as i32); // log-freq of the slot below
        if frac == 0.0 { return lo.exp2(); }
        let hi = slot_log2(t, floor as i32 + 1);
        (lo + (hi - lo) * frac).exp2()
    }

    // slot_log2(t, delta) = slots_log2[delta mod N] + (delta div N):
    // the slot's log-frequency plus one +1.0 per octave. Slot and octave
    // from the SAME delta, so a key below the root lands an octave down.
}
```

`octave_position` is the log-frequency fold the model now exposes as
first-class truth: the **continuous needle glides** to wherever a live
Hz lands on the circle (`octave_position(f0, ref) × TAU`), and the same
function placed against each tuning slot's frequency yields the **tuning
ring's tick angles**. The needle never snaps; the ticks are the targets
it glides between.

**The slide, concretely.** `hz` interpolating a *fractional* key is what
makes a voice sitting between two keyboard keys resolve to a real Hz: it
is the average-in-cents of the two bracketing ticks (their `slots_log2`
lerped, then `exp2`). This is the whole reason key space is `f32` and
the interpolation is done in log space. The asymmetry is deliberate and
load-bearing: **the needle/key line is continuous** (you slide between
pitches), **the scale-degree line stays discrete** (you don't slide
between scale degrees — `ScaleNote` is integer; see axis 4 and "the
affine model"). A degree-count is combinatorial; a key position is
physical.

### The mask is a head-side projection, not a shared View

The in-scale mask (which of the N slots are lit) is the dial's
**scale ring** (layer 4). It is *derived* render data — a projection of
the `Tonality` the head already holds — and it is **tuning-independent**:
the lit *slots* are the same integer set whether the tuning is 12-TET or
Hindustani Just. The asymmetry of Just changes where each tick is
*drawn* (the tuning ring, layer 3, which is Hz→angle render geometry),
not *which* ticks are lit. Walking the `widths()` is pure integer math
(the round-to-slot is exact by the whole-width invariant) that needs
only the `Tonality` and `N`, never the `Tuning`'s frequencies.

So the mask is **not** computed by the coach and **not** a shared
`tonality_view`. The head holds the `Tonality` (the same flat/`Copy`
type it sent across the port via `ConfigureSession`) and **the dial
walks the widths itself** (`in_scale_mask` in `dial.rs`) — sibling to
the angle math it already does head-side. There is no `MaskSnapshot`
type, no mask read-publisher: the mask never crosses the port, because
the head already has its source.

`Tonality` *does* cross the port — but as the **coach's frame of
reference for judging pitch** (the eventual scoring), not for the mask.
The two uses are independent: the coach holds `Tonality` to interpret
singing; the head holds the same `Tonality` to paint the scale ring.

**Where each function lands, and why:**

| Function | Layer | Lives in | Why |
|---|---|---|---|
| `Sub`/`Add` on `InstrumentKey`·`InstrumentKeyInterval` (and the `ScaleNote`·`ScaleNoteInterval` mirror) | affine operators | `music.rs` (the point/vector types) | the gauge algebra itself — `key − key = interval`, `key + interval = key`; `key + key` deliberately doesn't compile |
| `harmonium_key` | gauge constructor | `music.rs` | the one licensed door where a bare number becomes a C-origin `InstrumentKey` |
| `Tuning::new`, `Tonality::new` | State | the state struct | construction = birth; state owns being born |
| `Tonality::widths`, `note_count`, `key_of` | State (read) | `Tonality` | read a scale's *own* fields (the widths, the tonic) — no external state, so methods on the type; `key_of` is the scale-space → key-space bridge |
| `Tonality::well_formed(n)` | State-join | called at the Tuning×Tonality join (coach), not at `Tonality::new` | needs `N` from the *tuning*, which `Tonality` alone doesn't have |
| `tuning_view::octave_position`, `tuning_view::hz`, `tuning_view::key_of_hz` | View | `tuning_view` (coach) | the Hz↔key map; `octave_position` is the log-fold the needle uses (live `f0` against Sa's Hz), `hz` resolves a key (whole or sliding) to Hz, `key_of_hz` is its inverse; all read a `Tuning` |
| `tuning_view::slot_position_from(t, ref_key, i)` | View | `tuning_view` (coach) | the tuning ring's tick geometry — slot `i`'s real Hz folded against `ref_key`'s (the dial passes the tonic, so Sa = north). Hz-based so a non-uniform tuning keeps its uneven spacing; the dial only `× TAU` |
| `dial::in_scale_mask` (width walk) | head render | `dial.rs` (head) | tuning-independent **slot-space** integer projection (slot 0 = tuning root) of the `Tonality` the head holds; walked from the gauge-clean delta `(tonic − root) % N`; sibling to its angle math |

The dividing line: **state = how it's built and read from its own
fields; View = how it's read against a `Tuning`; render = how the head
paints it.** The affine operators are not a View-exception — they read
no *external* state, only the operands' own offsets, so they live with
the types. There is no longer any `fold`/`octave` method on
`InstrumentKey`: a key is just a point, and folding a *delta* into a slot
+ octave is `tuning_view`'s job (it needs the tuning's `N`).

## The two roots — precisely

The single most important structural point. Conflating the two roots
is the most common modelling mistake.

1. **Tuning reference** — the calibration peg.
   - "A = 440 Hz." A tuning fork. A physical anchor that tells the
     tuning system how to compute every other slot's Hz.
   - In 12-TET with A=440: C = 440 × 2^(-9/12) ≈ 261.63 Hz, etc.
   - In Hindustani Just: ratios are anchored to 1/1, but you still
     need a Hz number — "the reference frequency that everything is
     derived from." Stored once: `reference_hz`.
   - **Rarely changes.** Property of the instrument or convention.
     Most people set A=440 once and live there.

2. **Song's tonic** — the home note of the piece.
   - "I'm singing in D Major." D is the tonic.
   - Doesn't change the tuning. The instrument is still A=440-tuned.
     The singer just chose D as where the scale starts.
   - In Sargam: "Sa = D today" means "my tonic is D today; the
     instrument is still A=440-tuned." Sa **is** the tonic. Not slot
     0 of the tuning. Whichever slot the tonic lives on.
   - **Changes per song** — different keys for different singers,
     ragas, moods.

These are independent. A=440 doesn't move when you change keys. The
tonic does.

**Where I had this collapsed earlier (and may regress to again):** in
a trivial session — Bilawal in C, A=440, Sargam-with-Sa=C — the two
roots point to the same slot. That coincidence hides the distinction.
The moment the singer says "I'll sing in D", the two split:

- Tuning reference: still A=440 (or equivalently C=261.63).
- Tonic: D.
- *Where north of the dial sits depends on the label ring's lock
  mode*, not on which root is "the" root.

## Worked example

A Sargam user, A=440 standard, singing Bilawal in D on a 12-TET dial:

| Axis | Value | Stored as |
|---|---|---|
| Tuning reference | 440 Hz | `AppSettings.reference_hz = 440.0` |
| Tuning system | 12-TET | `AppSettings.tuning_kind = TwelveTet` |
| Note system | Sargam Latin | *(deferred axis-3 label layer — no `AppSettings` field today)* |
| Song tonic | D (key 2) | `Tonality.tonic = harmonium_key(2.0)` |
| Scale | Bilawal | `Tonality.widths = [2,2,1,2,2,2,1,0,…]` (whole-number `InstrumentKeyInterval`s, `0`-terminated) |

The first three axes are head-held `AppSettings`; `tuning_spec()`
marshals them into the `TuningSpec` that crosses the port (root pegged
at `harmonium_key(21.0)` = A in octave 1, `root_note_hz = reference_hz`).
The tonic +
scale ride in the head's `SongTonality(Tonality)` resource and cross
the port as a `Tonality` via `ConfigureSession`.

**North = the tonic (Sa), for everyone.** The dial's geometry anchors
on the song tonic: whatever key the singer planted Sa on renders at 12
o'clock. This is *tonic-first*, not Hindustani-first — a Western singer
in "D Major" is putting D as their tonic, so D sits at north for them
too. "Sa" is simply the Hindustani word for "the tonic"; the geometry
does not know or care what the labels call it. What differs between
cultures is **what the labels say** (the deferred label ring), *not*
where the tonic sits.

What the user sees (Sargam, Bilawal, Sa on D = key 14, tuning root A =
key 21):

- **Tuning ring:** 12 evenly-spaced ticks (12-TET), each placed by
  `tuning_view::slot_position_from(t, tonic, i)` — slot `i`'s real Hz
  folded against Sa's Hz. Sa lands at north; the tuning root A rotates
  to the 7-o'clock (Pa) position.
- **Scale ring:** slot-space positions `[0,2,4,5,7,9,10]` highlighted
  (the in-scale mask, walked head-side from `Tonality` starting at the
  tonic's *slot index* `(tonic − root).round() % 12 = 5` and adding each
  width mod 12). The mask is **slot-indexed** (slot 0 = the tuning root),
  the same index space as the tick angles, so they zip by index.
  Tuning-independent: the lit set is the same in 12-TET or Just; only the
  drawn angles differ.
- **Needle:** the live `f0` folded against Sa's Hz
  (`tuning_view::octave_position(f0, sa_hz) × TAU`) — the same Hz fold
  the ticks use, so a perfectly-sung Just Pa lands exactly on the uneven
  Just Pa tick.
- **Label ring (deferred):** would paint "Sa, Re, Ga, Ma, Pa, Dha, Ni"
  on the in-scale slots reading clockwise from north, plus komal/tivra
  forms on the out-of-scale slots. The labels ride *on top of* the
  already-tonic-anchored geometry; the ring's only job is *what the
  labels say*, never *which ring rotates*.
- **HUD badge:** "Safed-2 Bilawal" — "Safed-2" because that's how a
  Sargam user announces their tonic ("my Sa is on Safed-2 / the 2nd
  white key today"); "Bilawal" because that's the school's name for
  this step vector. The HUD does **not** say "Sa Bilawal" — Sa is always
  the tonic by definition.

Switch the same setup to Western note system, A=440, "Major in D":

- **Tuning ring, scale ring, needle:** identical. The geometry is
  tonic-anchored, so D is at north here too. Same step vector, same
  tonic key, same lit set, same tick angles.
- **Label ring (deferred):** the only difference. Western paints
  absolute note-names — "D" at north, then E, F♯, … clockwise — anchored
  to absolute Hz rather than movable-do. Same dial face, different
  stickers.
- **HUD badge:** "D Major" — "D" because Western users name their tonic
  by its absolute pitch; "Major" because that's the school's name for
  this step vector.

Note: the **dial geometry, scale-ring mask, AND tonic-at-north are
identical** between the two cases — D sits at the top for both. Only the
label ring's *vocabulary* differs (movable Sargam vs absolute Western).
That's the abstraction paying off: the Western/Hindustani split is
entirely a labelling concern, with no effect on geometry.

## School-of-music namespaces

Scales don't live in a flat global list. Each school defines its own.

- **Western** — Major, Minor, Dorian, Lydian, Mixolydian, ...
- **Hindustani** — Bilawal, Yaman, Bhairav, Kafi, Asavari, Todi,
  Marwa, ... (ten thaats, plus countless ragas built on them).
- **Carnatic** — 72 melakarta plus janya scales.

Each school has its own canonical names, conventions, and (sometimes)
its own scales that don't appear elsewhere. The names are not
interchangeable: Bilawal ≠ Major even though their step vectors
`[2,2,1,2,2,2,1]` are identical — the cultural meaning, ornaments,
and usage patterns differ.

**Canonical-rotation equivalence:** at load time, scales whose step
vectors are rotations of one another get grouped as equivalent for
*math purposes* (the dial mask is the same), but they keep their
distinct names in their respective school namespaces. Bilawal ↔ Major
are equivalent under rotation (in fact identity); Kafi ↔ Dorian are
rotations of each other; etc. The singer sees the name for *their*
school.

**Practical consequence:** a future `scale_name(steps, note_system)`
would return "Bilawal" for Sargam, "Major" for Western on the same
step vector — the school namespace acting through the note-system
axis. This is **not built** (an earlier 3-entry stub was removed with
the rest of the head vocabulary); the real machinery (per-school
catalogues with rotation-equivalence detection at load) is deferred
and lands alongside the note-system axis.

## What's in code today

The musical model lives in `domain-ports/src/music.rs` (the State +
View layer): the affine point/vector types `InstrumentKey { offset: f32 }`
/ `InstrumentKeyInterval(f32)` and their scale-space mirror `ScaleNote
{ offset: u8 }` / `ScaleNoteInterval(i8)`, the `harmonium_key`
constructor, `TuningKind`, `TuningSpec`, the opaque coach-internal
`Tuning { slots_linear, slots_log2, root }` (one octave, two
representations), `Tonality { tonic, widths }` with `widths()` /
`note_count()` / `key_of()` / `well_formed()`, the const
`MAX_SCALE_NOTES = 32`, and the `tuning_view` module of free functions
(`octave_position`, `hz`). The flat/`Copy` types (`InstrumentKey`,
`InstrumentKeyInterval`, `ScaleNote`, `ScaleNoteInterval`, `TuningKind`,
`TuningSpec`, `Tonality`) cross the AppCoach port; `Tuning` (owns its
`Vec`s) does not.

The coach receives the model via `Command::ConfigureSession { tuning:
TuningSpec, tonality: Tonality }`, **decoupled from the audio
lifecycle** — it's accepted in any state and causes no
`SessionState` change. On every configure the control plane builds the
`Tuning`, stores `(Tuning, Tonality)` as its session model, and
publishes the **event-sourcing pair** that lets any head reconstruct
the musical frame:

- a **snapshot** — `AppCoach::music_info() -> Option<MusicInfo>` where
  `MusicInfo { tuning: TuningSpec, tonality: Tonality }`. A materialized
  read-cache (lock-free `ArcSwap`). **Sticky**: `None` only before the
  first configure, survives start/stop, cleared only on shutdown.
- an **event** — `CoachEvent::SessionConfigured { tuning, tonality }`,
  the log entry whose fold reconstructs `music_info`. Snapshot is
  written *before* the event, so a head reacting to the event reads a
  coherent snapshot. (Same pattern as `audio_info` + `SessionStateChanged`.)

The head is **vocabulary-free**: nothing below names a slot, a scale,
or a tonic. Naming is the deferred [note-system axis](#3-note-system-vocabulary).

In `apps/coach-game/src/state.rs`:

- `AppSettings { reference_hz, tuning_kind: TuningKind }` — axes 1, 2
  only — plus `tuning_spec()`, which pegs the tuning root at
  `harmonium_key(21.0)` (A in octave 1) with `root_note_hz = reference_hz`
  and copies `tuning_kind` straight through (no head-side enum). There is
  **no** `note_system` field, no `tonic_label`, no `scale_name`. The root
  sits in octave 1 (not the lowest octave) so a song tonic an octave
  below it lands in the singing register rather than the cellar.
- `SongTonality(Tonality)` resource — axis 4. Default = Bilawal on
  `harmonium_key(12.0)` (C in octave 1, one octave below the A=440 root →
  middle register, C ≈ 262 Hz). Written to the coach via
  `ConfigureSession` on InGame entry.

In `apps/coach-game/src/game/dial.rs`:

- **North = the tonic (Sa).** No frequency is hardcoded; the anchor is
  the tonic, which is already in the `Tonality`. The dial does only the
  render step (`× TAU`); all pitch-math lives in `music.rs`.
- `build_slots(&MusicInfo)` produces the slot angles by folding every
  slot's real Hz against Sa's Hz via
  `tuning_view::slot_position_from(t, tonic, i) × TAU`. So Sa sits at
  north and a non-uniform tuning (Just) keeps its uneven tick spacing.
  No ratio table lives in the dial. Drives the tuning ring (layer 3).
- `in_scale_mask(tonality, root, n) -> Vec<bool>` — the head-side render
  projection of `Tonality`, **slot-indexed** (slot 0 = the tuning root),
  walked from the tonic's slot index — the gauge-clean delta
  `(tonic − root).round() % n` — by rounding each width. Tuning-
  independent; never crosses the port; zips by index with the tick
  angles. Drives the scale ring (layer 4).
- `needle_angle(&MusicInfo, f0)` turns the live `f0` into the needle
  angle by folding it against Sa's Hz
  (`tuning_view::octave_position(f0, sa_hz) × TAU`) — the same Hz fold
  the ticks use, so a perfectly-sung note lands exactly on its tick.
- The dial spawns *empty* and paints its slots from the `MusicInfoRes`
  read model (the coach's `music_info()` snapshot) — so the slots
  reflect the singer's real tuning + tonality, not a hardcoded default.
  The needle likewise needs the snapshot (no Sa to measure from without
  it), so no snapshot → no needle.

In `apps/coach-game/src/game/hud.rs`:

- Top-left panel shows the **math view** of the current tonality,
  sourced from the coach's `music_info()` snapshot (not the head's own
  `SongTonality` — reading the snapshot exercises the round-trip). It
  walks `Tonality::key_of(ScaleNote(0..note_count))` to get one
  `InstrumentKey` per scale note, then renders three rows of the same
  scale: `deg` (each key's Sa-relative semitone, `key − tonic`, e.g.
  `0 2 4 5 7 9 11`), `key` (those keys' `offset`s, in `InstrumentKey`
  space), and `Hz` (each key resolved through the active `Tuning` via
  `tuning_view::hz`). The keys are whole, so all three render as plain
  integers — the fractional slide never reaches this view. The Hz row
  ascends
  naturally with no octave-lifting at the call site: `hz` derives slot
  *and* octave from the same root-relative delta, so degrees below the
  tuning root simply read an octave down. For Bilawal on C (octave 1,
  `harmonium_key(12.0)`) against the A=440 12-TET tuning (root at
  `harmonium_key(21.0)`):

  ```
  deg   0   2   4   5   7   9  11
  key  12  14  16  17  19  21  23
  Hz  262 294 330 349 392 440 494
  ```

  Note the root A lands exactly on 440 (key 21 = degree 9), and Sa sits
  a clean octave below the root in the middle register.

  No note names — that's the deferred label layer. `None` snapshot
  renders an honest "—" placeholder.

In Settings UI:

- Audio + Music tabs, master/detail in Music. Music edits axes 1–2 only
  (reference Hz, tuning kind) — the note-system picker was removed with
  the head vocabulary.

## What's deferred

In rough order of priority:

- **`LabelRing` + `LockMode` machinery.** The right shape for axis 3
  rendering. Replaces the stub `slot_label` table. Lock modes:
  `RelativeToTonic { tonic_label_index }` for Sargam, `AbsoluteHz {
  label_index, hz }` for Western. Function
  `label_ring_rotation(lock, tonic_hz, reference_hz) -> radians`.
- **Dial label ring as a real visual layer.** Today the dial shows
  geometry + scale highlights but no slot labels. The label ring is
  layer 5 of the dial and hasn't been built yet.
- **Per-song picker UI.** Today `SongTonality` only takes its
  `Default`. A song picker / raga picker will write to it. Probably
  lives in a top-bar picker, per the earlier UI sketch.
- **School-namespaced scale catalogue.** Replaces the 3-entry
  `scale_name` stub. Each school owns its scales; canonical-rotation
  equivalence computed at load time.
- **Microtonal / non-12 tuning systems.** Carnatic 22-shruti would
  introduce a `slot_count` that varies per tuning. Step vectors and
  `in_scale_mask` would need to parametrise on it.
- **Reference-Hz arbitrary input.** Today the picker offers four
  presets (440, 442, 432, 415). Text input deferred until needed.
- **Internationalisation of menus/dialogs.** Note system is a
  *musical-vocabulary* choice, not a UI-language choice. Menu text
  goes through a separate `language` setting (en/hi/es). Not yet
  built; mentioned here so future-you knows to keep the two
  decoupled.
