//! Thread parking and unparking.
//!
//! This module exposes the same API as [`parking`](https://docs.rs/parking). The only difference
//! is that [`Parker`] in this module will wait on epoll/kqueue/wepoll and wake tasks blocked on
//! I/O or timers.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::mem::{self, ManuallyDrop};
use std::net::{Shutdown, TcpStream};
#[cfg(unix)]
use std::os::unix::io::{FromRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{FromRawSocket, RawSocket};
use std::panic;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;
use futures_util::future;
use once_cell::sync::Lazy;
use vec_arena::Arena;

use crate::sys;

static PARKER_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Creates a parker and an associated unparker.
///
/// # Examples
///
/// ```
/// let (p, u) = parking::pair();
/// ```
pub fn pair() -> (Parker, Unparker) {
    let p = Parker::new();
    let u = p.unparker();
    (p, u)
}

/// Parks a thread.
pub struct Parker {
    unparker: Unparker,
}

impl Parker {
    /// Creates a new [`Parker`].
    pub fn new() -> Parker {
        let parker = Parker {
            unparker: Unparker {
                inner: Arc::new(Inner {
                    state: AtomicUsize::new(EMPTY),
                    lock: Mutex::new(()),
                    cvar: Condvar::new(),
                }),
            },
        };
        PARKER_COUNT.fetch_add(1, Ordering::SeqCst);
        parker
    }

    /// Blocks the current thread until the token is made available.
    pub fn park(&self) {
        self.unparker.inner.park(None);
    }

    /// Blocks the current thread until the token is made available or the timeout is reached.
    pub fn park_timeout(&self, timeout: Duration) -> bool {
        self.unparker.inner.park(Some(timeout))
    }

    /// Blocks the current thread until the token is made available or the deadline is reached.
    pub fn park_deadline(&self, deadline: Instant) -> bool {
        self.unparker
            .inner
            .park(Some(deadline.saturating_duration_since(Instant::now())))
    }

    /// Atomically makes the token available if it is not already.
    pub fn unpark(&self) {
        self.unparker.unpark()
    }

    /// Returns a handle for unparking.
    pub fn unparker(&self) -> Unparker {
        self.unparker.clone()
    }
}

impl Drop for Parker {
    fn drop(&mut self) {
        PARKER_COUNT.fetch_sub(1, Ordering::SeqCst);
        Reactor::get().thread_unparker.unpark();
    }
}

impl fmt::Debug for Parker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Parker { .. }")
    }
}

/// Unparks a thread.
pub struct Unparker {
    inner: Arc<Inner>,
}

impl Unparker {
    /// Atomically makes the token available if it is not already.
    pub fn unpark(&self) {
        self.inner.unpark()
    }
}

impl fmt::Debug for Unparker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Unparker { .. }")
    }
}

impl Clone for Unparker {
    fn clone(&self) -> Unparker {
        Unparker {
            inner: self.inner.clone(),
        }
    }
}

const EMPTY: usize = 0;
const PARKED: usize = 1;
const POLLING: usize = 2;
const NOTIFIED: usize = 3;

struct Inner {
    state: AtomicUsize,
    lock: Mutex<()>,
    cvar: Condvar,
}

