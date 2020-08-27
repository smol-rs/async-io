//! Async I/O and timers.
//!
//! This crate provides two tools:
//!
//! * [`Async`], an adapter for standard networking types (and [many other] types) to use in
//!   async programs.
//! * [`Timer`], a future that expires at a point in time.
//!
//! For concrete async networking types built on top of this crate, see [`async-net`].
//!
//! [many other]: https://github.com/stjepang/async-io/tree/master/examples
//! [`async-net`]: https://docs.rs/async-net
//!
//! # Implementation
//!
//! The first time [`Async`] or [`Timer`] is used, a thread named "async-io" will be spawned.
//! The purpose of this thread is to wait for I/O events reported by the operating system, and then
//! wake appropriate futures blocked on I/O or timers when they can be resumed.
//!
//! To wait for the next I/O event, the "async-io" thread uses [epoll] on Linux/Android/illumos,
//! [kqueue] on macOS/iOS/BSD, [event ports] on illumos/Solaris, and [wepoll] on Windows. That
//! functionality is provided by the [`polling`] crate.
//!
//! However, note that you can also process I/O events and wake futures on any thread using the
//! [`block_on()`] function. The "async-io" thread is therefore just a fallback mechanism
//! processing I/O events in case no other threads are.
//!
//! [epoll]: https://en.wikipedia.org/wiki/Epoll
//! [kqueue]: https://en.wikipedia.org/wiki/Kqueue
//! [event ports]: https://illumos.org/man/port_create
//! [wepoll]: https://github.com/piscisaureus/wepoll
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

use std::future::Future;
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::mem::ManuallyDrop;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::{
    os::unix::io::{AsRawFd, FromRawFd, RawFd},
    os::unix::net::{SocketAddr as UnixSocketAddr, UnixDatagram, UnixListener, UnixStream},
    path::Path,
};

use futures_lite::io::{AsyncRead, AsyncWrite};
use futures_lite::stream::{self, Stream};
use futures_lite::{future, pin};
use socket2::{Domain, Protocol, Socket, Type};

use crate::reactor::{Reactor, Source};

mod reactor;

/// Blocks the current thread on a future, processing I/O events when idle.
///
/// # Examples
///
/// ```
/// use async_io::Timer;
/// use std::time::Duration;
///
/// async_io::block_on(async {
///     // This timer will likely be processed by the current
///     // thread rather than the fallback "async-io" thread.
///     Timer::after(Duration::from_millis(1)).await;
/// });
/// ```
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    Reactor::get().block_on(future)
}

/// A future that expires at a point in time.
///
/// Timers are futures that output the [`Instant`] at which they fired.
///
/// # Examples
///
/// Sleep for 1 second:
///
/// ```
/// use async_io::Timer;
/// use std::time::Duration;
///
/// async fn sleep(dur: Duration) {
///     Timer::after(dur).await;
/// }
///
/// # futures_lite::future::block_on(async {
/// sleep(Duration::from_secs(1)).await;
/// # });
/// ```
#[derive(Debug)]
pub struct Timer {
    /// This timer's ID and last waker that polled it.
    ///
    /// When this field is set to `None`, this timer is not registered in the reactor.
    id_and_waker: Option<(usize, Waker)>,

    /// When this timer fires.
    when: Instant,
}

impl Timer {
    /// Creates a timer that expires after the given duration of time.
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
        Timer::at(Instant::now() + duration)
    }

    /// Creates a timer that expires at the given time instant.
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
        Timer {
            id_and_waker: None,
            when: instant,
        }
    }

    /// Sets the timer to expire after the new duration of time.
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
        self.set_at(Instant::now() + duration);
    }

    /// Sets the timer to expire at the new time instant.
    ///
    /// Note that resetting a timer is different from creating a new timer because
    /// [`set_after()`][`Timer::set_after()`] does not remove the waker associated with the task
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
        if let Some((id, _)) = self.id_and_waker.as_ref() {
            // Deregister the timer from the reactor.
            Reactor::get().remove_timer(self.when, *id);
        }

        // Update the timeout.
        self.when = instant;

        if let Some((id, waker)) = self.id_and_waker.as_mut() {
            // Re-register the timer with the new timeout.
            *id = Reactor::get().insert_timer(self.when, waker);
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some((id, _)) = self.id_and_waker.take() {
            // Deregister the timer from the reactor.
            Reactor::get().remove_timer(self.when, id);
        }
    }
}

