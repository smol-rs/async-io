//! An executor for isolating blocking I/O in async programs.
//!
//! Sometimes there's no way to avoid blocking I/O. Consider files or stdin, which have weak async
//! support on modern operating systems. While [IOCP], [AIO], and [io_uring] are possible
//! solutions, they're not always available or ideal.
//!
//! Since blocking is not allowed inside futures, we must move blocking I/O onto a special thread
//! pool provided by this crate. The pool dynamically spawns and stops threads depending on the
//! current number of running I/O jobs.
//!
//! Note that there is a limit on the number of active threads. Once that limit is hit, a running
//! job has to finish before others get a chance to run. When a thread is idle, it waits for the
//! next job or shuts down after a certain timeout.
//!
//! [IOCP]: https://en.wikipedia.org/wiki/Input/output_completion_port
//! [AIO]: http://man7.org/linux/man-pages/man2/io_submit.2.html
//! [io_uring]: https://lwn.net/Articles/776703/
//!
//! # Examples
//!
//! Await a blocking I/O operation with [`Blocking::new()`]:
//!
//! ```no_run
//! use blocking::Blocking;
//! use std::fs;
//!
//! # futures::executor::block_on(async {
//! let contents = Blocking::new(|| fs::read_to_string("file.txt")).await?;
//! # std::io::Result::Ok(()) });
//! ```
//!
//! Or do the same with the [`blocking!`] macro:
//!
//! ```no_run
//! use blocking::blocking;
//! use std::fs;
//!
//! # futures::executor::block_on(async {
//! let contents = blocking!(fs::read_to_string("file.txt"))?;
//! # std::io::Result::Ok(()) });
//! ```
//!
//! Read a file and pipe its contents to stdout:
//!
//! ```no_run
//! use blocking::Blocking;
//! use std::fs::File;
//! use std::io::stdout;
//!
//! # futures::executor::block_on(async {
//! let input = Blocking::new(File::open("file.txt")?);
//! let mut output = Blocking::new(stdout());
//!
//! futures::io::copy(input, &mut output).await?;
//! # std::io::Result::Ok(()) });
//! ```
//!
//! Iterate over the contents of a directory:
//!
//! ```no_run
//! use blocking::Blocking;
//! use futures::prelude::*;
//! use std::fs;
//!
//! # futures::executor::block_on(async {
//! let mut dir = Blocking::new(fs::read_dir(".")?);
//!
//! while let Some(item) = dir.next().await {
//!     println!("{}", item?.file_name().to_string_lossy());
//! }
//! # std::io::Result::Ok(()) });
//! ```

