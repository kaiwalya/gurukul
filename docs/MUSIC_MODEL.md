# Music model

The musical configuration of a session decomposes into **four
independent axes** that drive a dial made of **five concentric
layers**. They are independent by design: changing one should never
force changes in any other. This document is the canonical statement
of the *product* model and the design that hasn't shipped yet.

## Source-of-truth split

The **geometry** — how a pitch, a tuning, and a scale are represented
and computed — lives in three module docs, and *they* are canonical
for it. Read them first; this doc never restates their arithmetic:

- [`domain-ports/src/pitch.rs`](../domain-ports/src/pitch.rs) — the
  `log2(Hz)` line wound into a helix; `PitchLog2` (a point) vs
  `PitchLog2Interval` (a difference); the gauge law (*no logic may
  depend on an absolute pitch, only on a difference*), encoded as the
  arithmetic that deliberately does not compile (`point + point`).
- [`domain-ports/src/tuning.rs`](../domain-ports/src/tuning.rs) — the
  cylinder: a tuning as a bundle of **lines (grooves)** at fixed
  **angles**, `TuningIntervals` (the rigid shape), `TuningAbsolute`
  (shape + the one global rotation a reference pitch sets), and
  `TuningRotated` (the same cylinder re-based to a different root line).
- [`domain-ports/src/scale.rs`](../domain-ports/src/scale.rs) — the
  outer cylinder whose **teeth** drop into some of those grooves:
  `ScaleIntervals` (the tooth pattern, a bitmask) and `Scale` (a
  pattern placed on a concrete tuning at a concrete register, exposing
  `tonic_pitch`, `degree_pitch`, `needle_angle`, `tick_angle`).

This doc owns what the code doesn't and shouldn't: the **dial** the
geometry feeds, and the **deferred** label/vocabulary layer. Where it
names a type or method, the three files above are the truth — if a name
here has drifted from the code, the code wins.

If you're about to change `AppSettings`, `SongTonality`, the dial
widget, or anything dial-rendering, **read this doc first**. The split
between geometry (clicky integer selection vs smooth real gauge) is
subtler than it looks, and the first time you write a "what does each
slot get labelled?" function you will get it wrong if you haven't
internalised the rings model.

## The dial as five concentric rings

The dial visualises one octave on a log-frequency circle — the helix
of [`pitch.rs`](../domain-ports/src/pitch.rs) viewed straight down its
axis, so every octave collapses onto one ring. North = 12 o'clock.
Think of it as five layers stacked at the same position, each
independently configurable:

```
            N (top)
        ┌───┴───┐
       /         \
      │  ┌─────┐  │   1. Compass body — the circle. North = Sa.
      │ /       \ │   2. Needle — live, continuous f0 folded to Sa.
      ││  needle ││   3. Tuning ring — the tuning's N grooves.
      │ \       / │   4. Scale ring — mask: which grooves are lit.
      │  └─────┘  │   5. Label ring — note names, deferred.
       \         /
        └───┬───┘
            S
```

1. **Compass body — North = Sa, and that is *structural*.** The dial
   anchors on the song tonic: whatever groove Sa sits on renders at the
   top. This is **not** a rendering convention the dial chooses. A
   [`Scale`](../domain-ports/src/scale.rs) owns a `TuningRotated` that
   has already been re-based so its **root line is Sa** — line 0 of the
   rotated cylinder *is* the tonic. The dial just reads line 0 and puts
   it north. There is no "where does north go?" decision left to make,
   and no dependence on the label ring or the note system: the geometry
   put Sa at the top before the dial ever drew it. (`Scale::tonic_pitch`
   resolves Sa; `tick_angle(0)` is 0 = north by construction.)

