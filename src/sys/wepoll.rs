//! Raw bindings to wepoll (Windows).

use std::convert::TryInto;
use std::io;
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::ptr;
use std::time::Duration;

use wepoll_sys_stjepang as we;
use winapi::um::winsock2;

use crate::sys::Event;

macro_rules! wepoll {
    ($fn:ident $args:tt) => {{
        let res = unsafe { we::$fn $args };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

pub struct Reactor {
    handle: we::HANDLE,
}

unsafe impl Send for Reactor {}
unsafe impl Sync for Reactor {}

impl Reactor {
    pub fn new() -> io::Result<Reactor> {
        let handle = unsafe { we::epoll_create1(0) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Reactor { handle })
    }

    pub fn insert(&self, sock: RawSocket, key: usize) -> io::Result<()> {
        unsafe {
            let mut nonblocking = true as libc::c_ulong;
            let res = winsock2::ioctlsocket(
                sock as winsock2::SOCKET,
                winsock2::FIONBIO,
                &mut nonblocking,
            );
            if res != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let mut ev = we::epoll_event {
            events: 0,
            data: we::epoll_data { u64: key as u64 },
        };
        wepoll!(epoll_ctl(
            self.handle,
            we::EPOLL_CTL_ADD as libc::c_int,
            sock as we::SOCKET,
            &mut ev,
        ))?;
        Ok(())
    }

    pub fn interest(
        &self,
        sock: RawSocket,
        key: usize,
        read: bool,
        write: bool,
    ) -> io::Result<()> {
        let mut flags = we::EPOLLONESHOT;
        if read {
            flags |= READ_FLAGS;
        }
        if write {
            flags |= WRITE_FLAGS;
        }
        let mut ev = we::epoll_event {
            events: flags as u32,
            data: we::epoll_data { u64: key as u64 },
        };
        wepoll!(epoll_ctl(
            self.handle,
            we::EPOLL_CTL_MOD as libc::c_int,
            sock as we::SOCKET,
            &mut ev,
        ))?;
        Ok(())
    }

    pub fn remove(&self, sock: RawSocket) -> io::Result<()> {
        wepoll!(epoll_ctl(
            self.handle,
            we::EPOLL_CTL_DEL as libc::c_int,
            sock as we::SOCKET,
            ptr::null_mut(),
        ))?;
        Ok(())
    }

    pub fn wait(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<usize> {
        let timeout_ms = match timeout {
            None => -1,
            Some(t) => {
                if t == Duration::from_millis(0) {
                    0
                } else {
                    t.max(Duration::from_millis(1))
                        .as_millis()
                        .try_into()
                        .unwrap_or(libc::c_int::max_value())
                }
            }
        };
        events.len = wepoll!(epoll_wait(
            self.handle,
            events.list.as_mut_ptr(),
            events.list.len() as libc::c_int,
            timeout_ms,
        ))? as usize;
        Ok(events.len)
    }

    pub fn notify(&self) -> io::Result<()> {
        unsafe {
            // This errors if a notification has already been posted, but that's okay.
            winapi::um::ioapiset::PostQueuedCompletionStatus(
                self.handle as winapi::um::winnt::HANDLE,
                0,
                0,
                ptr::null_mut(),
            );
        }
        Ok(())
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        unsafe {
            we::epoll_close(self.handle);
        }
    }
}

struct As(RawSocket);

impl AsRawSocket for As {
    fn as_raw_socket(&self) -> RawSocket {
        self.0
    }
}

const READ_FLAGS: u32 = we::EPOLLIN | we::EPOLLRDHUP | we::EPOLLHUP | we::EPOLLERR | we::EPOLLPRI;
const WRITE_FLAGS: u32 = we::EPOLLOUT | we::EPOLLHUP | we::EPOLLERR;

pub struct Events {
    list: Box<[we::epoll_event]>,
    len: usize,
}

unsafe impl Send for Events {}

impl Events {
    pub fn new() -> Events {
        let ev = we::epoll_event {
            events: 0,
            data: we::epoll_data { u64: 0 },
        };
        Events {
            list: vec![ev; 1000].into_boxed_slice(),
            len: 0,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        self.list[..self.len].iter().map(|ev| Event {
            readable: (ev.events & READ_FLAGS) != 0,
            writable: (ev.events & WRITE_FLAGS) != 0,
            key: unsafe { ev.data.u64 } as usize,
        })
    }
}
