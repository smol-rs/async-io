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
use std::task::{Context, Poll};
use std::time::Duration;

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;

use futures_lite::stream::Stream;

#[cfg(not(target_family = "wasm"))]
mod driver;
#[cfg(not(target_family = "wasm"))]
mod io;
#[cfg(not(target_family = "wasm"))]
mod reactor;

#[cfg_attr(not(target_family = "wasm"), path = "timer/native.rs")]
#[cfg_attr(target_family = "wasm", path = "timer/web.rs")]
mod timer;

pub mod os;

#[cfg(not(target_family = "wasm"))]
pub use driver::block_on;
#[cfg(not(target_family = "wasm"))]
pub use io::{Async, IoSafe};
#[cfg(not(target_family = "wasm"))]
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
pub struct Timer(timer::Timer);

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
    #[inline]
    pub fn never() -> Timer {
        Timer(timer::Timer::never())
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
    #[inline]
    pub fn after(duration: Duration) -> Timer {
        Timer(timer::Timer::after(duration))
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
    #[cfg(not(target_family = "wasm"))]
    #[inline]
    pub fn at(instant: Instant) -> Timer {
        Timer(timer::Timer::at(instant))
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
    #[inline]
    pub fn interval(period: Duration) -> Timer {
        Timer(timer::Timer::interval(period))
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
    #[cfg(not(target_family = "wasm"))]
    #[inline]
    pub fn interval_at(start: Instant, period: Duration) -> Timer {
        Timer(timer::Timer::interval_at(start, period))
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
        self.0.will_fire()
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
    #[inline]
    pub fn set_after(&mut self, duration: Duration) {
        self.0.set_after(duration)
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
    #[cfg(not(target_family = "wasm"))]
    #[inline]
    pub fn set_at(&mut self, instant: Instant) {
        self.0.set_at(instant)
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
    #[inline]
    pub fn set_interval(&mut self, period: Duration) {
        self.0.set_interval(period)
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
    #[cfg(not(target_family = "wasm"))]
    #[inline]
    pub fn set_interval_at(&mut self, start: Instant, period: Duration) {
        self.0.set_interval_at(start, period)
    }
}

impl Future for Timer {
    #[cfg(not(target_family = "wasm"))]
    type Output = Instant;

    #[cfg(target_family = "wasm")]
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.poll_next(cx) {
            Poll::Ready(Some(when)) => Poll::Ready(when),
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => unreachable!(),
        }
    }
}

impl Stream for Timer {
    #[cfg(not(target_family = "wasm"))]
    type Item = Instant;

    #[cfg(target_family = "wasm")]
    type Item = ();

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_next(cx)
    }
}
