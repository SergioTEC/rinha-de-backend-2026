use std::env;
use std::io;
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

const SMALL_STACK: usize = 32 * 1024;

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

    println!("[LB] Starting on port {}", port);
    println!("[LB] Backends: {:?}", backend_paths);

    let fd = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            panic!("Failed to create TCP socket: {}", io::Error::last_os_error());
        }
        let on: i32 = 1;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, &on as *const _ as *const libc::c_void, std::mem::size_of::<i32>() as u32);
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, &on as *const _ as *const libc::c_void, std::mem::size_of::<i32>() as u32);

        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        addr.sin_family = libc::AF_INET as _;
        addr.sin_port = (port as u16).to_be();
        addr.sin_addr.s_addr = libc::INADDR_ANY;

        if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_in>() as u32) < 0 {
            panic!("Failed to bind TCP: {}", io::Error::last_os_error());
        }
        if libc::listen(fd, 1024) < 0 {
            panic!("Failed to listen: {}", io::Error::last_os_error());
        }
        fd
    };
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    listener.set_nonblocking(true).ok();
    println!("[LB] TCP listener bound on port {}", port);

    let (backend_tx, backend_rx) = std::sync::mpsc::channel::<Option<io::Result<RawFd>>>();

    for path in &backend_paths {
        let path = path.clone();
        let tx = backend_tx.clone();
        let result = std::thread::Builder::new().stack_size(SMALL_STACK).spawn(move || {
            if wait_for_socket(&path, 600, 100).is_err() {
                println!("[LB] Timeout waiting for {}", path);
                let _ = tx.send(Some(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("Socket {} did not appear", path),
                ))));
                return;
            }
            println!("[LB] Socket {} exists. Connecting...", path);

            let result = connect_seqpacket(&path, 300, 100);
            let _ = tx.send(result);
        }).unwrap();
    }

    let mut backends: Vec<Option<RawFd>> = Vec::with_capacity(backend_paths.len());
    for _ in 0..backend_paths.len() {
        backends.push(None);
    }

    let mut next_backend = 0usize;
    let mut connected_count = 0usize;

    println!("[LB] Ready");

    for stream in listener.incoming() {
        while let Ok(Some(result)) = backend_rx.try_recv() {
            if let Some(idx) = backends.iter().position(|b| b.is_none()) {
                match result {
                    Ok(fd) => {
                        backends[idx] = Some(fd);
                        connected_count += 1;
                        println!("[LB] Backend {}/{} connected", connected_count, backends.len());
                    }
                    Err(e) => {
                        eprintln!("[LB] Backend connection failed: {}", e);
                    }
                }
            }
        }

        match stream {
            Ok(client) => {
                let _ = client.set_nodelay(true);

                if connected_count == 0 {
                    let resp = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = unsafe { libc::write(client.as_raw_fd(), resp.as_ptr() as *const libc::c_void, resp.len()) };
                    drop(client);
                    continue;
                }

                let client_fd = client.as_raw_fd();

                let mut attempts = 0;
                let mut sent = false;
                while attempts < backends.len() {
                    let idx = next_backend;
                    next_backend = (next_backend + 1) % backends.len();
                    attempts += 1;

                    if let Some(backend_fd) = backends[idx] {
                        if send_fd_seqpacket(backend_fd, client_fd).is_ok() {
                            sent = true;
                            break;
                        } else {
                            println!("[LB] Backend {} disconnected, reconnecting...", idx);
                            unsafe { libc::close(backend_fd); }
                            backends[idx] = None;
                            connected_count -= 1;

                            let path = backend_paths[idx].clone();
                            let tx = backend_tx.clone();
                            std::thread::spawn(move || {
                                if wait_for_socket(&path, 600, 100).is_ok() {
                                    let result = connect_seqpacket(&path, 100, 50);
                                    let _ = tx.send(result);
                                }
                            });
                        }
                    }
                }

                if !sent {
                    drop(client);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_micros(100));
            }
            Err(e) => {
                eprintln!("[LB] Accept error: {}", e);
            }
        }
    }
}

fn wait_for_socket(path: &str, max_tries: usize, delay_ms: u64) -> io::Result<()> {
    use std::path::Path;
    let p = Path::new(path);
    for _ in 0..max_tries {
        if p.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("Socket file {} did not appear after {} tries", path, max_tries),
    ))
}

fn connect_seqpacket(path: &str, max_retries: usize, delay_ms: u64) -> Option<io::Result<RawFd>> {
    let path_bytes = path.as_bytes();

    for attempt in 0..max_retries {
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0);
            if fd < 0 {
                return Some(Err(io::Error::last_os_error()));
            }

            let sndbuf: i32 = 256 * 1024;
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &sndbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<i32>() as u32,
            );

            let mut sun: libc::sockaddr_un = std::mem::zeroed();
            sun.sun_family = libc::AF_UNIX as _;
            let addr_ptr = sun.sun_path.as_mut_ptr();
            let path_ptr = path_bytes.as_ptr();
            let len = std::cmp::min(path_bytes.len(), sun.sun_path.len() - 1);
            for i in 0..len {
                *addr_ptr.add(i) = *(path_ptr.add(i) as *const libc::c_char);
            }

            let sun_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;

            if libc::connect(fd, &sun as *const _ as *const libc::sockaddr, sun_len) == 0 {
                return Some(Ok(fd));
            }

            let err = io::Error::last_os_error();
            libc::close(fd);

            if attempt < max_retries - 1 {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            } else {
                return Some(Err(err));
            }
        }
    }

    Some(Err(io::Error::new(io::ErrorKind::NotConnected, "Max retries exceeded")))
}

fn send_fd_seqpacket(backend_fd: RawFd, fd: RawFd) -> io::Result<()> {
    let dummy = [0u8; 1];
    let fd_size = std::mem::size_of::<RawFd>();
    let cmsg_space = unsafe { libc::CMSG_SPACE(fd_size as libc::c_uint) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

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
        *data_ptr = fd;
    }

    let res = unsafe { libc::sendmsg(backend_fd, &msg, libc::MSG_NOSIGNAL) };
    if res < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}