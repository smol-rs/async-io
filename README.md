# blocking

[![Build](https://github.com/stjepang/blocking/workflows/Build%20and%20test/badge.svg)](
https://github.com/stjepang/blocking/actions)
[![Coverage Status](https://coveralls.io/repos/github/stjepang/blocking/badge.svg?branch=master)](
https://coveralls.io/github/stjepang/blocking?branch=master)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](
https://github.com/stjepang/blocking)
[![Cargo](https://img.shields.io/crates/v/blocking.svg)](
https://crates.io/crates/blocking)
[![Documentation](https://docs.rs/blocking/badge.svg)](
https://docs.rs/blocking)
[![Chat](https://img.shields.io/discord/701824908866617385.svg?logo=discord)](
https://discord.gg/x6m5Vvt)

An executor for isolating blocking I/O in async programs.

Sometimes there's no way to avoid blocking I/O. Consider files or stdin, which have weak async
support on modern operating systems. While [IOCP], [AIO], and [io_uring] are possible
solutions, they're not always available or ideal.

Since blocking is not allowed inside futures, we must move blocking I/O onto a special
executor provided by this crate. On this executor, futures are allowed to "cheat" and block
without any restrictions. The executor dynamically spawns and stops threads depending on the
current number of running futures.

Note that there is a limit on the number of active threads. Once that limit is hit, a running
task has to complete or yield before other tasks get a chance to continue running. When a
thread is idle, it waits for the next task or shuts down after a certain timeout.

[IOCP]: https://en.wikipedia.org/wiki/Input/output_completion_port
[AIO]: http://man7.org/linux/man-pages/man2/io_submit.2.html
[io_uring]: https://lwn.net/Articles/776703/

## Examples

Spawn a blocking future with `Blocking::spawn()`:

```rust
use blocking::Blocking;
use std::fs;

let contents = Blocking::spawn(async { fs::read_to_string("file.txt") }).await?;
```

Or do the same with the `blocking!` macro:

```rust
use blocking::blocking;
use std::fs;

let contents = blocking!(fs::read_to_string("file.txt"))?;
```

Read a file and pipe its contents to stdout:

```rust
use blocking::Blocking;
use std::fs::File;
use std::io::stdout;

let input = Blocking::new(File::open("file.txt")?);
let mut output = Blocking::new(stdout());

futures::io::copy(input, &mut output).await?;
```

Iterate over the contents of a directory:

```rust
use blocking::Blocking;
use futures::prelude::*;
use std::fs;

let mut dir = Blocking::new(fs::read_dir(".")?);

while let Some(item) = dir.next().await {
    println!("{}", item?.file_name().to_string_lossy());
}
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
