use crate::reactor::OperationStatus;

use super::poll::{Reactor as PollReactor, ReactorLock as PollReactorLock};
use super::{Source, MAX_OPERATIONS};

use std::io;
use std::mem::MaybeUninit;
use std::os::unix::io::{AsRawFd, RawFd};
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};
use std::time::{Duration, Instant};

use io_uring::cqueue::Entry as CompletionEntry;
use io_uring::squeue::Entry as SubmissionEntry;
use io_uring::types::{Fd, SubmitArgs, Timespec};
use io_uring::{CompletionQueue, IoUring};

use concurrent_queue::ConcurrentQueue;
use polling::Poller;

const EPOLL_KEY: u64 = std::u64::MAX;
const EPOLL_IN: libc::c_short = libc::POLLIN | libc::POLLPRI | libc::POLLHUP | libc::POLLERR;

const OPERATION_SHIFT: u64 = (std::mem::size_of::<u64>() - 1) as u64;
const OPERATION_MASK: u64 = 0b1 << OPERATION_SHIFT;

/// A `Reactor` that augments the usual `polling`-based approach
/// with `io_uring`-based I/O.
///
/// This is useful for quick reads and writes that do not require
/// the total flexibility of `epoll`. If `io_uring` is not available,
/// it falls back to the usual `polling`-based reactor.
pub(super) struct Reactor {
    /// The internal `polling`-based reactor.
    ///
    /// This is used for all I/O that is not compatible with `io_uring`.
    polling: PollReactor,
    /// Interface to the `io_uring` system API.
    ///
    /// This is `None` if we failed to initialize the `io_uring` library,
    /// likely due to not using a compatible version of Linux.
    uring: Option<Uring>,
}

impl Reactor {
    /// Creates a new `Reactor` instance.
    pub(super) fn new() -> Self {
        const DEFAULT_RING_CAPACITY: u32 = 1024;

        Reactor {
            polling: PollReactor::new(),
            uring: IoUring::new(DEFAULT_RING_CAPACITY).ok().map(|io_uring| {
                Uring {
                    io_uring,
                    submit_lock: Mutex::new(()),
                    completion_buffer: Mutex::new({
                        // SAFETY: MaybeUninit is allowed to be uninitialized
                        let mut buffer = Vec::with_capacity(1024);
                        unsafe {
                            buffer.set_len(1024);
                        }
                        buffer.into_boxed_slice()
                    }),
                    querying_epoll: AtomicBool::new(false),
                    submission_wait: ConcurrentQueue::unbounded(),
                }
            }),
        }
    }

    /// Get the `Poller` backing this reactor.
    pub(super) fn poller(&self) -> &Poller {
        self.polling.poller()
    }

    /// Returns the current ticker.
    pub(super) fn ticker(&self) -> usize {
        self.polling.ticker()
    }

    /// Registers an I/O source in the reactor.
    pub(super) fn insert_io(&self, raw: RawFd) -> io::Result<Arc<Source>> {
        self.polling.insert_io(raw)
    }

    /// Deregisters an I/O source from the reactor.
    pub(super) fn remove_io(&self, source: &Source) -> io::Result<()> {
        self.polling.remove_io(source)
    }

    /// Registers a timer in the reactor.
    ///
    /// Returns the ID of the timer.
    pub(super) fn insert_timer(&self, when: Instant, waker: &Waker) -> usize {
        self.polling.insert_timer(when, waker)
    }

    /// Deregisters a timer from the reactor.
    pub(super) fn remove_timer(&self, when: Instant, id: usize) {
        self.polling.remove_timer(when, id);
    }

    /// Notifies the thread blocked on the reactor.
    pub(super) fn notify(&self) {
        self.polling.notify();
    }

