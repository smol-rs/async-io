use std::collections::BTreeMap;
use std::io;
use std::mem;
#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawSocket;
use std::panic;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;
use futures_lite::*;
use once_cell::sync::Lazy;
use polling::{Event, Poller};
use vec_arena::Arena;

/// The reactor.
///
/// There is only one global instance of this type, accessible by [`Reactor::get()`].
pub(crate) struct Reactor {
    /// Number of active `Parker`s.
    parker_count: AtomicUsize,

    /// Unparks the async-io thread.
    thread_unparker: parking::Unparker,

    /// Bindings to epoll/kqueue/event ports/wepoll.
    poller: Poller,

    /// Ticker bumped before polling.
    ticker: AtomicUsize,

    /// Registered sources.
    sources: Mutex<Arena<Arc<Source>>>,

    /// Temporary storage for I/O events when polling the reactor.
    events: Mutex<Vec<Event>>,

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
}

impl Reactor {
    /// Returns a reference to the reactor.
    pub(crate) fn get() -> &'static Reactor {
        static REACTOR: Lazy<Reactor> = Lazy::new(|| {
            let (parker, unparker) = parking::pair();

            // Spawn a helper thread driving the reactor.
            //
            // Note that this thread is not exactly necessary, it's only here to help push things
            // forward if there are no `Parker`s around or if `Parker`s are just idling and never
            // parking.
            thread::Builder::new()
                .name("async-io".to_string())
                .spawn(move || Reactor::get().main_loop(parker))
                .expect("cannot spawn async-io thread");

            Reactor {
                parker_count: AtomicUsize::new(0),
                thread_unparker: unparker,
                poller: Poller::new().expect("cannot initialize I/O event notification"),
                ticker: AtomicUsize::new(0),
                sources: Mutex::new(Arena::new()),
                events: Mutex::new(Vec::new()),
                timers: Mutex::new(BTreeMap::new()),
                timer_ops: ConcurrentQueue::bounded(1000),
            }
        });
        &REACTOR
    }

    /// The main loop for the async-io thread.
    fn main_loop(&self, parker: parking::Parker) {
        // The last observed reactor tick.
        let mut last_tick = 0;
        // Number of sleeps since this thread has called `react()`.
        let mut sleeps = 0u64;

        loop {
            let tick = self.ticker.load(Ordering::SeqCst);

            if last_tick == tick {
                let reactor_lock = if sleeps >= 10 {
                    // If no new ticks have occurred for a while, stop sleeping and
                    // spinning in this loop and just block on the reactor lock.
                    Some(self.lock())
                } else {
                    self.try_lock()
                };

                if let Some(reactor_lock) = reactor_lock {
                    let _ = reactor_lock.react(None);
                    last_tick = self.ticker.load(Ordering::SeqCst);
                    sleeps = 0;
                }
            } else {
                last_tick = tick;
            }

            if self.parker_count.load(Ordering::SeqCst) > 0 {
                // Exponential backoff from 50us to 10ms.
                let delay_us = [50, 75, 100, 250, 500, 750, 1000, 2500, 5000]
                    .get(sleeps as usize)
                    .unwrap_or(&10_000);

                if parker.park_timeout(Duration::from_micros(*delay_us)) {
                    // If woken before timeout, reset the last tick and sleep counter.
                    last_tick = self.ticker.load(Ordering::SeqCst);
                    sleeps = 0;
                } else {
                    sleeps += 1;
                }
            }
        }
    }

    /// Increments the number of `Parker`s.
    pub(crate) fn increment_parkers(&self) {
        self.parker_count.fetch_add(1, Ordering::SeqCst);
    }

    /// Decrements the number of `Parker`s.
    pub(crate) fn decrement_parkers(&self) {
        self.parker_count.fetch_sub(1, Ordering::SeqCst);
        self.thread_unparker.unpark();
    }

    /// Notifies the thread blocked on the reactor.
    pub(crate) fn notify(&self) {
        self.poller.notify().expect("failed to notify reactor");
    }

    /// Registers an I/O source in the reactor.
    pub(crate) fn insert_io(
        &self,
        #[cfg(unix)] raw: RawFd,
        #[cfg(windows)] raw: RawSocket,
    ) -> io::Result<Arc<Source>> {
        // Register the file descriptor.
        self.poller.insert(raw)?;

        // Create an I/O source for this file descriptor.
        let mut sources = self.sources.lock().unwrap();
        let key = sources.next_vacant();
        let source = Arc::new(Source {
            raw,
            key,
            wakers: Mutex::new(Wakers {
                tick_readable: 0,
                tick_writable: 0,
                readers: Vec::new(),
                writers: Vec::new(),
            }),
        });
        sources.insert(source.clone());

        Ok(source)
    }

    /// Deregisters an I/O source from the reactor.
    pub(crate) fn remove_io(&self, source: &Source) -> io::Result<()> {
        let mut sources = self.sources.lock().unwrap();
        sources.remove(source.key);
        self.poller.remove(source.raw)
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
        self.notify();

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

    /// Locks the reactor, potentially blocking if the lock is held by another thread.
    pub(crate) fn lock(&self) -> ReactorLock<'_> {
        let reactor = self;
        let events = self.events.lock().unwrap();
        ReactorLock { reactor, events }
    }

    /// Attempts to lock the reactor.
    pub(crate) fn try_lock(&self) -> Option<ReactorLock<'_>> {
        self.events.try_lock().ok().map(|events| {
            let reactor = self;
            ReactorLock { reactor, events }
        })
    }

    /// Processes ready timers and extends the list of wakers to wake.
    ///
    /// Returns the duration until the next timer before this method was called.
    fn process_timers(&self, wakers: &mut Vec<Waker>) -> Option<Duration> {
        let mut timers = self.timers.lock().unwrap();
        self.process_timer_ops(&mut timers);

        let now = Instant::now();

        // Split timers into ready and pending timers.
        let pending = timers.split_off(&(now, 0));
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
}