2. **Needle — live pitch, relative to Sa.** **Continuous** — it glides
   smoothly to wherever the voice actually is on the log-frequency
   circle, landing *between* the grooves when the singer slides (meend /
   glissando), never snapping. `Scale::needle_angle(f0_hz)` folds the
   live `f0` against Sa's pitch and reads it as an angle in `[0, TAU)`.
   Tuning-independent and register-free: a voice an octave high reads
   the same angle as one at Sa's own octave.

3. **Tuning ring — the tuning's grooves.** The `N` lines of the
   cylinder, each a faint tick at `Scale::tick_angle(i)` — the
   cumulative angle from Sa to groove `i`. **`N` is not always 12**:
   12-TET and Hindustani Just both have 12, a Carnatic 22-shruti grid
   has 22. And the spacing need not be even: 12-TET ticks sit at `k/12`,
   but **Just / shruti ticks are uneven** — Just Pa lands at
   `log2(3/2)`, not the even `7/12`. These grooves are the discrete
   *targets* the continuous needle glides between. No labels, no
   highlights — just where the grooves sit.

4. **Scale ring — which grooves are in the scale.** A mask on the
   tuning ring; renders as lit vs dim ticks. It is the **ordinal**
   tooth pattern of `ScaleIntervals` — `degree_slots()` gives the lit
   groove indices (`[0, 2, 4, 5, 7, 9, 11]` for Bilawal). Because the
   pattern is ordinal (which groove, counted up from Sa), it is
   **tuning-independent**: the same integer set is lit whether the
   tuning is 12-TET or Just — the tuning only changes where each groove
   is *drawn* (ring 3), never which is lit. The head walks this itself
   from the `Scale` it holds; it never crosses the port.

5. **Label ring — vocabulary only.** Carries the note names. The
   geometry already put the tonic at north, so the ring **doesn't
   rotate** — its only job is to choose *which vocabulary* lands on each
   groove (movable Sargam vs absolute Western note-names). This is the
   layer that distinguishes Western from Hindustani — not the geometry.
   *Deferred — see "What's deferred".*

The widget itself stays dumb: it takes pre-computed groove angles,
labels, and highlights, and renders them (`× TAU` at most; the `Scale`
methods already return radians). All musical semantics live above.

## Why two tuning types: the smooth slide vs the clicky one

The model carries **two roots** — the tuning's reference pitch and the
song's tonic (Sa) — and they ride two structurally different motions.
Getting this distinction into the types is what `TuningAbsolute` and
`TuningRotated` are *for*.

**Slide A — the tuning over the bare helix (smooth, real-valued).** The
helix is featureless. Slide the whole tuning along it by *any real
amount* and the shape is preserved — every interval unchanged, just
re-anchored in Hz. This is the "A = 440 vs A = 441" slide, and it is a
**symmetry**: free, continuous. In the code it is exactly the
`rotation: PitchLog2Interval` on `TuningAbsolute` — the single
octave-free residue a reference pitch contributes. Changing the
reference moves only this, never the grooves' spacing.

**Slide B — the tonic over the tuning (clicky, integer-valued).** Now
the surface underneath is **punched**: it has grooves at specific,
possibly uneven angles. Sa must land *on a groove*. Choosing which
groove is Sa is `Tuning::shift_up(k)` → a `TuningRotated`: an **integer
cursor** re-basing the cylinder. You cannot land Sa between grooves;
the move is discrete.

|        | what slides   | over what               | a symmetry? | the type |
|--------|---------------|-------------------------|-------------|----------|
| **A**  | whole tuning  | bare helix (smooth)     | **yes** — rigid | `rotation` (real) |
| **B**  | tonic (Sa)    | punched tuning (uneven) | **no** — clicky | `shift_up(k)` (integer) |

**Why they look like one knob in 12-TET — and why that's a trap.** In
equal temperament the grooves sit at `k/N`, so the staircase is linear,
and the two slides *fuse*: moving Sa one groove (`shift_up(1)`) is
indistinguishable from sliding the rotation by exactly `1/N` of an
octave. In 12-TET you can reach any result by moving *either* knob — so
it is tempting to model the tuning as a single real number.