use std::any::Any;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::mem;
use std::panic;
use std::pin::Pin;
use std::slice;
use std::sync::atomic::{self, AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use futures::channel::{mpsc, oneshot};
use futures::prelude::*;
use futures::task::{waker_ref, ArcWake, AtomicWaker};
use once_cell::sync::Lazy;

/// Awaits the output of a spawned future.
type Task<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A spawned future and its current state.
///
/// How this works was explained in a [blog post].
///
/// [blog post]: https://stjepang.github.io/2020/01/31/build-your-own-executor.html
struct Runnable {
    /// Current state of the task.
    state: AtomicUsize,

    /// The inner future.
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl Runnable {
    /// Runs the task.
    fn run(self: Arc<Runnable>) {
        // Set if the task has been woken.
        const WOKEN: usize = 0b01;
        // Set if the task is currently running.
        const RUNNING: usize = 0b10;

        impl ArcWake for Runnable {
            fn wake_by_ref(runnable: &Arc<Self>) {
                if runnable.state.fetch_or(WOKEN, Ordering::SeqCst) == 0 {
                    EXECUTOR.schedule(runnable.clone());
                }
            }
        }

        // The state is now "not woken" and "running".
        self.state.store(RUNNING, Ordering::SeqCst);

        // Poll the future.
        let waker = waker_ref(&self);
        let cx = &mut Context::from_waker(&waker);
        let poll = self.future.try_lock().unwrap().as_mut().poll(cx);

        // If the future hasn't completed and was woken while running, then reschedule it.
        if poll.is_pending() {
            if self.state.fetch_and(!RUNNING, Ordering::SeqCst) == WOKEN | RUNNING {
                EXECUTOR.schedule(self);
            }
        }
    }
}

/// Lazily initialized global executor.
static EXECUTOR: Lazy<Executor> = Lazy::new(|| Executor {
    inner: Mutex::new(Inner {
        idle_count: 0,
        thread_count: 0,
        queue: VecDeque::new(),
    }),
    cvar: Condvar::new(),
});

/// The blocking executor.
struct Executor {
    /// Inner state of the executor.
    inner: Mutex<Inner>,

    /// Used to put idle threads to sleep and wake them up when new work comes in.
    cvar: Condvar,
}

/// Inner state of the blocking executor.
struct Inner {
    /// Number of idle threads in the pool.
    ///
    /// Idle threads are sleeping, waiting to get a task to run.
    idle_count: usize,

    /// Total number of threads in the pool.
    ///
    /// This is the number of idle threads + the number of active threads.
    thread_count: usize,

    /// The queue of blocking tasks.
    queue: VecDeque<Arc<Runnable>>,
}

impl Executor {
    /// Spawns a future onto this executor.
    ///
    /// Returns a [`Task`] handle for the spawned task.
    fn spawn<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Task<T> {
        // Wrap the future into one that sends the output into a channel.
        let (s, r) = oneshot::channel();
        let future = async move {
            let _ = s.send(future.await);
        };

        // Create a task and schedule it for execution.
        let runnable = Arc::new(Runnable {
            state: AtomicUsize::new(0),
            future: Mutex::new(Box::pin(future)),
        });
        EXECUTOR.schedule(runnable);

        // Return a handle that retrieves the output of the future.
        Box::pin(async { r.await.expect("future has panicked") })
    }

    /// Runs the main loop on the current thread.
    ///
    /// This function runs blocking tasks until it becomes idle and times out.
    fn main_loop(&'static self) {
        let mut inner = self.inner.lock().unwrap();
        loop {
            // This thread is not idle anymore because it's going to run tasks.
            inner.idle_count -= 1;

            // Run tasks in the queue.
            while let Some(runnable) = inner.queue.pop_front() {
                // We have found a task - grow the pool if needed.
                self.grow_pool(inner);

                // Run the task.
                let _ = panic::catch_unwind(|| runnable.run());

                // Re-lock the inner state and continue.
                inner = self.inner.lock().unwrap();
            }

            // This thread is now becoming idle.
            inner.idle_count += 1;

            // Put the thread to sleep until another task is scheduled.
            let timeout = Duration::from_millis(500);
            let (lock, res) = self.cvar.wait_timeout(inner, timeout).unwrap();
            inner = lock;

            // If there are no tasks after a while, stop this thread.
            if res.timed_out() && inner.queue.is_empty() {
                inner.idle_count -= 1;
                inner.thread_count -= 1;
                break;
            }
        }
    }

    /// Schedules a runnable task for execution.
    fn schedule(&'static self, runnable: Arc<Runnable>) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.push_back(runnable);

        // Notify a sleeping thread and spawn more threads if needed.
        self.cvar.notify_one();
        self.grow_pool(inner);
    }

    /// Spawns more blocking threads if the pool is overloaded with work.
    fn grow_pool(&'static self, mut inner: MutexGuard<'static, Inner>) {
        // If runnable tasks greatly outnumber idle threads and there aren't too many threads
        // already, then be aggressive: wake all idle threads and spawn one more thread.
        while inner.queue.len() > inner.idle_count * 5 && inner.thread_count < 500 {
            // The new thread starts in idle state.
            inner.idle_count += 1;
            inner.thread_count += 1;

            // Notify all existing idle threads because we need to hurry up.
            self.cvar.notify_all();

            // Spawn the new thread.
            thread::spawn(move || self.main_loop());
        }
    }
}

/// Spawns blocking I/O onto a thread.
///
/// Note that `blocking!(expr)` is just syntax sugar for `Blocking::new(move || expr).await`.
///
/// # Examples
///
/// Read a file into a string:
///
/// ```no_run
/// use blocking::blocking;
/// use std::fs;
///
/// # futures::executor::block_on(async {
/// let contents = blocking!(fs::read_to_string("file.txt"))?;
/// # std::io::Result::Ok(()) });
/// ```
///
/// Spawn a process:
///
/// ```no_run
/// use blocking::blocking;
/// use std::process::Command;
///
/// # futures::executor::block_on(async {
/// let out = blocking!(Command::new("dir").output())?;
/// # std::io::Result::Ok(()) });
/// ```
#[macro_export]
macro_rules! blocking {
    ($expr:expr) => {
        $crate::Blocking::new(move || $expr).await
    };
}

/// Async I/O that runs on a thread.
///
/// This type implements traits [`Future`], [`Stream`], [`AsyncRead`], or [`AsyncWrite`] if the
/// inner type implements [`FnOnce`], [`Iterator`], [`Read`], or [`Write`], respectively.
///
/// If writing some data through the [`AsyncWrite`] trait, make sure to flush before dropping the
/// [`Blocking`] handle or some written data might get lost.
///
/// # Examples
///
/// ```
/// use blocking::Blocking;
/// use futures::prelude::*;
/// use std::io::stdout;
///
/// # futures::executor::block_on(async {
/// let mut stdout = Blocking::new(stdout());
/// stdout.write_all(b"Hello world!").await?;
/// stdout.flush().await?;
/// # std::io::Result::Ok(()) });
/// ```
pub struct Blocking<T>(State<T>);

impl<T> Blocking<T> {
    /// Wraps a blocking I/O handle or closure into an async interface.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use blocking::Blocking;
    /// use std::io::stdin;
    ///
    /// # futures::executor::block_on(async {
    /// // Create an async handle to standard input.
    /// let stdin = Blocking::new(stdin());
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn new(io: T) -> Blocking<T> {
        Blocking(State::Idle(Some(Box::new(io))))
    }

    /// Gets a mutable reference to the blocking I/O handle.
    ///
    /// This is an async method because the I/O handle might be on the thread pool and needs to
    /// be moved onto the current thread before we can get a reference to it.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use blocking::Blocking;
    /// use std::fs::File;
    ///
    /// # futures::executor::block_on(async {
    /// let mut file = Blocking::new(File::create("file.txt")?);
    /// let metadata = file.get_mut().await.metadata()?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn get_mut(&mut self) -> &mut T {
        // Wait for the running task to stop and ignore I/O errors if there are any.
        let _ = future::poll_fn(|cx| self.poll_stop(cx)).await;

        // Assume idle state and get a reference to the inner value.
        match &mut self.0 {
            State::Idle(t) => t.as_mut().expect("inner value was taken out"),
            State::WithMut(..)
            | State::Closure(..)
            | State::Streaming(..)
            | State::Reading(..)
            | State::Writing(..) => {
                unreachable!("when stopped, the state machine must be in idle state");
            }
        }
    }

    /// Performs a blocking operation on the I/O handle.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use blocking::Blocking;
    /// use std::fs::File;
    ///
    /// # futures::executor::block_on(async {
    /// let mut file = Blocking::new(File::create("file.txt")?);
    /// let metadata = file.with_mut(|f| f.metadata()).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn with_mut<R, F>(&mut self, op: F) -> R
    where
        F: FnOnce(&mut T) -> R + Send + 'static,
        R: Send + 'static,
        T: Send + 'static,
    {
        // Wait for the running task to stop and ignore I/O errors if there are any.
        let _ = future::poll_fn(|cx| self.poll_stop(cx)).await;

        // Assume idle state and take out the inner value.
        let mut t = match &mut self.0 {
            State::Idle(t) => t.take().expect("inner value was taken out"),
            State::WithMut(..)
            | State::Closure(..)
            | State::Streaming(..)
            | State::Reading(..)
            | State::Writing(..) => {
                unreachable!("when stopped, the state machine must be in idle state");
            }
        };

        let (sender, receiver) = oneshot::channel();
        let task = Executor::spawn(async move {
            let _ = sender.send(op(&mut t));
            t
        });
        self.0 = State::WithMut(task);

        receiver.await.expect("`with_mut()` operation has panicked")
    }

    /// Extracts the inner blocking I/O handle.
    ///
    /// This is an async method because the I/O handle might be on the thread pool and needs to
    /// be moved onto the current thread before we can extract it.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use blocking::Blocking;
    /// use futures::prelude::*;
    /// use std::fs::File;
    ///
    /// # futures::executor::block_on(async {
    /// let mut file = Blocking::new(File::create("file.txt")?);
    /// file.write_all(b"Hello world!").await?;
    ///
    /// let file = file.into_inner().await;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn into_inner(self) -> T {
        // There's a bug in rustdoc causing it to render `mut self` as `__arg0: Self`, so we just
        // bind `self` to a local mutable variable.
        let mut this = self;

        // Wait for the running task to stop and ignore I/O errors if there are any.
        let _ = future::poll_fn(|cx| this.poll_stop(cx)).await;

        // Assume idle state and extract the inner value.
        match &mut this.0 {
            State::Idle(t) => *t.take().expect("inner value was taken out"),
            State::WithMut(..)
            | State::Closure(..)
            | State::Streaming(..)
            | State::Reading(..)
            | State::Writing(..) => {
                unreachable!("when stopped, the state machine must be in idle state");
            }
        }
    }

    /// Waits for the running task to stop.
    ///
    /// On success, the state machine is moved into the idle state.
    fn poll_stop(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match &mut self.0 {
                State::Idle(_) => return Poll::Ready(Ok(())),

                State::WithMut(task) => {
                    // Poll the task to wait for it to finish.
                    let t = futures::ready!(Pin::new(task).poll(cx));
                    self.0 = State::Idle(Some(t));
                }

                State::Closure(any, task) => {
                    // Drop the receiver to close the channel.
                    any.take();

                    // Poll the task to wait for it to finish.
                    futures::ready!(Pin::new(task).poll(cx));
                    self.0 = State::Idle(None);
                }

                State::Streaming(any, task) => {
                    // Drop the receiver to close the channel. This stops the `send()` operation in
                    // the task, after which the task returns the iterator back.
                    any.take();

                    // Poll the task to retrieve the iterator.
                    let iter = futures::ready!(Pin::new(task).poll(cx));
                    self.0 = State::Idle(Some(iter));
                }

                State::Reading(reader, task) => {
                    // Drop the reader to close the pipe. This stops the `futures::io::copy`
                    // operation in the task, after which the task returns the I/O handle back.
                    reader.take();

                    // Poll the task to retrieve the I/O handle.
                    let (res, io) = futures::ready!(Pin::new(task).poll(cx));
                    // Make sure to move into the idle state before reporting errors.
                    self.0 = State::Idle(Some(io));
                    res?;
                }

                State::Writing(writer, task) => {
                    // Drop the writer to close the pipe. This stops the `futures::io::copy`
                    // operation in the task, after which the task flushes the I/O handle and
                    // returns it back.
                    writer.take();

                    // Poll the task to retrieve the I/O handle.
                    let (res, io) = futures::ready!(Pin::new(task).poll(cx));
                    // Make sure to move into the idle state before reporting errors.
                    self.0 = State::Idle(Some(io));
                    res?;
                }
            }
        }
    }
}

