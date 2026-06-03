# Music model

The musical configuration of a session decomposes into **four
independent axes** that drive a dial made of **five concentric
layers**. They are independent by design: changing one should never
force changes in any other. This document is the canonical statement
of the model and the reasoning behind the split.

Source-of-truth split:

- *Why* the model is shaped this way → this doc.
- *What* the Rust types look like → `apps/coach-game/src/state.rs`.

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
      │ /       \ │   2. Needle — live log(f) % 2.
      ││  needle ││   3. Tuning ring — 12 tick marks at tuning positions.
      │ \       / │   4. Scale ring — mask: which ticks are lit.
      │  └─────┘  │   5. Label ring — note names, separately rotatable.
       \         /
        └───┬───┘
            S
```

1. **Compass body (fixed):** the circle itself. North = 12 o'clock =
   `log(f) % 2 == 0` of whatever the root is. Pure geometry.
2. **Needle:** live `log(f) % 2`, displayed relative to the current
   root. *Tuning-independent.* (Already in code, working.)
3. **Tuning ring:** 12 faint tick marks at log-frequency positions
   set by the **tuning system**. 12-TET → evenly spaced.
   Hindustani Just → uneven. **No labels, no highlights — just where
   the slots sit.**
4. **Scale ring:** of those tuning slots, which ones are "in the
   scale". A mask on the tuning ring; renders as lit vs dim ticks.
   Derived from the `Tonality` (tonic key + scale intervals).
5. **Label ring:** carries the note names. **Rotates independently
   to find the right "lock" position.** This is the layer that
   distinguishes Western from Hindustani — not the geometry.

The widget itself stays dumb: it takes pre-computed slot angles,
labels, and highlights, and renders them. All musical semantics live
above.

## The label ring's lock mode is the whole game

The single most important conceptual move from earlier design
discussion: **the Western/Hindustani difference is not two different
models. It's one model with two values of "what does the label ring
lock to?"**

**Western lock — labels anchored to absolute Hz.**

- The "A" sticker lands at `log(440 / reference_hz) % 2`. C, D, E, F♯
  all sit at fixed absolute angles.
- Change song tonic from C to D → **the scale ring rotates**, the
  labels don't move.
- This is *absolute pitch*: labels anchored to Hz, scale moves.

**Hindustani lock — labels anchored to the tonic.**

- The "Sa" sticker is locked to north. Whatever Hz the tonic key lands
  on, that's where Sa renders.
- Change song tonic from C to D → **nothing rotates visually.** The
  frequency reference shifts under the labels; the dial looks the
  same; "Sa" still at the top.
- This is *relative pitch*: labels anchored to the tonic, scale
  effectively stays put.

Same machinery underneath. The only difference is which ring rotates
when you change key. **Once we get this abstraction right, the
"Western vs Hindustani" distinction stops needing special cases
elsewhere in the codebase.**

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

Plus a function `label_ring_rotation(lock, tonic_hz, reference_hz) ->
radians` that decides how much to rotate the label ring at render
time. *Not yet built* — see "What's deferred" below. The current
`NoteSystem::slot_label` table is a stub that pretends the lock mode
problem doesn't exist; it works for Western but is structurally wrong
for Sargam.

## The four axes

### 1+2. Tuning (slot geometry)

> **Note:** earlier drafts split this into two axes — "tuning
> reference" (a bare Hz) and "tuning system" (slot geometry). They are
> now modelled as **one domain** with three inputs, because Just
> intonation is not rotationally symmetric and therefore needs to know
> *which slot* the reference Hz pegs — a fact the two-axis split could
> not express. See "Why the peg is required" below.

The tuning domain answers exactly one question: **given the inputs,
where do the N slots sit in Hz?** It carries **no names and no
conventions** — names ("A", "Safed-6") are the note-system layer
(axis 3), which renders at display time and never reaches this domain.

**Two coordinate spaces — keep them apart.** The single source of bugs
here is that two different "which note" indices look identical (both
small integers). They are not the same space:

- **Keyboard space** — a physical key of the instrument. What the
  harmonium player and singer *speak*: "Safed-1", "Kaali-1". Fixed to
  the instrument. Modelled by **`InstrumentKey`**.
- **Slot space** — an index into the tuning's computed `slots` array,
  where `slots[0]` is the tuning's root note and the rest fan upward.
  What the tuning *math* works in.

```rust
/// A physical key of an instrument's keyboard. Name-free and `Copy`
/// (so it crosses the FFI boundary): the *name* ("Safed-1" vs "C") is
/// derived head-side from `offset` by the note system. `octave_size`
/// is the keyboard's key count per octave (12 harmonium/piano, 22
/// shruti); carrying it keeps modular arithmetic self-contained and
/// makes a 12-key set impossible to mix with a 22-key one.
struct InstrumentKey { offset: u8, octave_size: u8 }
// constructors stamp the size: harmonium_key(0) -> { offset: 0, octave_size: 12 }

