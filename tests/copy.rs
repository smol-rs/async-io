use std::io::Cursor;
use std::sync::mpsc;
use std::thread;

use blocking::{block_on, Unblock};
use futures::prelude::*;

#[test]
fn chan() {
    block_on! {
        let (s, r) = mpsc::sync_channel::<i32>(100);
        let handle = thread::spawn(move || {
            for i in 0..100_000 {
                s.send(i).unwrap();
            }
        });

        let mut r = Unblock::new(r.into_iter());
        for i in 0i32..100_000 {
            assert_eq!(r.next().await, Some(i));
        }

        handle.join().unwrap();
        assert!(r.next().await.is_none());
    }
}

#[test]
fn read() {
    block_on! {
        let mut v1 = vec![0u8; 20_000_000];
        for i in 0..v1.len() {
            v1[i] = i as u8;
        }
        let mut v1 = Unblock::new(Cursor::new(v1));

        let mut v2 = vec![];
        v1.read_to_end(&mut v2).await.unwrap();

        let v1 = v1.into_inner().await.into_inner();
        assert!(v1 == v2);
    }
}

#[test]
fn write() {
    block_on! {
        let mut v1 = vec![0u8; 20_000_000];
        for i in 0..v1.len() {
            v1[i] = i as u8;
        }

        let v2 = vec![];
        let mut v2 = Unblock::new(Cursor::new(v2));
        v2.write_all(&v1).await.unwrap();

        let v2 = v2.into_inner().await.into_inner();
        assert!(v1 == v2);
    }
}
