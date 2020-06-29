# async-lock

[![Build](https://github.com/stjepang/async-lock/workflows/Build%20and%20test/badge.svg)](
https://github.com/stjepang/async-lock/actions)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](
https://github.com/stjepang/async-lock)
[![Cargo](https://img.shields.io/crates/v/async-lock.svg)](
https://crates.io/crates/async-lock)
[![Documentation](https://docs.rs/async-lock/badge.svg)](
https://docs.rs/async-lock)

Reference-counted async lock.

The `Lock` type is similar to `std::sync::Mutex`, except locking is an async operation.

Note that `Lock` by itself acts like an `Arc` in the sense that cloning it returns just
another reference to the same lock.

Furthermore, `LockGuard` is not tied to `Lock` by a lifetime, so you can keep guards for
as long as you want. This is useful when you want to spawn a task and move a guard into its
future.

The locking mechanism uses eventual fairness to ensure locking will be fair on average without
sacrificing performance. This is done by forcing a fair lock whenever a lock operation is
starved for longer than 0.5 milliseconds.

## Examples

```rust
use async_lock::Lock;
use smol::Task;

let lock = Lock::new(0);
let mut tasks = vec![];

for _ in 0..10 {
    let lock = lock.clone();
    tasks.push(Task::spawn(async move { *lock.lock().await += 1 }));
}

for task in tasks {
    task.await;
}
assert_eq!(*lock.lock().await, 10);
```

## License

Licensed under either of

 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

#### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
