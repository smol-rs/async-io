use cfg_if::cfg_if;

#[cfg(unix)]
macro_rules! syscall {
    ($fn:ident $args:tt) => {{
        let res = unsafe { libc::$fn $args };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

cfg_if! {
    if #[cfg(any(target_os = "linux", target_os = "android", target_os = "illumos"))] {
        mod epoll;
        pub use self::epoll::*;
    } else if #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    ))] {
        mod kqueue;
        pub use self::kqueue::*;
    } else if #[cfg(target_os = "windows")] {
        mod wepoll;
        pub use self::wepoll::*;
    } else {
        compile_error!("async-io does not support this target OS");
    }
}

pub struct Event {
    pub readable: bool,
    pub writable: bool,
    pub key: usize,
}
