use std::borrow::Borrow;
use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawSocket;
use std::panic;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use futures_lite::ready;
use once_cell::sync::Lazy;
use slab::Slab;

mod poll;

cfg_if::cfg_if! {
    if #[cfg(target_os = "linux")] {
        mod uring;
        use uring as imp;
    } else {
        use poll as imp;
    }
}

const READ: usize = 0;
const WRITE: usize = 1;

const MAX_OPERATIONS: usize = 2;
const READ_OP: usize = 0;
const WRITE_OP: usize = 1;

/// The reactor.
///
/// There is only one global instance of this type, accessible by [`Reactor::get()`].
pub(crate) struct Reactor(imp::Reactor);

impl Reactor {
    /// Returns a reference to the reactor.
    pub(crate) fn get() -> &'static Reactor {
        static REACTOR: Lazy<Reactor> = Lazy::new(|| {
            crate::driver::init();
            Reactor(imp::Reactor::new())
        });
        &REACTOR
    }

    /// Returns the current ticker.
    pub(crate) fn ticker(&self) -> usize {
        self.0.ticker()
    }

    /// Registers an I/O source in the reactor.
    pub(crate) fn insert_io(
        &self,
        #[cfg(unix)] raw: RawFd,
        #[cfg(windows)] raw: RawSocket,
    ) -> io::Result<Arc<Source>> {
        self.0.insert_io(raw)
    }

    /// Deregisters an I/O source from the reactor.
    pub(crate) fn remove_io(&self, source: &Source) -> io::Result<()> {
        self.0.remove_io(source)
    }

    /// Registers a timer in the reactor.
    ///
    /// Returns the inserted timer's ID.
    pub(crate) fn insert_timer(&self, when: Instant, waker: &Waker) -> usize {
        self.0.insert_timer(when, waker)
    }

    /// Deregisters a timer from the reactor.
    pub(crate) fn remove_timer(&self, when: Instant, id: usize) {
        self.0.remove_timer(when, id)
    }

    /// Notifies the thread blocked on the reactor.
    pub(crate) fn notify(&self) {
        self.0.notify()
    }

    /// Try to read from the given source.
    ///
    /// If the read is not successful, it is registered in the
    /// reactor. The source is then notified when the read is
    /// successful.
    pub(crate) fn poll_read(
        &self,
        readable: &mut impl Read,
        source: &Source,
        buf: &mut [u8],
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<usize>> {
        self.0.poll_read(readable, source, buf, cx)
    }

    /// Try to write to the given source.
    ///
    /// If the write is not successful, it is registered in the
    /// reactor. In certain cases this can take advantage of
    /// completion-based APIs in order to be faster than normal.
    pub(crate) fn poll_write(
        &self,
        writable: &mut impl Write,
        source: &Source,
        buf: &[u8],
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<usize>> {
        self.0.poll_write(writable, source, buf, cx)
    }

    /// Locks the reactor, potentially blocking if the lock is held by another thread.
    pub(crate) fn lock(&self) -> ReactorLock<'_> {
        ReactorLock(self.0.lock())
    }

    /// Attempts to lock the reactor.
    pub(crate) fn try_lock(&self) -> Option<ReactorLock<'_>> {
        self.0.try_lock().map(ReactorLock)
    }

    /// Get the `Poller` backing this reactor.
    pub(crate) fn poller(&self) -> &polling::Poller {
        self.0.poller()
    }
}

/// A lock on the reactor.
pub(crate) struct ReactorLock<'a>(imp::ReactorLock<'a>);

