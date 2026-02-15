use async_channel::bounded;
use async_io::{block_on, is_async_io_thread_spawned, Async, Timer};
use futures_lite::prelude::*;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic;
use std::thread;
use std::time::Duration;

const TIMER_COUNT: u64 = 10_000;

#[cfg(not(target_os = "openbsd"))]
const IO_COUNT: usize = 200;
// OpenBSD imposes limits on the number of ports we can have open.
#[cfg(target_os = "openbsd")]
const IO_COUNT: usize = 10;

static TOTAL_IO: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

#[test]
fn test_one_block_on() {
    // The driver thread should not be spawned.
    assert!(!is_async_io_thread_spawned());

    // Running smol::block_on() should not spawn the driver thread.
    block_on(async {
        assert!(!is_async_io_thread_spawned());

        // We should be able to handle a lot of timers and sources.
        let mut timers = Vec::new();
        for i in 1..TIMER_COUNT {
            timers.push(Timer::after(Duration::from_millis(i / 5)));
        }

        let (spawner, executor) = async_channel::unbounded();
        let mut tasks = Vec::new();

        for _ in 0..IO_COUNT {
            let (runnable, task) = async_task::Builder::new().propagate_panic(true).spawn(
                move |_| async move {
                    let mut rng = fastrand::Rng::new();

                    // Create a TCP pipe and send bytes to and from.
                    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
                    let stream1 =
                        Async::<TcpStream>::connect(listener.get_ref().local_addr()?).await?;
                    let stream2 = listener.accept().await?.0;

                    let mut bytes = [0u8; 64];
                    let mut read_buffer = [0u8; 64];

                    loop {
                        rng.fill(&mut bytes);

                        Timer::after(Duration::from_micros(rng.u64(..1_000))).await;
                        (&stream1).write_all(&bytes).await?;
                        Timer::after(Duration::from_micros(rng.u64(..1_000))).await;
                        (&stream2).read_exact(&mut read_buffer).await?;

                        assert_eq!(bytes, read_buffer);
                        TOTAL_IO.fetch_add(bytes.len(), atomic::Ordering::Relaxed);
                        futures_lite::future::yield_now().await;
                    }

                    #[allow(unreachable_code)]
                    std::io::Result::Ok(())
                },
                {
                    let spawner = spawner.clone();
                    move |task| {
                        spawner.try_send(task).ok();
                    }
                },
            );
            runnable.schedule();
            tasks.push(task);
        }

        // Future to process timers.
        let process_timers = async move {
            for timer in timers {
                timer.await;
            }

            // After the timer is done, cancel every task.
            for task in tasks {
                if let Some(res) = task.cancel().await {
                    res.unwrap();
                }
            }
        };

        // Future to process sources.
        let process_sources = async move {
            while let Ok(task) = executor.recv().await {
                task.run();
                futures_lite::future::yield_now().await;
            }
        };

        process_timers.or(process_sources).await;

        // Spawning another thread should spawn the driver thread.
        let (thread_spawned_signal, thread_spawned) = bounded::<()>(1);
        let (signal, shutdown) = bounded::<()>(1);
        thread::spawn(move || {
            block_on(async move {
                thread_spawned_signal.send(()).await.unwrap();
                shutdown.recv().await.unwrap();
            })
        });

        thread_spawned.recv().await.unwrap();
        assert!(is_async_io_thread_spawned());

        // Stopping the other thread should not stop the driver thread.
        signal.send(()).await.unwrap();
        assert!(is_async_io_thread_spawned());
    });
}
