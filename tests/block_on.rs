use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use blocking::block_on;

#[test]
fn yielding() {
    struct Yield(i32);

    impl Future for Yield {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.0 == 0 {
                Poll::Ready(())
            } else {
                self.0 -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    for i in 0..100 {
        block_on(Yield(i));
        block_on!(Yield(i));
    }
}