impl ReactorLock<'_> {
    /// Processes new events, blocking until the first event or the timeout.
    pub(crate) fn react(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.react(timeout)
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

/// The inner state of a `Source`, with registered wakers.
#[derive(Debug, Default)]
struct State {
    /// The directions used for polling read/write readiness.
    readiness: [Direction; 2],
    /// The ongoing operations for completion-based I/O.
    operations: [Operation; MAX_OPERATIONS],
}

/// A read or write direction.
#[derive(Debug, Default)]
struct Direction {
    /// Last reactor tick that delivered an event.
    tick: usize,

    /// Ticks remembered by `Async::poll_readable()` or `Async::poll_writable()`.
    ticks: Option<(usize, usize)>,

    /// Waker stored by `Async::poll_readable()` or `Async::poll_writable()`.
    waker: Option<Waker>,

    /// Wakers of tasks waiting for the next event.
    ///
    /// Registered by `Async::readable()` and `Async::writable()`.
    wakers: Slab<Option<Waker>>,
}

impl Direction {
    /// Returns `true` if there are no wakers interested in this direction.
    fn is_empty(&self) -> bool {
        self.waker.is_none() && self.wakers.iter().all(|(_, opt)| opt.is_none())
    }

    /// Moves all wakers into a `Vec`.
    fn drain_into(&mut self, dst: &mut Vec<Waker>) {
        if let Some(w) = self.waker.take() {
            dst.push(w);
        }
        for (_, opt) in self.wakers.iter_mut() {
            if let Some(w) = opt.take() {
                dst.push(w);
            }
        }
    }
}

/// An operation for completion-based I/O.
#[derive(Debug, Default)]
struct Operation {
    /// The current status of the operation.
    status: OperationStatus,
    /// The waker registered for this operation.
    waker: Option<Waker>,
    /// The buffer for this operation.
    ///
    /// Once the completion operation begins, logically this buffer
    /// is no longer owned by the `Operation` but by the system API
    /// (e.g. `io_uring`). In order to assert to the type system this
    /// condition, we use an `UnsafeCell` to hold the buffer.
    ///
    /// # Invariants
    ///
    /// When `status` is `Pending`, this buffer is "owned"
    /// by the system, and it is unsound to access it.
    /// When `status` is something else, it can be accessed safely.
    buffer: UnsafeCell<Vec<MaybeUninit<u8>>>,
}

/// The status of an `Operation`.
#[derive(Debug)]
enum OperationStatus {
    /// The operation has not yet begun.
    NotStarted,
    /// The operation is currently running.
    Pending,
    /// The operation has completed with the given result.
    Complete(isize),
}

impl Default for OperationStatus {
    fn default() -> Self {
        OperationStatus::NotStarted
    }
}

impl Source {
    /// Polls the I/O source for readability.
    pub(crate) fn poll_readable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_ready(READ, cx)
    }

    /// Polls the I/O source for writability.
    pub(crate) fn poll_writable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_ready(WRITE, cx)
    }

    /// Registers a waker from `poll_readable()` or `poll_writable()`.
    ///
    /// If a different waker is already registered, it gets replaced and woken.
    fn poll_ready(&self, dir: usize, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut lock = self.state.lock().unwrap();
        let state = &mut lock.readiness;

        // Check if the reactor has delivered an event.
        if let Some((a, b)) = state[dir].ticks {
            // If `state[dir].tick` has changed to a value other than the old reactor tick,
            // that means a newer reactor tick has delivered an event.
            if state[dir].tick != a && state[dir].tick != b {
                state[dir].ticks = None;
                return Poll::Ready(Ok(()));
            }
        }

        let was_empty = state[dir].is_empty();

        // Register the current task's waker.
        if let Some(w) = state[dir].waker.take() {
            if w.will_wake(cx.waker()) {
                state[dir].waker = Some(w);
                return Poll::Pending;
            }
            // Wake the previous waker because it's going to get replaced.
            panic::catch_unwind(|| w.wake()).ok();
        }
        state[dir].waker = Some(cx.waker().clone());
        state[dir].ticks = Some((Reactor::get().ticker(), state[dir].tick));

        // Update interest in this I/O handle.
        if was_empty {
            Reactor::get().poller().modify(
                self.raw,
                polling::Event {
                    key: self.key,
                    readable: !state[READ].is_empty(),
                    writable: !state[WRITE].is_empty(),
                },
            )?;
        }

        Poll::Pending
    }

    /// Waits until the I/O source is readable.
    pub(crate) fn readable<T>(handle: &crate::Async<T>) -> Readable<'_, T> {
        Readable(Self::ready(handle, READ))
    }

    /// Waits until the I/O source is readable.
    pub(crate) fn readable_owned<T>(handle: Arc<crate::Async<T>>) -> ReadableOwned<T> {
        ReadableOwned(Self::ready(handle, READ))
    }

    /// Waits until the I/O source is writable.
    pub(crate) fn writable<T>(handle: &crate::Async<T>) -> Writable<'_, T> {
        Writable(Self::ready(handle, WRITE))
    }

    /// Waits until the I/O source is writable.
    pub(crate) fn writable_owned<T>(handle: Arc<crate::Async<T>>) -> WritableOwned<T> {
        WritableOwned(Self::ready(handle, WRITE))
    }

    /// Waits until the I/O source is readable or writable.
    fn ready<H: Borrow<crate::Async<T>> + Clone, T>(handle: H, dir: usize) -> Ready<H, T> {
        Ready {
            handle,
            dir,
            ticks: None,
            index: None,
            _guard: None,
        }
    }
}

