//! Platform-specific functionality.

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
pub mod kqueue;

mod __private {
    #[doc(hidden)]
    pub trait AsyncSealed {}

    impl<T> AsyncSealed for crate::Async<T> {}
}
