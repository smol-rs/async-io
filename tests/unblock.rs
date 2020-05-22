use std::thread;
use std::time::{Duration, Instant};

use blocking::{block_on, unblock};
use futures::future::{ready, select, Either};
use futures::pin_mut;

#[test]
fn sleep() {
    let dur = Duration::from_secs(1);
    let start = Instant::now();

    block_on! {
        let f1 = unblock(move || thread::sleep(dur));
        let f2 = ready(());
        pin_mut!(f1);
        pin_mut!(f2);

        match select(f1, f2).await {
            Either::Left(_) => panic!(),
            Either::Right(((), f2)) => f2.await,
        }
    }

    assert!(start.elapsed() >= dur);
}
