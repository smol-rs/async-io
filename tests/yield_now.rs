use async_io::{yield_now, Timer};
use std::time::{Duration, Instant};

#[test]
fn yield_now_returns_without_blocking() {
    let start = Instant::now();
    for _ in 0..10 {
        yield_now();
    }
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn yield_now_drives_timer() {
    let start = Instant::now();
    let timer = Timer::after(Duration::from_millis(50));
    futures_lite::pin!(timer);

    let waker = futures_lite::future::block_on(async { std::task::Waker::noop().clone() });
    let mut cx = std::task::Context::from_waker(&waker);

    use std::future::Future;
    while timer.as_mut().poll(&mut cx).is_pending() {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("timer never fired");
        }
        yield_now();
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(start.elapsed() >= Duration::from_millis(50));
}
