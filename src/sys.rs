//! Cross-platform type and trait aliases.

pub(crate) use self::sys::*;

/// Cross-platform alias to `AsRawFd` (Unix) or `AsRawSocket` (Windows).
///
/// Note: this is a slight hack around the rust type system. You should not
/// implement this trait directly, e.g. for a wrapper type, that will not work.
/// Instead you have to implement both `AsRawFd` and `AsRawSocket` separately.
pub trait AsRawSource {
  /// Cross-platform alias to `AsRawFd::as_raw_fd` (Unix) or `AsRawSocket` (Windows).
  fn as_raw_source(&self) -> RawSource;
}

/// Cross-platform alias to `FromRawFd` (Unix) or `FromRawSocket` (Windows).
pub trait FromRawSource {
  /// Cross-platform alias to `FromRawFd::from_raw_fd` (Unix) or `FromRawSocket::from_raw_socket` (Windows).
  unsafe fn from_raw_source(h: RawSource) -> Self;
}

#[cfg(unix)]
mod sys {
  use super::*;
  use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

  pub(crate) type RawSource = RawFd;

  impl<T> AsRawSource for T where T: AsRawFd {
    fn as_raw_source(&self) -> RawSource {
      self.as_raw_fd()
    }
  }

  impl<T> FromRawSource for T where T: FromRawFd {
    unsafe fn from_raw_source(h: RawSource) -> Self {
      Self::from_raw_fd(h)
    }
  }
}

#[cfg(windows)]
mod sys {
  use super::*;
  use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};

  pub(crate) type RawSource = RawSocket;

  impl<T> AsRawSource for T where T: AsRawSocket {
    fn as_raw_source(&self) -> RawSource {
      self.as_raw_socket()
    }
  }

  impl<T> FromRawSource for T where T: FromRawSocket {
    unsafe fn from_raw_source(h: RawSource) -> Self {
      Self::from_raw_socket(h)
    }
  }
}
