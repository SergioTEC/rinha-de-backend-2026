use std::env;
use std::io;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn main() {
<<<<<<< HEAD
    let port = env::var("LB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9999u16);

    let backends_env = env::var("LB_BACKENDS")
        .unwrap_or_else(|_| "/tmp/sockets/api1.sock,/tmp/sockets/api2.sock".to_string());
    
    let backend_sockets: Vec<String> = backends_env
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    println!("[LB] Starting on port {}", port);
    println!("[LB] Backends: {:?}", backend_sockets);

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
        .expect("Failed to bind TCP");

    let mut backends: Vec<Option<UnixStream>> = Vec::new();
    for path in &backend_sockets {
        match UnixStream::connect(path) {
            Ok(stream) => {
                println!("[LB] Connected to {}", path);
                backends.push(Some(stream));
            }
            Err(e) => {
                eprintln!("[LB] Failed to connect to {}: {}", path, e);
                backends.push(None);
            }
=======
    let port = env::var("PORT").unwrap_or_else(|_| "9999".to_string());
    let port: u16 = port.parse().unwrap_or(9999);
    let upstreams = env::var("FD_UPSTREAMS").unwrap_or_else(|_| "/tmp/sock/api1.sock,/tmp/sock/api2.sock".to_string());
    let upstream_paths: Vec<String> = upstreams.split(',').map(|s| s.trim().to_string()).collect();

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
>>>>>>> submission
        }
        if libc::listen(fd, 65535) < 0 { panic!("listen: {}", io::Error::last_os_error()); }
        fd
    };

    let backends: Arc<std::sync::Mutex<Vec<RawFd>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    for path in &upstream_paths {
        let path = path.clone();
        let b = Arc::clone(&backends);
        std::thread::spawn(move || {
            loop {
                if let Ok(fd) = connect_uds(&path) {
                    b.lock().unwrap().push(fd);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        });
    }

    let rr = AtomicUsize::new(0);

<<<<<<< HEAD
    for stream in listener.incoming() {
        match stream {
            Ok(client) => {
                // Round-robin
                let mut attempts = 0;
                while attempts < backends.len() {
                    let idx = next_backend;
                    next_backend = (next_backend + 1) % backends.len();
                    attempts += 1;

                    if let Some(ref mut unix) = backends[idx] {
                        let fd = client.as_raw_fd();
                        if let Err(e) = send_fd(unix, fd) {
                            eprintln!("[LB] send_fd error: {}", e);
                            // Try to reconnect
                            if let Ok(new) = UnixStream::connect(&backend_sockets[idx]) {
                                backends[idx] = Some(new);
                            }
                            continue;
                        }
                        break;
                    }
                }
=======
    loop {
        let cf = unsafe { libc::accept4(lfd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if cf < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock {
                let mut pfd = libc::pollfd { fd: lfd, events: libc::POLLIN, revents: 0 };
                unsafe { libc::poll(&mut pfd, 1, -1); }
>>>>>>> submission
            }
            continue;
        }

        unsafe {
            let on: i32 = 1;
            libc::setsockopt(cf, libc::IPPROTO_TCP, libc::TCP_NODELAY, &on as *const _ as *const libc::c_void, 4);
        }

        let bf = {
            let v = backends.lock().unwrap();
            if v.is_empty() { None } else {
                let idx = rr.fetch_add(1, Ordering::Relaxed) % v.len();
                Some(v[idx])
            }
        };

        match bf {
            Some(bfd) => {
                let _ = send_fd(bfd, cf);
            }
            None => {
                let mut buf = [0u8; 64];
                let _ = unsafe { libc::recv(cf, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            }
        }
        unsafe { libc::close(cf); }
    }
}

fn connect_uds(path: &str) -> io::Result<RawFd> {
    let pb = path.as_bytes();
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if fd < 0 { return Err(io::Error::last_os_error()); }
        let mut sun: libc::sockaddr_un = std::mem::zeroed();
        sun.sun_family = libc::AF_UNIX as _;
        let ap = sun.sun_path.as_mut_ptr();
        let len = std::cmp::min(pb.len(), sun.sun_path.len() - 1);
        for i in 0..len { *ap.add(i) = pb[i] as libc::c_char; }
        let sl = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        if libc::connect(fd, &sun as *const _ as *const libc::sockaddr, sl) < 0 {
            libc::close(fd);
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn send_fd(backend_fd: RawFd, client_fd: RawFd) -> io::Result<()> {
    let dummy = [0u8; 1];
    let fdsz = std::mem::size_of::<RawFd>();
    let cs = unsafe { libc::CMSG_SPACE(fdsz as u32) } as usize;
    let mut cbuf = [0u8; 64];
    let mut iov = libc::iovec { iov_base: dummy.as_ptr() as *mut libc::c_void, iov_len: 1 };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cs as _;
    unsafe {
        let cm = libc::CMSG_FIRSTHDR(&msg);
        (*cm).cmsg_level = libc::SOL_SOCKET;
        (*cm).cmsg_type = libc::SCM_RIGHTS;
        (*cm).cmsg_len = libc::CMSG_LEN(fdsz as u32) as _;
        *(libc::CMSG_DATA(cm) as *mut RawFd) = client_fd;
    }
<<<<<<< HEAD

    let res = unsafe { libc::sendmsg(sock_fd, &msg, 0) };
    if res < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}
=======
    let r = unsafe { libc::sendmsg(backend_fd, &msg, libc::MSG_NOSIGNAL) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
>>>>>>> submission