It breaks the moment the tuning is uneven, because the staircase stops
being additive — `cumulative_rotation_to(a + b) ≠
cumulative_rotation_to(a) + cumulative_rotation_to(b)`. You can no
longer fold the tonic into the rotation. The clicky slide does
something no smooth slide can reproduce.

> The two slides collapse into one **iff the tuning's staircase is
> linear** (equal-tempered). For any unequal tuning they are genuinely
> independent, and choosing the tonic is a real musical choice — not a
> gauge.

The system **already ships** the unequal case (`HindustaniJust`, and
22-shruti behind it), so the collapse does not hold here — which is the
whole reason the rotation (real) and the shift (integer) are two
different operations on two types, not one `f32`. (`Scale` then adds the
**third** motion the cylinder omits: an integer `octave`, the helix
floor — the male/female register. So the full placement of Sa is one
real rotation + one integer shift + one integer floor, each where it
belongs.)

## The four axes

### 1+2. Tuning (groove geometry)

The tuning domain answers exactly one question: **given the inputs,
where do the N grooves sit, as angles on the cylinder?** It carries
**no names and no conventions** — names ("A", "Safed-6") are the
note-system layer (axis 3), which renders at display time and never
reaches this domain.

Construction is two real choices, which is why
`TuningAbsolute::at_reference(intervals, reference)` takes two
arguments:

1. **A `TuningKind`** — the shape selector. Each kind declares its slot
   count `N` and the rule for spacing the grooves:
   - **12-TET** — `N = 12`, grooves `1/12` octave apart. Rotationally
     symmetric.
   - **Hindustani Just** — `N = 12`, 5-limit ratios. Pa at exactly 3/2
     (slightly sharper than 12-TET G); shuddha Ga at 5/4 (~14 cents
     flatter than 12-TET E). *Not* rotationally symmetric.
   - **22-shruti** — `N = 22`, a finer uneven grid. (Behind the cap;
     `MAX_TUNING_SLOTS = 32`.)
   `kind.intervals()` yields the reference-free `TuningIntervals`.
2. **A reference pitch** — the "A=" line. Only its **pitch class**
   matters: the fold drops the octave, keeping the one octave-free
   angle that is the whole content of "440." This becomes the
   `rotation`. The register a tuning is *read* at is the `Scale`'s
   integer floor, never a property of the cylinder.

**Why `N` is owned by the `TuningKind`.** The groove count is a
property of the *algorithm*, not a global constant — 12-TET and Just
both emit 12, 22-shruti emits 22. The cylinder stays N-agnostic.

**Why the reference's groove matters (Just is not symmetric).** 12-TET
is rotationally symmetric: the *set* of 12 grooves is identical no
matter which you anchor from, so the reference there is almost pure
labelling. Just intonation is **not** — its grooves are unequal ratios
fanning from the root. "440 Hz" alone is then ambiguous: 440 at *which*
groove? That is why the reference is a genuine input, load-bearing for
Just and near-vestigial for 12-TET.

**Why it's its own domain (independent of axis 3):** A Sargam user can
sing in 12-TET; a Western user can experiment with Just. The vocabulary
used to *name* grooves is unrelated to where the grooves *land*. Tuning
is acoustic; naming is language.

### 3. Note system (vocabulary)