impl<T, R> Future for Blocking<T>
where
    T: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    type Output = R;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match &mut self.0 {
                // If not in idle or task state, stop the running task.
                State::WithMut(..)
                | State::Streaming(..)
                | State::Reading(..)
                | State::Writing(..) => {
                    // Wait for the running task to stop.
                    let _ = futures::ready!(self.poll_stop(cx));
                }

                // If idle, spawn the closure.
                State::Idle(closure) => {
                    // Take the closure out to call it on a blocking task.
                    let closure = closure.take().expect("inner closure was taken out");

                    let (sender, receiver) = oneshot::channel();

                    // Spawn a blocking task that runs the iterator and returns it when done.
                    let task = Executor::spawn(async move {
                        let _ = sender.send(closure());
                    });

                    // Move into the task state and poll again.
                    self.0 = State::Closure(Some(Box::new(receiver)), task);
                }

                // If running the closure, await it.
                State::Closure(any, task) => {
                    // Poll the task to wait for it to finish.
                    futures::ready!(Pin::new(task).poll(cx));

                    // Extract the receiver.
                    let mut receiver = any
                        .take()
                        .expect("future has panicked")
                        .downcast::<oneshot::Receiver<R>>()
                        .unwrap();
                    self.0 = State::Idle(None);

                    // Return the output from the closure.
                    let r = receiver
                        .try_recv()
                        .expect("future has panicked")
                        .expect("future was awaited but the oneshot channel is empty");
                    return Poll::Ready(r);
                }
            }
        }
    }
}

