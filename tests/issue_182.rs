//! https://github.com/smol-rs/async-io/issues/182

use async_io::Async;
use std::net::{TcpStream, ToSocketAddrs};

#[test]
fn networking_initialized() {
    let address = ToSocketAddrs::to_socket_addrs(&("google.com", 80))
        .unwrap()
        .next()
        .unwrap();
    async_io::block_on(async move {
        let _ = Async::<TcpStream>::connect(address).await.unwrap();
    });
}