> **Status: designed, not built.** This axis is *deferred*. The head is
> currently **vocabulary-free** — it stores no note-naming scheme and
> invents no labels. The sections below describe the intended model so
> the label layer lands coherently; until then, nothing in code names a
> groove or a tonic. The HUD shows the [math view](#whats-in-code-today)
> (tooth-widths) instead. An earlier pass *did* implement a `NoteSystem`
> enum head-side; it was removed precisely because it duplicated the
> model's vocabulary outside the geometry and drifted. Reintroduce it
> only as the real `LabelRing` / `LockMode` layer below.

The labels used to talk about grooves. Pure presentation; no effect on
math, geometry, or audio.

- **Western** — C, C♯, D, E♭, E, F, F♯, G, A♭, A, B♭, B.
- **Sargam Latin** — Sa, Re, Ga, Ma, Pa, Dha, Ni (plus komal/tivra
  forms; the list is shorter than 12 — see below).
- **Sargam Devanagari** — सा, रे, ग, म, प, ध, नि.

**Why it's its own axis:** Vocabulary is orthogonal to acoustics. The
same 12-TET groove at 440 Hz is "A" in Western or some Sargam label
depending on the tonic. The user picks which system they're fluent in.

#### Western has one table; Sargam has two

A note system does **two** rendering jobs, and Western happens to use
the same vocabulary for both — which masks the asymmetry and made
earlier passes of this model fall over.

**Job A — labelling the dial label ring.** What's written on each
position around the dial. Driven by the label ring's lock mode.

**Job B — naming the tonic in HUDs and pickers.** When the HUD shows
the session's tonic, what string do we display? This is an *absolute*
job — it points at a specific chromatic groove.

**Western collapses both into one absolute-pitch table:** a Western
singer announces "I'm in D Major" — "D" is just the label for groove 2.
One vocabulary, two jobs.

**Sargam needs two distinct tables:**

| Groove | Job A (dial label, tonic-relative) | Job B (tonic name, absolute) |
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

- **Job A (Sa, Re, Ga, …)** is *tonic-relative*. "Sa" is always the
  tonic, never a specific chromatic groove — its dial position depends
  entirely on where the tonic is, because the label ring is locked to
  the tonic. A Hindustani singer never says "I'm singing in Sa" — Sa is
  *defined* to be the tonic.
- **Job B (Safed-1, Kaali-1, …)** is *absolute*. These name physical
  harmonium keys: Safed = white, Kaali = black. "Kaali-1" is *always*
  the first black key. A Hindustani singer announces "I'm singing
  Kaali-1 Bilawal" the way a Western singer announces "I'm singing D
  Major": *here is where I put my Sa, here is the scale.*

This is why the HUD badge would read "Kaali-1 Bilawal" for a Sargam
user with the tonic on groove 1, and "C♯ Major" for a Western user with
the same tonic, *once the label layer ships*. Same data, two
absolute-naming vocabularies. The dial face would meanwhile show "Sa"
at the top in both Sargam cases. **Today**, vocabulary-free, the HUD
shows the math view (the tooth-widths) — the same data, no names.

#### The `LabelRing` abstraction

The dial-label vocabulary (Job A) and the tonic-naming vocabulary
(Job B) are **independent choices** that happen to be bundled per note
system because culturally they go together. The lock mode selects
*which name lands on each groove*, not how far to rotate a ring — the
geometry already anchors Sa at north (ring 1):

- **Sargam — `RelativeToTonic`:** read names off a tonic-relative table
  (groove 0 of the scale = "Sa"). Same labels at the same dial
  positions no matter the key, because the names are tonic-relative and
  the geometry already put the tonic north.
- **Western — `AbsoluteHz`:** read names off an absolute-pitch table
  (the groove whose Hz is 440 = "A", rest follow). The tonic's name
  lands at north because the tonic is north; changing key re-letters the
  ring without rotating the scale highlights.

A future "Western movable-do" would reuse Western's tonic-naming with
Sargam's lock mode — which is why the two are modelled as separable
concerns even though v1 bundles them per note system.

### 4. Tonality (the per-song scale)

The singer's per-song choice, layered on a tuning: *of the tuning's `N`
grooves, which one is Sa, and which ones are in the song?* This is the
`Scale` of [`scale.rs`](../domain-ports/src/scale.rs):

- **Which grooves are in the song** is the tooth pattern,
  `ScaleIntervals` — a `u32` bitmask, bit 0 = Sa, bit `i` = "ordinal
  groove `i` up from Sa is a degree." Stored as **widths**
  (`[2,2,1,2,2,2,1]` for Bilawal: the gaps between successive degrees,
  summing to `N` and closing the octave) or equivalently as the bitmask.
  Reference-free, tuning-free, rotation-free — pure combinatorics. The
  catalogue lives here.
- **Which groove is Sa** is the `TuningRotated`'s integer cursor (slide
  B above); the `Scale` does not store it separately — re-rooting Sa
  *is* re-basing the tuning.