impl Future for Timer {
    type Output = Instant;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Check if the timer has already fired.
        if Instant::now() >= self.when {
            if let Some((id, _)) = self.id_and_waker.take() {
                // Deregister the timer from the reactor.
                Reactor::get().remove_timer(self.when, id);
            }
            Poll::Ready(self.when)
        } else {
            match &self.id_and_waker {
                None => {
                    // Register the timer in the reactor.
                    let id = Reactor::get().insert_timer(self.when, cx.waker());
                    self.id_and_waker = Some((id, cx.waker().clone()));
                }
                Some((id, w)) if !w.will_wake(cx.waker()) => {
                    // Deregister the timer from the reactor to remove the old waker.
                    Reactor::get().remove_timer(self.when, *id);

                    // Register the timer in the reactor with the new waker.
                    let id = Reactor::get().insert_timer(self.when, cx.waker());
                    self.id_and_waker = Some((id, cx.waker().clone()));
                }
                Some(_) => {}
            }
            Poll::Pending
        }
    }
}

/// Async adapter for I/O types.
///
/// This type puts an I/O handle into non-blocking mode, registers it in
/// [epoll]/[kqueue]/[event ports]/[wepoll], and then provides an async interface for it.
///
/// **NOTE:** Do not use this type with [`File`][`std::fs::File`], [`Stdin`][`std::io::Stdin`],
/// [`Stdout`][`std::io::Stdout`], or [`Stderr`][`std::io::Stderr`] because all of the supported
/// operating systems have issues with them when put in non-blocking mode.
///
/// [epoll]: https://en.wikipedia.org/wiki/Epoll
/// [kqueue]: https://en.wikipedia.org/wiki/Kqueue
/// [event ports]: https://illumos.org/man/port_create
/// [wepoll]: https://github.com/piscisaureus/wepoll
///
/// # Examples
///
/// Connect to a server and echo incoming messages back to the server:
///
/// ```no_run
/// use async_io::Async;
/// use futures_lite::io;
/// use std::net::TcpStream;
///
/// # futures_lite::future::block_on(async {
/// // Connect to a local server.
/// let stream = Async::<TcpStream>::connect(([127, 0, 0, 1], 8000)).await?;
///
/// // Echo all messages from the read side of the stream into the write side.
/// io::copy(&stream, &stream).await?;
/// # std::io::Result::Ok(()) });
/// ```
///
/// You can use either predefined async methods or wrap blocking I/O operations in
/// [`Async::read_with()`], [`Async::read_with_mut()`], [`Async::write_with()`], and
/// [`Async::write_with_mut()`]:
///
/// ```no_run
/// use async_io::Async;
/// use std::net::TcpListener;
///
/// # futures_lite::future::block_on(async {
/// let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
///
/// // These two lines are equivalent:
/// let (stream, addr) = listener.accept().await?;
/// let (stream, addr) = listener.read_with(|inner| inner.accept()).await?;
/// # std::io::Result::Ok(()) });
/// ```
#[derive(Debug)]
pub struct Async<T> {
    /// A source registered in the reactor.
    source: Arc<Source>,

    /// The inner I/O handle.
    io: Option<Box<T>>,
}

#[cfg(unix)]
impl<T: AsRawFd> Async<T> {
    /// Creates an async I/O handle.
    ///
    /// This function will put the handle in non-blocking mode and register it in
    /// [epoll]/[kqueue]/[event ports]/[wepoll].
    ///
    /// On Unix systems, the handle must implement `AsRawFd`, while on Windows it must implement
    /// `AsRawSocket`.
    ///
    /// [epoll]: https://en.wikipedia.org/wiki/Epoll
    /// [kqueue]: https://en.wikipedia.org/wiki/Kqueue
    /// [event ports]: https://illumos.org/man/port_create
    /// [wepoll]: https://github.com/piscisaureus/wepoll
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Async;
    /// use std::net::{SocketAddr, TcpListener};
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    /// let listener = Async::new(listener)?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn new(io: T) -> io::Result<Async<T>> {
        Ok(Async {
            source: Reactor::get().insert_io(io.as_raw_fd())?,
            io: Some(Box::new(io)),
        })
    }
}

