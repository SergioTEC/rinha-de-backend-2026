use std::env;
use std::io;
use std::net::TcpStream;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};

static RR: AtomicUsize = AtomicUsize::new(0);

fn main() {
    let port = env::var("LB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9999u16);

    let backends: Vec<String> = env::var("LB_BACKENDS")
        .unwrap_or_else(|_| "api1:9001,api2:9002".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let lfd = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0);
        if fd < 0 { panic!("socket: {}", io::Error::last_os_error()); }
        let on: i32 = 1;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, &on as *const _ as *const libc::c_void, 4);
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, &on as *const _ as *const libc::c_void, 4);
        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        addr.sin_family = libc::AF_INET as _;
        addr.sin_port = (port as u16).to_be();
        addr.sin_addr.s_addr = libc::INADDR_ANY;
        if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_in>() as u32) < 0 {
            panic!("bind: {}", io::Error::last_os_error());
        }
        if libc::listen(fd, 65535) < 0 { panic!("listen: {}", io::Error::last_os_error()); }
        fd
    };

    let n = backends.len();

    loop {
        let client_fd = unsafe {
            libc::accept4(lfd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC)
        };
        if client_fd < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                let mut pfd = libc::pollfd { fd: lfd, events: libc::POLLIN, revents: 0 };
                unsafe { libc::poll(&mut pfd, 1, -1); }
            }
            continue;
        }

        let idx = RR.fetch_add(1, Ordering::Relaxed) % n;

        let backend_fd = match TcpStream::connect(&backends[idx]) {
            Ok(s) => s.into_raw_fd(),
            Err(_) => { unsafe { libc::close(client_fd); } continue; }
        };

        std::thread::spawn(move || {
            unsafe {
                let on: i32 = 1;
                libc::setsockopt(client_fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, &on as *const _ as *const libc::c_void, 4);
                libc::setsockopt(backend_fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, &on as *const _ as *const libc::c_void, 4);
            }

            let c2b = std::thread::spawn(move || { forward(client_fd, backend_fd); });
            forward(backend_fd, client_fd);
            let _ = c2b.join();

            unsafe { libc::close(client_fd); libc::close(backend_fd); }
        });
    }
}

fn forward(from: RawFd, to: RawFd) {
    let mut buf = [0u8; 16384];
    loop {
        let n = unsafe { libc::read(from, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 { break; }
        let mut written = 0isize;
        while written < n {
            let w = unsafe { libc::write(to, buf.as_ptr().offset(written) as *mut libc::c_void, (n - written) as usize) };
            if w <= 0 { return; }
            written += w;
        }
    }
}