/// Current state of a blocking task.
enum State<T> {
    /// There is no blocking task.
    ///
    /// The inner value is readily available, unless it has already been extracted. The value is
    /// extracted out by [`Blocking::into_inner()`], [`AsyncWrite::poll_close()`], or by awaiting
    /// [`Blocking`].
    Idle(Option<Box<T>>),

    /// A [`Blocking::with_mut()`] closure was spawned and is still running.
    WithMut(Task<Box<T>>),

    /// The inner closure was spawned and is still running.
    ///
    /// The `dyn Any` value here is a `oneshot::Receiver<<T as FnOnce>::Output>`.
    Closure(Option<Box<dyn Any + Send>>, Task<()>),

    /// The inner value is an [`Iterator`] currently iterating in a task.
    ///
    /// The `dyn Any` value here is a `mpsc::Receiver<<T as Iterator>::Item>`.
    Streaming(Option<Box<dyn Any + Send>>, Task<Box<T>>),

    /// The inner value is a [`Read`] currently reading in a task.
    Reading(Option<Reader>, Task<(io::Result<()>, Box<T>)>),

    /// The inner value is a [`Write`] currently writing in a task.
    Writing(Option<Writer>, Task<(io::Result<()>, Box<T>)>),
}

impl<T: Iterator + Send + 'static> Stream for Blocking<T>
where
    T::Item: Send + 'static,
{
    type Item = T::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T::Item>> {
        loop {
            match &mut self.0 {
                // If not in idle or active streaming state, stop the running task.
                State::WithMut(..)
                | State::Closure(..)
                | State::Streaming(None, _)
                | State::Reading(..)
                | State::Writing(..) => {
                    // Wait for the running task to stop.
                    let _ = futures::ready!(self.poll_stop(cx));
                }

                // If idle, start a streaming task.
                State::Idle(iter) => {
                    // Take the iterator out to run it on a blocking task.
                    let mut iter = iter.take().expect("inner iterator was taken out");

                    // This channel capacity seems to work well in practice. If it's too low, there
                    // will be too much synchronization between tasks. If too high, memory
                    // consumption increases.
                    let (mut sender, receiver) = mpsc::channel(8 * 1024); // 8192 items

                    // Spawn a blocking task that runs the iterator and returns it when done.
                    let task = Executor::spawn(async move {
                        for item in &mut iter {
                            if sender.send(item).await.is_err() {
                                break;
                            }
                        }
                        iter
                    });

                    // Move into the busy state and poll again.
                    self.0 = State::Streaming(Some(Box::new(receiver)), task);
                }

                // If streaming, receive an item.
                State::Streaming(Some(any), task) => {
                    let receiver = any.downcast_mut::<mpsc::Receiver<T::Item>>().unwrap();

                    // Poll the channel.
                    let opt = futures::ready!(Pin::new(receiver).poll_next(cx));

                    // If the channel is closed, retrieve the iterator back from the blocking task.
                    // This is not really a required step, but it's cleaner to drop the iterator on
                    // the same thread that created it.
                    if opt.is_none() {
                        // Poll the task to retrieve the iterator.
                        let iter = futures::ready!(Pin::new(task).poll(cx));
                        self.0 = State::Idle(Some(iter));
                    }

                    return Poll::Ready(opt);
                }
            }
        }
    }
}