#[cfg(unix)]
impl<T: AsRawFd> AsRawFd for Async<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.source.raw
    }
}

#[cfg(windows)]
impl<T: AsRawSocket> Async<T> {
    /// Creates an async I/O handle.
    ///
    /// This function will put the handle in non-blocking mode and register it in
    /// [epoll]/[kqueue]/[event ports]/[wepoll].
    ///
    /// On Unix systems, the handle must implement `AsRawFd`, while on Windows it must implement
    /// `AsRawSocket`.
    ///
    /// [epoll]: https://en.wikipedia.org/wiki/Epoll
    /// [kqueue]: https://en.wikipedia.org/wiki/Kqueue
    /// [event ports]: https://illumos.org/man/port_create
    /// [wepoll]: https://github.com/piscisaureus/wepoll
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Async;
    /// use std::net::{SocketAddr, TcpListener};
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    /// let listener = Async::new(listener)?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn new(io: T) -> io::Result<Async<T>> {
        Ok(Async {
            source: Reactor::get().insert_io(io.as_raw_socket())?,
            io: Some(Box::new(io)),
        })
    }
}

#[cfg(windows)]
impl<T: AsRawSocket> AsRawSocket for Async<T> {
    fn as_raw_socket(&self) -> RawSocket {
        self.source.raw
    }
}

impl<T> Async<T> {
    /// Gets a reference to the inner I/O handle.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Async;
    /// use std::net::TcpListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    /// let inner = listener.get_ref();
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn get_ref(&self) -> &T {
        self.io.as_ref().unwrap()
    }

    /// Gets a mutable reference to the inner I/O handle.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Async;
    /// use std::net::TcpListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    /// let inner = listener.get_mut();
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn get_mut(&mut self) -> &mut T {
        self.io.as_mut().unwrap()
    }

