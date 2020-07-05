//! Thread parking and unparking.
//!
//! This module exposes the same API as [`parking`](https://docs.rs/parking). The only difference
//! is that [`Parker`] in this module will wait on epoll/kqueue/wepoll and wake tasks blocked on
//! I/O or timers.

#[cfg(not(any(
    target_os = "linux",     // epoll
    target_os = "android",   // epoll
    target_os = "illumos",   // epoll
    target_os = "macos",     // kqueue
    target_os = "ios",       // kqueue
    target_os = "freebsd",   // kqueue
    target_os = "netbsd",    // kqueue
    target_os = "openbsd",   // kqueue
    target_os = "dragonfly", // kqueue
    target_os = "windows",   // wepoll
)))]
compile_error!("async-io does not support this target OS");

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::mem;
#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawSocket;
use std::panic;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;
use futures_util::future;
use once_cell::sync::Lazy;
use slab::Slab;

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
    sources: Mutex<Slab<Arc<Source>>>,

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
                sources: Mutex::new(Slab::new()),
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
        let vacant = sources.vacant_entry();

        // Create a source and register it.
        let key = vacant.key();
        self.sys.register(raw, key)?;

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
        Ok(vacant.insert(source).clone())
    }

    /// Deregisters an I/O source from the reactor.
    pub(crate) fn remove_io(&self, source: &Source) -> io::Result<()> {
        let mut sources = self.sources.lock().unwrap();
        sources.remove(source.key);
        self.sys.deregister(source.raw)
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
                            self.reactor.sys.reregister(
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
                    .reregister(self.raw, self.key, true, !w.writers.is_empty())?;
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
                    .reregister(self.raw, self.key, !w.readers.is_empty(), true)?;
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

/// Raw bindings to epoll (Linux, Android, illumos).
#[cfg(any(target_os = "linux", target_os = "android", target_os = "illumos"))]
mod sys {
    use std::convert::TryInto;
    use std::io;
    use std::os::unix::io::RawFd;
    use std::ptr;
    use std::time::Duration;

    macro_rules! syscall {
        ($fn:ident $args:tt) => {{
            let res = unsafe { libc::$fn $args };
            if res == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(res)
            }
        }};
    }

    pub struct Reactor {
        epoll_fd: RawFd,
        event_fd: RawFd,
    }
    impl Reactor {
        pub fn new() -> io::Result<Reactor> {
            // According to libuv, `EPOLL_CLOEXEC` is not defined on Android API < 21.
            // But `EPOLL_CLOEXEC` is an alias for `O_CLOEXEC` on that platform, so we use it instead.
            #[cfg(target_os = "android")]
            const CLOEXEC: libc::c_int = libc::O_CLOEXEC;
            #[cfg(not(target_os = "android"))]
            const CLOEXEC: libc::c_int = libc::EPOLL_CLOEXEC;

            let epoll_fd = unsafe {
                // Check if the `epoll_create1` symbol is available on this platform.
                let ptr = libc::dlsym(
                    libc::RTLD_DEFAULT,
                    "epoll_create1\0".as_ptr() as *const libc::c_char,
                );

                if ptr.is_null() {
                    // If not, use `epoll_create` and manually set `CLOEXEC`.
                    let fd = match libc::epoll_create(1024) {
                        -1 => return Err(io::Error::last_os_error()),
                        fd => fd,
                    };
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                    fd
                } else {
                    // Use `epoll_create1` with `CLOEXEC`.
                    let epoll_create1 = std::mem::transmute::<
                        *mut libc::c_void,
                        unsafe extern "C" fn(libc::c_int) -> libc::c_int,
                    >(ptr);
                    match epoll_create1(CLOEXEC) {
                        -1 => return Err(io::Error::last_os_error()),
                        fd => fd,
                    }
                }
            };

            let event_fd = syscall!(eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))?;
            let reactor = Reactor { epoll_fd, event_fd };
            reactor.register(event_fd, !0)?;
            reactor.reregister(event_fd, !0, true, false)?;
            Ok(reactor)
        }
        pub fn register(&self, fd: RawFd, key: usize) -> io::Result<()> {
            let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
            syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
            let mut ev = libc::epoll_event {
                events: 0,
                u64: key as u64,
            };
            syscall!(epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev))?;
            Ok(())
        }
        pub fn reregister(&self, fd: RawFd, key: usize, read: bool, write: bool) -> io::Result<()> {
            let mut flags = libc::EPOLLONESHOT;
            if read {
                flags |= read_flags();
            }
            if write {
                flags |= write_flags();
            }
            let mut ev = libc::epoll_event {
                events: flags as _,
                u64: key as u64,
            };
            syscall!(epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_MOD, fd, &mut ev))?;
            Ok(())
        }
        pub fn deregister(&self, fd: RawFd) -> io::Result<()> {
            syscall!(epoll_ctl(
                self.epoll_fd,
                libc::EPOLL_CTL_DEL,
                fd,
                ptr::null_mut()
            ))?;
            Ok(())
        }
        pub fn wait(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<usize> {
            let timeout_ms = timeout
                .map(|t| {
                    if t == Duration::from_millis(0) {
                        t
                    } else {
                        t.max(Duration::from_millis(1))
                    }
                })
                .and_then(|t| t.as_millis().try_into().ok())
                .unwrap_or(-1);

            let res = syscall!(epoll_wait(
                self.epoll_fd,
                events.list.as_mut_ptr() as *mut libc::epoll_event,
                events.list.len() as libc::c_int,
                timeout_ms as libc::c_int,
            ))?;
            events.len = res as usize;

            let mut buf = [0u8; 8];
            let _ = syscall!(read(
                self.event_fd,
                &mut buf[0] as *mut u8 as *mut libc::c_void,
                buf.len()
            ));
            self.reregister(self.event_fd, !0, true, false)?;

            Ok(events.len)
        }
        pub fn notify(&self) -> io::Result<()> {
            let buf: [u8; 8] = 1u64.to_ne_bytes();
            let _ = syscall!(write(
                self.event_fd,
                &buf[0] as *const u8 as *const libc::c_void,
                buf.len()
            ));
            Ok(())
        }
    }
    fn read_flags() -> libc::c_int {
        libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLHUP | libc::EPOLLERR | libc::EPOLLPRI
    }
    fn write_flags() -> libc::c_int {
        libc::EPOLLOUT | libc::EPOLLHUP | libc::EPOLLERR
    }

    pub struct Events {
        list: Box<[libc::epoll_event]>,
        len: usize,
    }
    impl Events {
        pub fn new() -> Events {
            let ev = libc::epoll_event { events: 0, u64: 0 };
            let list = vec![ev; 1000].into_boxed_slice();
            let len = 0;
            Events { list, len }
        }
        pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
            self.list[..self.len].iter().map(|ev| Event {
                readable: (ev.events as libc::c_int & read_flags()) != 0,
                writable: (ev.events as libc::c_int & write_flags()) != 0,
                key: ev.u64 as usize,
            })
        }
    }
    pub struct Event {
        pub readable: bool,
        pub writable: bool,
        pub key: usize,
    }
}

