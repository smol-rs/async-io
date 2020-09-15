use std::io;
use std::mem::{self, ManuallyDrop, MaybeUninit};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::ptr;

#[cfg(unix)]
use {
    libc::{sockaddr, sockaddr_storage, socklen_t},
    std::os::unix::net::UnixStream,
    std::os::unix::prelude::{FromRawFd, RawFd},
    std::path::Path,
};

#[cfg(windows)]
use {
    std::os::windows::io::{FromRawSocket, RawSocket},
    winapi::shared::ws2def::{SOCKADDR as sockaddr, SOCKADDR_STORAGE as sockaddr_storage},
    winapi::um::ws2tcpip::socklen_t,
};

/// A raw socket address.
struct Addr {
    storage: sockaddr_storage,
    len: socklen_t,
}

impl Addr {
    /// Creates a raw socket address from `SocketAddr`.
    fn new(addr: SocketAddr) -> Self {
        let (addr, len): (*const sockaddr, socklen_t) = match &addr {
            SocketAddr::V4(addr) => (addr as *const _ as *const _, mem::size_of_val(addr) as _),
            SocketAddr::V6(addr) => (addr as *const _ as *const _, mem::size_of_val(addr) as _),
        };
        unsafe { Self::from_raw_parts(addr, len) }
    }

    /// Creates an `Addr` from its raw parts.
    unsafe fn from_raw_parts(addr: *const sockaddr, len: socklen_t) -> Self {
        let mut storage = MaybeUninit::<sockaddr_storage>::uninit();
        ptr::copy_nonoverlapping(
            addr as *const _ as *const u8,
            &mut storage as *mut _ as *mut u8,
            len as usize,
        );
        Self {
            storage: storage.assume_init(),
            len,
        }
    }
}

#[cfg(unix)]
fn connect(addr: Addr, family: libc::c_int, protocol: libc::c_int) -> io::Result<RawFd> {
    /// Calls a libc function and results in `io::Result`.
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

    // On linux, we pass the `SOCK_CLOEXEC` flag to atomically create the socket and set it as
    // CLOEXEC.
    #[cfg(target_os = "linux")]
    let fd = syscall!(socket(
        family,
        libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
        protocol
    ))?;

    // On other systems, we first create the socket and then set it as CLOEXEC.
    #[cfg(not(target_os = "linux"))]
    let fd = {
        let fd = syscall!(socket(family, libc::SOCK_STREAM, protocol))?;
        let flags = syscall!(fcntl(fd, libc::F_GETFD))? | libc::FD_CLOEXEC;
        syscall!(fcntl(fd, libc::F_SETFD, flags))?;

        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            let payload = &1i32 as *const i32 as *const libc::c_void;
            syscall!(setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                payload,
                std::mem::size_of::<i32>() as libc::socklen_t,
            ))?;
        }
        fd
    };

    // Put socket into non-blocking mode.
    let flags = syscall!(fcntl(fd, libc::F_GETFL))? | libc::O_NONBLOCK;
    syscall!(fcntl(fd, libc::F_SETFL, flags))?;

    // Start connecting.
    match syscall!(connect(fd, &addr.storage as *const _ as *const _, addr.len)) {
        Ok(_) => {}
        Err(err) if err.raw_os_error() == Some(libc::EINPROGRESS) => {}
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
        Err(err) => return Err(err),
    }

    Ok(fd)
}

/// Creates a Unix stream in non-blocking mode that is connecting to the address.
///
/// The socket has only *begun* connecting but has not necessarily completed. To finish connecting,
/// the caller of this function should wait for the socket to become writable.
#[cfg(unix)]
pub fn unix_connect(path: impl AsRef<Path>) -> io::Result<UnixStream> {
    use std::cmp::Ordering;
    use std::os::unix::ffi::OsStrExt;

    let addr = unsafe {
        let mut addr = mem::zeroed::<libc::sockaddr_un>();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;

        let bytes = path.as_ref().as_os_str().as_bytes();

        match (bytes.get(0), bytes.len().cmp(&addr.sun_path.len())) {
            // Abstract paths don't need a null terminator
            (Some(&0), Ordering::Greater) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path must be no longer than SUN_LEN",
                ));
            }
            (Some(&0), _) => {}
            (_, Ordering::Greater) | (_, Ordering::Equal) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path must be shorter than SUN_LEN",
                ));
            }
            _ => {}
        }

        for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
            *dst = *src as libc::c_char;
        }
        // null byte for pathname is already there since we zeroed up front

        let base = &addr as *const _ as usize;
        let path = &addr.sun_path as *const _ as usize;
        let sun_path_offset = path - base;

        let mut len = sun_path_offset + bytes.len();
        match bytes.get(0) {
            Some(&0) | None => {}
            Some(_) => len += 1,
        }
        Addr::from_raw_parts(&addr as *const _ as *const _, len as libc::socklen_t)
    };

    let fd = connect(addr, libc::AF_UNIX, 0)?;
    unsafe { Ok(UnixStream::from_raw_fd(fd)) }
}