impl<T: Read + Send + 'static> AsyncRead for Blocking<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match &mut self.0 {
                // If not in idle or active reading state, stop the running task.
                State::WithMut(..)
                | State::Closure(..)
                | State::Reading(None, _)
                | State::Streaming(..)
                | State::Writing(..) => {
                    // Wait for the running task to stop.
                    futures::ready!(self.poll_stop(cx))?;
                }

                // If idle, start a reading task.
                State::Idle(io) => {
                    // Take the I/O handle out to read it on a blocking task.
                    let mut io = io.take().expect("inner value was taken out");

                    // This pipe capacity seems to work well in practice. If it's too low, there
                    // will be too much synchronization between tasks. If too high, memory
                    // consumption increases.
                    let (reader, mut writer) = pipe(8 * 1024 * 1024); // 8 MB

                    // Spawn a blocking task that reads and returns the I/O handle when done.
                    let task = Executor::spawn(async move {
                        // Copy bytes from the I/O handle into the pipe until the pipe is closed or
                        // an error occurs.
                        loop {
                            match future::poll_fn(|cx| writer.poll_write(cx, &mut io)).await {
                                Ok(0) => return (Ok(()), io),
                                Ok(_) => {}
                                Err(err) => return (Err(err), io),
                            }
                        }
                    });

                    // Move into the busy state and poll again.
                    self.0 = State::Reading(Some(reader), task);
                }

                // If reading, read bytes from the pipe.
                State::Reading(Some(reader), task) => {
                    // Poll the pipe.
                    let n = futures::ready!(Pin::new(reader).poll_read(cx, buf))?;

                    // If the pipe is closed, retrieve the I/O handle back from the blocking task.
                    // This is not really a required step, but it's cleaner to drop the handle on
                    // the same thread that created it.
                    if n == 0 {
                        // Poll the task to retrieve the I/O handle.
                        let (res, io) = futures::ready!(Pin::new(task).poll(cx));
                        // Make sure to move into the idle state before reporting errors.
                        self.0 = State::Idle(Some(io));
                        res?;
                    }

                    return Poll::Ready(Ok(n));
                }
            }
        }
    }
}

