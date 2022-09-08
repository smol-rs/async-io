use std::io::Result;
#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawSocket;
#[cfg(not(target_family = "wasm"))]
use std::sync::Arc;

use std::task::Waker;
use std::time::Instant;

use once_cell::sync::Lazy;
use std::time::Duration;

mod timer;

cfg_if::cfg_if! {
    if #[cfg(target_family = "wasm")] {
        use timer as sys;
    } else {
        mod io;
        use io as sys;

        pub use io::{Readable, ReadableOwned, Writable, WritableOwned};
        pub(crate) use io::Source;
    }
}

/// The reactor.
///
/// There is only one global instance of this type, accessible by [`Reactor::get()`].
pub(crate) struct Reactor(sys::Reactor);

impl Reactor {
    /// Returns a reference to the reactor.
    pub(crate) fn get() -> &'static Reactor {
        static REACTOR: Lazy<Reactor> = Lazy::new(|| {
            crate::driver::init();
            Reactor(sys::Reactor::new())
        });
        &REACTOR
    }

    /// Returns the current ticker.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn ticker(&self) -> usize {
        self.0.ticker()
    }

    /// Registers an I/O source in the reactor.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn insert_io(
        &self,
        #[cfg(unix)] raw: RawFd,
        #[cfg(windows)] raw: RawSocket,
    ) -> Result<Arc<Source>> {
        self.0.insert_io(raw)
    }

    /// Deregisters an I/O source from the reactor.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn remove_io(&self, source: &Source) -> Result<()> {
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

    /// Locks the reactor, potentially blocking if the lock is held by another thread.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn lock(&self) -> ReactorLock<'_> {
        ReactorLock(self.0.lock())
    }

    /// Attempts to lock the reactor.
    pub(crate) fn try_lock(&self) -> Option<ReactorLock<'_>> {
        self.0.try_lock().map(ReactorLock)
    }
}

/// A lock on the reactor.
pub(crate) struct ReactorLock<'a>(sys::ReactorLock<'a>);

impl ReactorLock<'_> {
    /// Processes new events, blocking until the first event or the timeout.
    pub(crate) fn react(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.0.react(timeout)
    }
}