/// Creates a TCP stream in non-blocking mode that is connecting to the address.
///
/// The socket has only *begun* connecting but has not necessarily completed. To finish connecting,
/// the caller of this function should wait for the socket to become writable.
#[cfg(unix)]
pub fn tcp_connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let fd = connect(
        Addr::new(addr),
        if addr.is_ipv6() {
            libc::AF_INET6
        } else {
            libc::AF_INET
        },
        libc::IPPROTO_TCP,
    )?;
    unsafe { Ok(TcpStream::from_raw_fd(fd)) }
}

/// Creates a TCP stream in non-blocking mode that is connecting to the address.
///
/// The socket has only *begun* connecting but has not necessarily completed. To finish connecting,
/// the caller of this function should wait for the socket to become writable.
#[cfg(windows)]
pub fn tcp_connect(addr: std::net::SocketAddr) -> io::Result<TcpStream> {
    use std::net::UdpSocket;
    use std::sync::Once;

    use winapi::ctypes::{c_int, c_ulong};
    use winapi::shared::minwindef::DWORD;
    use winapi::shared::ntdef::HANDLE;
    use winapi::shared::ws2def::{AF_INET, AF_INET6, IPPROTO_TCP, SOCK_STREAM};
    use winapi::um::handleapi::SetHandleInformation;
    use winapi::um::winsock2 as sock;

    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Initialize winsock through the standard library by just creating a dummy socket.
        // Whether this is successful or not we drop the result as libstd will be sure to have
        // initialized winsock.
        let _ = UdpSocket::bind("127.0.0.1:34254");
    });

    const HANDLE_FLAG_INHERIT: DWORD = 0x00000001;
    const WSA_FLAG_OVERLAPPED: DWORD = 0x01;

    let family = if addr.is_ipv6() { AF_INET6 } else { AF_INET };
    let addr = Addr::new(addr);

    unsafe {
        let socket = match sock::WSASocketW(
            family,
            SOCK_STREAM,
            IPPROTO_TCP as _,
            ptr::null_mut(),
            0,
            WSA_FLAG_OVERLAPPED,
        ) {
            sock::INVALID_SOCKET => {
                return Err(io::Error::from_raw_os_error(sock::WSAGetLastError()))
            }
            socket => socket,
        };

        // Set no inherit.
        if SetHandleInformation(socket as HANDLE, HANDLE_FLAG_INHERIT, 0) == 0 {
            return Err(io::Error::last_os_error());
        }

        // Put socket into nonblocking mode.
        let mut nonblocking = true as c_ulong;
        if sock::ioctlsocket(socket, sock::FIONBIO as c_int, &mut nonblocking) != 0 {
            return Err(io::Error::last_os_error());
        }

        // Start connecting.
        match sock::connect(socket, &addr.storage as *const _ as *const _, addr.len) {
            0 => {}
            _ => match io::Error::from_raw_os_error(sock::WSAGetLastError()) {
                err if err.kind() == io::ErrorKind::WouldBlock => {}
                err => return Err(err),
            },
        }

        Ok(TcpStream::from_raw_socket(socket as _))
    }
}

/// Shuts down the write side of a socket.
///
/// If this source is not a socket, the `shutdown()` syscall error is ignored.
pub fn shutdown_write(#[cfg(unix)] raw: RawFd, #[cfg(windows)] raw: RawSocket) -> io::Result<()> {
    // This may not be a TCP stream, but that's okay. All we do is attempt a `shutdown()` on the
    // raw descriptor and ignore errors.
    let stream = unsafe {
        ManuallyDrop::new(
            #[cfg(unix)]
            TcpStream::from_raw_fd(raw),
            #[cfg(windows)]
            TcpStream::from_raw_socket(raw),
        )
    };

    // If the socket is a TCP stream, the only actual error can be ENOTCONN.
    match stream.shutdown(Shutdown::Write) {
        Err(err) if err.kind() == io::ErrorKind::NotConnected => Err(err),
        _ => Ok(()),
    }
}