/// Future for [`Async::readable`](crate::Async::readable).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Readable<'a, T>(Ready<&'a crate::Async<T>, T>);

impl<T> Future for Readable<'_, T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(Pin::new(&mut self.0).poll(cx))?;
        log::trace!("readable: fd={}", self.0.handle.source.raw);
        Poll::Ready(Ok(()))
    }
}

impl<T> fmt::Debug for Readable<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Readable").finish()
    }
}

/// Future for [`Async::readable_owned`](crate::Async::readable_owned).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ReadableOwned<T>(Ready<Arc<crate::Async<T>>, T>);

impl<T> Future for ReadableOwned<T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(Pin::new(&mut self.0).poll(cx))?;
        log::trace!("readable_owned: fd={}", self.0.handle.source.raw);
        Poll::Ready(Ok(()))
    }
}

impl<T> fmt::Debug for ReadableOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadableOwned").finish()
    }
}

/// Future for [`Async::writable`](crate::Async::writable).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Writable<'a, T>(Ready<&'a crate::Async<T>, T>);

impl<T> Future for Writable<'_, T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(Pin::new(&mut self.0).poll(cx))?;
        log::trace!("writable: fd={}", self.0.handle.source.raw);
        Poll::Ready(Ok(()))
    }
}

impl<T> fmt::Debug for Writable<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writable").finish()
    }
}

/// Future for [`Async::writable_owned`](crate::Async::writable_owned).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct WritableOwned<T>(Ready<Arc<crate::Async<T>>, T>);

impl<T> Future for WritableOwned<T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(Pin::new(&mut self.0).poll(cx))?;
        log::trace!("writable_owned: fd={}", self.0.handle.source.raw);
        Poll::Ready(Ok(()))
    }
}

impl<T> fmt::Debug for WritableOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WritableOwned").finish()
    }
}

struct Ready<H: Borrow<crate::Async<T>>, T> {
    handle: H,
    dir: usize,
    ticks: Option<(usize, usize)>,
    index: Option<usize>,
    _guard: Option<RemoveOnDrop<H, T>>,
}

impl<H: Borrow<crate::Async<T>>, T> Unpin for Ready<H, T> {}

impl<H: Borrow<crate::Async<T>> + Clone, T> Future for Ready<H, T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self {
            ref handle,
            dir,
            ticks,
            index,
            _guard,
            ..
        } = &mut *self;

        let mut lock = handle.borrow().source.state.lock().unwrap();
        let state = &mut lock.readiness;

        // Check if the reactor has delivered an event.
        if let Some((a, b)) = *ticks {
            // If `state[dir].tick` has changed to a value other than the old reactor tick,
            // that means a newer reactor tick has delivered an event.
            if state[*dir].tick != a && state[*dir].tick != b {
                return Poll::Ready(Ok(()));
            }
        }

        let was_empty = state[*dir].is_empty();

        // Register the current task's waker.
        let i = match *index {
            Some(i) => i,
            None => {
                let i = state[*dir].wakers.insert(None);
                *_guard = Some(RemoveOnDrop {
                    handle: handle.clone(),
                    dir: *dir,
                    key: i,
                    _marker: PhantomData,
                });
                *index = Some(i);
                *ticks = Some((Reactor::get().ticker(), state[*dir].tick));
                i
            }
        };
        state[*dir].wakers[i] = Some(cx.waker().clone());

        // Update interest in this I/O handle.
        if was_empty {
            Reactor::get().poller().modify(
                handle.borrow().source.raw,
                polling::Event {
                    key: handle.borrow().source.key,
                    readable: !state[READ].is_empty(),
                    writable: !state[WRITE].is_empty(),
                },
            )?;
        }

        Poll::Pending
    }
}

/// Remove waker when dropped.
struct RemoveOnDrop<H: Borrow<crate::Async<T>>, T> {
    handle: H,
    dir: usize,
    key: usize,
    _marker: PhantomData<fn() -> T>,
}

impl<H: Borrow<crate::Async<T>>, T> Drop for RemoveOnDrop<H, T> {
    fn drop(&mut self) {
        let mut state = self.handle.borrow().source.state.lock().unwrap();
        let wakers = &mut state.readiness[self.dir].wakers;
        if wakers.contains(self.key) {
            wakers.remove(self.key);
        }
    }
}
