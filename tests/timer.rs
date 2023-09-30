use std::future::Future;
#[cfg(not(all(target_family = "wasm", not(target_os = "wasi"))))]
use std::pin::Pin;
#[cfg(not(all(target_family = "wasm", not(target_os = "wasi"))))]
use std::sync::{Arc, Mutex};
#[cfg(not(all(target_family = "wasm", not(target_os = "wasi"))))]
use std::thread;

#[cfg(not(all(target_family = "wasm", not(target_os = "wasi"))))]
use std::time::{Duration, Instant};
#[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
use web_time::{Duration, Instant};

#[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

use async_io::Timer;
use futures_lite::{FutureExt, StreamExt};

#[cfg(not(all(target_family = "wasm", not(target_os = "wasi"))))]
use futures_lite::future;

#[cfg(not(all(target_family = "wasm", not(target_os = "wasi"))))]
fn spawn<T: Send + 'static>(
    f: impl Future<Output = T> + Send + 'static,
) -> impl Future<Output = T> + Send + 'static {
    let (s, r) = async_channel::bounded(1);

    thread::spawn(move || {
        future::block_on(async {
            s.send(f.await).await.ok();
        })
    });

    Box::pin(async move { r.recv().await.unwrap() })
}

#[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
fn spawn<T: 'static>(f: impl Future<Output = T> + 'static) -> impl Future<Output = T> + 'static {
    let (s, r) = async_channel::bounded(1);

    #[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
    wasm_bindgen_futures::spawn_local(async move {
        s.send(f.await).await.ok();
    });

    Box::pin(async move { r.recv().await.unwrap() })
}

#[cfg(not(all(target_family = "wasm", not(target_os = "wasi"))))]
macro_rules! test {
    (
        $(#[$meta:meta])*
        async fn $name:ident () $bl:block
    ) => {
        #[test]
        $(#[$meta])*
        fn $name() {
            futures_lite::future::block_on(async {
                $bl
            })
        }
    };
}

#[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
macro_rules! test {
    (
        $(#[$meta:meta])*
        async fn $name:ident () $bl:block
    ) => {
        // wasm-bindgen-test handles waiting on the future for us
        #[wasm_bindgen_test::wasm_bindgen_test]
        $(#[$meta])*
        async fn $name() {
            console_error_panic_hook::set_once();
            $bl
        }
    };
}

test! {
    async fn smoke() {
        let start = Instant::now();
        Timer::after(Duration::from_secs(1)).await;
        assert!(start.elapsed() >= Duration::from_secs(1));
    }
}

test! {
    async fn interval() {
        let period = Duration::from_secs(1);
        let jitter = Duration::from_millis(500);
        let start = Instant::now();
        let mut timer = Timer::interval(period);
        timer.next().await;
        let elapsed = start.elapsed();
        assert!(elapsed >= period && elapsed - period < jitter);
        timer.next().await;
        let elapsed = start.elapsed();
        assert!(elapsed >= period * 2 && elapsed - period * 2 < jitter);
    }
}

test! {
    async fn poll_across_tasks() {
        let start = Instant::now();
        let (sender, receiver) = async_channel::bounded(1);

        let task1 = spawn(async move {
            let mut timer = Timer::after(Duration::from_secs(1));

            async {
                (&mut timer).await;
                panic!("timer should not be ready")
            }
            .or(async {})
            .await;

            sender.send(timer).await.ok();
        });

        let task2 = spawn(async move {
            let timer = receiver.recv().await.unwrap();
            timer.await;
        });

        task1.await;
        task2.await;

        assert!(start.elapsed() >= Duration::from_secs(1));
    }
}

#[cfg(not(all(target_family = "wasm", not(target_os = "wasi"))))]
#[test]
fn set() {
    future::block_on(async {
        let start = Instant::now();
        let timer = Arc::new(Mutex::new(Timer::after(Duration::from_secs(10))));

        thread::spawn({
            let timer = timer.clone();
            move || {
                thread::sleep(Duration::from_secs(1));
                timer.lock().unwrap().set_after(Duration::from_secs(2));
            }
        });

        future::poll_fn(|cx| Pin::new(&mut *timer.lock().unwrap()).poll(cx)).await;

        assert!(start.elapsed() >= Duration::from_secs(2));
        assert!(start.elapsed() < Duration::from_secs(10));
    });
}
