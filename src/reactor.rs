use std::cell::Cell;
use std::collections::BTreeMap;
use std::io;
use std::mem;
#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawSocket;
use std::panic;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;
use futures_lite::*;
use once_cell::sync::Lazy;
use polling::{Event, Poller};
use vec_arena::Arena;
use waker_fn::waker_fn;

/// The reactor.
///
/// There is only one global instance of this type, accessible by [`Reactor::get()`].
pub(crate) struct Reactor {
    /// Number of active `block_on()`s.
    block_on_count: AtomicUsize,

    /// Unparks the "async-io" thread.
    thread_unparker: parking::Unparker,

    /// Portable bindings to epoll/kqueue/event ports/wepoll.
    ///
    /// This is where I/O is polled, producing I/O events.
    poller: Poller,

    /// Ticker bumped before polling.
    ///
    /// This is useful for checking what is the current "round" of `ReactorLock::react()` when
    /// synchronizing things in `Source::readable()` and `Source::writable()`. Both of those
    /// methods must make sure they don't receive stale I/O events - they only accept events from a
    /// fresh "round" of `ReactorLock::react()`.
    ticker: AtomicUsize,

    /// Registered sources.
    sources: Mutex<Arena<Arc<Source>>>,

    /// Temporary storage for I/O events when polling the reactor.
    ///
    /// Holding a lock on this event list implies the exclusive right to poll I/O.
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
                block_on_count: AtomicUsize::new(0),
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

    /// The main loop for the "async-io" thread.
    fn main_loop(&self, parker: parking::Parker) {
        // The last observed reactor tick.
        let mut last_tick = 0;
        // Number of sleeps since this thread has called `react()`.
        let mut sleeps = 0u64;

        loop {
            let tick = self.ticker.load(Ordering::SeqCst);

            if last_tick == tick {
                let reactor_lock = if sleeps >= 10 {
                    // If no new ticks have occurred for a while, stop sleeping and spinning in
                    // this loop and just block on the reactor lock.
                    Some(self.lock())
                } else {
                    self.try_lock()
                };

                if let Some(mut reactor_lock) = reactor_lock {
                    log::trace!("main_loop: waiting on I/O");
                    reactor_lock.react(None).ok();
                    last_tick = self.ticker.load(Ordering::SeqCst);
                    sleeps = 0;
                }
            } else {
                last_tick = tick;
            }

            if self.block_on_count.load(Ordering::SeqCst) > 0 {
                // Exponential backoff from 50us to 10ms.
                let delay_us = [50, 75, 100, 250, 500, 750, 1000, 2500, 5000]
                    .get(sleeps as usize)
                    .unwrap_or(&10_000);

                log::trace!("main_loop: sleeping for {} us", delay_us);
                if parker.park_timeout(Duration::from_micros(*delay_us)) {
                    log::trace!("main_loop: notified");

                    // If notified before timeout, reset the last tick and the sleep counter.
                    last_tick = self.ticker.load(Ordering::SeqCst);
                    sleeps = 0;
                } else {
                    sleeps += 1;
                }
            }
        }
    }

    /// Blocks the current thread on a future, processing I/O events when idle.
    pub(crate) fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        log::trace!("block_on()");

        // Increment `block_on_count` so that the "async-io" thread becomes less aggressive.
        self.block_on_count.fetch_add(1, Ordering::SeqCst);

        // Make sure to decrement `block_on_count` at the end and wake the "async-io" thread.
        let _guard = CallOnDrop(|| {
            Reactor::get().block_on_count.fetch_sub(1, Ordering::SeqCst);
            Reactor::get().thread_unparker.unpark();
        });

        // Parker and unparker for notifying the current thread.
        let (p, u) = parking::pair();
        // This boolean is set to `true` when the current thread is blocked on I/O.
        let io_blocked = Arc::new(AtomicBool::new(false));

        thread_local! {
            // Indicates that the current thread is polling I/O, but not necessarily blocked on it.
            static IO_POLLING: Cell<bool> = Cell::new(false);
        }

        // Prepare the waker.
        let waker = waker_fn({
            let io_blocked = io_blocked.clone();
            move || {
                if u.unpark() {
                    // Check if waking from another thread and if currently blocked on I/O.
                    if !IO_POLLING.with(Cell::get) && io_blocked.load(Ordering::SeqCst) {
                        Reactor::get().notify();
                    }
                }
            }
        });
        let cx = &mut Context::from_waker(&waker);
        pin!(future);