- **Which octave** is the integer `octave` — the helix floor the
  cylinder omits, the register.

`widths` is *tonic-relative* — the scale's **shape**, planted by the
rotation onto a concrete tonic. The two are genuinely separate.

**Well-formedness — widths sum to N.** Walking the widths from Sa must
traverse exactly one octave and land back on Sa one floor up, so they
sum to the tuning's groove count `N`. Because `N` comes from the
*tuning*, this is checked at the **join** where a scale meets a tuning,
not at the scale's construction: the same 7-note scale is valid on a
12-groove tuning and invalid on a 22-groove one.

**Why it's its own axis:** These change per song. "I'll sing Vande
Mataram in D Yaman" → Sa = D, scale = Yaman. "I'll sing Yesterday in F
Major" → Sa = F, scale = Major. The tuning and vocabulary haven't moved.

**Why Sa is a groove, not a note name like "D":** "D" is a Western
name. The same data renders as "D" for a Western user and "Kaali-1" for
a Sargam user. Storing the *groove* and resolving the *name* at render
time (Job B) keeps storage neutral to vocabulary.

**The in-scale mask is a head-side render projection.** Given a `Scale`,
the lit-groove set is derived by reading `ScaleIntervals::degree_slots()`
— pure integer math, tuning-independent (the lit set is the same in
12-TET or Just; the tuning only moves where each groove is drawn). So
the head computes it directly from the `Scale` it holds; it does not
cross the port and the coach does not compute it. The `Scale` *does*
cross the port — as the coach's frame of reference for judging pitch
(the eventual scoring) — but that use is independent of the mask.

## The two roots — precisely

The single most important structural point. Conflating the two roots is
the most common modelling mistake. (In the types: the first is the
`TuningAbsolute` `rotation`, the second is the `TuningRotated` cursor +
`Scale` octave — exactly the two slides above.)

1. **Tuning reference** — the calibration peg.
   - "A = 440 Hz." A physical anchor that tells the tuning how to place
     every groove. Stored once as the reference pitch behind the
     `rotation`.
   - **Rarely changes.** Most people set A=440 once and live there.

2. **Song's tonic (Sa)** — the home note of the piece.
   - "I'm singing in D Major." D is Sa. Doesn't change the tuning — the
     instrument is still A=440-tuned. The singer just chose D as where
     the scale starts. Sa **is** the tonic, whichever groove it lands on.
   - **Changes per song** — different keys for different singers, ragas,
     moods.

These are independent. A=440 doesn't move when you change keys; the
tonic does. In a trivial session — Bilawal in C, A=440, Sa=C — the two
roots point to the same groove, and that coincidence hides the
distinction. The moment the singer says "I'll sing in D," they split.

## Worked example

A Sargam user, A=440 standard, singing Bilawal in C on a 12-TET dial
(the current default — Sa on C, one octave below the A=440 reference):

| Axis | Value | Stored as |
|---|---|---|
| Tuning reference | 440 Hz | `AppSettings.reference_hz = 440.0` |
| Tuning system | 12-TET | `AppSettings.tuning_kind = TwelveTet` |
| Note system | Sargam Latin | *(deferred axis-3 label layer — no field today)* |
| Song tonic | C (Sa) | `TuningRotated` cursor `shift_up(3)` + `octave = 8` |
| Scale | Bilawal | `ScaleIntervals::from_widths(&[2,2,1,2,2,2,1])` |