/// Raw bindings to kqueue (macOS, iOS, FreeBSD, NetBSD, OpenBSD, DragonFly BSD).
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
mod sys {
    use std::io::{self, Read, Write};
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::ptr;
    use std::time::Duration;

    macro_rules! syscall {
        ($fn:ident $args:tt) => {{
            let res = unsafe { libc::$fn $args };
            if res == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(res)
            }
        }};
    }

    pub struct Reactor {
        kqueue_fd: RawFd,
        read_stream: UnixStream,
        write_stream: UnixStream,
    }
    impl Reactor {
        pub fn new() -> io::Result<Reactor> {
            let kqueue_fd = syscall!(kqueue())?;
            syscall!(fcntl(kqueue_fd, libc::F_SETFD, libc::FD_CLOEXEC))?;
            let (read_stream, write_stream) = UnixStream::pair()?;
            read_stream.set_nonblocking(true)?;
            write_stream.set_nonblocking(true)?;
            let reactor = Reactor {
                kqueue_fd,
                read_stream,
                write_stream,
            };
            reactor.reregister(reactor.read_stream.as_raw_fd(), !0, true, false)?;
            Ok(reactor)
        }
        pub fn register(&self, fd: RawFd, _key: usize) -> io::Result<()> {
            let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
            syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
            Ok(())
        }
        pub fn reregister(&self, fd: RawFd, key: usize, read: bool, write: bool) -> io::Result<()> {
            let mut read_flags = libc::EV_ONESHOT | libc::EV_RECEIPT;
            let mut write_flags = libc::EV_ONESHOT | libc::EV_RECEIPT;
            if read {
                read_flags |= libc::EV_ADD;
            } else {
                read_flags |= libc::EV_DELETE;
            }
            if write {
                write_flags |= libc::EV_ADD;
            } else {
                write_flags |= libc::EV_DELETE;
            }
            let changelist = [
                libc::kevent {
                    ident: fd as _,
                    filter: libc::EVFILT_READ,
                    flags: read_flags,
                    fflags: 0,
                    data: 0,
                    udata: key as _,
                },
                libc::kevent {
                    ident: fd as _,
                    filter: libc::EVFILT_WRITE,
                    flags: write_flags,
                    fflags: 0,
                    data: 0,
                    udata: key as _,
                },
            ];
            let mut eventlist = changelist;
            syscall!(kevent(
                self.kqueue_fd,
                changelist.as_ptr() as *const libc::kevent,
                changelist.len() as _,
                eventlist.as_mut_ptr() as *mut libc::kevent,
                eventlist.len() as _,
                ptr::null(),
            ))?;
            for ev in &eventlist {
                // Explanation for ignoring EPIPE: https://github.com/tokio-rs/mio/issues/582
                if (ev.flags & libc::EV_ERROR) != 0
                    && ev.data != 0
                    && ev.data != libc::ENOENT as _
                    && ev.data != libc::EPIPE as _
                {
                    return Err(io::Error::from_raw_os_error(ev.data as _));
                }
            }
            Ok(())
        }
        pub fn deregister(&self, fd: RawFd) -> io::Result<()> {
            let flags = libc::EV_DELETE | libc::EV_RECEIPT;
            let changelist = [
                libc::kevent {
                    ident: fd as _,
                    filter: libc::EVFILT_READ,
                    flags: flags,
                    fflags: 0,
                    data: 0,
                    udata: 0 as _,
                },
                libc::kevent {
                    ident: fd as _,
                    filter: libc::EVFILT_WRITE,
                    flags: flags,
                    fflags: 0,
                    data: 0,
                    udata: 0 as _,
                },
            ];
            let mut eventlist = changelist;
            syscall!(kevent(
                self.kqueue_fd,
                changelist.as_ptr() as *const libc::kevent,
                changelist.len() as _,
                eventlist.as_mut_ptr() as *mut libc::kevent,
                eventlist.len() as _,
                ptr::null(),
            ))?;
            for ev in &eventlist {
                if (ev.flags & libc::EV_ERROR) != 0 && ev.data != 0 && ev.data != libc::ENOENT as _
                {
                    return Err(io::Error::from_raw_os_error(ev.data as _));
                }
            }
            Ok(())
        }
        pub fn wait(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<usize> {
            let timeout = timeout.map(|t| libc::timespec {
                tv_sec: t.as_secs() as libc::time_t,
                tv_nsec: t.subsec_nanos() as libc::c_long,
            });
            let changelist = [];
            let eventlist = &mut events.list;
            let res = syscall!(kevent(
                self.kqueue_fd,
                changelist.as_ptr() as *const libc::kevent,
                changelist.len() as _,
                eventlist.as_mut_ptr() as *mut libc::kevent,
                eventlist.len() as _,
                match &timeout {
                    None => ptr::null(),
                    Some(t) => t,
                }
            ))?;
            events.len = res as usize;

            while (&self.read_stream).read(&mut [0; 64]).is_ok() {}
            self.reregister(self.read_stream.as_raw_fd(), !0, true, false)?;

            Ok(events.len)
        }
        pub fn notify(&self) -> io::Result<()> {
            let _ = (&self.write_stream).write(&[1]);
            Ok(())
        }
    }

    pub struct Events {
        list: Box<[libc::kevent]>,
        len: usize,
    }
    impl Events {
        pub fn new() -> Events {
            let event = libc::kevent {
                ident: 0 as _,
                filter: 0,
                flags: 0,
                fflags: 0,
                data: 0,
                udata: 0 as _,
            };
            let list = vec![event; 1000].into_boxed_slice();
            let len = 0;
            Events { list, len }
        }
        pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
            // On some platforms, closing the read end of a pipe wakes up writers, but the
            // event is reported as EVFILT_READ with the EV_EOF flag.
            //
            // https://github.com/golang/go/commit/23aad448b1e3f7c3b4ba2af90120bde91ac865b4
            self.list[..self.len].iter().map(|ev| Event {
                readable: ev.filter == libc::EVFILT_READ,
                writable: ev.filter == libc::EVFILT_WRITE
                    || (ev.filter == libc::EVFILT_READ && (ev.flags & libc::EV_EOF) != 0),
                key: ev.udata as usize,
            })
        }
    }
    unsafe impl Send for Events {}
    pub struct Event {
        pub readable: bool,
        pub writable: bool,
        pub key: usize,
    }
}

