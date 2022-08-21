//! Benchmarks for a variety of I/O operations.

use async_io::Async;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use futures_lite::prelude::*;
use std::net::{TcpListener, TcpStream};

pub fn read_and_write(b: &mut Criterion) {
    const TCP_AMOUNT: usize = 1024 * 1024;

    let mut group = b.benchmark_group("poll_read");

    for (driver_name, exec) in [("Undriven", false), ("Driven", true)] {
        group.bench_function(format!("TcpStream.{}", driver_name), |b| {
            let listener = TcpListener::bind("localhost:12345").unwrap();
            let read_stream = TcpStream::connect("localhost:12345").unwrap();
            let (write_stream, _) = listener.accept().unwrap();

            let mut reader = Async::new(read_stream).unwrap();
            let mut writer = Async::new(write_stream).unwrap();
            let mut buf = vec![0; TCP_AMOUNT];

            b.iter(|| {
                let buf = &mut buf;
                let future = async { 
                    black_box(writer.write_all(&*buf).await.ok());
                    black_box(reader.read_exact(buf).await.ok());
                };

                if exec {
                    async_io::block_on(future);
                } else {
                    futures_lite::future::block_on(future);
                }
            });
        });

        #[cfg(unix)]
        group.bench_function(format!("UnixStream.{}", driver_name), |b| {
            use std::os::unix::net::UnixStream;

            const UNIX_AMOUNT: usize = 1024 * 48;

            let (read_stream, write_stream) = UnixStream::pair().unwrap();

            let mut reader = Async::new(read_stream).unwrap();
            let mut writer = Async::new(write_stream).unwrap();
            let mut buf = vec![0x42; UNIX_AMOUNT];

            b.iter(|| {
                let buf = &mut buf;

                let future = async {
                    black_box(writer.write_all(&*buf).await.ok());
                    black_box(reader.read_exact(buf).await.ok());
                };

                if exec {
                    async_io::block_on(future);
                } else {
                    futures_lite::future::block_on(future);
                }
            });
        });
    }
}

criterion_group! {
    io_benchmarks,
    read_and_write,
}

criterion_main!(io_benchmarks);
