// SPDX-License-Identifier: MIT OR Apache-2.0

use polling::{Event, Poller};

use std::fmt;
use std::io::Result;
use std::os::unix::io::RawFd;

/// The raw registration into the reactor.
#[doc(hidden)]
pub struct Registration {
    /// Raw file descriptor on Unix.
    raw: RawFd,
}

impl fmt::Debug for Registration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.raw, f)
    }
}

impl From<RawFd> for Registration {
    #[inline]
    fn from(raw: RawFd) -> Self {
        Self { raw }
    }
}

impl Registration {
    /// Registers the object into the reactor.
    #[inline]
    pub(crate) fn add(&self, poller: &Poller, token: usize) -> Result<()> {
        poller.add(self.raw, Event::none(token))
    }

    /// Re-registers the object into the reactor.
    #[inline]
    pub(crate) fn modify(&self, poller: &Poller, interest: Event) -> Result<()> {
        poller.modify(self.raw, interest)
    }

    /// Deregisters the object from the reactor.
    #[inline]
    pub(crate) fn delete(&self, poller: &Poller) -> Result<()> {
        poller.delete(self.raw)
    }
}