        loop {
            // Poll the future.
            if let Poll::Ready(t) = future.as_mut().poll(cx) {
                log::trace!("block_on: completed");
                return t;
            }

            // Check if a notification was received.
            if p.park_timeout(Duration::from_secs(0)) {
                log::trace!("block_on: notified");

                // Try grabbing a lock on the reactor to process I/O events.
                if let Some(mut reactor_lock) = Reactor::get().try_lock() {
                    // First let wakers know this parker is processing I/O events.
                    IO_POLLING.with(|io| io.set(true));
                    let _guard = CallOnDrop(|| {
                        IO_POLLING.with(|io| io.set(false));
                    });

                    // Process available I/O events.
                    reactor_lock.react(Some(Duration::from_secs(0))).ok();
                }
                continue;
            }

            // Try grabbing a lock on the reactor to wait on I/O.
            if let Some(mut reactor_lock) = Reactor::get().try_lock() {
                // Record the instant at which the lock was grabbed.
                let start = Instant::now();

                loop {
                    // First let wakers know this parker is blocked on I/O.
                    IO_POLLING.with(|io| io.set(true));
                    io_blocked.store(true, Ordering::SeqCst);
                    let _guard = CallOnDrop(|| {
                        IO_POLLING.with(|io| io.set(false));
                        io_blocked.store(false, Ordering::SeqCst);
                    });

                    // Check if a notification has been received before `io_blocked` was updated
                    // because in that case the reactor won't receive a wakeup.
                    if p.park_timeout(Duration::from_secs(0)) {
                        log::trace!("block_on: notified");
                        break;
                    }

                    // Wait for I/O events.
                    log::trace!("block_on: waiting on I/O");
                    reactor_lock.react(None).ok();

                    // Check if a notification has been received.
                    if p.park_timeout(Duration::from_secs(0)) {
                        log::trace!("block_on: notified");
                        break;
                    }

                    // Check if this thread been handling I/O events for a long time.
                    if start.elapsed() > Duration::from_micros(500) {
                        log::trace!("block_on: stops hogging the reactor");

                        // This thread is clearly processing I/O events for some other threads
                        // because it didn't get a notification yet. It's best to stop hogging the
                        // reactor and give other threads a chance to process I/O events for
                        // themselves.
                        drop(reactor_lock);

                        // Unpark the "async-io" thread in case no other thread is ready to start
                        // processing I/O events. This way we prevent a potential latency spike.
                        self.thread_unparker.unpark();

                        // Wait for a notification.
                        p.park();
                        break;
                    }
                }
            } else {
                // Wait for an actual notification.
                log::trace!("block_on: sleep until notification");
                p.park();
            }
        }
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
            state: Mutex::new(State {
                read: Direction {
                    tick: 0,
                    waker: None,
                    wakers: Vec::new(),
                },
                write: Direction {
                    tick: 0,
                    waker: None,
                    wakers: Vec::new(),
                },
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

    /// Notifies the thread blocked on the reactor.
    fn notify(&self) {
        self.poller.notify().expect("failed to notify reactor");
    }

    /// Locks the reactor, potentially blocking if the lock is held by another thread.
    fn lock(&self) -> ReactorLock<'_> {
        let reactor = self;
        let events = self.events.lock().unwrap();
        ReactorLock { reactor, events }
    }

    /// Attempts to lock the reactor.
    fn try_lock(&self) -> Option<ReactorLock<'_>> {
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
}

/// A lock on the reactor.
struct ReactorLock<'a> {
    reactor: &'a Reactor,
    events: MutexGuard<'a, Vec<Event>>,
}