/// A lock on the reactor.
pub(crate) struct ReactorLock<'a> {
    reactor: &'a Reactor,
    events: MutexGuard<'a, Vec<Event>>,
}

impl ReactorLock<'_> {
    /// Processes new events, blocking until the first event or the timeout.
    pub(crate) fn react(mut self, timeout: Option<Duration>) -> io::Result<()> {
        let mut wakers = Vec::new();

        // Process ready timers.
        let next_timer = self.reactor.process_timers(&mut wakers);

        // compute the timeout for blocking on I/O events.
        let timeout = match (next_timer, timeout) {
            (None, None) => None,
            (Some(t), None) | (None, Some(t)) => Some(t),
            (Some(a), Some(b)) => Some(a.min(b)),
        };

        // Bump the ticker before polling I/O.
        let tick = self
            .reactor
            .ticker
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);

        self.events.clear();

        // Block on I/O events.
        let res = match self.reactor.poller.wait(&mut self.events, timeout) {
            // No I/O events occurred.
            Ok(0) => {
                if timeout != Some(Duration::from_secs(0)) {
                    // The non-zero timeout was hit so fire ready timers.
                    self.reactor.process_timers(&mut wakers);
                }
                Ok(())
            }

            // At least one I/O event occurred.
            Ok(_) => {
                // Iterate over sources in the event list.
                let sources = self.reactor.sources.lock().unwrap();

                for ev in self.events.iter() {
                    // Check if there is a source in the table with this key.
                    if let Some(source) = sources.get(ev.key) {
                        let mut w = source.wakers.lock().unwrap();

                        // Wake readers if a readability event was emitted.
                        if ev.readable {
                            w.tick_readable = tick;
                            wakers.append(&mut w.readers);
                        }

                        // Wake writers if a writability event was emitted.
                        if ev.writable {
                            w.tick_writable = tick;
                            wakers.append(&mut w.writers);
                        }

                        // Re-register if there are still writers or
                        // readers. The can happen if e.g. we were
                        // previously interested in both readability and
                        // writability, but only one of them was emitted.
                        if !(w.writers.is_empty() && w.readers.is_empty()) {
                            self.reactor.poller.interest(
                                source.raw,
                                Event {
                                    key: source.key,
                                    readable: !w.readers.is_empty(),
                                    writable: !w.writers.is_empty(),
                                },
                            )?;
                        }
                    }
                }

                Ok(())
            }

            // The syscall was interrupted.
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(()),

            // An actual error occureed.
            Err(err) => Err(err),
        };

        // Drop the lock before waking.
        drop(self);

        // Wake up ready tasks.
        for waker in wakers {
            // Don't let a panicking waker blow everything up.
            let _ = panic::catch_unwind(|| waker.wake());
        }

        res
    }
}

