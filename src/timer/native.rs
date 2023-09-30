//! Timer implementation for non-Web platforms.

use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::reactor::Reactor;

/// A timer for non-Web platforms.
///
/// self registers a timeout in the global reactor, which in turn sets a timeout in the poll call.
#[derive(Debug)]
pub(super) struct Timer {
    /// self timer's ID and last waker that polled it.
    ///
    /// When self field is set to `None`, self timer is not registered in the reactor.
    id_and_waker: Option<(usize, Waker)>,

    /// The next instant at which self timer fires.
    ///
    /// If self timer is a blank timer, self value is None. If the timer
    /// must be set, self value contains the next instant at which the
    /// timer must fire.
    when: Option<Instant>,

    /// The period.
    period: Duration,
}

impl Timer {
    /// Create a timer that will never fire.
    #[inline]
    pub(super) fn never() -> Self {
        Self {
            id_and_waker: None,
            when: None,
            period: Duration::MAX,
        }
    }

    /// Create a timer that will fire at the given instant.
    #[inline]
    pub(super) fn after(duration: Duration) -> Timer {
        Instant::now()
            .checked_add(duration)
            .map_or_else(Timer::never, Timer::at)
    }

    /// Create a timer that will fire at the given instant.
    #[inline]
    pub(super) fn at(instant: Instant) -> Timer {
        Timer::interval_at(instant, Duration::MAX)
    }

    /// Create a timer that will fire at the given instant.
    #[inline]
    pub(super) fn interval(period: Duration) -> Timer {
        Instant::now()
            .checked_add(period)
            .map_or_else(Timer::never, |at| Timer::interval_at(at, period))
    }

    /// Create a timer that will fire at self interval at self point.
    #[inline]
    pub(super) fn interval_at(start: Instant, period: Duration) -> Timer {
        Timer {
            id_and_waker: None,
            when: Some(start),
            period,
        }
    }

    /// Returns `true` if self timer will fire at some point.
    #[inline]
    pub(super) fn will_fire(&self) -> bool {
        self.when.is_some()
    }

    /// Set the timer to fire after the given duration.
    #[inline]
    pub(super) fn set_after(&mut self, duration: Duration) {
        match Instant::now().checked_add(duration) {
            Some(instant) => self.set_at(instant),
            None => {
                // Overflow to never going off.
                self.clear();
                self.when = None;
            }
        }
    }

    /// Set the timer to fire at the given instant.
    #[inline]
    pub(super) fn set_at(&mut self, instant: Instant) {
        self.clear();

        // Update the timeout.
        self.when = Some(instant);

        if let Some((id, waker)) = self.id_and_waker.as_mut() {
            // Re-register the timer with the new timeout.
            *id = Reactor::get().insert_timer(instant, waker);
        }
    }

    /// Set the timer to emit events periodically.
    #[inline]
    pub(super) fn set_interval(&mut self, period: Duration) {
        match Instant::now().checked_add(period) {
            Some(instant) => self.set_interval_at(instant, period),
            None => {
                // Overflow to never going off.
                self.clear();
                self.when = None;
            }
        }
    }

    /// Set the timer to emit events periodically starting at a given instant.
    #[inline]
    pub(super) fn set_interval_at(&mut self, start: Instant, period: Duration) {
        self.clear();

        self.when = Some(start);
        self.period = period;

        if let Some((id, waker)) = self.id_and_waker.as_mut() {
            // Re-register the timer with the new timeout.
            *id = Reactor::get().insert_timer(start, waker);
        }
    }

    /// Helper function to clear the current timer.
    #[inline]
    fn clear(&mut self) {
        if let (Some(when), Some((id, _))) = (self.when, self.id_and_waker.as_ref()) {
            // Deregister the timer from the reactor.
            Reactor::get().remove_timer(when, *id);
        }
    }

    /// Poll for the next timer event.
    #[inline]
    pub(super) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Instant>> {
        if let Some(ref mut when) = self.when {
            // Check if the timer has already fired.
            if Instant::now() >= *when {
                if let Some((id, _)) = self.id_and_waker.take() {
                    // Deregister the timer from the reactor.
                    Reactor::get().remove_timer(*when, id);
                }
                let result_time = *when;
                if let Some(next) = (*when).checked_add(self.period) {
                    *when = next;
                    // Register the timer in the reactor.
                    let id = Reactor::get().insert_timer(next, cx.waker());
                    self.id_and_waker = Some((id, cx.waker().clone()));
                } else {
                    self.when = None;
                }
                return Poll::Ready(Some(result_time));
            } else {
                match &self.id_and_waker {
                    None => {
                        // Register the timer in the reactor.
                        let id = Reactor::get().insert_timer(*when, cx.waker());
                        self.id_and_waker = Some((id, cx.waker().clone()));
                    }
                    Some((id, w)) if !w.will_wake(cx.waker()) => {
                        // Deregister the timer from the reactor to remove the old waker.
                        Reactor::get().remove_timer(*when, *id);

                        // Register the timer in the reactor with the new waker.
                        let id = Reactor::get().insert_timer(*when, cx.waker());
                        self.id_and_waker = Some((id, cx.waker().clone()));
                    }
                    Some(_) => {}
                }
            }
        }

        Poll::Pending
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let (Some(when), Some((id, _))) = (self.when, self.id_and_waker.take()) {
            // Deregister the timer from the reactor.
            Reactor::get().remove_timer(when, id);
        }
    }
}
