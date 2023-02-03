//! Uses the `async_io::os::kqueue` module to wait for a process to terminate.
//!
//! Run with:
//!
//! ```
//! cargo run --example kqueue-process
//! ```

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
fn main() -> std::io::Result<()> {
    use std::process::Command;

    use async_io::os::kqueue::{AsyncKqueueExt, Exit};
    use async_io::Async;
    use futures_lite::future;

    future::block_on(async {
        // Spawn a process.
        let process = Command::new("sleep")
            .arg("3")
            .spawn()
            .expect("failed to spawn process");

        // Wrap the process in an `Async` object that waits for it to exit.
        let process = Async::with_filter(Exit::new(process))?;

        // Wait for the process to exit.
        process.readable().await?;

        Ok(())
    })
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
)))]
fn main() {
    println!("This example only works for kqueue-enabled platforms.");
}
