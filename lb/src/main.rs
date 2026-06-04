use std::env;
use std::io;
use std::os::unix::io::RawFd;

fn main() {
    let port = env::var("LB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9999u16);

    let backends_env = env::var("LB_BACKENDS")
        .unwrap_or_else(|_| "/tmp/sockets/api1.sock,/tmp/sockets/api2.sock".to_string());

    let backend_paths: Vec<String> = backends_env
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let mut backends: Vec<RawFd> = Vec::with_capacity(backend_paths.len());
    for path in &backend_paths {
        loop {
            match connect_seqpacket(path) {
                Ok(fd) => {
                    backends.push(fd);
                    break;
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
    }

    let lfd = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            panic!("socket: {}", io::Error::last_os_error());
        }
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
        if libc::listen(fd, 65535) < 0 {
            panic!("listen: {}", io::Error::last_os_error());
        }
        fd
    };

    let n = backends.len();
    let mut rr = 0usize;

    loop {
        let client_fd = unsafe {
            libc::accept4(lfd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
        };
        if client_fd < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                let mut pfd = libc::pollfd {
                    fd: lfd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe { libc::poll(&mut pfd, 1, -1); }
            }
            continue;
        }

        unsafe {
            let on: i32 = 1;
            libc::setsockopt(client_fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, &on as *const _ as *const libc::c_void, 4);
            libc::setsockopt(client_fd, libc::IPPROTO_TCP, libc::TCP_QUICKACK, &on as *const _ as *const libc::c_void, 4);
        }

        let first = rr;
        rr = (rr + 1) % n;
        let mut sent = false;
        for attempt in 0..n {
            let ch = backends[(first + attempt) % n];
            if send_fd_seqpacket(ch, client_fd).is_ok() {
                sent = true;
                break;
            }
        }
        if !sent {
            let ch = backends[first];
            let _ = send_fd_seqpacket(ch, client_fd);
        }
        unsafe { libc::close(client_fd); }
    }
}

fn connect_seqpacket(path: &str) -> io::Result<RawFd> {
    let path_bytes = path.as_bytes();
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut sun: libc::sockaddr_un = std::mem::zeroed();
        sun.sun_family = libc::AF_UNIX as _;
        let addr_ptr = sun.sun_path.as_mut_ptr();
        let path_ptr = path_bytes.as_ptr();
        let len = std::cmp::min(path_bytes.len(), sun.sun_path.len() - 1);
        for i in 0..len {
            *addr_ptr.add(i) = *(path_ptr.add(i) as *const libc::c_char);
        }
        let sun_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;

        if libc::connect(fd, &sun as *const _ as *const libc::sockaddr, sun_len) < 0 {
            libc::close(fd);
            return Err(io::Error::last_os_error());
        }

        Ok(fd)
    }
}

fn send_fd_seqpacket(backend_fd: RawFd, client_fd: RawFd) -> io::Result<()> {
    let dummy = [0u8; 1];
    let fd_size = std::mem::size_of::<RawFd>();
    let cmsg_space = unsafe { libc::CMSG_SPACE(fd_size as libc::c_uint) } as usize;
    let mut cmsg_buf = [0u8; 64];

    let mut iov = libc::iovec {
        iov_base: dummy.as_ptr() as *mut libc::c_void,
        iov_len: dummy.len(),
    };

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::new(io::ErrorKind::Other, "CMSG_FIRSTHDR failed"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_size as libc::c_uint) as _;
        let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
        *data_ptr = client_fd;
    }

    let res = unsafe { libc::sendmsg(backend_fd, &msg, libc::MSG_NOSIGNAL) };
    if res < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}