/// Raw bindings to wepoll (Windows).
#[cfg(target_os = "windows")]
mod sys {
    use std::convert::TryInto;
    use std::io;
    use std::os::windows::io::{AsRawSocket, RawSocket};
    use std::ptr;
    use std::time::Duration;

    use wepoll_sys_stjepang as we;
    use winapi::um::winsock2;

    macro_rules! syscall {
        ($fn:ident $args:tt) => {{
            let res = unsafe { we::$fn $args };
            if res == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(res)
            }
        }};
    }

    pub struct Reactor {
        handle: we::HANDLE,
    }
    unsafe impl Send for Reactor {}
    unsafe impl Sync for Reactor {}
    impl Reactor {
        pub fn new() -> io::Result<Reactor> {
            let handle = unsafe { we::epoll_create1(0) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Reactor { handle })
        }
        pub fn register(&self, sock: RawSocket, key: usize) -> io::Result<()> {
            unsafe {
                let mut nonblocking = true as libc::c_ulong;
                let res = winsock2::ioctlsocket(
                    sock as winsock2::SOCKET,
                    winsock2::FIONBIO,
                    &mut nonblocking,
                );
                if res != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            let mut ev = we::epoll_event {
                events: 0,
                data: we::epoll_data { u64: key as u64 },
            };
            syscall!(epoll_ctl(
                self.handle,
                we::EPOLL_CTL_ADD as libc::c_int,
                sock as we::SOCKET,
                &mut ev,
            ))?;
            Ok(())
        }
        pub fn reregister(
            &self,
            sock: RawSocket,
            key: usize,
            read: bool,
            write: bool,
        ) -> io::Result<()> {
            let mut flags = we::EPOLLONESHOT;
            if read {
                flags |= READ_FLAGS;
            }
            if write {
                flags |= WRITE_FLAGS;
            }
            let mut ev = we::epoll_event {
                events: flags as u32,
                data: we::epoll_data { u64: key as u64 },
            };
            syscall!(epoll_ctl(
                self.handle,
                we::EPOLL_CTL_MOD as libc::c_int,
                sock as we::SOCKET,
                &mut ev,
            ))?;
            Ok(())
        }
        pub fn deregister(&self, sock: RawSocket) -> io::Result<()> {
            syscall!(epoll_ctl(
                self.handle,
                we::EPOLL_CTL_DEL as libc::c_int,
                sock as we::SOCKET,
                ptr::null_mut(),
            ))?;
            Ok(())
        }
        pub fn wait(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<usize> {
            let timeout_ms = match timeout {
                None => -1,
                Some(t) => {
                    if t == Duration::from_millis(0) {
                        0
                    } else {
                        t.max(Duration::from_millis(1))
                            .as_millis()
                            .try_into()
                            .unwrap_or(libc::c_int::max_value())
                    }
                }
            };
            events.len = syscall!(epoll_wait(
                self.handle,
                events.list.as_mut_ptr(),
                events.list.len() as libc::c_int,
                timeout_ms,
            ))? as usize;
            Ok(events.len)
        }
        pub fn notify(&self) -> io::Result<()> {
            unsafe {
                // This errors if a notification has already been posted, but that's okay.
                winapi::um::ioapiset::PostQueuedCompletionStatus(
                    self.handle as winapi::um::winnt::HANDLE,
                    0,
                    0,
                    ptr::null_mut(),
                );
            }
            Ok(())
        }
    }
    struct As(RawSocket);
    impl AsRawSocket for As {
        fn as_raw_socket(&self) -> RawSocket {
            self.0
        }
    }
    const READ_FLAGS: u32 =
        we::EPOLLIN | we::EPOLLRDHUP | we::EPOLLHUP | we::EPOLLERR | we::EPOLLPRI;
    const WRITE_FLAGS: u32 = we::EPOLLOUT | we::EPOLLHUP | we::EPOLLERR;

    pub struct Events {
        list: Box<[we::epoll_event]>,
        len: usize,
    }
    unsafe impl Send for Events {}
    unsafe impl Sync for Events {}
    impl Events {
        pub fn new() -> Events {
            let ev = we::epoll_event {
                events: 0,
                data: we::epoll_data { u64: 0 },
            };
            Events {
                list: vec![ev; 1000].into_boxed_slice(),
                len: 0,
            }
        }
        pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
            self.list[..self.len].iter().map(|ev| Event {
                readable: (ev.events & READ_FLAGS) != 0,
                writable: (ev.events & WRITE_FLAGS) != 0,
                key: unsafe { ev.data.u64 } as usize,
            })
        }
    }
    pub struct Event {
        pub readable: bool,
        pub writable: bool,
        pub key: usize,
    }
}
