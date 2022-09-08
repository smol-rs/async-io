use std::cmp;
use std::collections::BTreeMap;
#[cfg(target_family = "wasm")]
use std::io;
use std::mem;
#[cfg(target_family = "wasm")]
use std::panic;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::task::Waker;
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;
#[cfg(target_family = "wasm")]
use parking::{pair, Parker, Unparker};

/// A reactor that is capable of processing timers.
pub(crate) struct Reactor {
    /// An ordered map of registered timers.
    ///
    /// Timers are in the order in which they fire. The `usize` in this type is a timer ID used to
    /// distinguish timers that fire at the same time. The `Waker` represents the task awaiting the
    /// timer.
    timers: Mutex<BTreeMap<(Instant, usize), Waker>>,

    /// A queue of timer operations (insert and remove).
    ///
    /// When inserting or removing a timer, we don't process it immediately - we just push it into
    /// this queue. Timers actually get processed when the queue fills up or the reactor is polled.
    timer_ops: ConcurrentQueue<TimerOp>,

    /// Ticker bumped before polling.
    ///
    /// This is useful for checking what is the current "round" of `ReactorLock::react()` when
    /// synchronizing things in `Source::readable()` and `Source::writable()`. Both of those
    /// methods must make sure they don't receive stale I/O events - they only accept events from a
    /// fresh "round" of `ReactorLock::react()`.
    ticker: AtomicUsize,

    /// The object used to unpark the reactor.
    ///
    /// Not needed when I/O is available since we block on `polling` instead.
    #[cfg(target_family = "wasm")]
    unparker: Unparker,

    /// The object used to park the reactor.
    ///
    /// The mutex used to hold this value implies the exclusive right to poll the reactor.
    #[cfg(target_family = "wasm")]
    parker: Mutex<Parker>,
}

impl Reactor {
    /// Create a new `Reactor`.
    pub(crate) fn new() -> Reactor { 
        #[cfg(target_family = "wasm")]
        let (parker, unparker) = pair();

        Reactor {
            timers: Mutex::new(BTreeMap::new()),
            timer_ops: ConcurrentQueue::bounded(1000),
            ticker: AtomicUsize::new(0),
            #[cfg(target_family = "wasm")]
            unparker,
            #[cfg(target_family = "wasm")]
            parker: Mutex::new(parker),
        }
    }

    /// Returns the current ticker.
    pub(crate) fn ticker(&self) -> usize {
        self.ticker.load(Ordering::SeqCst)
    }

    /// Bump the current ticker and return the new value.
    pub(crate) fn bump_ticker(&self) -> usize {
        self.ticker.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }

    /// Registers a timer in the reactor.
    ///
    /// Returns the inserted timer's ID.
    pub(crate) fn insert_timer(&self, when: Instant, waker: &Waker) -> usize {
        // Generate a new timer ID.
        static ID_GENERATOR: AtomicUsize = AtomicUsize::new(1);
        let id = ID_GENERATOR.fetch_add(1, Ordering::Relaxed);

        // Push an insert operation.
        while self
            .timer_ops
            .push(TimerOp::Insert(when, id, waker.clone()))
            .is_err()
        {
            // If the queue is full, drain it and try again.
            let mut timers = self.timers.lock().unwrap();
            self.process_timer_ops(&mut timers);
        }

        // Notify that a timer has been inserted.
        crate::reactor::Reactor::get().notify();

        id
    }

    /// Deregisters a timer from the reactor.
    pub(crate) fn remove_timer(&self, when: Instant, id: usize) {
        // Push a remove operation.
        while self.timer_ops.push(TimerOp::Remove(when, id)).is_err() {
            // If the queue is full, drain it and try again.
            let mut timers = self.timers.lock().unwrap();
            self.process_timer_ops(&mut timers);
        }
    }

    /// Notify the thread blocked on this reactor.
    #[cfg(target_family = "wasm")]
    pub(crate) fn notify(&self) {
        self.unparker.unpark();
    }

