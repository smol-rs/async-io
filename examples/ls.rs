//! Lists directory contents.
//!
//! Run with:
//!
//! ```
//! cargo run --example ls .
//! ```

use std::fs;
use std::io;
use std::env;

use blocking::Blocking;
use futures::executor::block_on;
use futures::prelude::*;

fn main() -> io::Result<()> {
    let path = env::args().nth(1).unwrap_or(".".into());

    block_on(async {
        let mut dir = Blocking::new(fs::read_dir(path)?);

        while let Some(item) = dir.next().await {
            println!("{}", item?.file_name().to_string_lossy());
        }

        Ok(())
    })
}
