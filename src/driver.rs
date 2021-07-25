use std::cell::Cell;
use std::future::Future;
use std::mem;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

use futures_lite::pin;
use once_cell::sync::OnceCell;
use waker_fn::waker_fn;

use crate::reactor::Reactor;
use crate::reactor::ReactorLock;

#[derive(Debug)]
enum State {
    /// Currently disabled and not yet init()ed
    Inactive,
    /// Reactor lock is free for the taking
    Idle,
    /// Main thread will always handle the reactor
    DedicatedReactor,
    /// Reactor lock is taken by the thread that set the state to this value
    Polling { wakers: Vec<Arc<BlockOn>> },
    /// Reactor lock should be taken by the leader
    Handoff {
        leader: Arc<BlockOn>,
        wakers: Vec<Arc<BlockOn>>,
    },
}
// Note: this uses Vec instead of something like LinkedHashSet because we assume the number of
// block_on threads is relatively small.  If you have dozens of block_on threads, you should use
// the dedicated reactor mode.

#[derive(Debug)]
struct MainThreadStateInner {
    thread_enabled: bool,
    state: State,
}

/// Controller for the "async-io" thread.
#[derive(Debug)]
struct MainThreadState {
    lock: Mutex<MainThreadStateInner>,
    cond: Condvar,
}

impl MainThreadState {
    fn new() -> Self {
        let inner = MainThreadStateInner {
            state: State::Inactive,
            thread_enabled: true,
        };
        MainThreadState {
            lock: Mutex::new(inner),
            cond: Condvar::new(),
        }
    }