    /// Unwraps the inner I/O handle.
    ///
    /// This method will **not** put the I/O handle back into blocking mode.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Async;
    /// use std::net::TcpListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    /// let inner = listener.into_inner()?;
    ///
    /// // Put the listener back into blocking mode.
    /// inner.set_nonblocking(false)?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn into_inner(mut self) -> io::Result<T> {
        let io = *self.io.take().unwrap();
        Reactor::get().remove_io(&self.source)?;
        Ok(io)
    }

    /// Waits until the I/O handle is readable.
    ///
    /// This function completes when a read operation on this I/O handle wouldn't block.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::TcpListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    ///
    /// // Wait until a client can be accepted.
    /// listener.readable().await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn readable(&self) -> io::Result<()> {
        self.source.readable().await
    }

    /// Waits until the I/O handle is writable.
    ///
    /// This function completes when a write operation on this I/O handle wouldn't block.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Async;
    /// use std::net::{TcpStream, ToSocketAddrs};
    ///
    /// # futures_lite::future::block_on(async {
    /// let addr = "example.com:80".to_socket_addrs()?.next().unwrap();
    /// let stream = Async::<TcpStream>::connect(addr).await?;
    ///
    /// // Wait until the stream is writable.
    /// stream.writable().await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn writable(&self) -> io::Result<()> {
        self.source.writable().await
    }

    /// Performs a read operation asynchronously.
    ///
    /// The I/O handle is registered in the reactor and put in non-blocking mode. This function
    /// invokes the `op` closure in a loop until it succeeds or returns an error other than
    /// [`io::ErrorKind::WouldBlock`]. In between iterations of the loop, it waits until the OS
    /// sends a notification that the I/O handle is readable.
    ///
    /// The closure receives a shared reference to the I/O handle.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::TcpListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    ///
    /// // Accept a new client asynchronously.
    /// let (stream, addr) = listener.read_with(|l| l.accept()).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn read_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
        let mut op = op;
        loop {
            // If there are no blocked readers, attempt the read operation.
            if !self.source.readers_registered() {
                // Yield with some small probability - this improves fairness.
                maybe_yield().await;

                match op(self.get_ref()) {
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                    res => return res,
                }
            }

            // Wait until the I/O handle becomes readable.
            optimistic(self.readable()).await?;
        }
    }

    /// Performs a read operation asynchronously.
    ///
    /// The I/O handle is registered in the reactor and put in non-blocking mode. This function
    /// invokes the `op` closure in a loop until it succeeds or returns an error other than
    /// [`io::ErrorKind::WouldBlock`]. In between iterations of the loop, it waits until the OS
    /// sends a notification that the I/O handle is readable.
    ///
    /// The closure receives a mutable reference to the I/O handle.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::TcpListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    ///
    /// // Accept a new client asynchronously.
    /// let (stream, addr) = listener.read_with_mut(|l| l.accept()).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn read_with_mut<R>(
        &mut self,
        op: impl FnMut(&mut T) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut op = op;
        loop {
            // If there are no blocked readers, attempt the read operation.
            if !self.source.readers_registered() {
                // Yield with some small probability - this improves fairness.
                maybe_yield().await;

                match op(self.get_mut()) {
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                    res => return res,
                }
            }

            // Wait until the I/O handle becomes readable.
            optimistic(self.readable()).await?;
        }
    }

    /// Performs a write operation asynchronously.
    ///
    /// The I/O handle is registered in the reactor and put in non-blocking mode. This function
    /// invokes the `op` closure in a loop until it succeeds or returns an error other than
    /// [`io::ErrorKind::WouldBlock`]. In between iterations of the loop, it waits until the OS
    /// sends a notification that the I/O handle is writable.
    ///
    /// The closure receives a shared reference to the I/O handle.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::UdpSocket;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 8000))?;
    /// socket.get_ref().connect("127.0.0.1:9000")?;
    ///
    /// let msg = b"hello";
    /// let len = socket.write_with(|s| s.send(msg)).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn write_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
        let mut op = op;
        loop {
            // If there are no blocked readers, attempt the write operation.
            if !self.source.writers_registered() {
                // Yield with some small probability - this improves fairness.
                maybe_yield().await;

                match op(self.get_ref()) {
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                    res => return res,
                }
            }

            // Wait until the I/O handle becomes writable.
            optimistic(self.writable()).await?;
        }
    }

    /// Performs a write operation asynchronously.
    ///
    /// The I/O handle is registered in the reactor and put in non-blocking mode. This function
    /// invokes the `op` closure in a loop until it succeeds or returns an error other than
    /// [`io::ErrorKind::WouldBlock`]. In between iterations of the loop, it waits until the OS
    /// sends a notification that the I/O handle is writable.
    ///
    /// The closure receives a mutable reference to the I/O handle.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::UdpSocket;
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 8000))?;
    /// socket.get_ref().connect("127.0.0.1:9000")?;
    ///
    /// let msg = b"hello";
    /// let len = socket.write_with_mut(|s| s.send(msg)).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn write_with_mut<R>(
        &mut self,
        op: impl FnMut(&mut T) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut op = op;
        loop {
            // If there are no blocked readers, attempt the write operation.
            if !self.source.writers_registered() {
                // Yield with some small probability - this improves fairness.
                maybe_yield().await;

                match op(self.get_mut()) {
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                    res => return res,
                }
            }

            // Wait until the I/O handle becomes writable.
            optimistic(self.writable()).await?;
        }
    }
}

impl<T> Drop for Async<T> {
    fn drop(&mut self) {
        if self.io.is_some() {
            // Deregister and ignore errors because destructors should not panic.
            let _ = Reactor::get().remove_io(&self.source);

            // Drop the I/O handle to close it.
            self.io.take();
        }
    }
}

impl<T: Read> AsyncRead for Async<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.read_with_mut(|io| io.read(buf)))
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.read_with_mut(|io| io.read_vectored(bufs)))
    }
}

impl<T> AsyncRead for &Async<T>
where
    for<'a> &'a T: Read,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.read_with(|io| (&*io).read(buf)))
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.read_with(|io| (&*io).read_vectored(bufs)))
    }
}

