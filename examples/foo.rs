use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::time::Instant;

use blocking::Blocking;
use futures::prelude::*;

fn main() -> io::Result<()> {
    futures::executor::block_on(async {
        let start = Instant::now();
        let mut file = File::open("big.txt")?;
        let mut contents = Vec::<u8>::new();

        // file.read_to_end(&mut contents)?;
        // Blocking::new(file).read_to_end(&mut contents).await?;
        // smol::reader(file).read_to_end(&mut contents).await?;

        let mut file2 = File::create("big2.txt")?;
        // Blocking::new(file2).write_all(&contents).await?;
        futures::io::copy(Blocking::new(file), &mut Blocking::new(file2)).await?;

        dbg!(contents.len());
        dbg!(start.elapsed());

        Ok(())
    })
}