    fn get() -> &'static Self {
        static MAIN_THREAD: OnceCell<MainThreadState> = OnceCell::new();
        MAIN_THREAD.get_or_init(Self::new)
    }

    fn spawn(&self) {
        // Spawn a helper thread driving the reactor.
        //
        // Note that this thread is only necessary if you poll a future outside of block_on.
        thread::Builder::new()
            .name("async-io".to_string())
            .spawn(main_loop)
            .expect("cannot spawn async-io thread");
    }

    fn start(&self) {
        let mut lock = self.lock.lock().unwrap();
        if let State::Inactive = lock.state {
            lock.state = State::Idle;
            if lock.thread_enabled {
                self.spawn();
            }
        }
    }

    fn disable(&self) {
        let mut lock = self.lock.lock().unwrap();
        if lock.thread_enabled {
            match lock.state {
                State::Inactive => {}
                State::DedicatedReactor => {
                    // Dedicated reactor cannot be disabled
                    return;
                }
                _ => {
                    Reactor::get().notify();
                    self.cond.notify_one();
                }
            }
            lock.thread_enabled = false;
        }
    }

    fn enable(&self) {
        let lock = self.lock.lock().unwrap();
        if !lock.thread_enabled {
            match lock.state {
                State::Inactive => {}
                _ => {
                    self.spawn();
                }
            }
        }
    }

    fn start_dedicated(&self) {
        let mut lock = self.lock.lock().unwrap();
        match lock.state {
            State::DedicatedReactor => {
                return;
            }
            State::Inactive => {
                self.spawn();
            }
            _ => {
                if lock.thread_enabled {
                    self.cond.notify_one();
                } else {
                    self.spawn();
                }
            }
        }
        lock.thread_enabled = true;
        lock.state = State::DedicatedReactor;
    }

    /// Try to become the leader and get the reactor lock.
    ///
    /// If this function returns Some, the caller is the leader.
    ///
    /// If this function returns None, the calller was added to a list of waiting pollers, and in
    /// the future may be nominated to become the leader.
    fn get_reactor(&self, parker: &Arc<BlockOn>) -> Option<ReactorLock<'static>> {
        let mut lock = self.lock.lock().unwrap();
        match &mut lock.state {
            State::DedicatedReactor => {
                return None;
            }
            State::Idle => {
                lock.state = State::Polling { wakers: Vec::new() };
                drop(lock);
                // Nobody else is allowed to hold this lock for a long duration (although they
                // might hold it in the optimistic lock), so this will not block for long.
                return Some(Reactor::get().lock());
            }
            State::Inactive => {
                // same as above, but kick off the main thread if enabled
                lock.state = State::Polling { wakers: Vec::new() };
                if lock.thread_enabled {
                    self.spawn();
                }
                drop(lock);
                return Some(Reactor::get().lock());
            }
            State::Handoff { leader, wakers } if Arc::ptr_eq(leader, parker) => {
                // complete the handoff
                let wakers = mem::replace(wakers, Vec::new());
                lock.state = State::Polling { wakers };
                drop(lock);
                return Some(Reactor::get().lock());
            }
            State::Handoff { wakers, .. } | State::Polling { wakers } => {
                // another thready is responsible for running the reactor.  Add our parker to the
                // end of the list because it was most recently active.
                wakers.retain(|w| !Arc::ptr_eq(w, parker));
                wakers.push(parker.clone());
                drop(lock);
                return None;
            }
        }
    }

    /// Unconditionally nominate another thread as the leader, falling back to the main thread if
    /// no block_on threads are available.
    ///
    /// Caller must be the current leader.
    fn handoff(&self) {
        let mut lock = self.lock.lock().unwrap();
        match &mut lock.state {
            State::Polling { wakers } => {
                // the last state in the list was the most recently active, and so will respond
                // fastest.
                if let Some(leader) = wakers.pop() {
                    leader.nomination_wake();
                    let wakers = mem::replace(wakers, Vec::new());
                    lock.state = State::Handoff { leader, wakers };
                } else {
                    // Back to the main thread, if any
                    lock.state = State::Idle;
                    self.cond.notify_one();
                }
            }
            State::DedicatedReactor => {}
            invalid => unreachable!("invalid state {:?}", invalid),
        }
    }

    /// Try to nominate another thread as the leader.
    ///
    /// Caller must be the current leader.
    ///
    /// If this function returns false, the caller remains the leader.
    ///
    /// If this function returns true, the caller is demoted to a waiter, but may still be
    /// nominated as a replacement leader if no other threads are active.
    fn try_handoff(&self, parker: &Arc<BlockOn>) -> bool {
        let mut lock = self.lock.lock().unwrap();
        match &mut lock.state {
            State::Polling { wakers } => {
                // the last state in the list is most likely to be responsible
                if let Some(leader) = wakers.pop() {
                    wakers.push(parker.clone());
                    leader.nomination_wake();
                    let wakers = mem::replace(wakers, Vec::new());
                    lock.state = State::Handoff { leader, wakers };
                    return true;
                }
            }
            State::DedicatedReactor => {
                return true;
            }
            invalid => unreachable!("invalid state {:?}", invalid),
        }
        false
    }

    /// Unconditionally remove the given parker from the list of waiting BlockOn instances.
    fn disqualify(&self, parker: &Arc<BlockOn>) {
        let mut lock = self.lock.lock().unwrap();
        match &mut lock.state {
            State::Handoff { leader, wakers } if Arc::ptr_eq(leader, parker) => {
                // We were nominated as the leader but got a task wakeup before we could lead.
                // Either find a new leader or mark the lock as free.
                if let Some(new_leader) = wakers.pop() {
                    new_leader.nomination_wake();
                    *leader = new_leader;
                } else {
                    lock.state = State::Idle;
                    self.cond.notify_one();
                }
            }
            State::Handoff { wakers, .. } | State::Polling { wakers } => {
                // normal case: we were waiting, now we aren't
                wakers.retain(|w| !Arc::ptr_eq(w, parker));
            }
            State::DedicatedReactor => {}
            invalid => unreachable!("invalid state {:?}", invalid),
        }
    }
}

/// Disable the "async-io" thread.
///
/// This will signal the thread to stop if it is running, but does not wait for it to terminate.
///
/// Note: if this thread is disabled, any Future not run using [block_on] may block forever even if
/// I/O is available.
pub fn disable_park_thread() {
    MainThreadState::get().disable();
}

/// Enable the "async-io" thread.
///
/// This will start the thread if there are any I/O objects that might need waking.
pub fn enable_park_thread() {
    MainThreadState::get().enable();
}

/// Start the "async-io" thread and give it exclusive ownership of the reactor.
///
/// This can be more efficient if you have many threads using block_on, but normally just causes
/// extra wakeups as the I/O is redirected between threads.
pub fn start_dedicated_thread() {
    MainThreadState::get().start_dedicated();
}

/// Initializes the "async-io" thread.
pub(crate) fn init() {
    MainThreadState::get().start();
}