impl<T: Write + Send + 'static> AsyncWrite for Blocking<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match &mut self.0 {
                // If not in idle or active writing state, stop the running task.
                State::WithMut(..)
                | State::Closure(..)
                | State::Writing(None, _)
                | State::Streaming(..)
                | State::Reading(..) => {
                    // Wait for the running task to stop.
                    futures::ready!(self.poll_stop(cx))?;
                }

                // If idle, start the writing task.
                State::Idle(io) => {
                    // Take the I/O handle out to write on a blocking task.
                    let mut io = io.take().expect("inner value was taken out");

                    // This pipe capacity seems to work well in practice. If it's too low, there will
                    // be too much synchronization between tasks. If too high, memory consumption
                    // increases.
                    let (mut reader, writer) = pipe(8 * 1024 * 1024); // 8 MB

                    // Spawn a blocking task that writes and returns the I/O handle when done.
                    let task = Executor::spawn(async move {
                        // Copy bytes from the pipe into the I/O handle until the pipe is closed or an
                        // error occurs. Flush the I/O handle at the end.
                        loop {
                            match future::poll_fn(|cx| reader.poll_read(cx, &mut io)).await {
                                Ok(0) => return (io.flush(), io),
                                Ok(_) => {}
                                Err(err) => {
                                    let _ = io.flush();
                                    return (Err(err), io);
                                }
                            }
                        }
                    });

                    // Move into the busy state and poll again.
                    self.0 = State::Writing(Some(writer), task);
                }

                // If writing, write more bytes into the pipe.
                State::Writing(Some(writer), _) => return Pin::new(writer).poll_write(cx, buf),
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match &mut self.0 {
                // If not in idle state, stop the running task.
                State::WithMut(..)
                | State::Closure(..)
                | State::Streaming(..)
                | State::Writing(..)
                | State::Reading(..) => {
                    // Wait for the running task to stop.
                    futures::ready!(self.poll_stop(cx))?;
                }

                // Idle implies flushed.
                State::Idle(_) => return Poll::Ready(Ok(())),
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // First, make sure the I/O handle is flushed.
        futures::ready!(Pin::new(&mut *self).poll_flush(cx))?;

        // Then move into the idle state with no I/O handle, thus dropping it.
        self.0 = State::Idle(None);
        Poll::Ready(Ok(()))
    }
}

/// Creates a bounded single-producer single-consumer pipe.
///
/// A pipe is a ring buffer of `cap` bytes that can be asynchronously read from and written to.
///
/// When the sender is dropped, remaining bytes in the pipe can still be read. After that, attempts
/// to read will result in `Ok(0)`, i.e. they will always 'successfully' read 0 bytes.
///
/// When the receiver is dropped, the pipe is closed and no more bytes and be written into it.
/// Further writes will result in `Ok(0)`, i.e. they will always 'successfully' write 0 bytes.
fn pipe(cap: usize) -> (Reader, Writer) {
    assert!(cap > 0, "capacity must be positive");
    assert!(cap.checked_mul(2).is_some(), "capacity is too large");

    // Allocate the ring buffer.
    let mut v = Vec::with_capacity(cap);
    let buffer = v.as_mut_ptr();
    mem::forget(v);

    let inner = Arc::new(Pipe {
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        reader: AtomicWaker::new(),
        writer: AtomicWaker::new(),
        closed: AtomicBool::new(false),
        buffer,
        cap,
    });

    let r = Reader {
        inner: inner.clone(),
        head: 0,
        tail: 0,
    };

    let w = Writer {
        inner,
        head: 0,
        tail: 0,
        zeroed_until: 0,
    };

    (r, w)
}