impl<T: Write> AsyncWrite for Async<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.write_with_mut(|io| io.write(buf)))
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.write_with_mut(|io| io.write_vectored(bufs)))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_future(cx, self.write_with_mut(|io| io.flush()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(shutdown_write(self.source.raw))
    }
}

impl<T> AsyncWrite for &Async<T>
where
    for<'a> &'a T: Write,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.write_with(|io| (&*io).write(buf)))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.write_with(|io| (&*io).write_vectored(bufs)))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_future(cx, self.write_with(|io| (&*io).flush()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(shutdown_write(self.source.raw))
    }
}

impl Async<TcpListener> {
    /// Creates a TCP listener bound to the specified address.
    ///
    /// Binding with port number 0 will request an available port from the OS.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Async;
    /// use std::net::TcpListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    /// println!("Listening on {}", listener.get_ref().local_addr()?);
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Async<TcpListener>> {
        let addr = addr.into();
        Ok(Async::new(TcpListener::bind(addr)?)?)
    }

    /// Accepts a new incoming TCP connection.
    ///
    /// When a connection is established, it will be returned as a TCP stream together with its
    /// remote address.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::TcpListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 8000))?;
    /// let (stream, addr) = listener.accept().await?;
    /// println!("Accepted client: {}", addr);
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn accept(&self) -> io::Result<(Async<TcpStream>, SocketAddr)> {
        let (stream, addr) = self.read_with(|io| io.accept()).await?;
        Ok((Async::new(stream)?, addr))
    }

    /// Returns a stream of incoming TCP connections.
    ///
    /// The stream is infinite, i.e. it never stops with a [`None`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use futures_lite::stream::StreamExt;
    /// use std::net::TcpListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 8000))?;
    /// let mut incoming = listener.incoming();
    ///
    /// while let Some(stream) = incoming.next().await {
    ///     let stream = stream?;
    ///     println!("Accepted client: {}", stream.get_ref().peer_addr()?);
    /// }
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn incoming(&self) -> impl Stream<Item = io::Result<Async<TcpStream>>> + Send + Unpin + '_ {
        Box::pin(stream::unfold(self, |listener| async move {
            let res = listener.accept().await.map(|(stream, _)| stream);
            Some((res, listener))
        }))
    }
}

impl Async<TcpStream> {
    /// Creates a TCP connection to the specified address.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Async;
    /// use std::net::{TcpStream, ToSocketAddrs};
    ///
    /// # futures_lite::future::block_on(async {
    /// let addr = "example.com:80".to_socket_addrs()?.next().unwrap();
    /// let stream = Async::<TcpStream>::connect(addr).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn connect<A: Into<SocketAddr>>(addr: A) -> io::Result<Async<TcpStream>> {
        let addr = addr.into();

        // Create a socket.
        let domain = if addr.is_ipv6() {
            Domain::ipv6()
        } else {
            Domain::ipv4()
        };
        let socket = Socket::new(domain, Type::stream(), Some(Protocol::tcp()))?;

        // Begin async connect and ignore the inevitable "in progress" error.
        socket.set_nonblocking(true)?;
        socket.connect(&addr.into()).or_else(|err| {
            // Check for EINPROGRESS on Unix and WSAEWOULDBLOCK on Windows.
            #[cfg(unix)]
            let in_progress = err.raw_os_error() == Some(libc::EINPROGRESS);
            #[cfg(windows)]
            let in_progress = err.kind() == io::ErrorKind::WouldBlock;

            // If connect results with an "in progress" error, that's not an error.
            if in_progress {
                Ok(())
            } else {
                Err(err)
            }
        })?;
        let stream = Async::new(socket.into_tcp_stream())?;

        // The stream becomes writable when connected.
        stream.writable().await?;

