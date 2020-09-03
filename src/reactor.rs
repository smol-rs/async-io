use std::cell::Cell;
use std::collections::BTreeMap;
use std::io;
use std::mem;
#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawSocket;
use std::panic;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
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

    /// Bindings to epoll/kqueue/event ports/wepoll.
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
                    // If no new ticks have occurred for a while, stop sleeping and
                    // spinning in this loop and just block on the reactor lock.
                    Some(self.lock())
                } else {
                    self.try_lock()
                };

                if let Some(mut reactor_lock) = reactor_lock {
                    log::trace!("main_loop: waiting on I/O");
                    let _ = reactor_lock.react(None);
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
                    // If woken before timeout, reset the last tick and sleep counter.
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
                    let _ = reactor_lock.react(Some(Duration::from_secs(0)));
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
                    let _ = reactor_lock.react(None);

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
                log::trace!("block_on: sleep until notification");
                // Wait for an actual notification.
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
            wakers: Mutex::new(Wakers {
                tick_readable: 0,
                tick_writable: 0,
                readers: Vec::new(),
                writers: Vec::new(),
            }),
            wakers_registered: AtomicU8::new(0),
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
                        let mut w = source.wakers.lock().unwrap();

                        // Wake readers if a readability event was emitted.
                        if ev.readable {
                            w.tick_readable = tick;
                            wakers.append(&mut w.readers);
                            source
                                .wakers_registered
                                .fetch_and(!READERS_REGISTERED, Ordering::SeqCst);
                        }

                        // Wake writers if a writability event was emitted.
                        if ev.writable {
                            w.tick_writable = tick;
                            wakers.append(&mut w.writers);
                            source
                                .wakers_registered
                                .fetch_and(!WRITERS_REGISTERED, Ordering::SeqCst);
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
        log::trace!("react: {} ready wakers", wakers.len());
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

    /// Whether there are wakers interested in events on this source.
    ///
    /// Hold two bits of information: `READERS_REGISTERED` and `WRITERS_REGISTERED`.
    wakers_registered: AtomicU8,
}

const READERS_REGISTERED: u8 = 1 << 0;
const WRITERS_REGISTERED: u8 = 1 << 1;

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
                    log::trace!("readable: fd={}", self.raw);
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
                self.wakers_registered
                    .fetch_or(READERS_REGISTERED, Ordering::SeqCst);
            }

            // Register the current task's waker if not present already.
            if w.readers.iter().all(|w| !w.will_wake(cx.waker())) {
                w.readers.push(cx.waker().clone());
                if limit_waker_list(&mut w.readers) {
                    self.wakers_registered
                        .fetch_and(!READERS_REGISTERED, Ordering::SeqCst);
                }
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

    pub(crate) fn readers_registered(&self) -> bool {
        self.wakers_registered.load(Ordering::SeqCst) & READERS_REGISTERED != 0
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
                    log::trace!("writable: fd={}", self.raw);
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
                self.wakers_registered
                    .fetch_or(WRITERS_REGISTERED, Ordering::SeqCst);
            }

            // Register the current task's waker if not present already.
            if w.writers.iter().all(|w| !w.will_wake(cx.waker())) {
                w.writers.push(cx.waker().clone());
                if limit_waker_list(&mut w.writers) {
                    self.wakers_registered
                        .fetch_and(!WRITERS_REGISTERED, Ordering::SeqCst);
                }
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

    pub(crate) fn writers_registered(&self) -> bool {
        self.wakers_registered.load(Ordering::SeqCst) & WRITERS_REGISTERED != 0
    }
}

/// Wakes up all wakers in the list if it grew too big and returns whether it did.
///
/// The waker list keeps growing in pathological cases where a single async I/O handle has lots of
/// different reader or writer tasks. If the number of interested wakers crosses some threshold, we
/// clear the list and wake all of them at once.
///
/// This strategy prevents memory leaks by bounding the number of stored wakers. However, since all
/// wakers get woken, tasks might simply re-register their interest again, thus creating an
/// infinite loop and burning CPU cycles forever.
///
/// However, we don't worry about such scenarios because it's very unlikely to have more than two
/// actually concurrent tasks operating on a single async I/O handle. If we happen to cross the
/// aforementioned threshold, we have bigger problems to worry about.
fn limit_waker_list(wakers: &mut Vec<Waker>) -> bool {
    if wakers.len() > 50 {
        log::trace!("limit_waker_list: clearing the list");
        for waker in wakers.drain(..) {
            // Don't let a panicking waker blow everything up.
            let _ = panic::catch_unwind(|| waker.wake());
        }
        true
    } else {
        false
    }
}

/// Runs a closure when dropped.
struct CallOnDrop<F: Fn()>(F);

impl<F: Fn()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
