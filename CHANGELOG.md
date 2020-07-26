# Version 0.1.6

- Add `Timer::reset()`.
- Add third party licenses.
- Code cleanup.

# Version 0.1.5

- Make `Parker` and `Unparker` unwind-safe.

# Version 0.1.4

- Initialize the reactor in `Parker::new()`.

# Version 0.1.3

- Always use the last waker given to `Timer`.
- Shutdown the socket in `AsyncWrite::poll_close()`.
- Reduce the number of dependencies.

# Version 0.1.2

- Shutdown the write side of the socket in `AsyncWrite::poll_close()`.
- Code and dependency cleanup.
- Always use the last waker when polling a timer.

# Version 0.1.1

- Initial version