impl Inner {
    fn park(&self, timeout: Option<Duration>) -> bool {
        // If we were previously notified then we consume this notification and return quickly.
        if self
            .state
            .compare_exchange(NOTIFIED, EMPTY, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // Process available I/O events.
            if let Some(reactor_lock) = Reactor::get().try_lock() {
                let _ = reactor_lock.react(Some(Duration::from_secs(0)));
            }
            return true;
        }

        // If the timeout is zero, then there is no need to actually block.
        if let Some(dur) = timeout {
            if dur == Duration::from_millis(0) {
                // Process available I/O events.
                if let Some(reactor_lock) = Reactor::get().try_lock() {
                    let _ = reactor_lock.react(Some(Duration::from_secs(0)));
                }
                return false;
            }
        }

        // Otherwise, we need to coordinate going to sleep.
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            let reactor_lock = Reactor::get().try_lock();

            let state = match reactor_lock {
                None => PARKED,
                Some(_) => POLLING,
            };
            let mut m = self.lock.lock().unwrap();

            match self
                .state
                .compare_exchange(EMPTY, state, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {}
                // Consume this notification to avoid spurious wakeups in the next park.
                Err(NOTIFIED) => {
                    // We must read `state` here, even though we know it will be `NOTIFIED`. This is
                    // because `unpark` may have been called again since we read `NOTIFIED` in the
                    // `compare_exchange` above. We must perform an acquire operation that synchronizes
                    // with that `unpark` to observe any writes it made before the call to `unpark`. To
                    // do that we must read from the write it made to `state`.
                    let old = self.state.swap(EMPTY, Ordering::SeqCst);
                    assert_eq!(old, NOTIFIED, "park state changed unexpectedly");
                    return true;
                }
                Err(n) => panic!("inconsistent park_timeout state: {}", n),
            }

            match deadline {
                None => {
                    // Block the current thread on the conditional variable.
                    match reactor_lock {
                        None => m = self.cvar.wait(m).unwrap(),
                        Some(reactor_lock) => {
                            drop(m);

                            let _ = reactor_lock.react(None);

                            m = self.lock.lock().unwrap();
                        }
                    }

                    match self.state.swap(EMPTY, Ordering::SeqCst) {
                        NOTIFIED => return true, // got a notification
                        PARKED | POLLING => {}   // spurious wakeup
                        n => panic!("inconsistent state: {}", n),
                    }
                }
                Some(deadline) => {
                    // Wait with a timeout, and if we spuriously wake up or otherwise wake up from a
                    // notification we just want to unconditionally set `state` back to `EMPTY`, either
                    // consuming a notification or un-flagging ourselves as parked.
                    let timeout = deadline.saturating_duration_since(Instant::now());

                    m = match reactor_lock {
                        None => self.cvar.wait_timeout(m, timeout).unwrap().0,
                        Some(reactor_lock) => {
                            drop(m);
                            let _ = reactor_lock.react(Some(timeout));
                            self.lock.lock().unwrap()
                        }
                    };

                    match self.state.swap(EMPTY, Ordering::SeqCst) {
                        NOTIFIED => return true, // got a notification
                        PARKED | POLLING => {}   // no notification
                        n => panic!("inconsistent state: {}", n),
                    }

                    if Instant::now() >= deadline {
                        return false;
                    }
                }
            }

            drop(m);
        }
    }

    pub fn unpark(&self) {
        // To ensure the unparked thread will observe any writes we made before this call, we must
        // perform a release operation that `park` can synchronize with. To do that we must write
        // `NOTIFIED` even if `state` is already `NOTIFIED`. That is why this must be a swap rather
        // than a compare-and-swap that returns if it reads `NOTIFIED` on failure.
        let state = match self.state.swap(NOTIFIED, Ordering::SeqCst) {
            EMPTY => return,    // no one was waiting
            NOTIFIED => return, // already unparked
            state => state,     // gotta go wake someone up
        };

        // There is a period between when the parked thread sets `state` to `PARKED` (or last
        // checked `state` in the case of a spurious wakeup) and when it actually waits on `cvar`.
        // If we were to notify during this period it would be ignored and then when the parked
        // thread went to sleep it would never wake up. Fortunately, it has `lock` locked at this
        // stage so we can acquire `lock` to wait until it is ready to receive the notification.
        //
        // Releasing `lock` before the call to `notify_one` means that when the parked thread wakes
        // it doesn't get woken only to have to wait for us to release `lock`.
        drop(self.lock.lock().unwrap());

        if state == PARKED {
            self.cvar.notify_one();
        } else {
            Reactor::get().notify();
        }
    }
}

/// The reactor.
///
/// Every async I/O handle and every timer is registered here. Invocations of
/// [`run()`][`crate::run()`] poll the reactor to check for new events every now and then.
///
/// There is only one global instance of this type, accessible by [`Reactor::get()`].
pub(crate) struct Reactor {
    thread_unparker: parking::Unparker,

    /// Raw bindings to epoll/kqueue/wepoll.
    sys: sys::Reactor,

