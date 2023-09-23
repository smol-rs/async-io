//! Async I/O and timers.
//!
//! This crate provides two tools:
//!
//! * [`Async`], an adapter for standard networking types (and [many other] types) to use in
//!   async programs.
//! * [`Timer`], a future or stream that emits timed events.
//!
//! For concrete async networking types built on top of this crate, see [`async-net`].
//!
//! [many other]: https://github.com/smol-rs/async-io/tree/master/examples
//! [`async-net`]: https://docs.rs/async-net
//!
//! # Implementation
//!
//! The first time [`Async`] or [`Timer`] is used, a thread named "async-io" will be spawned.
//! The purpose of this thread is to wait for I/O events reported by the operating system, and then
//! wake appropriate futures blocked on I/O or timers when they can be resumed.
//!
//! To wait for the next I/O event, the "async-io" thread uses [epoll] on Linux/Android/illumos,
//! [kqueue] on macOS/iOS/BSD, [event ports] on illumos/Solaris, and [IOCP] on Windows. That
//! functionality is provided by the [`polling`] crate.
//!
//! However, note that you can also process I/O events and wake futures on any thread using the
//! [`block_on()`] function. The "async-io" thread is therefore just a fallback mechanism
//! processing I/O events in case no other threads are.
//!
//! [epoll]: https://en.wikipedia.org/wiki/Epoll
//! [kqueue]: https://en.wikipedia.org/wiki/Kqueue
//! [event ports]: https://illumos.org/man/port_create
//! [IOCP]: https://learn.microsoft.com/en-us/windows/win32/fileio/i-o-completion-ports
//! [`polling`]: https://docs.rs/polling
//!
//! # Examples
//!
//! Connect to `example.com:80`, or time out after 10 seconds.
//!
//! ```
//! use async_io::{Async, Timer};
//! use futures_lite::{future::FutureExt, io};
//!
//! use std::net::{TcpStream, ToSocketAddrs};
//! use std::time::Duration;
//!
//! # futures_lite::future::block_on(async {
//! let addr = "example.com:80".to_socket_addrs()?.next().unwrap();
//!
//! let stream = Async::<TcpStream>::connect(addr).or(async {
//!     Timer::after(Duration::from_secs(10)).await;
//!     Err(io::ErrorKind::TimedOut.into())
//! })
//! .await?;
//! # std::io::Result::Ok(()) });
//! ```

#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]
#![doc(
    html_favicon_url = "https://raw.githubusercontent.com/smol-rs/smol/master/assets/images/logo_fullsize_transparent.png"
)]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/smol-rs/smol/master/assets/images/logo_fullsize_transparent.png"
)]

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use futures_lite::stream::Stream;

use crate::reactor::Reactor;

mod driver;
mod io;
mod reactor;

pub mod os;

pub use driver::block_on;
pub use io::{Async, IoSafe};
pub use reactor::{Readable, ReadableOwned, Writable, WritableOwned};

/// A future or stream that emits timed events.
///
/// Timers are futures that output a single [`Instant`] when they fire.
///
/// Timers are also streams that can output [`Instant`]s periodically.
///
/// # Precision
///
/// There is a limit on the maximum precision that a `Timer` can provide. This limit is
/// dependent on the current platform; for instance, on Windows, the maximum precision is
/// about 16 milliseconds. Because of this limit, the timer may sleep for longer than the
/// requested duration. It will never sleep for less.
///
/// # Examples
///
/// Sleep for 1 second:
///
/// ```
/// use async_io::Timer;
/// use std::time::Duration;
///
/// # futures_lite::future::block_on(async {
/// Timer::after(Duration::from_secs(1)).await;
/// # });
/// ```
///
/// Timeout after 1 second:
///
/// ```
/// use async_io::Timer;
/// use futures_lite::FutureExt;
/// use std::time::Duration;
///
/// # futures_lite::future::block_on(async {
/// let addrs = async_net::resolve("google.com:80")
///     .or(async {
///         Timer::after(Duration::from_secs(1)).await;
///         Err(std::io::ErrorKind::TimedOut.into())
///     })
///     .await?;
/// # std::io::Result::Ok(()) });
/// ```
#[derive(Debug)]
pub struct Timer {
    /// This timer's ID and last waker that polled it.
    ///
    /// When this field is set to `None`, this timer is not registered in the reactor.
    id_and_waker: Option<(usize, Waker)>,

    /// The next instant at which this timer fires.
    ///
    /// If this timer is a blank timer, this value is None. If the timer
    /// must be set, this value contains the next instant at which the
    /// timer must fire.
    when: Option<Instant>,

    /// The period.
    period: Duration,
}

