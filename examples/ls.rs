//! Lists directory contents.
//!
//! Run with:
//!
//! ```
//! cargo run --example ls .
//! ```

use std::env;
use std::fs;
use std::io;

use blocking::{block_on, Unblock};
use futures::prelude::*;

fn main() -> io::Result<()> {
    let path = env::args().nth(1).unwrap_or(".".into());

    block_on(async {
        let mut dir = Unblock::new(fs::read_dir(path)?);

        while let Some(item) = dir.next().await {
            println!("{}", item?.file_name().to_string_lossy());
        }

        Ok(())
    })
}
