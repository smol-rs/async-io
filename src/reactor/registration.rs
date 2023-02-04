#[cfg(windows)]
mod impl_ {
    use polling::{Event, Poller};
    use std::io::Result;
    use std::os::windows::io::RawSocket;

    /// The raw registration into the reactor.
    #[derive(Debug)]
    #[doc(hidden)]
    pub struct Registration {
        /// Raw socket handle on Windows.
        raw: RawSocket,
    }

    impl From<RawSocket> for Registration {
        fn from(raw: RawSocket) -> Self {
            Self { raw }
        }
    }

    impl Registration {
        /// Registers the object into the reactor.
        pub(crate) fn add(&self, poller: &Poller, token: usize) -> Result<()> {
            poller.add(self.raw, Event::none(token))
        }

        /// Re-registers the object into the reactor.
        pub(crate) fn modify(&self, poller: &Poller, interest: Event) -> Result<()> {
            poller.modify(self.raw, interest)
        }

        /// Deregisters the object from the reactor.
        pub(crate) fn delete(&self, poller: &Poller) -> Result<()> {
            poller.delete(self.raw)
        }
    }
}

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
mod impl_ {
    use crate::os::kqueue::Signal;

    use polling::os::kqueue::{PollerKqueueExt, Process, ProcessOps, Signal as PollSignal};
    use polling::{Event, PollMode, Poller};

    use std::io::Result;
    use std::os::unix::io::RawFd;
    use std::process::Child;

    /// The raw registration into the reactor.
    ///
    /// This needs to be public, since it is technically exposed through the `QueueableSealed` trait.
    #[derive(Debug)]
    #[doc(hidden)]
    pub enum Registration {
        /// Raw file descriptor for readability/writability.
        Fd(RawFd),

        /// Raw signal number for signal delivery.
        Signal(Signal),

        /// Process for process termination.
        Process(Child),
    }

    impl From<RawFd> for Registration {
        fn from(raw: RawFd) -> Self {
            Self::Fd(raw)
        }
    }

    impl From<Signal> for Registration {
        fn from(signal: Signal) -> Self {
            Self::Signal(signal)
        }
    }

    impl From<Child> for Registration {
        fn from(process: Child) -> Self {
            Self::Process(process)
        }
    }

    impl Registration {
        /// Registers the object into the reactor.
        pub(crate) fn add(&self, poller: &Poller, token: usize) -> Result<()> {
            match self {
                Self::Fd(raw) => poller.add(*raw, Event::none(token)),
                Self::Signal(signal) => {
                    poller.add_filter(PollSignal(signal.0), token, PollMode::Oneshot)
                }
                Self::Process(process) => poller.add_filter(
                    Process::new(process, ProcessOps::Exit),
                    token,
                    PollMode::Oneshot,
                ),
            }
        }

        /// Re-registers the object into the reactor.
        pub(crate) fn modify(&self, poller: &Poller, interest: Event) -> Result<()> {
            match self {
                Self::Fd(raw) => poller.modify(*raw, interest),
                Self::Signal(signal) => {
                    poller.modify_filter(PollSignal(signal.0), interest.key, PollMode::Oneshot)
                }
                Self::Process(process) => poller.modify_filter(
                    Process::new(process, ProcessOps::Exit),
                    interest.key,
                    PollMode::Oneshot,
                ),
            }
        }

        /// Deregisters the object from the reactor.
        pub(crate) fn delete(&self, poller: &Poller) -> Result<()> {
            match self {
                Self::Fd(raw) => poller.delete(*raw),
                Self::Signal(signal) => poller.delete_filter(PollSignal(signal.0)),
                Self::Process(process) => {
                    poller.delete_filter(Process::new(process, ProcessOps::Exit))
                }
            }
        }
    }
}

#[cfg(not(any(
    windows,
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
)))]
mod impl_ {
    use polling::{Event, Poller};
    use std::io::Result;
    use std::os::unix::io::RawFd;

    /// The raw registration into the reactor.
    #[derive(Debug)]
    #[doc(hidden)]
    pub struct Registration {
        /// Raw file descriptor on Unix.
        raw: RawFd,
    }

    impl From<RawFd> for Registration {
        fn from(raw: RawFd) -> Self {
            Self { raw }
        }
    }

    impl Registration {
        /// Registers the object into the reactor.
        pub(crate) fn add(&self, poller: &Poller, token: usize) -> Result<()> {
            poller.add(self.raw, Event::none(token))
        }

        /// Re-registers the object into the reactor.
        pub(crate) fn modify(&self, poller: &Poller, interest: Event) -> Result<()> {
            poller.modify(self.raw, interest)
        }

        /// Deregisters the object from the reactor.
        pub(crate) fn delete(&self, poller: &Poller) -> Result<()> {
            poller.delete(self.raw)
        }
    }
}

pub use impl_::Registration;