The first two axes are head-held `AppSettings`; `tuning_absolute()`
marshals them into a `TuningAbsolute` via
`TuningAbsolute::at_reference(kind.intervals(), reference)`. The scale +
tonic ride in `SongTonality(Scale)` and cross the port via
`ConfigureSession`.

**North = Sa, for everyone.** The dial geometry anchors on Sa: whatever
groove the singer planted Sa on renders at 12 o'clock. This is
*tonic-first*, not Hindustani-first — a Western singer in "D Major" puts
D as their tonic, so D sits north for them too. "Sa" is simply the
Hindustani word for "the tonic"; the geometry does not know what the
labels call it.

What the user sees:

- **Tuning ring:** 12 evenly-spaced ticks (12-TET), each at
  `Scale::tick_angle(i)`. Sa at north; the tuning reference A rotates to
  the Pa position.
- **Scale ring:** grooves `[0,2,4,5,7,9,11]` lit (`degree_slots()`).
  Tuning-independent: the same set is lit in 12-TET or Just; only the
  drawn angles differ.
- **Needle:** the live `f0` folded against Sa
  (`Scale::needle_angle(f0)`) — the same fold the ticks use, so a
  perfectly-sung Just Pa lands exactly on the uneven Just Pa tick.
- **Label ring (deferred):** would paint "Sa, Re, Ga, …" on the lit
  grooves clockwise from north. The labels ride *on top of* the
  already-tonic-anchored geometry; the ring's only job is *what the
  labels say*, never *which ring rotates*.
- **HUD badge (deferred):** "Safed-1 Bilawal" — "Safed-1" because
  that's how a Sargam user announces Sa-on-C; "Bilawal" the school's
  name for this tooth pattern. Not "Sa Bilawal" — Sa is always the tonic.

Switch the same setup to Western, "Major in C": the **tuning ring,
scale ring, needle, and tonic-at-north are identical**. Only the label
ring's *vocabulary* differs (movable Sargam vs absolute "C, D, E, …"),
and the badge reads "C Major." That's the abstraction paying off: the
Western/Hindustani split is entirely a labelling concern, with no effect
on geometry.

## School-of-music namespaces

Scales don't live in a flat global list. Each school defines its own.

- **Western** — Major, Minor, Dorian, Lydian, Mixolydian, …
- **Hindustani** — Bilawal, Yaman, Bhairav, Kafi, Asavari, … (ten
  thaats, plus countless ragas built on them).
- **Carnatic** — 72 melakarta plus janya scales.

The names are not interchangeable: Bilawal ≠ Major even though their
tooth patterns `[2,2,1,2,2,2,1]` are identical — the cultural meaning,
ornaments, and usage differ.

**Canonical-rotation equivalence:** at load time, scales whose tooth
patterns are rotations of one another get grouped as equivalent for
*math purposes* (the dial mask is the same), but keep their distinct
names per school. Bilawal ↔ Major (identity); Kafi ↔ Dorian
(rotations). The singer sees the name for *their* school.

This is **not built** (an earlier stub was removed with the rest of the
head vocabulary); the real machinery — per-school catalogues with
rotation-equivalence detection at load — is deferred and lands alongside
the note-system axis.

## What's in code today

The geometry lives in the three port modules (canonical docs linked at
the top): `PitchLog2` / `PitchLog2Interval`, `TuningIntervals` /
`TuningAbsolute` / `TuningRotated` / `TuningKind`, and `ScaleIntervals` /
`Scale`. The flat, `Copy` types cross the AppCoach port directly.

The coach receives the model via `Command::ConfigureSession`,
**decoupled from the audio lifecycle** — accepted in any state, causing
no `SessionState` change. On every configure it stores the session
`Scale` and publishes the **event-sourcing pair** that lets any head
reconstruct the musical frame:

- a **snapshot** — `AppCoach::music_info() -> Option<MusicInfo>`, a
  materialized read-cache (lock-free `ArcSwap`). **Sticky**: `None` only
  before the first configure, survives start/stop, cleared on shutdown.