/// The reading side of a pipe.
#[derive(Debug)]
struct Reader {
    /// The inner ring buffer.
    inner: Arc<Pipe>,

    /// The head index, moved by the reader, in the range `0..2*cap`.
    ///
    /// This index always matches `inner.head`.
    head: usize,

    /// The tail index, moved by the writer, in the range `0..2*cap`.
    ///
    /// This index is a snapshot of `index.tail` that might become stale at any point.
    tail: usize,
}

/// The writing side of a pipe.
#[derive(Debug)]
struct Writer {
    /// The inner ring buffer.
    inner: Arc<Pipe>,

    /// The head index, moved by the reader, in the range `0..2*cap`.
    ///
    /// This index is a snapshot of `index.head` that might become stale at any point.
    head: usize,

    /// The tail index, moved by the writer, in the range `0..2*cap`.
    ///
    /// This index always matches `inner.tail`.
    tail: usize,

    /// How many bytes at the beginning of the buffer have been zeroed.
    ///
    /// The pipe allocates an uninitialized buffer, and we must be careful about passing
    /// uninitialized data to user code. Zeroing the buffer right after allocation would be too
    /// expensive, so we zero it in smaller chunks as the writer makes progress.
    zeroed_until: usize,
}

unsafe impl Send for Reader {}
unsafe impl Send for Writer {}

/// The inner ring buffer.
///
/// Head and tail indices are in the range `0..2*cap`, even though they really map onto the
/// `0..cap` range. The distance between head and tail indices is never more than `cap`.
///
/// The reason why indices are not in the range `0..cap` is because we need to distinguish between
/// the pipe being empty and being full. If head and tail were in `0..cap`, then `head == tail`
/// could mean the pipe is either empty or full, but we don't know which!
#[derive(Debug)]
struct Pipe {
    /// The head index, moved by the reader, in the range `0..2*cap`.
    head: AtomicUsize,

    /// The tail index, moved by the writer, in the range `0..2*cap`.
    tail: AtomicUsize,

    /// A waker representing the blocked reader.
    reader: AtomicWaker,

    /// A waker representing the blocked writer.
    writer: AtomicWaker,

    /// Set to `true` if the reader or writer was dropped.
    closed: AtomicBool,

    /// The byte buffer.
    buffer: *mut u8,

    /// The buffer capacity.
    cap: usize,
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // Deallocate the byte buffer.
        unsafe {
            Vec::from_raw_parts(self.buffer, 0, self.cap);
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // Dropping closes the pipe and then wakes the writer.
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.writer.wake();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Dropping closes the pipe and then wakes the reader.
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.reader.wake();
    }
}

impl Reader {
    fn poll_read(&mut self, cx: &mut Context<'_>, mut dest: impl Write) -> Poll<io::Result<usize>> {
        let cap = self.inner.cap;

        // Calculates the distance between two indices.
        let distance = |a: usize, b: usize| {
            if a <= b {
                b - a
            } else {
                2 * cap - (a - b)
            }
        };

        // If the pipe appears to be empty...
        if distance(self.head, self.tail) == 0 {
            // Reload the tail in case it's become stale.
            self.tail = self.inner.tail.load(Ordering::Acquire);

            // If the pipe is now really empty...
            if distance(self.head, self.tail) == 0 {
                // Register the waker.
                self.inner.reader.register(cx.waker());
                atomic::fence(Ordering::SeqCst);

                // Reload the tail after registering the waker.
                self.tail = self.inner.tail.load(Ordering::Acquire);

                // If the pipe is still empty...
                if distance(self.head, self.tail) == 0 {
                    // Check whether the pipe is closed or just empty.
                    if self.inner.closed.load(Ordering::Relaxed) {
                        return Poll::Ready(Ok(0));
                    } else {
                        return Poll::Pending;
                    }
                }
            }
        }

        // The pipe is not empty so remove the waker.
        self.inner.reader.take();

        // Given an index in `0..2*cap`, returns the real index in `0..cap`.
        let real_index = |i: usize| {
            if i < cap {
                i
            } else {
                i - cap
            }
        };

        // Number of bytes read so far.
        let mut count = 0;

        loop {
            // Calculate how many bytes to read in this iteration.
            let n = (128 * 1024) // Not too many bytes in one go - better to wake the writer soon!
                .min(distance(self.head, self.tail)) // No more than bytes in the pipe.
                .min(cap - real_index(self.head)); // Don't go past the buffer boundary.

            // Create a slice of data in the pipe buffer.
            let pipe_slice =
                unsafe { slice::from_raw_parts(self.inner.buffer.add(real_index(self.head)), n) };

            // Copy bytes from the pipe buffer into `dest`.
            let n = dest
                .write(pipe_slice)
                .expect("shouldn't fail because `dest` is a slice");
            count += n;

            // If pipe is empty or `dest` is full, return.
            if n == 0 {
                return Poll::Ready(Ok(count));
            }

            // Move the head forward.
            if self.head + n < 2 * cap {
                self.head += n;
            } else {
                self.head = 0;
            }

            // Store the current head index.
            self.inner.head.store(self.head, Ordering::Release);

            // Wake the writer because the pipe is not full.
            self.inner.writer.wake();
        }
    }
}