impl InstrumentKey {
    /// Position within one octave: 0..octave_size. The "fold to one
    /// octave" operation.
    fn fold(self) -> u8 { self.offset.rem_euclid(self.octave_size) }
    /// Which octave this key sits in (the part folding discards).
    fn octave(self) -> i32 {
        (self.offset as i32).div_euclid(self.octave_size as i32)
    }
}
```

**`offset` is the whole multi-octave line, `octave_size` splits it.** A
key's `offset` is one integer on the *absolute* instrument line; the
octave and the within-octave position are the two halves of one
`divmod`: `octave = offset.div_euclid(octave_size)` (the part folding
throws away), `fold = offset.rem_euclid(octave_size)` (0..octave_size).
So "fold to one octave" and "which octave" are the same operation read
two ways — no separate octave field, no redundancy to keep in sync. A
single-octave key is just `offset` already in range; an absolute key
lets `offset` run past `octave_size` (14 on a 12-key board = octave 1,
position 2) and recovers both by `divmod`.

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
    fn shape(&self, root_hz: f32) -> Vec<f32> { /* ... */ }
}
```

`Tuning::new` runs the shape and stores it alongside the `root` key:

```rust
struct Tuning {
    /// **One octave** of slot frequencies in **slot space**: `slots[0]
    /// == root_note_hz`, the rest fan upward by the kind's pattern for
    /// exactly N slots. `slots.len()` == N (no separate slot_count
    /// field — it falls out of the vector). The pattern repeats every
    /// octave, so a key an octave up is `slots[..] × 2`; the array
    /// never stores more than one octave (see `TuningView::hz`). Feeds
    /// the dial's tuning ring (layer 3).
    slots: Vec<f32>,
    /// The physical key (**keyboard space**) that slot 0 sits on — the
    /// key the instrument was tuned from. This is the bridge between
    /// the two spaces: keyboard key `k` maps to slot `(k.offset -
    /// root.offset).rem_euclid(N)`.
    root: InstrumentKey,
}

