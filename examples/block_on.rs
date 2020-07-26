use std::future::Future;
use std::task::{Context, Poll};

use futures_lite::pin;
use waker_fn::waker_fn;

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let (p, u) = parking::pair();
    let waker = waker_fn(move || u.unpark());
    let cx = &mut Context::from_waker(&waker);

    pin!(future);
    loop {
        match future.as_mut().poll(cx) {
            Poll::Ready(t) => return t,
            Poll::Pending => p.park(),
        }
    }
}

fn main() {
    block_on(async {
        println!("Hello world!");
    })
}
