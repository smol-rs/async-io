use std::future::Future;
use std::thread;
use std::time::{Duration, Instant};

use async_io::Timer;
use blocking::block_on;
use futures::future;

fn spawn<T: Send + 'static>(
    f: impl Future<Output = T> + Send + 'static,
) -> impl Future<Output = T> + Send + 'static {
    let (s, r) = async_channel::bounded(1);

    thread::spawn(move || {
        block_on(async {
            let _ = s.send(f.await).await;
        })
    });

    Box::pin(async move { r.recv().await.unwrap() })
}

#[test]
fn smoke() {
    block_on(async {
        let start = Instant::now();
        Timer::new(Duration::from_secs(1)).await;
        assert!(start.elapsed() >= Duration::from_secs(1));
    });
}

#[test]
fn poll_across_tasks() {
    let before = block_on(async {
        let now = Instant::now();
        let (sender, receiver) = async_channel::bounded(1);

        let task1 = spawn(async move {
            let mut timer = Timer::new(Duration::from_secs(1));
            match future::select(&mut timer, future::ready(())).await {
                future::Either::Left(_) => panic!("timer should not be ready"),
                future::Either::Right(_) => {}
            }
            let _ = sender.send(timer).await;
        });

        let task2 = spawn(async move {
            let timer = receiver.recv().await.unwrap();
            timer.await;
        });

        task1.await;
        task2.await;
        now
    });

    assert!(before.elapsed() >= Duration::from_secs(1));
}