impl Tuning {
    fn new(root_note_hz: f32, kind: TuningKind, root: InstrumentKey) -> Tuning {
        // slot 0 IS the root note; the kind fans upward from it. No
        // re-pegging — slots[0] == root_note_hz by construction.
        let slots = kind.shape(root_note_hz);
        Tuning { slots, root }
    }
}
```

The *reading* of a `Tuning` — keyboard→slot, slot→Hz — lives in the
View layer below, not on the struct. The struct owns only its birth
(`new`) and its data (`slots`, `root`).

`root_note_hz` and the kind are absorbed into `slots`; the `root` key
survives because it bridges keyboard space to slot space. `slots` is a
*read* value computed inside the coach — not a flat FFI command payload
— so a kind-defined length is unproblematic here. (`InstrumentKey` and
the head's three construction inputs *are* flat/`Copy` and do cross the
wire; the constructed `Tuning` does not.)

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
kind declare `N` (and reading it back as `slots.len()`) keeps the
domain N-agnostic without building the microtonal case now — the extra
kinds are deferred, but the shape already admits them.

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
    /// The physical key the singer calls home (Sa). **Keyboard space**
    /// — the same space as the tuning's `root` (what the singer says:
    /// "Kaali-1"). This is the *song* root — the second of the two
    /// roots, distinct from the tuning's `root` (the key the
    /// instrument was tuned from). Both are `InstrumentKey`s on the
    /// same keyboard: one is where the tuner anchored, one is where the
    /// singer puts Sa.
    tonic: InstrumentKey,

    /// The scale's shape as **intervals between successive notes** (in
    /// slot units), walking up from the tonic. `[2,2,1,2,2,2,1]` for
    /// Bilawal/Major. These are *gaps*, not notes: the tonic (Sa) is
    /// implicit at the start, `scale_intervals[0]` is the Sa→Re step,
    /// `[1]` is Re→Ga, and so on. So the number of intervals (to the
    /// terminator) equals the number of notes, and they sum to N.
    ///
    /// Fixed-capacity and **0-terminated**: read intervals until the
    /// first `0`. A `0` interval is never musically valid (it would put
    /// two scale notes on one slot), so the sentinel is self-validating.
    /// Cap of 32 covers any 12- or 22-slot scale with room. This keeps
    /// `Tonality` flat and `Copy` so it can cross the FFI command
    /// boundary directly (unlike `Tuning`, which is coach-internal).
    scale_intervals: [u8; 32],
}
```

`scale_intervals` is *tonic-relative* — it describes the scale's shape
starting at the tonic. `tonic` then says which physical key that shape
is planted on. The two are genuinely separate: `scale_intervals` is the
scale's **shape**, `tonic` is **where you plant it**.

**Well-formedness invariant — intervals sum to N.** Walking the
intervals from the tonic must traverse exactly one octave and land back
on the tonic. So the intervals (up to the `0` terminator) must sum to
the tuning's slot count `N`. Sum < N never closes the octave; sum > N
wraps past it. This is the whole invariant — no interior `0` (the
`take_while` below enforces that by construction) and no separate
per-step bound (sum == N with non-negative intervals already caps any
single interval at N).

Because `N` comes from the *tuning* (`slots.len()`), not from
`Tonality` alone, the check lives at the **join point** — where a
`Tonality` meets a `Tuning` (the coach computing `in_scale_mask` /
accepting the config). The same 7-note scale is valid on a 12-slot
tuning and invalid on a 22-slot one.

```rust
fn tonality_well_formed(t: &Tonality, n: u8) -> bool {
    t.scale_intervals.iter().take_while(|&&s| s != 0)
        .map(|&s| s as u16).sum::<u16>() == n as u16
}
```

