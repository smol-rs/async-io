# Version 0.3.2

- Make `Blocking` implement `Send` in more cases.

# Version 0.3.1

- Add `Blocking::with_mut()`.

# Version 0.3.0

- Remove `Blocking::spawn()`.
- Implement `Future` for `Blocking` only when the inner type is a `FnOnce`.

# Version 0.2.0

- Initial version

# Version 0.1.0

- Reserved crate name
