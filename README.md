# blocking

[![Build](https://github.com/stjepang/blocking/workflows/Build%20and%20test/badge.svg)](
https://github.com/stjepang/blocking/actions)
![Rustc version](https://img.shields.io/badge/rustc-1.40+-lightgray.svg)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](
https://github.com/stjepang/blocking)
[![Cargo](https://img.shields.io/crates/v/blocking.svg)](
https://crates.io/crates/blocking)
[![Documentation](https://docs.rs/blocking/badge.svg)](
https://docs.rs/blocking)

Block on async code or await blocking code.

To convert async to blocking, *block on* async code with `block_on()`, `block_on!`, or
`BlockOn`.

To convert blocking to async, *unblock* blocking code with `unblock()`, `unblock!`, or
`Unblock`.

## Thread pool

Sometimes there's no way to avoid blocking I/O in async programs. Consider files or stdin,
which have weak async support on modern operating systems. While [IOCP], [AIO], and [io_uring]
are possible solutions, they're not always available or ideal.

Since blocking is not allowed inside futures, we must move blocking I/O onto a special thread
pool provided by this crate. The pool dynamically spawns and stops threads depending on the
current number of running I/O jobs.

Note that there is a limit on the number of active threads. Once that limit is hit, a running
job has to finish before others get a chance to run. When a thread is idle, it waits for the
next job or shuts down after a certain timeout.

[IOCP]: https://en.wikipedia.org/wiki/Input/output_completion_port
[AIO]: http://man7.org/linux/man-pages/man2/io_submit.2.html
[io_uring]: https://lwn.net/Articles/776703/

## Examples

Await a blocking file read with `unblock!`:

```rust
use blocking::{block_on, unblock};
use std::{fs, io};

block_on(async {
    let contents = unblock!(fs::read_to_string("file.txt"))?;
    println!("{}", contents);
    io::Result::Ok(())
});
```

Read a file and pipe its contents to stdout:

```rust
use blocking::{block_on, Unblock};
use std::fs::File;
use std::io::{self, stdout};

block_on(async {
    let input = Unblock::new(File::open("file.txt")?);
    let mut output = Unblock::new(stdout());

    futures::io::copy(input, &mut output).await?;
    io::Result::Ok(())
});
```

Iterate over the contents of a directory:

```rust
use blocking::{block_on, Unblock};
use futures::prelude::*;
use std::{fs, io};

block_on(async {
    let mut dir = Unblock::new(fs::read_dir(".")?);
    while let Some(item) = dir.next().await {
        println!("{}", item?.file_name().to_string_lossy());
    }
    io::Result::Ok(())
});
```

Convert a stream into an iterator:

```rust
use blocking::BlockOn;
use futures::stream;

let stream = stream::once(async { 7 });
let mut iter = BlockOn::new(Box::pin(stream));

assert_eq!(iter.next(), Some(7));
assert_eq!(iter.next(), None);
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