/// The main loop for the "async-io" thread.
fn main_loop() {
    let state = MainThreadState::get();
    let mut lock = state.lock.lock().unwrap();
    let mut is_polling = false;
    let mut last_tick = 0;

    while lock.thread_enabled {
        match &mut lock.state {
            State::DedicatedReactor => {
                drop(lock);
                let mut reactor_lock = Reactor::get().lock();
                log::trace!("main_loop: dedicated reactor");
                loop {
                    reactor_lock.react(None).ok();
                }
            }
            State::Idle => {
                let tick = Reactor::get().ticker();
                if last_tick == tick {
                    // no evidence of anyone polling - we should start
                    lock.state = State::Polling { wakers: Vec::new() };
                    is_polling = true;
                } else {
                    // someone else advanced the tick counter
                    last_tick = tick;
                    drop(lock);
                    // Give a block_on thread 10ms to start polling or to advance the tick counter.
                    // Otherwise, we will take over ticking again.
                    std::thread::sleep(Duration::from_millis(10));
                    lock = state.lock.lock().unwrap();
                }
            }
            State::Polling { wakers } if is_polling => {
                if let Some(leader) = wakers.pop() {
                    is_polling = false;
                    leader.nomination_wake();
                    let wakers = mem::replace(wakers, Vec::new());
                    lock.state = State::Handoff { leader, wakers };
                } else {
                    // empty, main_thread should poll
                    drop(lock);
                    let mut reactor_lock = Reactor::get().lock();

                    log::trace!("main_loop: waiting on I/O");
                    reactor_lock.react(None).ok();
                    last_tick = Reactor::get().ticker();
                    drop(reactor_lock);

                    lock = state.lock.lock().unwrap();
                }
            }
            State::Handoff { .. } | State::Polling { .. } => {
                lock = state.cond.wait(lock).unwrap();
            }
            State::Inactive => {
                unreachable!("inactive?");
            }
        }
    }
    log::trace!("main_loop: exiting");
}

#[derive(Debug)]
struct BlockOn {
    mutex: Mutex<BlockOnInner>,
    cond: Condvar,
}

#[derive(Debug, Default)]
struct BlockOnInner {
    is_polling: bool,
    is_task: bool,
    is_nominated: bool,
}

thread_local! {
    // Indicates that the current thread is polling I/O, but not necessarily blocked on it.
    static IO_POLLING: Cell<bool> = Cell::new(false);
}

impl BlockOn {
    fn new() -> Arc<Self> {
        Arc::new(BlockOn {
            mutex: Mutex::new(BlockOnInner::default()),
            cond: Condvar::new(),
        })
    }

    fn task_wake(&self) {
        let mut lock = self.mutex.lock().unwrap();
        if lock.is_task {
            // already woken, no-op
            return;
        }
        lock.is_task = true;
        if !IO_POLLING.with(Cell::get) && lock.is_polling {
            // This wake is coming from another thread and may also need to wake via the reactor
            Reactor::get().notify();
        }
        self.cond.notify_one();
    }

    fn nomination_wake(&self) {
        let mut lock = self.mutex.lock().unwrap();
        lock.is_nominated = true;
        self.cond.notify_one();
    }

    fn guard<'a>(self: &'a Arc<Self>) -> BlockOnGuard<'a> {
        BlockOnGuard { inner: self }
    }

    fn take_wake(&self) -> bool {
        let mut lock = self.mutex.lock().unwrap();
        let prev = lock.is_task;
        lock.is_task = false;
        prev
    }
}

struct BlockOnGuard<'a> {
    inner: &'a Arc<BlockOn>,
}

impl<'a> BlockOnGuard<'a> {
    fn try_start_polling(&mut self, is_polling: bool) -> bool {
        let mut lock = self.inner.mutex.lock().unwrap();
        debug_assert!(!lock.is_polling);
        let cancel = lock.is_task;
        lock.is_polling = is_polling;
        lock.is_task = false;
        IO_POLLING.with(|io| io.set(is_polling));

        !cancel
    }

    fn try_handoff(&mut self) -> bool {
        if MainThreadState::get().try_handoff(self.inner) {
            let mut lock = self.inner.mutex.lock().unwrap();
            debug_assert!(lock.is_polling);
            lock.is_polling = false;
            IO_POLLING.with(|io| io.set(false));
            true
        } else {
            false
        }
    }