/// A single timer operation.
enum TimerOp {
    Insert(Instant, usize, Waker),
    Remove(Instant, usize),
}

/// A registered source of I/O events.
#[derive(Debug)]
pub(crate) struct Source {
    /// Raw file descriptor on Unix platforms.
    #[cfg(unix)]
    pub(crate) raw: RawFd,

    /// Raw socket handle on Windows.
    #[cfg(windows)]
    pub(crate) raw: RawSocket,

    /// The key of this source obtained during registration.
    key: usize,

    /// Tasks interested in events on this source.
    wakers: Mutex<Wakers>,
}

/// Tasks interested in events on a source.
#[derive(Debug)]
struct Wakers {
    /// Last reactor tick that delivered a readability event.
    tick_readable: usize,

    /// Last reactor tick that delivered a writability event.
    tick_writable: usize,

    /// Tasks waiting for the next readability event.
    readers: Vec<Waker>,

    /// Tasks waiting for the next writability event.
    writers: Vec<Waker>,
}

impl Source {
    /// Waits until the I/O source is readable.
    pub(crate) async fn readable(&self) -> io::Result<()> {
        let mut ticks = None;

        future::poll_fn(|cx| {
            let mut w = self.wakers.lock().unwrap();

            // Check if the reactor has delivered a readability event.
            if let Some((a, b)) = ticks {
                // If `tick_readable` has changed to a value other than the old reactor tick, that
                // means a newer reactor tick has delivered a readability event.
                if w.tick_readable != a && w.tick_readable != b {
                    return Poll::Ready(Ok(()));
                }
            }

            // If there are no other readers, re-register in the reactor.
            if w.readers.is_empty() {
                Reactor::get().poller.interest(
                    self.raw,
                    Event {
                        key: self.key,
                        readable: true,
                        writable: !w.writers.is_empty(),
                    },
                )?;
            }

            // Register the current task's waker if not present already.
            if w.readers.iter().all(|w| !w.will_wake(cx.waker())) {
                w.readers.push(cx.waker().clone());
            }

            // Remember the current ticks.
            if ticks.is_none() {
                ticks = Some((
                    Reactor::get().ticker.load(Ordering::SeqCst),
                    w.tick_readable,
                ));
            }

            Poll::Pending
        })
        .await
    }

    /// Waits until the I/O source is writable.
    pub(crate) async fn writable(&self) -> io::Result<()> {
        let mut ticks = None;

        future::poll_fn(|cx| {
            let mut w = self.wakers.lock().unwrap();

            // Check if the reactor has delivered a writability event.
            if let Some((a, b)) = ticks {
                // If `tick_writable` has changed to a value other than the old reactor tick, that
                // means a newer reactor tick has delivered a writability event.
                if w.tick_writable != a && w.tick_writable != b {
                    return Poll::Ready(Ok(()));
                }
            }

            // If there are no other writers, re-register in the reactor.
            if w.writers.is_empty() {
                Reactor::get().poller.interest(
                    self.raw,
                    Event {
                        key: self.key,
                        readable: !w.readers.is_empty(),
                        writable: true,
                    },
                )?;
            }

            // Register the current task's waker if not present already.
            if w.writers.iter().all(|w| !w.will_wake(cx.waker())) {
                w.writers.push(cx.waker().clone());
            }

            // Remember the current ticks.
            if ticks.is_none() {
                ticks = Some((
                    Reactor::get().ticker.load(Ordering::SeqCst),
                    w.tick_writable,
                ));
            }

            Poll::Pending
        })
        .await
    }
}