        // Check if there was an error while connecting.
        match stream.get_ref().take_error()? {
            None => Ok(stream),
            Some(err) => Err(err),
        }
    }

    /// Reads data from the stream without removing it from the buffer.
    ///
    /// Returns the number of bytes read. Successive calls of this method read the same data.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Async;
    /// use futures_lite::{io::AsyncWriteExt, stream::StreamExt};
    /// use std::net::{TcpStream, ToSocketAddrs};
    ///
    /// # futures_lite::future::block_on(async {
    /// let addr = "example.com:80".to_socket_addrs()?.next().unwrap();
    /// let mut stream = Async::<TcpStream>::connect(addr).await?;
    ///
    /// stream
    ///     .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
    ///     .await?;
    ///
    /// let mut buf = [0u8; 1024];
    /// let len = stream.peek(&mut buf).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_with(|io| io.peek(buf)).await
    }
}

impl Async<UdpSocket> {
    /// Creates a UDP socket bound to the specified address.
    ///
    /// Binding with port number 0 will request an available port from the OS.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Async;
    /// use std::net::UdpSocket;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 0))?;
    /// println!("Bound to {}", socket.get_ref().local_addr()?);
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Async<UdpSocket>> {
        let addr = addr.into();
        Ok(Async::new(UdpSocket::bind(addr)?)?)
    }

    /// Receives a single datagram message.
    ///
    /// Returns the number of bytes read and the address the message came from.
    ///
    /// This method must be called with a valid byte slice of sufficient size to hold the message.
    /// If the message is too long to fit, excess bytes may get discarded.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::UdpSocket;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 8000))?;
    ///
    /// let mut buf = [0u8; 1024];
    /// let (len, addr) = socket.recv_from(&mut buf).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.read_with(|io| io.recv_from(buf)).await
    }

    /// Receives a single datagram message without removing it from the queue.
    ///
    /// Returns the number of bytes read and the address the message came from.
    ///
    /// This method must be called with a valid byte slice of sufficient size to hold the message.
    /// If the message is too long to fit, excess bytes may get discarded.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::UdpSocket;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 8000))?;
    ///
    /// let mut buf = [0u8; 1024];
    /// let (len, addr) = socket.peek_from(&mut buf).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.read_with(|io| io.peek_from(buf)).await
    }

    /// Sends data to the specified address.
    ///
    /// Returns the number of bytes writen.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::UdpSocket;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 0))?;
    /// let addr = socket.get_ref().local_addr()?;
    ///
    /// let msg = b"hello";
    /// let len = socket.send_to(msg, addr).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn send_to<A: Into<SocketAddr>>(&self, buf: &[u8], addr: A) -> io::Result<usize> {
        let addr = addr.into();
        self.write_with(|io| io.send_to(buf, addr)).await
    }

    /// Receives a single datagram message from the connected peer.
    ///
    /// Returns the number of bytes read.
    ///
    /// This method must be called with a valid byte slice of sufficient size to hold the message.
    /// If the message is too long to fit, excess bytes may get discarded.
    ///
    /// The [`connect`][`UdpSocket::connect()`] method connects this socket to a remote address.
    /// This method will fail if the socket is not connected.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::UdpSocket;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 8000))?;
    /// socket.get_ref().connect("127.0.0.1:9000")?;
    ///
    /// let mut buf = [0u8; 1024];
    /// let len = socket.recv(&mut buf).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_with(|io| io.recv(buf)).await
    }

    /// Receives a single datagram message from the connected peer without removing it from the
    /// queue.
    ///
    /// Returns the number of bytes read and the address the message came from.
    ///
    /// This method must be called with a valid byte slice of sufficient size to hold the message.
    /// If the message is too long to fit, excess bytes may get discarded.
    ///
    /// The [`connect`][`UdpSocket::connect()`] method connects this socket to a remote address.
    /// This method will fail if the socket is not connected.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::UdpSocket;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 8000))?;
    /// socket.get_ref().connect("127.0.0.1:9000")?;
    ///
    /// let mut buf = [0u8; 1024];
    /// let len = socket.peek(&mut buf).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_with(|io| io.peek(buf)).await
    }

    /// Sends data to the connected peer.
    ///
    /// Returns the number of bytes written.
    ///
    /// The [`connect`][`UdpSocket::connect()`] method connects this socket to a remote address.
    /// This method will fail if the socket is not connected.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::net::UdpSocket;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 8000))?;
    /// socket.get_ref().connect("127.0.0.1:9000")?;
    ///
    /// let msg = b"hello";
    /// let len = socket.send(msg).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.write_with(|io| io.send(buf)).await
    }
}

