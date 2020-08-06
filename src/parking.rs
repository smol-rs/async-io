//! Thread parking and unparking.
//!
//! This module exposes the exact same API as the [`parking`][docs-parking] crate. The only
//! difference is that [`Parker`] in this module will wait on epoll/kqueue/wepoll and wake futures
//! blocked on I/O or timers instead of *just* sleeping.
//!
//! Executors may use this mechanism to go to sleep when idle and wake up when more work is
//! scheduled. The benefit is in that when going to sleep using [`Parker`], futures blocked on I/O
//! or timers will often be woken and polled by the same executor thread. This is sometimes a
//! significant optimization because no context switch is needed between waiting on I/O and polling
//! futures.
//!
//! [docs-parking]: https://docs.rs/parking
//!
//! # Examples
//!
//! A simple `block_on()` that runs a single future and waits on I/O when the future is idle:
//!
//! ```
//! use std::future::Future;
//! use std::task::{Context, Poll};
//!
//! use async_io::parking;
//! use futures_lite::{future, pin};
//! use waker_fn::waker_fn;
//!
//! // Blocks on a future to complete, processing I/O events when idle.
//! fn block_on<T>(future: impl Future<Output = T>) -> T {
//!     let (p, u) = parking::pair();
//!     let waker = waker_fn(move || u.unpark());
//!     let cx = &mut Context::from_waker(&waker);
//!
//!     pin!(future);
//!     loop {
//!         match future.as_mut().poll(cx) {
//!             Poll::Ready(t) => return t,
//!             Poll::Pending => {
//!                 // Wait until unparked, processing I/O events in the meantime.
//!                 p.park();
//!             }
//!         }
//!     }
//! }
//!
//! block_on(async {
//!     println!("Hello world!");
//!     future::yield_now().await;
//!     println!("Hello again!");
//! });
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::reactor::Reactor;

/// Creates a parker and an associated unparker.
///
/// # Examples
///
/// ```
/// use async_io::parking;
///
/// let (p, u) = parking::pair();
/// ```
pub fn pair() -> (Parker, Unparker) {
    let p = Parker::new();
    let u = p.unparker();
    (p, u)
}

/// Waits for a notification.
#[derive(Debug)]
pub struct Parker {
    /// The inner parker implementation.
    inner: parking::Parker,

    /// Set to `true` when the parker is polling I/O.
    io: Arc<AtomicBool>,
}

impl Parker {
    /// Creates a new parker.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::parking::Parker;
    ///
    /// let p = Parker::new();
    /// ```
    ///
    pub fn new() -> Parker {
        let inner = parking::Parker::new();
        let io = Arc::new(AtomicBool::new(false));
        Reactor::get().increment_parkers();
        Parker { inner, io }
    }

    /// Blocks until notified and then goes back into unnotified state.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::parking::Parker;
    ///
    /// let p = Parker::new();
    /// let u = p.unparker();
    ///
    /// // Notify the parker.
    /// u.unpark();
    ///
    /// // Wakes up immediately because the parker is notified.
    /// p.park();
    /// ```
    pub fn park(&self) {
        self.park_inner(None);
    }

    /// Blocks until notified and then goes back into unnotified state, or times out after
    /// `duration`.
    ///
    /// Returns `true` if notified before the timeout.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::parking::Parker;
    /// use std::time::Duration;
    ///
    /// let p = Parker::new();
    ///
    /// // Wait for a notification, or time out after 500 ms.
    /// p.park_timeout(Duration::from_millis(500));
    /// ```
    pub fn park_timeout(&self, timeout: Duration) -> bool {
        self.park_inner(Some(timeout))
    }

    /// Blocks until notified and then goes back into unnotified state, or times out at `instant`.
    ///
    /// Returns `true` if notified before the deadline.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::parking::Parker;
    /// use std::time::{Duration, Instant};
    ///
    /// let p = Parker::new();
    ///
    /// // Wait for a notification, or time out after 500 ms.
    /// p.park_deadline(Instant::now() + Duration::from_millis(500));
    /// ```
    pub fn park_deadline(&self, deadline: Instant) -> bool {
        self.park_inner(Some(deadline.saturating_duration_since(Instant::now())))
    }

