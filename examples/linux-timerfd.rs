//! Uses the `timerfd` crate to sleep using an OS timer.
//!
//! Run with:
//!
//! ```
//! cargo run --example linux-timerfd
//! ```

#[cfg(all(feature = "io", target_os = "linux"))]
fn main() -> std::io::Result<()> {
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::time::{Duration, Instant};

    use async_io::Async;
    use futures_lite::future;
    use timerfd::{SetTimeFlags, TimerFd, TimerState};

    /// Sleeps using an OS timer.
    async fn sleep(dur: Duration) -> io::Result<()> {
        // Create an OS timer.
        let mut timer = TimerFd::new()?;
        timer.set_state(TimerState::Oneshot(dur), SetTimeFlags::Default);

        // When the OS timer fires, a 64-bit integer can be read from it.
        Async::new(timer)?
            .read_with(|t| nix::unistd::read(t.as_raw_fd(), &mut [0u8; 8]).map_err(io::Error::from))
            .await?;
        Ok(())
    }

    future::block_on(async {
        let start = Instant::now();
        println!("Sleeping...");

        // Sleep for a second using an OS timer.
        sleep(Duration::from_secs(1)).await?;

        println!("Woke up after {:?}", start.elapsed());
        Ok(())
    })
}

#[cfg(not(all(feature = "io", target_os = "linux")))]
fn main() {
    println!("This example works only on Linux!");
}