    fn sleep(&self) -> bool {
        let mut lock = self.inner.mutex.lock().unwrap();
        loop {
            debug_assert!(!lock.is_polling);
            if lock.is_task {
                lock.is_task = false;
                lock.is_nominated = false;
                return true;
            }
            if lock.is_nominated {
                lock.is_nominated = false;
                return false;
            }
            lock = self.inner.cond.wait(lock).unwrap();
        }
    }
}

impl<'a> Drop for BlockOnGuard<'a> {
    fn drop(&mut self) {
        IO_POLLING.with(|io| io.set(false));
        let mut lock = self.inner.mutex.lock().unwrap();
        let was_polling = lock.is_polling;
        lock.is_polling = false;
        drop(lock);
        if was_polling {
            MainThreadState::get().handoff();
        } else {
            MainThreadState::get().disqualify(self.inner);
        }
    }
}

/// Blocks the current thread on a future, processing I/O events when idle.
///
/// # Examples
///
/// ```
/// use async_io::Timer;
/// use std::time::Duration;
///
/// async_io::block_on(async {
///     Timer::after(Duration::from_millis(1)).await;
///     // The second timer will likely be processed by the current
///     // thread rather than the fallback "async-io" thread.
///     Timer::after(Duration::from_millis(1)).await;
/// });
/// ```
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    let state = MainThreadState::get();
    log::trace!("block_on()");

    let parker = BlockOn::new();

    // Prepare the waker.
    let waker = waker_fn({
        let parker = parker.clone();
        move || {
            parker.task_wake();
        }
    });
    let cx = &mut Context::from_waker(&waker);
    pin!(future);

    'future: loop {
        // Poll the future.
        if let Poll::Ready(t) = future.as_mut().poll(cx) {
            log::trace!("block_on: completed");
            return t;
        }

        // Check if a notification was received.
        if parker.take_wake() {
            log::trace!("block_on: notified during poll");

            // Optimistically try grabbing a lock on the reactor to process I/O events.
            if let Some(mut reactor_lock) = Reactor::get().try_lock() {
                // First let wakers know this parker is processing I/O events.
                IO_POLLING.with(|io| io.set(true));
                let _guard = CallOnDrop(|| {
                    IO_POLLING.with(|io| io.set(false));
                });

                // Process available I/O events.
                reactor_lock.react(Some(Duration::from_secs(0))).ok();
            }
            continue 'future;
        }

        let mut guard = parker.guard();
        'reactor: loop {
            // Ensure someone is taking care of I/O
            let reactor_lock = state.get_reactor(&parker);

            if !guard.try_start_polling(reactor_lock.is_some()) {
                // A notification was received before `io_blocked` was updated, and so won't
                // interrupt the react() call.
                log::trace!("block_on: notified pre-wait");
                continue 'future;
            }

            if let Some(mut reactor_lock) = reactor_lock {
                // Record the instant at which the lock was grabbed.
                let start = Instant::now();
                for count in 0..=u32::MAX {
                    // Wait for I/O events.
                    log::trace!("block_on: waiting on I/O");
                    reactor_lock.react(None).ok();

                    // Check if a notification has been received.
                    if parker.take_wake() {
                        log::trace!("block_on: notified");
                        continue 'future;
                    }

                    // Check if this thread been handling many I/O events for a long time.
                    if start.elapsed() > Duration::from_micros(500) && count > 2 {
                        // This thread is clearly processing I/O events for some other threads
                        // because it didn't get a notification yet.  Try to hand off the reactor
                        // to one of the active threads.
                        if guard.try_handoff() {
                            log::trace!("block_on: stops hogging the reactor");
                            drop(reactor_lock);

                            let full_wake = guard.sleep();
                            if full_wake {
                                continue 'future;
                            } else {
                                continue 'reactor;
                            }
                        }
                    }
                }
                continue 'future;
            } else {
                log::trace!("block_on: sleep until notification");
                let full_wake = guard.sleep();
                if full_wake {
                    continue 'future;
                } else {
                    continue 'reactor;
                }
            }
        }
    }
}

/// Runs a closure when dropped.
struct CallOnDrop<F: Fn()>(F);

impl<F: Fn()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