    /// Ticker bumped before polling.
    ticker: AtomicUsize,

    /// Registered sources.
    sources: Mutex<Arena<Arc<Source>>>,

    /// Temporary storage for I/O events when polling the reactor.
    events: Mutex<sys::Events>,

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

            thread::Builder::new()
                .name("async-io".to_string())
                .spawn(move || {
                    let reactor = Reactor::get();
                    let mut sleeps = 0u64;
                    let mut last_tick = 0;

                    loop {
                        let tick = reactor.ticker.load(Ordering::SeqCst);

                        if last_tick == tick {
                            let reactor_lock = if sleeps >= 60 {
                                Some(reactor.lock())
                            } else {
                                reactor.try_lock()
                            };

                            if let Some(reactor_lock) = reactor_lock {
                                let _ = reactor_lock.react(None);
                                last_tick = reactor.ticker.load(Ordering::SeqCst);
                            }

                            sleeps = 0;
                        } else {
                            last_tick = tick;
                            sleeps += 1;
                        }

                        if PARKER_COUNT.load(Ordering::SeqCst) == 0 {
                            sleeps = 0;
                        } else {
                            let delay_us = if sleeps < 50 {
                                20
                            } else {
                                20 << (sleeps - 50).min(9)
                            };

                            if parker.park_timeout(Duration::from_micros(delay_us)) {
                                sleeps = 0;
                            }
                        }
                    }
                })
                .expect("cannot spawn async-io thread");

            Reactor {
                thread_unparker: unparker,
                sys: sys::Reactor::new().expect("cannot initialize I/O event notification"),
                ticker: AtomicUsize::new(0),
                sources: Mutex::new(Arena::new()),
                events: Mutex::new(sys::Events::new()),
                timers: Mutex::new(BTreeMap::new()),
                timer_ops: ConcurrentQueue::bounded(1000),
            }
        });
        &REACTOR
    }

    /// Notifies the thread blocked on the reactor.
    pub(crate) fn notify(&self) {
        self.sys.notify().expect("failed to notify reactor");
    }

    /// Registers an I/O source in the reactor.
    pub(crate) fn insert_io(
        &self,
        #[cfg(unix)] raw: RawFd,
        #[cfg(windows)] raw: RawSocket,
    ) -> io::Result<Arc<Source>> {
        let mut sources = self.sources.lock().unwrap();
        let key = sources.next_vacant();

        // Create a source and register it.
        self.sys.insert(raw, key)?;

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
        self.sys.remove(source.raw)
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
    events: MutexGuard<'a, sys::Events>,
}

impl ReactorLock<'_> {
    /// Processes new events, blocking until the first event or the timeout.
    fn react(mut self, timeout: Option<Duration>) -> io::Result<()> {
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

        // Block on I/O events.
        let res = match self.reactor.sys.wait(&mut self.events, timeout) {
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
                            self.reactor.sys.interest(
                                source.raw,
                                source.key,
                                !w.readers.is_empty(),
                                !w.writers.is_empty(),
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
                Reactor::get()
                    .sys
                    .interest(self.raw, self.key, true, !w.writers.is_empty())?;
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
                Reactor::get()
                    .sys
                    .interest(self.raw, self.key, !w.readers.is_empty(), true)?;
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

    /// Shuts down the write side of the socket.
    ///
    /// If this source is not a socket, the `shutdown` syscall error is ignored.
    pub(crate) fn shutdown_write(&self) -> io::Result<()> {
        // This may not be a TCP stream, but that's okay - all we do is call `shutdown()` on it.
        #[cfg(unix)]
        let stream = unsafe { ManuallyDrop::new(TcpStream::from_raw_fd(self.raw)) };
        #[cfg(windows)]
        let stream = unsafe { ManuallyDrop::new(TcpStream::from_raw_socket(self.raw)) };

        // The only actual error may be ENOTCONN.
        match stream.shutdown(Shutdown::Write) {
            Err(err) if err.kind() == io::ErrorKind::NotConnected => Err(err),
            _ => Ok(()),
        }
    }
}