impl ReactorLock<'_> {
    /// Processes new events, blocking until the first event or the timeout.
    fn react(&mut self, timeout: Option<Duration>) -> io::Result<()> {
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
                        let mut state = source.state.lock().unwrap();

                        // Wake writers if a writability event was emitted.
                        if ev.writable {
                            state.write.tick = tick;
                            state.write.drain_into(&mut wakers);
                        }

                        // Wake readers if a readability event was emitted.
                        if ev.readable {
                            state.read.tick = tick;
                            state.read.drain_into(&mut wakers);
                        }

                        // Re-register if there are still writers or readers. The can happen if
                        // e.g. we were previously interested in both readability and writability,
                        // but only one of them was emitted.
                        if !(state.write.is_empty() && state.read.is_empty()) {
                            self.reactor.poller.interest(
                                source.raw,
                                Event {
                                    key: source.key,
                                    readable: !state.read.is_empty(),
                                    writable: !state.write.is_empty(),
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

        // Wake up ready tasks.
        log::trace!("react: {} ready wakers", wakers.len());
        for waker in wakers {
            // Don't let a panicking waker blow everything up.
            panic::catch_unwind(|| waker.wake()).ok();
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

    /// Inner state with registered wakers.
    state: Mutex<State>,
}

/// Inner state with registered wakers.
#[derive(Debug)]
struct State {
    /// State of the read direction.
    read: Direction,

    /// State of the write direction.
    write: Direction,
}

/// A read or write direction.
#[derive(Debug)]
struct Direction {
    /// Last reactor tick that delivered an event.
    tick: usize,

    /// Waker stored by `AsyncRead` or `AsyncWrite`.
    waker: Option<Waker>,

    /// Wakers of tasks waiting for the next event.
    wakers: Vec<Waker>,
}

impl Direction {
    /// Returns `true` if there are no wakers interested in this direction.
    fn is_empty(&self) -> bool {
        self.waker.is_none() && self.wakers.is_empty()
    }

    fn drain_into(&mut self, dst: &mut Vec<Waker>) {
        if let Some(w) = self.waker.take() {
            dst.push(w);
        }
        dst.append(&mut self.wakers);
    }
}

impl Source {
    /// Registers a waker from `AsyncRead`.
    ///
    /// If a different waker is already registered, it gets replaced and woken.
    pub(crate) fn register_reader(&self, waker: &Waker) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();

        // If there are no other readers, re-register in the reactor.
        if state.read.is_empty() {
            Reactor::get().poller.interest(
                self.raw,
                Event {
                    key: self.key,
                    readable: true,
                    writable: !state.write.is_empty(),
                },
            )?;
        }

        if let Some(w) = state.read.waker.take() {
            if w.will_wake(waker) {
                state.read.waker = Some(w);
                return Ok(());
            }

            // Don't let a panicking waker blow everything up.
            panic::catch_unwind(|| w.wake()).ok();
        }

        state.read.waker = Some(waker.clone());
        Ok(())
    }

    /// Waits until the I/O source is readable.
    pub(crate) async fn readable(&self) -> io::Result<()> {
        let mut ticks = None;

        future::poll_fn(|cx| {
            let mut state = self.state.lock().unwrap();

            // Check if the reactor has delivered a readability event.
            if let Some((a, b)) = ticks {
                // If `state.read.tick` has changed to a value other than the old reactor tick,
                // that means a newer reactor tick has delivered a readability event.
                if state.read.tick != a && state.read.tick != b {
                    log::trace!("readable: fd={}", self.raw);
                    return Poll::Ready(Ok(()));
                }
            }

            let is_empty = (state.read.is_empty(), state.write.is_empty());

            // Register the current task's waker if not present already.
            if state.read.wakers.iter().all(|w| !w.will_wake(cx.waker())) {
                state.read.wakers.push(cx.waker().clone());
            }

            // Update interest in this I/O handle.
            if is_empty != (state.read.is_empty(), state.write.is_empty()) {
                Reactor::get().poller.interest(
                    self.raw,
                    Event {
                        key: self.key,
                        readable: !state.read.is_empty(),
                        writable: !state.write.is_empty(),
                    },
                )?;
            }

            // Remember the current ticks.
            if ticks.is_none() {
                ticks = Some((
                    Reactor::get().ticker.load(Ordering::SeqCst),
                    state.read.tick,
                ));
            }

            Poll::Pending
        })
        .await
    }

    /// Registers a waker from `AsyncWrite`.
    ///
    /// If a different waker is already registered, it gets replaced and woken.
    pub(crate) fn register_writer(&self, waker: &Waker) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();

        // If there are no other writers, re-register in the reactor.
        if state.write.is_empty() {
            Reactor::get().poller.interest(
                self.raw,
                Event {
                    key: self.key,
                    readable: !state.read.is_empty(),
                    writable: true,
                },
            )?;
        }

        if let Some(w) = state.write.waker.take() {
            if w.will_wake(waker) {
                state.write.waker = Some(w);
                return Ok(());
            }

            // Don't let a panicking waker blow everything up.
            panic::catch_unwind(|| w.wake()).ok();
        }

        state.write.waker = Some(waker.clone());
        Ok(())
    }

    /// Waits until the I/O source is writable.
    pub(crate) async fn writable(&self) -> io::Result<()> {
        let mut ticks = None;

        future::poll_fn(|cx| {
            let mut state = self.state.lock().unwrap();

            // Check if the reactor has delivered a writability event.
            if let Some((a, b)) = ticks {
                // If `state.write.tick` has changed to a value other than the old reactor tick,
                // that means a newer reactor tick has delivered a writability event.
                if state.write.tick != a && state.write.tick != b {
                    log::trace!("writable: fd={}", self.raw);
                    return Poll::Ready(Ok(()));
                }
            }

            let is_empty = (state.read.is_empty(), state.write.is_empty());

            // Register the current task's waker if not present already.
            if state.write.wakers.iter().all(|w| !w.will_wake(cx.waker())) {
                state.write.wakers.push(cx.waker().clone());
            }

            // Update interest in this I/O handle.
            if is_empty != (state.read.is_empty(), state.write.is_empty()) {
                Reactor::get().poller.interest(
                    self.raw,
                    Event {
                        key: self.key,
                        readable: !state.read.is_empty(),
                        writable: !state.write.is_empty(),
                    },
                )?;
            }

            // Remember the current ticks.
            if ticks.is_none() {
                ticks = Some((
                    Reactor::get().ticker.load(Ordering::SeqCst),
                    state.write.tick,
                ));
            }

            Poll::Pending
        })
        .await
    }
}

/// Runs a closure when dropped.
struct CallOnDrop<F: Fn()>(F);

impl<F: Fn()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