impl Writer {
    fn poll_write(&mut self, cx: &mut Context<'_>, mut src: impl Read) -> Poll<io::Result<usize>> {
        // Just a quick check if the pipe is closed, which is why a relaxed load is okay.
        if self.inner.closed.load(Ordering::Relaxed) {
            return Poll::Ready(Ok(0));
        }

        // Calculates the distance between two indices.
        let cap = self.inner.cap;
        let distance = |a: usize, b: usize| {
            if a <= b {
                b - a
            } else {
                2 * cap - (a - b)
            }
        };

        // If the pipe appears to be full...
        if distance(self.head, self.tail) == cap {
            // Reload the head in case it's become stale.
            self.head = self.inner.head.load(Ordering::Acquire);

            // If the pipe is now really empty...
            if distance(self.head, self.tail) == cap {
                // Register the waker.
                self.inner.writer.register(cx.waker());
                atomic::fence(Ordering::SeqCst);

                // Reload the head after registering the waker.
                self.head = self.inner.head.load(Ordering::Acquire);

                // If the pipe is still full...
                if distance(self.head, self.tail) == cap {
                    // Check whether the pipe is closed or just full.
                    if self.inner.closed.load(Ordering::Relaxed) {
                        return Poll::Ready(Ok(0));
                    } else {
                        return Poll::Pending;
                    }
                }
            }
        }

        // The pipe is not full so remove the waker.
        self.inner.writer.take();

        // Given an index in `0..2*cap`, returns the real index in `0..cap`.
        let real_index = |i: usize| {
            if i < cap {
                i
            } else {
                i - cap
            }
        };

        // Number of bytes written so far.
        let mut count = 0;

        loop {
            // Calculate how many bytes to write in this iteration.
            let n = (128 * 1024) // Not too many bytes in one go - better to wake the reader soon!
                .min(self.zeroed_until * 2 + 4096) // Don't zero too many bytes when starting.
                .min(cap - distance(self.head, self.tail)) // No more than space in the pipe.
                .min(cap - real_index(self.tail)); // Don't go past the buffer boundary.

            // Create a slice of available space in the pipe buffer.
            let pipe_slice_mut = unsafe {
                let from = real_index(self.tail);
                let to = from + n;

                // Make sure all bytes in the slice are initialized.
                if self.zeroed_until < to {
                    self.inner
                        .buffer
                        .add(self.zeroed_until)
                        .write_bytes(0u8, to - self.zeroed_until);
                    self.zeroed_until = to;
                }

                slice::from_raw_parts_mut(self.inner.buffer.add(from), n)
            };

            // Copy bytes from `src` into the piper buffer.
            let n = src
                .read(pipe_slice_mut)
                .expect("shouldn't fail because `src` is a slice");
            count += n;

            // If the pipe is full or `src` is empty, return.
            if n == 0 {
                return Poll::Ready(Ok(count));
            }

            // Move the tail forward.
            if self.tail + n < 2 * cap {
                self.tail += n;
            } else {
                self.tail = 0;
            }

            // Store the current tail index.
            self.inner.tail.store(self.tail, Ordering::Release);

            // Wake the reader because the pipe is not empty.
            self.inner.reader.wake();
        }
    }
}