    /// Acquires a lock on the reactor.
    pub(super) fn lock(&self) -> ReactorLock<'_> {
        let reactor = self;
        let inner = self.polling.lock();
        ReactorLock { inner, reactor }
    }

    /// Tries to acquire a lock on the reactor.
    pub(super) fn try_lock(&self) -> Option<ReactorLock<'_>> {
        self.polling.try_lock().map(|inner| {
            let reactor = self;
            ReactorLock { inner, reactor }
        })
    }
}

/// A lock on the polling capabilities of the `Reactor`.
pub(super) struct ReactorLock<'a> {
    /// The inner lock on the `PollReactor`.
    inner: PollReactorLock<'a>,
    /// A reference to the main reactor.
    reactor: &'a Reactor,
}

impl<'a> ReactorLock<'a> {
    /// Processes new events, blocking until the first event or timeout.
    pub(super) fn react(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        if let Some(ref uring) = self.reactor.uring {
            log::trace!("react: beginning uring: {:?}", timeout);

            // Prepare timers/deadline for polling.
            let mut wakers = vec![];
            let (timeout, tick) = self.inner.prepare_for_polling(timeout, &mut wakers);

            // Register polling into io_uring if we haven't already.
            uring.register_polling(self.reactor.poller());

            // Wait for io_uring events.
            let submitter = uring.io_uring.submitter();
            let mut args = SubmitArgs::new();

            let timespec = timeout.map(cvt_timeout);
            if let Some(ref timespec) = timespec {
                args = args.timespec(timespec);
            }

            // Wait for at least one event.
            let res = match submitter.submit_with_args(1, &args) {
                Ok(_) => {
                    // Process the events that we've received.
                    let mut buffer = uring.completion_buffer.lock().unwrap();
                    // SAFETY: we hold the lock, we can read the completion queue
                    let mut completion_queue = unsafe { uring.io_uring.completion_shared() };

                    // If there are not events, the timer deadline must have fired.
                    // Check to see if we need to wake any timers.
                    if completion_queue.is_empty() && timeout != Some(Duration::from_secs(0)) {
                        self.reactor.polling.process_timers(&mut wakers);
                    }

                    self.process_queue(uring, tick, &mut completion_queue, &mut buffer, &mut wakers)
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(()),
                Err(e) => Err(e),
            };

            // Wake up ready tasks.
            log::trace!("react: {} ready wakers", wakers.len());
            for waker in wakers {
                // Prevent a panicking waker from blowing up.
                panic::catch_unwind(|| waker.wake()).ok();
            }

            res
        } else {
            // Fall back to the polling reactor.
            self.inner.react(timeout)
        }
    }

    /// Process the events received from `io_uring`.
    fn process_queue(
        &mut self,
        uring: &Uring,
        tick: usize,
        completion_queue: &mut CompletionQueue<'_>,
        buffer: &mut [MaybeUninit<CompletionEntry>],
        wakers: &mut Vec<Waker>,
    ) -> io::Result<()> {
        let mut res = Ok(());

        // Use the associated method here to avoid using the
        // iterator's method.
        while !CompletionQueue::is_empty(completion_queue) {
            let entries = completion_queue.fill(buffer);

            // Iterate over the entries that we've received.
            for entry in entries {
                let data = entry.user_data();

                // If this is the key used for epoll, process
                // the epoll events.
                if data == EPOLL_KEY {
                    // We are no longer querying epoll.
                    uring.querying_epoll.store(false, Ordering::SeqCst);

                    res = res.and(self.inner.pump_events(
                        Some(Duration::from_secs(0)),
                        tick,
                        wakers,
                    ));
                } else {
                    let sources = self.reactor.polling.sources.lock().unwrap();

                    // determine the operation involved in the event
                    // as well as the source we need
                    let key = (data & !OPERATION_MASK) as usize;
                    let operation = ((data & OPERATION_MASK) >> OPERATION_SHIFT) as usize;

                    if let Some(source) = sources.get(key) {
                        // Get the operation in question.
                        let mut state = source.state.lock().unwrap();
                        let operation = &mut state.operations[operation];

                        // Indicate that the operation is complete, and wake the waker
                        // if there is one.
                        operation.status = OperationStatus::Complete(entry.result() as isize);
                        wakers.extend(operation.waker.take());
                    }
                }
            }

            // Also, since there is now more room in the submission queue,
            // we can submit more events.
            while let Ok(waker) = uring.submission_wait.pop() {
                wakers.push(waker);
            }
        }

        res
    }
}

struct Uring {
    /// The interface to the `io_uring` system API.
    io_uring: IoUring,
    /// A lock to protect the submission queue.
    ///
    /// Holding this lock implies the exclusive right to submit new
    /// I/O operations to the `io_uring`.
    submit_lock: Mutex<()>,
    /// A buffer used to hold completion queue events.
    ///
    /// Holding this lock implies the exclusive right to read from the
    /// completion queue.
    completion_buffer: Mutex<Box<[MaybeUninit<CompletionEntry>]>>,
    /// Whether or not there is currently an entry in the ring
    /// for polling `epoll`.
    querying_epoll: AtomicBool,
    /// Tasks waiting on there to be more room in the submission queue.
    submission_wait: ConcurrentQueue<Waker>,
}

impl Uring {
    /// Register the `polling` instance into the ring if it hasn't
    /// been already.
    fn register_polling(&self, poller: &Poller) {
        if !self.querying_epoll.swap(true, Ordering::SeqCst) {
            // Create a submission queue entry for the instance.
            let entry = io_uring::opcode::PollAdd::new(Fd(poller.as_raw_fd()), EPOLL_IN as _)
                .build()
                .user_data(EPOLL_KEY);

            // Acquire the lock to submit new operations to the ring.
            let _guard = self.submit_lock.lock().unwrap();

            // SAFETY: We have acquired the lock, so we are the only ones
            // submitting new operations to the ring.
            let mut submit_queue = unsafe { self.io_uring.submission_shared() };

            // SAFETY: `polling::as_raw_fd()` is a valid file descriptor that will
            // remain valid for the lifetime of this entry.
            unsafe {
                submit_queue
                    .push(&entry)
                    .expect("No room left for the polling entry");
            }
        }
    }

