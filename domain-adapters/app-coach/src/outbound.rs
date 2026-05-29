//! Bounded outbound event queue, drained by [`AppCoach::poll_events`].
//!
//! The control plane pushes [`CoachEvent`]s; the head drains them on
//! its own cadence. On overflow the queue drops the *oldest* event and
//! coalesces a single [`CoachEvent::EventsDropped`] at the head of the
//! next drain, so the consumer always learns about drops before
//! observing the surviving events.

use domain_ports::app_coach::CoachEvent;
use std::collections::VecDeque;

pub(crate) struct OutboundQueue {
    cap: usize,
    inner: VecDeque<CoachEvent>,
    pending_dropped: u32,
}

impl OutboundQueue {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: VecDeque::with_capacity(cap),
            pending_dropped: 0,
        }
    }

    /// Push an event. If the queue is at capacity, drop the *oldest*
    /// event to make room and count it. The dropped count surfaces
    /// later as a coalesced [`CoachEvent::EventsDropped`] on the next
    /// [`Self::drain_into`] call.
    pub(crate) fn push(&mut self, ev: CoachEvent) {
        if self.inner.len() >= self.cap {
            self.inner.pop_front();
            self.pending_dropped = self.pending_dropped.saturating_add(1);
        }
        self.inner.push_back(ev);
    }

    /// Drain into `out`. If we had pending drops since the last flush,
    /// emit a single [`CoachEvent::EventsDropped`] at the head of the
    /// drained sequence so the consumer learns about it before the
    /// subsequent events.
    pub(crate) fn drain_into(&mut self, out: &mut Vec<CoachEvent>) {
        if self.pending_dropped > 0 {
            out.push(CoachEvent::EventsDropped {
                count: self.pending_dropped,
            });
            self.pending_dropped = 0;
        }
        out.extend(self.inner.drain(..));
    }
}