#[cfg(unix)]
impl Async<UnixListener> {
    /// Creates a UDS listener bound to the specified path.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = Async::<UnixListener>::bind("/tmp/socket")?;
    /// println!("Listening on {:?}", listener.get_ref().local_addr()?);
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Async<UnixListener>> {
        let path = path.as_ref().to_owned();
        Ok(Async::new(UnixListener::bind(path)?)?)
    }

    /// Accepts a new incoming UDS stream connection.
    ///
    /// When a connection is established, it will be returned as a stream together with its remote
    /// address.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = Async::<UnixListener>::bind("/tmp/socket")?;
    /// let (stream, addr) = listener.accept().await?;
    /// println!("Accepted client: {:?}", addr);
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn accept(&self) -> io::Result<(Async<UnixStream>, UnixSocketAddr)> {
        let (stream, addr) = self.read_with(|io| io.accept()).await?;
        Ok((Async::new(stream)?, addr))
    }

    /// Returns a stream of incoming UDS connections.
    ///
    /// The stream is infinite, i.e. it never stops with a [`None`] item.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use futures_lite::stream::StreamExt;
    /// use std::os::unix::net::UnixListener;
    ///
    /// # futures_lite::future::block_on(async {
    /// let listener = Async::<UnixListener>::bind("/tmp/socket")?;
    /// let mut incoming = listener.incoming();
    ///
    /// while let Some(stream) = incoming.next().await {
    ///     let stream = stream?;
    ///     println!("Accepted client: {:?}", stream.get_ref().peer_addr()?);
    /// }
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn incoming(
        &self,
    ) -> impl Stream<Item = io::Result<Async<UnixStream>>> + Send + Unpin + '_ {
        Box::pin(stream::unfold(self, |listener| async move {
            let res = listener.accept().await.map(|(stream, _)| stream);
            Some((res, listener))
        }))
    }
}

#[cfg(unix)]
impl Async<UnixStream> {
    /// Creates a UDS stream connected to the specified path.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixStream;
    ///
    /// # futures_lite::future::block_on(async {
    /// let stream = Async::<UnixStream>::connect("/tmp/socket").await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn connect<P: AsRef<Path>>(path: P) -> io::Result<Async<UnixStream>> {
        // Create a socket.
        let socket = Socket::new(Domain::unix(), Type::stream(), None)?;

        // Begin async connect and ignore the inevitable "in progress" error.
        socket.set_nonblocking(true)?;
        socket
            .connect(&socket2::SockAddr::unix(path)?)
            .or_else(|err| {
                if err.kind() == io::ErrorKind::WouldBlock {
                    Ok(())
                } else {
                    Err(err)
                }
            })?;
        let stream = Async::new(socket.into_unix_stream())?;

        // The stream becomes writable when connected.
        stream.writable().await?;

        Ok(stream)
    }

    /// Creates an unnamed pair of connected UDS stream sockets.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixStream;
    ///
    /// # futures_lite::future::block_on(async {
    /// let (stream1, stream2) = Async::<UnixStream>::pair()?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn pair() -> io::Result<(Async<UnixStream>, Async<UnixStream>)> {
        let (stream1, stream2) = UnixStream::pair()?;
        Ok((Async::new(stream1)?, Async::new(stream2)?))
    }
}

