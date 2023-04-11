// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::os::kqueue::Signal;

use polling::os::kqueue::{PollerKqueueExt, Process, ProcessOps, Signal as PollSignal};
use polling::{Event, PollMode, Poller};

use std::fmt;
use std::io::Result;
use std::os::unix::io::RawFd;
use std::process::Child;

/// The raw registration into the reactor.
///
/// This needs to be public, since it is technically exposed through the `QueueableSealed` trait.
#[doc(hidden)]
pub enum Registration {
    /// Raw file descriptor for readability/writability.
    Fd(RawFd),

    /// Raw signal number for signal delivery.
    Signal(Signal),

    /// Process for process termination.
    Process(Child),
}

impl fmt::Debug for Registration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fd(raw) => fmt::Debug::fmt(raw, f),
            Self::Signal(signal) => fmt::Debug::fmt(signal, f),
            Self::Process(process) => fmt::Debug::fmt(process, f),
        }
    }
}

impl From<RawFd> for Registration {
    fn from(raw: RawFd) -> Self {
        Self::Fd(raw)
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
            Self::Process(process) => poller.delete_filter(Process::new(process, ProcessOps::Exit)),
        }
    }
}