- an **event** — `CoachEvent::SessionConfigured`, the log entry whose
  fold reconstructs `music_info`. The snapshot is written *before* the
  event, so a head reacting to the event reads a coherent snapshot.

The head is **vocabulary-free**: nothing below names a groove, a scale,
or a tonic. Naming is the deferred [note-system axis](#3-note-system-vocabulary).

In `apps/coach-game/src/state.rs`:

- `AppSettings { reference_hz, tuning_kind }` — axes 1, 2 — plus
  `tuning_absolute()`, which builds the placed cylinder via
  `TuningAbsolute::at_reference`, and `song_scale(intervals, sa_shift,
  octave)`, which re-bases it (`shift_up(sa_shift)`) and places it at a
  register. There is **no** `note_system` field.
- `SongTonality(Scale)` — axis 4. Default = Bilawal on C
  (`shift_up(SA_ON_C_SHIFT = 3)`, `octave = SA_ON_C_OCTAVE = 8`, one
  octave below the A=440 reference → middle register, C ≈ 262 Hz).
  Written to the coach via `ConfigureSession` on InGame entry.

In `apps/coach-game/src/game/dial.rs`:

- **North = Sa.** No frequency is hardcoded; the anchor is Sa, already
  baked into the `Scale`'s `TuningRotated`. The dial does only the
  render step; all pitch-math lives in `Scale`.
- `build_slots(&MusicInfo)` produces the groove angles from
  `Scale::tick_angle(i)`, lit per `intervals().degree_slots()` — so Sa
  sits north and a non-uniform tuning keeps its uneven spacing. No ratio
  table lives in the dial.
- `needle_angle` (the live `f0`) routes through `Scale::needle_angle` —
  the same fold the ticks use, so a perfectly-sung note lands on its
  tick. No snapshot → no needle (no Sa to measure from).

In `apps/coach-game/src/game/hud.rs`:

- Top-left panel shows the **math view** of the current tonality,
  sourced from the coach's `music_info()` snapshot (reading the snapshot
  exercises the round-trip). One row, the scale's **tooth-widths**:

  ```
  int 2 2 1 2 2 2 1
  ```

  the gaps between successive degrees walking up from Sa, closing the
  octave (`ScaleIntervals::widths(n)` against the tuning's groove count).
  No note names — that's the deferred label layer. `None` snapshot
  renders an honest "int —" placeholder.

In Settings UI:

- Audio + Music tabs. Music edits axes 1–2 only (reference Hz, tuning
  kind) — the note-system picker was removed with the head vocabulary.

In the scale picker:

- An in-game overlay (opened by clicking the HUD badge) lists the
  catalogue from `CoachEvent::ScalesListed` — `ScaleIntervals` shapes,
  labelled by their tooth-widths (no names yet). Selecting one
  reconfigures the live session.

## What's deferred

In rough order of priority:

- **`LabelRing` + `LockMode` machinery.** The right shape for axis-3
  rendering. Lock modes: `RelativeToTonic` (Sargam), `AbsoluteHz`
  (Western).
- **Dial label ring as a real visual layer.** Today the dial shows
  geometry + scale highlights but no groove labels (layer 5).
- **Per-song picker beyond the catalogue.** A raga / song picker that
  also sets the tonic and register, not just the tooth pattern.
- **School-namespaced scale catalogue.** Each school owns its scales;
  canonical-rotation equivalence computed at load.
- **Microtonal / non-12 tuning systems.** 22-shruti is behind the
  `MAX_TUNING_SLOTS` cap; wiring it through the UI is deferred.
- **Reference-Hz arbitrary input.** Today the picker offers four presets
  (440, 442, 432, 415). Text input deferred.
- **Internationalisation of menus/dialogs.** Note system is a
  *musical-vocabulary* choice, not a UI-language choice — kept decoupled
  from a future `language` setting (en/hi/es).