    /// Acquire a lock on the reactor.
    #[cfg(target_family = "wasm")]
    pub(crate) fn lock(&self) -> ReactorLock<'_> {
        let reactor = self;
        let parker = reactor.parker.lock().unwrap();
        ReactorLock { reactor, parker }
    }

    /// Try to acquire a lock on the reactor.
    #[cfg(target_family = "wasm")]
    pub(crate) fn try_lock(&self) -> Option<ReactorLock<'_>> {
        let reactor = self;
        let parker = reactor.parker.try_lock().ok()?;
        Some(ReactorLock { reactor, parker })
    }

    /// Processes ready timers and extends the list of wakers to wake.
    ///
    /// Returns the duration until the next timer before this method was called.
    pub(crate) fn process_timers(&self, wakers: &mut Vec<Waker>) -> Option<Duration> {
        let mut timers = self.timers.lock().unwrap();
        self.process_timer_ops(&mut timers);

        let now = Instant::now();

        // Split timers into ready and pending timers.
        //
        // Careful to split just *after* `now`, so that a timer set for exactly `now` is considered
        // ready.
        let pending = timers.split_off(&(now + Duration::from_nanos(1), 0));
        let ready = mem::replace(&mut *timers, pending);

        // Calculate the duration until the next event.
        let dur = if ready.is_empty() {
            // Duration until the next timer.
            timers
                .keys()
                .next()
                .map(|(when, _)| when.saturating_duration_since(now))
        } else {
            // Timers are about to fire right now.
            Some(Duration::from_secs(0))
        };

        // Drop the lock before waking.
        drop(timers);

        // Add wakers to the list.
        log::trace!("process_timers: {} ready wakers", ready.len());
        for (_, waker) in ready {
            wakers.push(waker);
        }

        dur
    }

    /// Processes queued timer operations.
    fn process_timer_ops(&self, timers: &mut MutexGuard<'_, BTreeMap<(Instant, usize), Waker>>) {
        // Process only as much as fits into the queue, or else this loop could in theory run
        // forever.
        for _ in 0..self.timer_ops.capacity().unwrap() {
            match self.timer_ops.pop() {
                Ok(TimerOp::Insert(when, id, waker)) => {
                    timers.insert((when, id), waker);
                }
                Ok(TimerOp::Remove(when, id)) => {
                    timers.remove(&(when, id));
                }
                Err(_) => break,
            }
        }
    }

    /// Determine the timeout duration for the reactor.
    pub(crate) fn timeout_duration(
        &self,
        timeout: Option<Duration>,
        wakers: &mut Vec<Waker>,
    ) -> Option<Duration> {
        let next_ready_timer = self.process_timers(wakers);

        // Determine the timeout duration.
        match (next_ready_timer, timeout) {
            (None, None) => None,
            (Some(dur), None) | (None, Some(dur)) => Some(dur),
            (Some(dur1), Some(dur2)) => Some(cmp::min(dur1, dur2)),
        }
    }
}

#[cfg(target_family = "wasm")]
pub(crate) struct ReactorLock<'a> {
    /// The reference to the reactor.
    reactor: &'a Reactor,

    /// The guard used to lock the reactor.
    parker: MutexGuard<'a, Parker>,
}

#[cfg(target_family = "wasm")]
impl ReactorLock<'_> {
    /// Processes timers and then blocks until the next timer is ready.
    pub(crate) fn react(&self, timeout: Option<Duration>) -> io::Result<()> {
        // Process the upcoming timers and determine the timeout duration.
        let mut wakers = vec![];
        let timeout = self.reactor.timeout_duration(timeout, &mut wakers);

        // Bump the ticker.
        self.reactor.bump_ticker();

        // Park the thread until either the timeout duration has elapsed or a timer is ready.
        let timers_ready = match timeout {
            None => {
                // Park until a notification.
                self.parker.park();
                false
            }
            Some(timeout) => !self.parker.park_timeout(timeout),
        };

        // If we hit the timeout, we may need to process more timers.
        if timers_ready {
            self.reactor.process_timers(&mut wakers);
        }

        // Fire off wakers.
        log::trace!("react: {} wakers", wakers.len());
        for waker in wakers {
            panic::catch_unwind(|| waker.wake()).ok();
        }

        Ok(())
    }
}

/// A single timer operation.
enum TimerOp {
    Insert(Instant, usize, Waker),
    Remove(Instant, usize),
}
