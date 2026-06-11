//! `ReplayCoach`: an [`AppCoach`] that serves a recorded trace's reads instead
//! of a live mic + DSP engine.
//!
//! Shaped like the test `FakeCoach` — interior-mutable pending queues drained on
//! read, commands logged-and-ignored — but driven by the replay loader rather
//! than a test. The defining rule (the port-types rule; see
//! [`TraceBuffer`](crate::trace::TraceBuffer)): it hands `drain_events` the recorded
//! `FeatureSnapshot`s **verbatim**, so the real `f0_hz → PitchLog2` lift re-runs
//! on identical input — replay reproduces the head's own conversion code, it
//! does not invert it.
//!
//! `!Send` like every `AppCoach`, single-threaded on the trace side, so
//! `RefCell`, not `Mutex` (the `recording_coach` precedent).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use domain_ports::app_coach::{
    AppCoach, AudioInfo, CoachEvent, Command, FeatureSnapshot, MusicInfo, ShutdownResult,
};

use super::load::CoachRead;

/// The mutable queues a frame's recorded reads land in, drained as the head
/// reads them. One frame's payload at a time: the driver loads frame N before
/// `drain_events` runs, the head drains it, the next frame overwrites.
#[derive(Default)]
struct Pending {
    events: Vec<CoachEvent>,
    latest: Option<FeatureSnapshot>,
    drained: Vec<FeatureSnapshot>,
}

/// An [`AppCoach`] backed by a recorded trace. Insert it as the `Coach` NonSend
/// handle in a `--replay` build; the driver calls [`ReplayCoach::load_frame`]
/// each frame before the head reads.
pub struct ReplayCoach {
    pending: RefCell<Pending>,
    /// Reconstructed musical frame of reference. The recorder doesn't capture
    /// `music_info()` directly, but the port guarantees every `ConfigureSession`
    /// emits a `SessionConfigured` carrying the same `Scale` (snapshot written
    /// before event), so folding the last replayed `SessionConfigured` rebuilds
    /// it exactly. `None` until the first one is replayed.
    music: RefCell<Option<MusicInfo>>,
}

impl Default for ReplayCoach {
    fn default() -> Self {
        Self {
            pending: RefCell::new(Pending::default()),
            music: RefCell::new(None),
        }
    }
}

impl ReplayCoach {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load one frame's recorded `coach` payload into the pending queues, ready
    /// for the head's reads this frame. Folds any `SessionConfigured` event into
    /// the reconstructed [`music_info`](AppCoach::music_info) before the events
    /// are queued (so a head reading `music_info()` this frame sees the update,
    /// matching the live snapshot-before-event ordering).
    ///
    /// Takes the payload **by value** — the driver `mem::take`s it out of the
    /// loaded trace as it advances (each frame served once), so no `CoachEvent`
    /// is cloned (the port keeps events move-only).
    pub fn load_frame(&self, read: CoachRead) {
        for ev in &read.events {
            if let CoachEvent::SessionConfigured { scale } = ev {
                *self.music.borrow_mut() = Some(MusicInfo { scale: *scale });
            }
        }
        let mut p = self.pending.borrow_mut();
        p.events = read.events;
        p.latest = read.latest;
        p.drained = read.drained;
    }
}

impl AppCoach for ReplayCoach {
    /// Commands are already in the recorded `cmd` channel and drive nothing on
    /// replay (the coach is canned). Drop them.
    fn send_command(&self, _cmd: Command) {}

    fn poll_events(&self, out: &mut Vec<CoachEvent>) {
        out.append(&mut self.pending.borrow_mut().events);
    }

    fn shutdown(&self, _timeout: Duration) -> ShutdownResult {
        ShutdownResult::Clean
    }

    fn latest_features(&self) -> Option<FeatureSnapshot> {
        // Served verbatim — the head lifts it, replay does not pre-lift.
        self.pending.borrow().latest
    }

    fn drain_features(&self, out: &mut Vec<FeatureSnapshot>) {
        out.append(&mut self.pending.borrow_mut().drained);
    }

    /// No Bevy-side reader today; nothing to reconstruct from the trace.
    fn audio_info(&self) -> Option<AudioInfo> {
        None
    }

    fn music_info(&self) -> Option<MusicInfo> {
        *self.music.borrow()
    }
}

/// A shared handle to a [`ReplayCoach`] that satisfies the `Coach(Box<dyn
/// AppCoach>)` handle the head reads through, while the driver keeps a second
/// `Rc` clone to call [`ReplayCoach::load_frame`]. Mirrors how `RecordingCoach`
/// shares its trace buffer by a second `Rc`. All methods forward to the inner
/// coach (every `AppCoach` method takes `&self`, so the `Rc` needs no interior
/// wrapping of its own).
pub struct SharedReplayCoach(pub Rc<ReplayCoach>);

impl AppCoach for SharedReplayCoach {
    fn send_command(&self, cmd: Command) {
        self.0.send_command(cmd)
    }
    fn poll_events(&self, out: &mut Vec<CoachEvent>) {
        self.0.poll_events(out)
    }
    fn shutdown(&self, timeout: Duration) -> ShutdownResult {
        self.0.shutdown(timeout)
    }
    fn latest_features(&self) -> Option<FeatureSnapshot> {
        self.0.latest_features()
    }
    fn drain_features(&self, out: &mut Vec<FeatureSnapshot>) {
        self.0.drain_features(out)
    }
    fn audio_info(&self) -> Option<AudioInfo> {
        self.0.audio_info()
    }
    fn music_info(&self) -> Option<MusicInfo> {
        self.0.music_info()
    }
}
