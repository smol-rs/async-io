//! Uses the `async_io::os::kqueue` module to wait for a process to terminate.
//!
//! Run with:
//!
//! ```
//! cargo run --example kqueue-process
//! ```

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
fn main() -> std::io::Result<()> {
    use std::{num::NonZeroI32, process::Command};

    use async_io::os::kqueue::{Exit, Filter};
    use futures_lite::future;

    future::block_on(async {
        // Spawn a process.
        let mut process = Command::new("sleep")
            .arg("3")
            .spawn()
            .expect("failed to spawn process");

        // Wrap the process in an `Async` object that waits for it to exit.
        let process_handle = Filter::new(Exit::new(
            NonZeroI32::new(process.id().try_into().expect("invalid process pid"))
                .expect("non zero pid"),
        ))?;

        // Wait for the process to exit.
        process_handle.ready().await?;

        println!(
            "Process exit code {:?}",
            process
                .try_wait()
                .expect("error while waiting process")
                .expect("process did not exit yet")
        );
        Ok(())
    })
}

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
)))]
fn main() {
    println!("This example only works for kqueue-enabled platforms.");
}