#[cfg(unix)]
impl Async<UnixDatagram> {
    /// Creates a UDS datagram socket bound to the specified path.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixDatagram;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UnixDatagram>::bind("/tmp/socket")?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Async<UnixDatagram>> {
        let path = path.as_ref().to_owned();
        Ok(Async::new(UnixDatagram::bind(path)?)?)
    }

    /// Creates a UDS datagram socket not bound to any address.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixDatagram;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UnixDatagram>::unbound()?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn unbound() -> io::Result<Async<UnixDatagram>> {
        Ok(Async::new(UnixDatagram::unbound()?)?)
    }

    /// Creates an unnamed pair of connected Unix datagram sockets.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixDatagram;
    ///
    /// # futures_lite::future::block_on(async {
    /// let (socket1, socket2) = Async::<UnixDatagram>::pair()?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn pair() -> io::Result<(Async<UnixDatagram>, Async<UnixDatagram>)> {
        let (socket1, socket2) = UnixDatagram::pair()?;
        Ok((Async::new(socket1)?, Async::new(socket2)?))
    }

    /// Receives data from the socket.
    ///
    /// Returns the number of bytes read and the address the message came from.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixDatagram;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UnixDatagram>::bind("/tmp/socket")?;
    ///
    /// let mut buf = [0u8; 1024];
    /// let (len, addr) = socket.recv_from(&mut buf).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, UnixSocketAddr)> {
        self.read_with(|io| io.recv_from(buf)).await
    }

    /// Sends data to the specified address.
    ///
    /// Returns the number of bytes written.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixDatagram;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UnixDatagram>::unbound()?;
    ///
    /// let msg = b"hello";
    /// let addr = "/tmp/socket";
    /// let len = socket.send_to(msg, addr).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn send_to<P: AsRef<Path>>(&self, buf: &[u8], path: P) -> io::Result<usize> {
        self.write_with(|io| io.send_to(buf, &path)).await
    }

    /// Receives data from the connected peer.
    ///
    /// Returns the number of bytes read and the address the message came from.
    ///
    /// The [`connect`][`UnixDatagram::connect()`] method connects this socket to a remote address.
    /// This method will fail if the socket is not connected.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixDatagram;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UnixDatagram>::bind("/tmp/socket1")?;
    /// socket.get_ref().connect("/tmp/socket2")?;
    ///
    /// let mut buf = [0u8; 1024];
    /// let len = socket.recv(&mut buf).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_with(|io| io.recv(buf)).await
    }

    /// Sends data to the connected peer.
    ///
    /// Returns the number of bytes written.
    ///
    /// The [`connect`][`UnixDatagram::connect()`] method connects this socket to a remote address.
    /// This method will fail if the socket is not connected.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_io::Async;
    /// use std::os::unix::net::UnixDatagram;
    ///
    /// # futures_lite::future::block_on(async {
    /// let socket = Async::<UnixDatagram>::bind("/tmp/socket1")?;
    /// socket.get_ref().connect("/tmp/socket2")?;
    ///
    /// let msg = b"hello";
    /// let len = socket.send(msg).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.write_with(|io| io.send(buf)).await
    }
}

/// Polls a future once.
fn poll_future<T>(cx: &mut Context<'_>, fut: impl Future<Output = T>) -> Poll<T> {
    pin!(fut);
    fut.poll(cx)
}

/// Polls a future once, waits for a wakeup, and then optimistically assumes the future is ready.
async fn optimistic(fut: impl Future<Output = io::Result<()>>) -> io::Result<()> {
    let mut polled = false;
    pin!(fut);

    future::poll_fn(|cx| {
        if !polled {
            polled = true;
            fut.as_mut().poll(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    })
    .await
}

/// Yield with some small probability.
async fn maybe_yield() {
    if fastrand::usize(..100) == 0 {
        future::yield_now().await;
    }
}

/// Shuts down the write side of a socket.
///
/// If this source is not a socket, the `shutdown()` syscall error is ignored.
fn shutdown_write(#[cfg(unix)] raw: RawFd, #[cfg(windows)] raw: RawSocket) -> io::Result<()> {
    // This may not be a TCP stream, but that's okay. All we do is attempt a `shutdown()` on the
    // raw descriptor and ignore errors.
    let stream = unsafe {
        ManuallyDrop::new(
            #[cfg(unix)]
            TcpStream::from_raw_fd(raw),
            #[cfg(windows)]
            TcpStream::from_raw_socket(raw),
        )
    };

    // If the socket is a TCP stream, the only actual error can be ENOTCONN.
    match stream.shutdown(Shutdown::Write) {
        Err(err) if err.kind() == io::ErrorKind::NotConnected => Err(err),
        _ => Ok(()),
    }
}
