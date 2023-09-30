//! Timers for web targets.
//!
//! These use the `setTimeout` function on the web to handle timing.  

use std::convert::TryInto;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use atomic_waker::AtomicWaker;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// A timer for non-Web platforms.
///
/// self registers a timeout in the global reactor, which in turn sets a timeout in the poll call.
#[derive(Debug)]
pub(super) struct Timer {
    /// The waker to wake when the timer fires.
    waker: Arc<State>,

    /// The ongoing timeout or interval.
    ongoing_timeout: TimerId,

    /// Keep the closure alive so we don't drop it.
    closure: Option<Closure<dyn FnMut()>>,
}

#[derive(Debug)]
struct State {
    /// The number of times this timer has been woken.
    woken: AtomicUsize,

    /// The waker to wake when the timer fires.
    waker: AtomicWaker,
}

#[derive(Debug)]
enum TimerId {
    NoTimer,
    Timeout(i32),
    Interval(i32),
}

impl Timer {
    /// Create a timer that will never fire.
    #[inline]
    pub(super) fn never() -> Self {
        Self {
            waker: Arc::new(State {
                woken: AtomicUsize::new(0),
                waker: AtomicWaker::new(),
            }),
            ongoing_timeout: TimerId::NoTimer,
            closure: None,
        }
    }

    /// Create a timer that will fire at the given instant.
    #[inline]
    pub(super) fn after(duration: Duration) -> Timer {
        let mut this = Self::never();
        this.set_after(duration);
        this
    }

    /// Create a timer that will fire at the given instant.
    #[inline]
    pub(super) fn interval(period: Duration) -> Timer {
        let mut this = Self::never();
        this.set_interval(period);
        this
    }

    /// Returns `true` if self timer will fire at some point.
    #[inline]
    pub(super) fn will_fire(&self) -> bool {
        matches!(
            self.ongoing_timeout,
            TimerId::Timeout(_) | TimerId::Interval(_)
        )
    }

    /// Set the timer to fire after the given duration.
    #[inline]
    pub(super) fn set_after(&mut self, duration: Duration) {
        // Set the timeout.
        let id = {
            let waker = self.waker.clone();
            let closure: Closure<dyn FnMut()> = Closure::wrap(Box::new(move || {
                waker.wake();
            }));

            let result = web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    closure.as_ref().unchecked_ref(),
                    duration.as_millis().try_into().expect("timeout too long"),
                );

            // Make sure we don't drop the closure before it's called.
            self.closure = Some(closure);

            match result {
                Ok(id) => id,
                Err(_) => {
                    panic!("failed to set timeout")
                }
            }
        };

        // Set our ID.
        self.ongoing_timeout = TimerId::Timeout(id);
    }

    /// Set the timer to emit events periodically.
    #[inline]
    pub(super) fn set_interval(&mut self, period: Duration) {
        // Set the timeout.
        let id = {
            let waker = self.waker.clone();
            let closure: Closure<dyn FnMut()> = Closure::wrap(Box::new(move || {
                waker.wake();
            }));

            let result = web_sys::window()
                .unwrap()
                .set_interval_with_callback_and_timeout_and_arguments_0(
                    closure.as_ref().unchecked_ref(),
                    period.as_millis().try_into().expect("timeout too long"),
                );

            // Make sure we don't drop the closure before it's called.
            self.closure = Some(closure);

            match result {
                Ok(id) => id,
                Err(_) => {
                    panic!("failed to set interval")
                }
            }
        };

        // Set our ID.
        self.ongoing_timeout = TimerId::Interval(id);
    }

    /// Poll for the next timer event.
    #[inline]
    pub(super) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<()>> {
        let mut registered = false;
        let mut woken = self.waker.woken.load(Ordering::Acquire);

        loop {
            if woken > 0 {
                // Try to decrement the number of woken events.
                if let Err(new_woken) = self.waker.woken.compare_exchange(
                    woken,
                    woken - 1,
                    Ordering::SeqCst,
                    Ordering::Acquire,
                ) {
                    woken = new_woken;
                    continue;
                }

                // If we are using a one-shot timer, clear it.
                if let TimerId::Timeout(_) = self.ongoing_timeout {
                    self.clear();
                }

                return Poll::Ready(Some(()));
            }

            if !registered {
                // Register the waker.
                self.waker.waker.register(cx.waker());
                registered = true;
            } else {
                // We've already registered, so we can just return pending.
                return Poll::Pending;
            }
        }
    }

    /// Clear the current timeout.
    fn clear(&mut self) {
        match self.ongoing_timeout {
            TimerId::NoTimer => {}
            TimerId::Timeout(id) => {
                web_sys::window().unwrap().clear_timeout_with_handle(id);
            }
            TimerId::Interval(id) => {
                web_sys::window().unwrap().clear_interval_with_handle(id);
            }
        }
    }
}

impl State {
    fn wake(&self) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        self.waker.wake();
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.clear();
    }
}
