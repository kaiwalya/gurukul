# Gurukul

An AI singing coach. Detection, diagnosis, and pedagogical feedback for singers.

Gurukul is not a tuner. A tuner tells you that you were flat. A coach tells you *why* — that your tongue root retracted on the high note, that your chest voice was holding too high into the passaggio, that your vibrato is 7Hz (fast, likely a tension tremor), and that your breath support collapsed two beats before the phrase ended. Gurukul is being built to close that gap.

## Status

Greenfield. Design phase.

The target deployment is **phone-first** (iOS + Android), with an optional watch peripheral for haptics and body sensors.

## Where to start

- [`docs/VISION.md`](docs/VISION.md) — what gurukul is and who it's for
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — engine / plugin / world split, ECS layering
- [`docs/TESTING.md`](docs/TESTING.md) — synthesis-as-oracle testing strategy
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — staged build plan
- [`docs/RESEARCH.md`](docs/RESEARCH.md) — voice modelling frontier notes + references