impl Timer {
    /// Creates a timer that will never fire.
    ///
    /// # Examples
    ///
    /// This function may also be useful for creating a function with an optional timeout.
    ///
    /// ```
    /// # futures_lite::future::block_on(async {
    /// use async_io::Timer;
    /// use futures_lite::prelude::*;
    /// use std::time::Duration;
    ///
    /// async fn run_with_timeout(timeout: Option<Duration>) {
    ///     let timer = timeout
    ///         .map(|timeout| Timer::after(timeout))
    ///         .unwrap_or_else(Timer::never);
    ///
    ///     run_lengthy_operation().or(timer).await;
    /// }
    /// # // Note that since a Timer as a Future returns an Instant,
    /// # // this function needs to return an Instant to be used
    /// # // in "or".
    /// # async fn run_lengthy_operation() -> std::time::Instant {
    /// #    std::time::Instant::now()
    /// # }
    ///
    /// // Times out after 5 seconds.
    /// run_with_timeout(Some(Duration::from_secs(5))).await;
    /// // Does not time out.
    /// run_with_timeout(None).await;
    /// # });
    /// ```
    pub fn never() -> Timer {
        Timer {
            id_and_waker: None,
            when: None,
            period: Duration::MAX,
        }
    }

    /// Creates a timer that emits an event once after the given duration of time.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Timer;
    /// use std::time::Duration;
    ///
    /// # futures_lite::future::block_on(async {
    /// Timer::after(Duration::from_secs(1)).await;
    /// # });
    /// ```
    pub fn after(duration: Duration) -> Timer {
        Instant::now()
            .checked_add(duration)
            .map_or_else(Timer::never, Timer::at)
    }

    /// Creates a timer that emits an event once at the given time instant.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Timer;
    /// use std::time::{Duration, Instant};
    ///
    /// # futures_lite::future::block_on(async {
    /// let now = Instant::now();
    /// let when = now + Duration::from_secs(1);
    /// Timer::at(when).await;
    /// # });
    /// ```
    pub fn at(instant: Instant) -> Timer {
        Timer::interval_at(instant, Duration::MAX)
    }

    /// Creates a timer that emits events periodically.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Timer;
    /// use futures_lite::StreamExt;
    /// use std::time::{Duration, Instant};
    ///
    /// # futures_lite::future::block_on(async {
    /// let period = Duration::from_secs(1);
    /// Timer::interval(period).next().await;
    /// # });
    /// ```
    pub fn interval(period: Duration) -> Timer {
        Instant::now()
            .checked_add(period)
            .map_or_else(Timer::never, |at| Timer::interval_at(at, period))
    }

    /// Creates a timer that emits events periodically, starting at `start`.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Timer;
    /// use futures_lite::StreamExt;
    /// use std::time::{Duration, Instant};
    ///
    /// # futures_lite::future::block_on(async {
    /// let start = Instant::now();
    /// let period = Duration::from_secs(1);
    /// Timer::interval_at(start, period).next().await;
    /// # });
    /// ```
    pub fn interval_at(start: Instant, period: Duration) -> Timer {
        Timer {
            id_and_waker: None,
            when: Some(start),
            period,
        }
    }

    /// Indicates whether or not this timer will ever fire.
    ///
    /// [`never()`] will never fire, and timers created with [`after()`] or [`at()`] will fire
    /// if the duration is not too large.
    ///
    /// [`never()`]: Timer::never()
    /// [`after()`]: Timer::after()
    /// [`at()`]: Timer::at()
    ///
    /// # Examples
    ///
    /// ```
    /// # futures_lite::future::block_on(async {
    /// use async_io::Timer;
    /// use futures_lite::prelude::*;
    /// use std::time::Duration;
    ///
    /// // `never` will never fire.
    /// assert!(!Timer::never().will_fire());
    ///
    /// // `after` will fire if the duration is not too large.
    /// assert!(Timer::after(Duration::from_secs(1)).will_fire());
    /// assert!(!Timer::after(Duration::MAX).will_fire());
    ///
    /// // However, once an `after` timer has fired, it will never fire again.
    /// let mut t = Timer::after(Duration::from_secs(1));
    /// assert!(t.will_fire());
    /// (&mut t).await;
    /// assert!(!t.will_fire());
    ///
    /// // Interval timers will fire periodically.
    /// let mut t = Timer::interval(Duration::from_secs(1));
    /// assert!(t.will_fire());
    /// t.next().await;
    /// assert!(t.will_fire());
    /// # });
    /// ```
    #[inline]
    pub fn will_fire(&self) -> bool {
        self.when.is_some()
    }

    /// Sets the timer to emit an en event once after the given duration of time.
    ///
    /// Note that resetting a timer is different from creating a new timer because
    /// [`set_after()`][`Timer::set_after()`] does not remove the waker associated with the task
    /// that is polling the timer.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Timer;
    /// use std::time::Duration;
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut t = Timer::after(Duration::from_secs(1));
    /// t.set_after(Duration::from_millis(100));
    /// # });
    /// ```
    pub fn set_after(&mut self, duration: Duration) {
        match Instant::now().checked_add(duration) {
            Some(instant) => self.set_at(instant),
            None => {
                // Overflow to never going off.
                self.clear();
                self.when = None;
            }
        }
    }

