//! Raw bindings to epoll (Linux, Android, illumos).

use std::convert::TryInto;
use std::io;
use std::os::unix::io::RawFd;
use std::ptr;
use std::time::Duration;

use crate::sys::Event;

pub struct Reactor {
    epoll_fd: RawFd,
    event_fd: RawFd,
}

impl Reactor {
    pub fn new() -> io::Result<Reactor> {
        // According to libuv, `EPOLL_CLOEXEC` is not defined on Android API < 21.
        // But `EPOLL_CLOEXEC` is an alias for `O_CLOEXEC` on that platform, so we use it instead.
        #[cfg(target_os = "android")]
        const CLOEXEC: libc::c_int = libc::O_CLOEXEC;
        #[cfg(not(target_os = "android"))]
        const CLOEXEC: libc::c_int = libc::EPOLL_CLOEXEC;

        let epoll_fd = unsafe {
            // Check if the `epoll_create1` symbol is available on this platform.
            let ptr = libc::dlsym(
                libc::RTLD_DEFAULT,
                "epoll_create1\0".as_ptr() as *const libc::c_char,
            );

            if ptr.is_null() {
                // If not, use `epoll_create` and manually set `CLOEXEC`.
                let fd = match libc::epoll_create(1024) {
                    -1 => return Err(io::Error::last_os_error()),
                    fd => fd,
                };
                let flags = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                fd
            } else {
                // Use `epoll_create1` with `CLOEXEC`.
                let epoll_create1 = std::mem::transmute::<
                    *mut libc::c_void,
                    unsafe extern "C" fn(libc::c_int) -> libc::c_int,
                >(ptr);
                match epoll_create1(CLOEXEC) {
                    -1 => return Err(io::Error::last_os_error()),
                    fd => fd,
                }
            }
        };

        let event_fd = syscall!(eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))?;
        let reactor = Reactor { epoll_fd, event_fd };
        reactor.insert(event_fd, !0)?;
        reactor.interest(event_fd, !0, true, false)?;
        Ok(reactor)
    }

    pub fn insert(&self, fd: RawFd, key: usize) -> io::Result<()> {
        let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
        syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        let mut ev = libc::epoll_event {
            events: 0,
            u64: key as u64,
        };
        syscall!(epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev))?;
        Ok(())
    }

    pub fn interest(&self, fd: RawFd, key: usize, read: bool, write: bool) -> io::Result<()> {
        let mut flags = libc::EPOLLONESHOT;
        if read {
            flags |= read_flags();
        }
        if write {
            flags |= write_flags();
        }
        let mut ev = libc::epoll_event {
            events: flags as _,
            u64: key as u64,
        };
        syscall!(epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_MOD, fd, &mut ev))?;
        Ok(())
    }

    pub fn remove(&self, fd: RawFd) -> io::Result<()> {
        syscall!(epoll_ctl(
            self.epoll_fd,
            libc::EPOLL_CTL_DEL,
            fd,
            ptr::null_mut()
        ))?;
        Ok(())
    }

    pub fn wait(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<usize> {
        let timeout_ms = timeout
            .map(|t| {
                if t == Duration::from_millis(0) {
                    t
                } else {
                    t.max(Duration::from_millis(1))
                }
            })
            .and_then(|t| t.as_millis().try_into().ok())
            .unwrap_or(-1);

        let res = syscall!(epoll_wait(
            self.epoll_fd,
            events.list.as_mut_ptr() as *mut libc::epoll_event,
            events.list.len() as libc::c_int,
            timeout_ms as libc::c_int,
        ))?;
        events.len = res as usize;

        let mut buf = [0u8; 8];
        let _ = syscall!(read(
            self.event_fd,
            &mut buf[0] as *mut u8 as *mut libc::c_void,
            buf.len()
        ));
        self.interest(self.event_fd, !0, true, false)?;

        Ok(events.len)
    }

    pub fn notify(&self) -> io::Result<()> {
        let buf: [u8; 8] = 1u64.to_ne_bytes();
        let _ = syscall!(write(
            self.event_fd,
            &buf[0] as *const u8 as *const libc::c_void,
            buf.len()
        ));
        Ok(())
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        let _ = self.remove(self.event_fd);
        let _ = syscall!(close(self.event_fd));
        let _ = syscall!(close(self.epoll_fd));
    }
}

fn read_flags() -> libc::c_int {
    libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLHUP | libc::EPOLLERR | libc::EPOLLPRI
}

fn write_flags() -> libc::c_int {
    libc::EPOLLOUT | libc::EPOLLHUP | libc::EPOLLERR
}

pub struct Events {
    list: Box<[libc::epoll_event]>,
    len: usize,
}

impl Events {
    pub fn new() -> Events {
        let ev = libc::epoll_event { events: 0, u64: 0 };
        let list = vec![ev; 1000].into_boxed_slice();
        let len = 0;
        Events { list, len }
    }

    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        self.list[..self.len].iter().map(|ev| Event {
            readable: (ev.events as libc::c_int & read_flags()) != 0,
            writable: (ev.events as libc::c_int & write_flags()) != 0,
            key: ev.u64 as usize,
        })
    }
}
