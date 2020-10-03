# Version 1.1.8

- Re-enable `async-net` dependency and fix CI.

# Version 1.1.7

- Update `polling` to v2.0.0

# Version 1.1.6

- Remove randomized yielding everywhere.

# Version 1.1.5

- Remove randomized yielding in write operations.

# Version 1.1.4

- Implement proper cancelation for `readable()` and `writable()`.

# Version 1.1.3

- Improve docs.

# Version 1.1.2

- Add `nb-connect` dependency.
- Remove `wepoll-sys-stjepang` dependency.

# Version 1.1.1

- Remove `socket2` dependency.

# Version 1.1.0

- Add `TryFrom` conversion impls for `Async`.

# Version 1.0.2

- Don't box `T` in `Async<T>`.
- `Async::incoming()` doesn't return `Unpin` streams anymore.

# Version 1.0.1

- Update dependencies.

# Version 1.0.0

- Stabilize.

# Version 0.2.7

- Replace `log::debug!` with `log::trace!`.
-
# Version 0.2.6

- Add logging.

# Version 0.2.5

- On Linux, fail fast if `writable()` succeeds after connecting to `UnixStream`,
  but the connection is not really established.

# Version 0.2.4

- Prevent threads in `async_io::block_on()` from hogging the reactor forever.

# Version 0.2.3

- Performance optimizations in `block_on()`.

# Version 0.2.2

- Add probabilistic yielding to improve fairness.

# Version 0.2.1

- Update readme.

# Version 0.2.0

- Replace `parking` module with `block_on()`.
- Fix a bug in `Async::<UnixStream>::connect()`.

# Version 0.1.11

- Bug fix: clear events list before polling.

# Version 0.1.10

- Simpler implementation of the `parking` module.
- Extracted raw bindings to epoll/kqueue/wepoll into the `polling` crate.

# Version 0.1.9

- Update dependencies.
- More documentation.

# Version 0.1.8

- Tweak the async-io to poll I/O less aggressively.

# Version 0.1.7

- Tweak the async-io thread to use less CPU.
- More examples.

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