    /// Notifies the parker.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::parking::Parker;
    /// use std::thread;
    /// use std::time::Duration;
    ///
    /// let p = Parker::new();
    /// let u = p.unparker();
    ///
    /// thread::spawn(move || {
    ///     thread::sleep(Duration::from_millis(500));
    ///     u.unpark();
    /// });
    ///
    /// // Wakes up when `u.unpark()` notifies and then goes back into unnotified state.
    /// p.park();
    /// ```
    pub fn unpark(&self) {
        if self.inner.unpark() && self.io.load(Ordering::SeqCst) {
            Reactor::get().notify();
        }
    }

    /// Returns a handle for unparking.
    ///
    /// The returned [`Unparker`] can be cloned and shared among threads.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::parking::Parker;
    ///
    /// let p = Parker::new();
    /// let u = p.unparker();
    ///
    /// // Notify the parker.
    /// u.unpark();
    ///
    /// // Wakes up immediately because the parker is notified.
    /// p.park();
    /// ```
    pub fn unparker(&self) -> Unparker {
        Unparker {
            inner: self.inner.unparker(),
            io: self.io.clone(),
        }
    }

    fn park_inner(&self, timeout: Option<Duration>) -> bool {
        // If we were previously notified then we consume this notification and return quickly.
        if self.inner.park_timeout(Duration::from_secs(0)) {
            // Process available I/O events.
            if let Some(reactor_lock) = Reactor::get().try_lock() {
                let _ = reactor_lock.react(Some(Duration::from_secs(0)));
            }
            return true;
        }

        // If the timeout is zero, then there is no need to actually block.
        if let Some(dur) = timeout {
            if dur == Duration::from_secs(0) {
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
            // Attempt grabbing a lock on the reactor.
            match Reactor::get().try_lock() {
                None => {
                    if let Some(deadline) = deadline {
                        // Wait for a notification or timeout.
                        return self.inner.park_deadline(deadline);
                    } else {
                        // Wait for a notification.
                        self.inner.park();
                        return true;
                    }
                }
                Some(reactor_lock) => {
                    // First let others know this parker is waiting on I/O.
                    self.io.store(true, Ordering::SeqCst);

                    // Check if a notification was received.
                    if self.inner.park_timeout(Duration::from_secs(0)) {
                        self.io.store(false, Ordering::SeqCst);
                        return true;
                    }

                    // Wait for I/O events.
                    let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
                    let _ = reactor_lock.react(timeout);
                    self.io.store(false, Ordering::SeqCst);

                    // Check if a notification was received.
                    if self.inner.park_timeout(Duration::from_secs(0)) {
                        return true;
                    }

                    // Check for timeout.
                    if let Some(deadline) = deadline {
                        if Instant::now() >= deadline {
                            return false;
                        }
                    }
                }
            }
        }
    }
}

impl Drop for Parker {
    fn drop(&mut self) {
        Reactor::get().decrement_parkers();
    }
}

impl Default for Parker {
    fn default() -> Parker {
        Parker::new()
    }
}

/// Notifies a parker.
#[derive(Clone, Debug)]
pub struct Unparker {
    /// The inner unparker implementation.
    inner: parking::Unparker,

    /// Set to `true` when the parker is polling I/O.
    io: Arc<AtomicBool>,
}

impl Unparker {
    /// Notifies the associated parker.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::parking::Parker;
    /// use std::thread;
    /// use std::time::Duration;
    ///
    /// let p = Parker::new();
    /// let u = p.unparker();
    ///
    /// thread::spawn(move || {
    ///     thread::sleep(Duration::from_millis(500));
    ///     u.unpark();
    /// });
    ///
    /// // Wakes up when `u.unpark()` notifies and then goes back into unnotified state.
    /// p.park();
    /// ```
    pub fn unpark(&self) {
        if self.inner.unpark() && self.io.load(Ordering::SeqCst) {
            Reactor::get().notify();
        }
    }
}
