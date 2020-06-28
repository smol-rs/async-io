use std::time::{Duration, Instant};

use async_io::Timer;
use blocking::block_on;

#[test]
fn timer_at() {
    let before = block_on(async {
        let now = Instant::now();
        let when = now + Duration::from_secs(1);
        Timer::at(when).await;
        now
    });

    assert!(before.elapsed() >= Duration::from_secs(1));
}

#[test]
fn timer_after() {
    let before = block_on(async {
        let now = Instant::now();
        Timer::after(Duration::from_secs(1)).await;
        now
    });

    assert!(before.elapsed() >= Duration::from_secs(1));
}
