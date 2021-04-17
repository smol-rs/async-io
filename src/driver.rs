use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use once_cell::sync::Lazy;

use crate::reactor::Reactor;

/// Number of currently active `block_on()` invocations.
pub(crate) static BLOCK_ON_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Unparker for the "async-io" thread.
pub(crate) static UNPARKER: Lazy<parking::Unparker> = Lazy::new(|| {
    let (parker, unparker) = parking::pair();

    // Spawn a helper thread driving the reactor.
    //
    // Note that this thread is not exactly necessary, it's only here to help push things
    // forward if there are no `Parker`s around or if `Parker`s are just idling and never
    // parking.
    thread::Builder::new()
        .name("async-io".to_string())
        .spawn(move || main_loop(parker))
        .expect("cannot spawn async-io thread");

    unparker
});

/// Initializes the "async-io" thread.
pub(crate) fn init() {
    Lazy::force(&UNPARKER);
}

/// The main loop for the "async-io" thread.
fn main_loop(parker: parking::Parker) {
    // The last observed reactor tick.
    let mut last_tick = 0;
    // Number of sleeps since this thread has called `react()`.
    let mut sleeps = 0u64;

    loop {
        let tick = Reactor::get().ticker();

        if last_tick == tick {
            let reactor_lock = if sleeps >= 10 {
                // If no new ticks have occurred for a while, stop sleeping and spinning in
                // this loop and just block on the reactor lock.
                Some(Reactor::get().lock())
            } else {
                Reactor::get().try_lock()
            };

            if let Some(mut reactor_lock) = reactor_lock {
                log::trace!("main_loop: waiting on I/O");
                reactor_lock.react(None).ok();
                last_tick = Reactor::get().ticker();
                sleeps = 0;
            }
        } else {
            last_tick = tick;
        }

        if BLOCK_ON_COUNT.load(Ordering::SeqCst) > 0 {
            // Exponential backoff from 50us to 10ms.
            let delay_us = [50, 75, 100, 250, 500, 750, 1000, 2500, 5000]
                .get(sleeps as usize)
                .unwrap_or(&10_000);

            log::trace!("main_loop: sleeping for {} us", delay_us);
            if parker.park_timeout(Duration::from_micros(*delay_us)) {
                log::trace!("main_loop: notified");

                // If notified before timeout, reset the last tick and the sleep counter.
                last_tick = Reactor::get().ticker();
                sleeps = 0;
            } else {
                sleeps += 1;
            }
        }
    }
}
