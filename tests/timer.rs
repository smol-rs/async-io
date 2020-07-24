use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use async_io::Timer;
use futures_lite::{future, FutureExt};

fn spawn<T: Send + 'static>(
    f: impl Future<Output = T> + Send + 'static,
) -> impl Future<Output = T> + Send + 'static {
    let (s, r) = async_channel::bounded(1);

    thread::spawn(move || {
        future::block_on(async {
            let _ = s.send(f.await).await;
        })
    });

    Box::pin(async move { r.recv().await.unwrap() })
}

#[test]
fn smoke() {
    future::block_on(async {
        let start = Instant::now();
        Timer::new(Duration::from_secs(1)).await;
        assert!(start.elapsed() >= Duration::from_secs(1));
    });
}

#[test]
fn poll_across_tasks() {
    future::block_on(async {
        let start = Instant::now();
        let (sender, receiver) = async_channel::bounded(1);

        let task1 = spawn(async move {
            let mut timer = Timer::new(Duration::from_secs(1));

            async {
                (&mut timer).await;
                panic!("timer should not be ready")
            }
            .or(async {})
            .await;

            let _ = sender.send(timer).await;
        });

        let task2 = spawn(async move {
            let timer = receiver.recv().await.unwrap();
            timer.await;
        });

        task1.await;
        task2.await;

        assert!(start.elapsed() >= Duration::from_secs(1));
    });
}

#[test]
fn reset() {
    future::block_on(async {
        let start = Instant::now();
        let timer = Arc::new(Mutex::new(Timer::new(Duration::from_secs(10))));

        thread::spawn({
            let timer = timer.clone();
            move || {
                thread::sleep(Duration::from_secs(1));
                timer.lock().unwrap().reset(Duration::from_secs(2));
            }
        });

        future::poll_fn(|cx| Pin::new(&mut *timer.lock().unwrap()).poll(cx)).await;

        assert!(start.elapsed() >= Duration::from_secs(2));
        assert!(start.elapsed() < Duration::from_secs(10));
    });
}
