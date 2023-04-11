// SPDX-License-Identifier: MIT OR Apache-2.0

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
