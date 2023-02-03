//! Functionality that is only available for `kqueue`-based platforms.

use super::__private::AsyncSealed;
use __private::FilterSealed;

use crate::reactor::{Reactor, Registration};
use crate::Async;

use std::io::Result;
use std::process::Child;

/// An extension trait for [`Async`](crate::Async) that provides the ability to register other
/// queueable objects into the reactor.
///
/// The underlying `kqueue` implementation can be used to poll for events besides file descriptor
/// read/write readiness. This API makes these faculties available to the user.
///
/// See the [`Filter`] trait and its implementors for objects that currently support being registered
/// into the reactor.
pub trait AsyncKqueueExt<T: Filter>: AsyncSealed {
    /// Create a new [`Async`](crate::Async) around a [`Filter`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::process::Command;
    ///
    /// use async_io::Async;
    /// use async_io::os::kqueue::{AsyncKqueueExt, Exit};
    ///
    /// // Create a new process to wait for.
    /// let mut child = Command::new("sleep").arg("5").spawn().unwrap();
    ///
    /// // Wrap the process in an `Async` object that waits for it to exit.
    /// let process = Async::with_filter(Exit::new(child)).unwrap();
    ///
    /// // Wait for the process to exit.
    /// # async_io::block_on(async {
    /// process.readable().await.unwrap();
    /// # });
    /// ```
    fn with_filter(filter: T) -> Result<Async<T>>;
}

impl<T: Filter> AsyncKqueueExt<T> for Async<T> {
    fn with_filter(mut filter: T) -> Result<Async<T>> {
        Ok(Async {
            source: Reactor::get().insert_io(filter.registration())?,
            io: Some(filter),
        })
    }
}

/// Objects that can be registered into the reactor via a [`Async`](crate::Async).
pub trait Filter: FilterSealed {}

/// An object representing a signal.
///
/// When registered into [`Async`](crate::Async) via [`with_filter`](AsyncKqueueExt::with_filter),
/// it will return a [`readable`](crate::Async::readable) event when the signal is received.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct Signal(pub i32);

impl FilterSealed for Signal {
    fn registration(&mut self) -> Registration {
        (*self).into()
    }
}
impl Filter for Signal {}

/// Wait for a child process to exit.
///
/// When registered into [`Async`](crate::Async) via [`with_filter`](AsyncKqueueExt::with_filter),
/// it will return a [`readable`](crate::Async::readable) event when the child process exits.
#[derive(Debug)]
pub struct Exit(Option<Child>);

impl Exit {
    /// Create a new `Exit` object.
    pub fn new(child: Child) -> Self {
        Self(Some(child))
    }
}

impl FilterSealed for Exit {
    fn registration(&mut self) -> Registration {
        self.0.take().expect("Cannot reregister child").into()
    }
}
impl Filter for Exit {}

mod __private {
    use crate::reactor::Registration;

    #[doc(hidden)]
    pub trait FilterSealed {
        /// Get a registration object for this filter.
        fn registration(&mut self) -> Registration;
    }
}
