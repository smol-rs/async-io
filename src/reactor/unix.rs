// SPDX-License-Identifier: MIT OR Apache-2.0

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
