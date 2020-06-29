use std::time::{Duration, Instant};

use async_io::Timer;
use blocking::block_on;

#[test]
fn timer() {
    block_on(async {
        let start = Instant::now();
        Timer::new(Duration::from_secs(1)).await;
        assert!(start.elapsed() >= Duration::from_secs(1));
    });
}
