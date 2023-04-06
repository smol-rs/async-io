//! Functionality that is only available for `unix` platforms.

#[cfg(not(async_io_no_io_safety))]
use std::os::unix::io::BorrowedFd;

/// Get a file descriptor that can be used to wait for readiness in an external runtime.
#[cfg(not(async_io_no_io_safety))]
pub fn reactor_fd() -> Option<BorrowedFd<'static>> {
    cfg_if::cfg_if! {
        if #[cfg(all(
            any(
                target_os = "linux",
                target_os = "android",
                target_os = "illumos",
                target_os = "solaris",
                target_os = "macos",
                target_os = "ios",
                target_os = "tvos",
                target_os = "watchos",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd",
                target_os = "dragonfly",
            ),
            not(polling_test_poll_backend),
        ))] {
            use std::os::unix::io::AsFd;
            Some(crate::Reactor::get().poller.as_fd())
        } else {
            None
        }
    }
}