    /// Register an event from the given source, of the given operation.
    ///
    /// # Safety
    ///
    /// The `Entry` should be a valid entry.
    unsafe fn submit_entry(
        &self,
        key: usize,
        operation: usize,
        entry: SubmissionEntry,
        task: &mut Context<'_>,
    ) -> bool {
        // Add user data combining the source's key and the operation key
        // to the entry.
        debug_assert!(key as u64 & OPERATION_MASK == 0);
        let data = (key as u64) | ((operation as u64) << OPERATION_SHIFT);
        debug_assert_ne!(data, EPOLL_KEY);

        let entry = entry.user_data(data);

        // Lock the submission queue.
        let _guard = self.submit_lock.lock().unwrap();
        // SAFETY: We have acquired the lock, so we can access the queue.
        let mut submit_queue = self.io_uring.submission_shared();

        // If the queue is almost full, wait for space to become available.
        //
        // We always leave one space available for the epoll entry at the end.
        // This way, nothing will ever hamper the epoll entry being added.
        if submit_queue.len() >= submit_queue.capacity() - 1 {
            self.submission_wait.push(task.waker().clone()).ok();
            return false;
        }

        // Push the entry to the queue.
        // SAFETY: The caller asserts that `entry` is a valid entry.
        submit_queue
            .push(&entry)
            .expect("Submit queue cannot be pushed to.");

        true
    }
}

/// Convert a `Duration` to a `timespec` suitable for passing to `io_uring_submit`.
fn cvt_timeout(timeout: Duration) -> Timespec {
    Timespec::new()
        .sec(timeout.as_secs())
        .nsec(timeout.subsec_nanos())
}
