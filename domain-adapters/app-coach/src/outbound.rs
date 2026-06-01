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

#[cfg(test)]
mod tests {
    use super::*;

    fn list_devices() -> CoachEvent {
        CoachEvent::DevicesListed { devices: vec![] }
    }

    #[test]
    fn overflow_drops_oldest_and_coalesces_count() {
        let mut q = OutboundQueue::new(4);
        for _ in 0..10 {
            q.push(list_devices());
        }

        let mut out = Vec::new();
        q.drain_into(&mut out);

        let dropped = match out.first() {
            Some(CoachEvent::EventsDropped { count }) => *count,
            _ => panic!("expected EventsDropped at head of drain"),
        };
        assert_eq!(dropped, 6, "10 pushed, cap 4 → 6 dropped");
        assert_eq!(out.len(), 5, "EventsDropped + 4 surviving events");
    }

    #[test]
    fn no_overflow_means_no_dropped_event() {
        let mut q = OutboundQueue::new(4);
        q.push(list_devices());
        q.push(list_devices());

        let mut out = Vec::new();
        q.drain_into(&mut out);

        assert_eq!(out.len(), 2);
        assert!(
            !matches!(out.first(), Some(CoachEvent::EventsDropped { .. })),
            "no overflow should not emit EventsDropped",
        );
    }

    #[test]
    fn drained_count_resets_between_flushes() {
        let mut q = OutboundQueue::new(2);
        for _ in 0..5 {
            q.push(list_devices());
        }

        let mut out = Vec::new();
        q.drain_into(&mut out);
        assert!(matches!(
            out.first(),
            Some(CoachEvent::EventsDropped { count: 3 })
        ));

        // Second flush with no new drops must not re-emit.
        q.push(list_devices());
        out.clear();
        q.drain_into(&mut out);
        assert_eq!(out.len(), 1);
        assert!(!matches!(out[0], CoachEvent::EventsDropped { .. }));
    }
}