    /// Sets the timer to emit an event once at the given time instant.
    ///
    /// Note that resetting a timer is different from creating a new timer because
    /// [`set_at()`][`Timer::set_at()`] does not remove the waker associated with the task
    /// that is polling the timer.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Timer;
    /// use std::time::{Duration, Instant};
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut t = Timer::after(Duration::from_secs(1));
    ///
    /// let now = Instant::now();
    /// let when = now + Duration::from_secs(1);
    /// t.set_at(when);
    /// # });
    /// ```
    pub fn set_at(&mut self, instant: Instant) {
        self.clear();

        // Update the timeout.
        self.when = Some(instant);

        if let Some((id, waker)) = self.id_and_waker.as_mut() {
            // Re-register the timer with the new timeout.
            *id = Reactor::get().insert_timer(instant, waker);
        }
    }

    /// Sets the timer to emit events periodically.
    ///
    /// Note that resetting a timer is different from creating a new timer because
    /// [`set_interval()`][`Timer::set_interval()`] does not remove the waker associated with the
    /// task that is polling the timer.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Timer;
    /// use futures_lite::StreamExt;
    /// use std::time::{Duration, Instant};
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut t = Timer::after(Duration::from_secs(1));
    ///
    /// let period = Duration::from_secs(2);
    /// t.set_interval(period);
    /// # });
    /// ```
    pub fn set_interval(&mut self, period: Duration) {
        match Instant::now().checked_add(period) {
            Some(instant) => self.set_interval_at(instant, period),
            None => {
                // Overflow to never going off.
                self.clear();
                self.when = None;
            }
        }
    }

    /// Sets the timer to emit events periodically, starting at `start`.
    ///
    /// Note that resetting a timer is different from creating a new timer because
    /// [`set_interval_at()`][`Timer::set_interval_at()`] does not remove the waker associated with
    /// the task that is polling the timer.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Timer;
    /// use futures_lite::StreamExt;
    /// use std::time::{Duration, Instant};
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut t = Timer::after(Duration::from_secs(1));
    ///
    /// let start = Instant::now();
    /// let period = Duration::from_secs(2);
    /// t.set_interval_at(start, period);
    /// # });
    /// ```
    pub fn set_interval_at(&mut self, start: Instant, period: Duration) {
        self.clear();

        self.when = Some(start);
        self.period = period;

        if let Some((id, waker)) = self.id_and_waker.as_mut() {
            // Re-register the timer with the new timeout.
            *id = Reactor::get().insert_timer(start, waker);
        }
    }

    /// Helper function to clear the current timer.
    fn clear(&mut self) {
        if let (Some(when), Some((id, _))) = (self.when, self.id_and_waker.as_ref()) {
            // Deregister the timer from the reactor.
            Reactor::get().remove_timer(when, *id);
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let (Some(when), Some((id, _))) = (self.when, self.id_and_waker.take()) {
            // Deregister the timer from the reactor.
            Reactor::get().remove_timer(when, id);
        }
    }
}

impl Future for Timer {
    type Output = Instant;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.poll_next(cx) {
            Poll::Ready(Some(when)) => Poll::Ready(when),
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => unreachable!(),
        }
    }
}

impl Stream for Timer {
    type Item = Instant;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if let Some(ref mut when) = this.when {
            // Check if the timer has already fired.
            if Instant::now() >= *when {
                if let Some((id, _)) = this.id_and_waker.take() {
                    // Deregister the timer from the reactor.
                    Reactor::get().remove_timer(*when, id);
                }
                let result_time = *when;
                if let Some(next) = (*when).checked_add(this.period) {
                    *when = next;
                    // Register the timer in the reactor.
                    let id = Reactor::get().insert_timer(next, cx.waker());
                    this.id_and_waker = Some((id, cx.waker().clone()));
                } else {
                    this.when = None;
                }
                return Poll::Ready(Some(result_time));
            } else {
                match &this.id_and_waker {
                    None => {
                        // Register the timer in the reactor.
                        let id = Reactor::get().insert_timer(*when, cx.waker());
                        this.id_and_waker = Some((id, cx.waker().clone()));
                    }
                    Some((id, w)) if !w.will_wake(cx.waker()) => {
                        // Deregister the timer from the reactor to remove the old waker.
                        Reactor::get().remove_timer(*when, *id);

                        // Register the timer in the reactor with the new waker.
                        let id = Reactor::get().insert_timer(*when, cx.waker());
                        this.id_and_waker = Some((id, cx.waker().clone()));
                    }
                    Some(_) => {}
                }
            }
        }

        Poll::Pending
    }
}