**Failure policy (today): construct-time guard.** Only code builds
`Tonality` right now — there is no per-song picker producing arbitrary
input — so a bad sum is a *programming bug*, caught by
`debug_assert!(tonality_well_formed(..))` at the seam. This graduates
to a runtime **reject** (log + keep the prior/default `Tonality`,
matching the coach's silent-no-op convention for illegal commands)
when the picker lands and untrusted input can reach it.

**Why it's its own axis:** These change per song. Singer's choice,
not harmonium-maker's. "I'll sing Vande Mataram in D Yaman" → tonic
= D, scale = Yaman. "I'll sing Yesterday in F Major" → tonic = F,
scale = Major. The tuning and vocabulary haven't moved.

**Why tonic is an `InstrumentKey`, not a note name like "D":** Because
"D" is a Western name. The same data needs to render as "D" for a
Western user and "Kaali-1" (or whichever harmonium position the key
corresponds to) for a Sargam user. Storing the *key* (offset + size)
and resolving the *name* at render time via the note system's tonic-
naming table (Job B above) keeps the storage neutral to vocabulary
choice.

**The in-scale mask is a head-side render projection** (see "The mask
is a head-side projection" below). Given a `Tonality`, the mask
(`[bool; N]` — which slots are in the song) is derived by walking
`scale_intervals` from the tonic: the walk starts at the tonic's
within-octave position (`tonality.tonic.fold()`) and adds intervals
modulo `N`. It is **tuning-independent** — the lit slot set is the same
integer pattern regardless of whether the tuning is 12-TET or Just (the
tuning only changes where each slot is *drawn*, not which is lit). So
the head computes it directly from the `Tonality` it holds; it does not
cross the port and the coach does not compute it. It feeds the dial's
scale ring (layer 4).

**Worked: what frequency is "Sa"?** The trap is that `scale_intervals`
plays *no part* in finding Sa — Sa is the tonic, before any interval.
The intervals only locate Re, Ga, … So the path to Sa's Hz is the
short one. Example: a 12-key harmonium tuned from A in octave 1
(`root = { offset: 21, octave_size: 12 }`), the singer puts Sa on the D
just below it (`tonic = { offset: 14, octave_size: 12 }`):

1. **Keyboard → delta** (subtract the gauge first, the only path to Hz):
   `delta = tonic.offset − root.offset = 14 − 21 = −7`. Sa sits seven
   12-TET steps *below* the A we tuned from.
2. **Delta → Hz** (slot and octave from the *same* delta, via one
   divmod): slot = `(−7).rem_euclid(12) = 5`, octave = `(−7).div_euclid(12)
   = −1`, so `slots[5] × 2^(−1) = 440 × 2^(5/12) × 2^(−1) ≈ 293.7 Hz`
   (D one octave below the A). That's Sa.

A general scale degree `d` *does* use the intervals — place it on the
keyboard first (`key_d.offset = tonic.offset + Σ scale_intervals[0..d]`),
then resolve that key through the same delta-from-root path:
`hz(key_d)` computes `delta = key_d − root` and folds slot *and* octave
from that one delta (`slots[delta mod N] × 2^(delta div N)`). Degree 0
(Sa) has an empty sum (`key_0 = tonic`), recovering the trace above.
Crucially the octave comes from the *full* delta, not from folding the
degree into the root's octave — that keeps the scale ascending instead
of wrapping (the octave-wrap bug we fixed).

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
Rust, `mod tuning_view { fn slot_of(..) .. }`, called as
`tuning_view::slot_of(&t, key)`. A module — not an empty struct — is
the idiomatic Rust way to group pure functions: it confines the
conversions to one named place per line, the same disambiguation
`InstrumentKey` gives keyboard-vs-slot, without a never-instantiated
phantom type.)

Only **one** View is genuinely shared coach-side: `tuning_view`, the
Hz↔Instrument map, because it reads a `Tuning` (which only the coach
holds). The Tonal-line projection that *would* have been a
`tonality_view` does **not** live here — see "The mask is a head-side
projection" below.

```rust
/// The Hz↔Instrument map — the only place real frequencies and
/// keyboard↔slot arithmetic live. Reads a `Tuning`, never owns it.
mod tuning_view {
    /// Keyboard space → slot space. The one bridge between the two
    /// spaces. Folds to one octave (the slot pattern repeats).
    fn slot_of(t: &Tuning, key: InstrumentKey) -> usize {
        let n = t.slots.len() as i32;
        ((key.offset as i32 - t.root.offset as i32).rem_euclid(n)) as usize
    }

    /// Hz of any physical key, at any octave. `slots` holds one octave;
    /// the octave the key sits in is a power-of-two multiplier applied
    /// after the fold. `octave` is measured from `root`'s octave — there
    /// is **no** requirement that the root sit in octave 0; this form is
    /// correct for any `root.octave()`.
    fn hz(t: &Tuning, key: InstrumentKey) -> f32 {
        let octave = key.octave() - t.root.octave();
        t.slots[slot_of(t, key)] * 2f32.powi(octave)
    }
}
```

### The mask is a head-side projection, not a shared View

The in-scale mask (which of the N slots are lit) is the dial's
**scale ring** (layer 4). It is *derived* render data — a projection of
the `Tonality` the head already holds — and it is **tuning-independent**:
the lit *slots* are the same integer set whether the tuning is 12-TET or
Hindustani Just. The asymmetry of Just changes where each tick is
*drawn* (the tuning ring, layer 3, which is Hz→angle render geometry),
not *which* ticks are lit. Walking `scale_intervals` is pure integer
math that needs only the `Tonality` and `N`, never the `Tuning`'s
frequencies.

So the mask is **not** computed by the coach and **not** a shared
`tonality_view`. The head holds the `Tonality` (the same flat/`Copy`
type it sent across the port via `ConfigureSession`) and **the dial
walks the intervals itself** — sibling to the angle math it already does
head-side. There is no `MaskSnapshot` type, no mask read-publisher: the
mask never crosses the port, because the head already has its source.

`Tonality` *does* cross the port — but as the **coach's frame of
reference for judging pitch** (the eventual scoring), not for the mask.
The two uses are independent: the coach holds `Tonality` to interpret
singing; the head holds the same `Tonality` to paint the scale ring.

**Where each function lands, and why:**

| Function | Layer | Lives in | Why |
|---|---|---|---|
| `fold`, `octave` | method | `InstrumentKey` | arithmetic on the key's *own* fields — no external state read, so it's a method, not a view |
| `Tuning::new`, `Tonality::new` | State | the state struct | construction = birth; state owns being born |
| `tonality_well_formed` | State-join | called at the Tuning×Tonality join (coach), not at `Tonality::new` | needs `N` from the *tuning*, which `Tonality` alone doesn't have |
| `slot_of`, `hz` | View | `tuning_view` (coach) | the Hz↔Instrument map; reads a `Tuning` |
| in-scale mask (interval walk) | head render | `dial.rs` (head) | tuning-independent integer projection of the `Tonality` the head holds; sibling to its angle math |

The dividing line: **state = how it's built; View = how it's read as a
line; render = how the head paints it.** `fold`/`octave` are the one
apparent View-exception — but they read no *external* state, only the
key's own two fields, so they stay methods.

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
| Song tonic | D (key 2) | `Tonality.tonic = harmonium_key(2)` |
| Scale | Bilawal | `Tonality.scale_intervals = [2,2,1,2,2,2,1,0,…]` |

The first three axes are head-held `AppSettings`; `tuning_spec()`
marshals them into the `TuningSpec` that crosses the port (root pegged
at `harmonium_key(21)` = A in octave 1, `root_note_hz = reference_hz`).
The tonic +
scale ride in the head's `SongTonality(Tonality)` resource and cross
the port as a `Tonality` via `ConfigureSession`.

What the user sees:

- **Tuning ring:** 12 evenly-spaced ticks (12-TET).
- **Scale ring:** keyboard positions `[2,4,5,7,9,11,1]` highlighted,
  others dim (the in-scale mask, walked head-side from `Tonality`
  starting at the tonic's `fold()` = 2 and adding each interval mod
  12). The mask is indexed in keyboard space (0 = C). Tuning-
  independent: the lit set is the same in 12-TET or Just; only the
  angles differ.
- **Label ring:** Sargam lock = RelativeToTonic. "Sa" sticker locked
  to north. Slot 2 (the singer's D) is at north. The 5 chromatic
  out-of-scale slots either get no label or get komal/tivra labels at
  their scale-degree positions; the 7 in-scale slots get Sa, Re, Ga,
  Ma, Pa, Dha, Ni reading clockwise.
- **HUD badge:** "Safed-2 Bilawal" — "Safed-2" because that's how a
  Sargam user announces their tonic ("my Sa is on Safed-2 / the 2nd
  white key today"); "Bilawal" because that's the school's name for
  this step vector. Note: the HUD does **not** say "Sa Bilawal" — Sa
  is always the tonic by definition, so saying "Sa" tells the singer
  nothing they don't already know; what they want to be reminded of
  is *which key* their Sa is on today. (The fact that this is "D
  Major" in Western terms doesn't need to surface to a Hindustani
  user — and shouldn't, per the UI design.)

Switch the same setup to Western note system, A=440, "Major in D":

- **Tuning ring:** identical (tuning is the same).
- **Scale ring:** identical (same step vector, same tonic key).
- **Label ring:** Western lock = AbsoluteHz, anchored on the A
  sticker at 440 Hz. C, D, E, F♯, ... sit at their fixed absolute
  positions. North = C (because `reference_hz=440` puts A at slot 9,
  so slot 0 = C is at north). D is at the 2-o'clock position.
- **HUD badge:** "D Major" — "D" because Western users name their
  tonic by its absolute pitch; "Major" because that's the school's
  name for this step vector.

Note: the **dial geometry and scale-ring mask are identical** between
the two cases. Only the label ring's rotation differs. That's the
abstraction paying off.

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
View layer): `InstrumentKey { offset, octave_size }` with `fold()` /
`octave()` divmod, `TuningKind`, `TuningSpec`, the coach-internal
`Tuning { slots, root }` (one octave of Hz), `Tonality { tonic,
scale_intervals }`, and the `tuning_view` module of free functions
(`slot_of`, `hz`). The flat/`Copy` types (`InstrumentKey`,
`TuningKind`, `TuningSpec`, `Tonality`) cross the AppCoach port;
`Tuning` (owns a `Vec<f32>`) does not.

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
  `harmonium_key(21)` (A in octave 1) with `root_note_hz = reference_hz`
  and copies `tuning_kind` straight through (no head-side enum). There is
  **no** `note_system` field, no `tonic_label`, no `scale_name`. The root
  sits in octave 1 (not the lowest octave) so a song tonic an octave
  below it lands in the singing register rather than the cellar.
- `SongTonality(Tonality)` resource — axis 4. Default = Bilawal on
  `harmonium_key(12)` (C in octave 1, one octave below the A=440 root →
  middle register, C ≈ 262 Hz). Written to the coach via
  `ConfigureSession` on InGame entry.

In `apps/coach-game/src/game/dial.rs`:

- `tuning_12tet()`, `tuning_hindustani_just()` — produce `[f32; 12]`
  slot angles. Drive the tuning ring (layer 3).
- `in_scale_mask(tonality: &Tonality) -> [bool; 12]` — the head-side
  render projection of `Tonality`, walked from the tonic's `fold()`.
  Tuning-independent; never crosses the port. Drives the scale ring
  (layer 4).
- Dial spawn reads `SongTonality` and applies `in_scale_mask` to
  `DialSlot.active`.

In `apps/coach-game/src/game/hud.rs`:

- Top-left panel shows the **math view** of the current tonality,
  sourced from the coach's `music_info()` snapshot (not the head's own
  `SongTonality` — reading the snapshot exercises the round-trip). Three
  rows describe the same scale: `deg` (0-based prefix sum of the scale
  intervals, e.g. `0 2 4 5 7 9 11`), `key` (those degrees + tonic
  offset, in `InstrumentKey` space), and `Hz` (each key resolved through
  the active `Tuning` via `tuning_view::hz`). The Hz row ascends
  naturally with no octave-lifting at the call site: `hz` derives slot
  *and* octave from the same root-relative delta, so degrees below the
  tuning root simply read an octave down. For Bilawal on C (octave 1,
  `harmonium_key(12)`) against the A=440 12-TET tuning (root at
  `harmonium_key(21)`):

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
