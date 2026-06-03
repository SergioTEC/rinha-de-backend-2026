use std::env;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

fn main() {
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

    // Set non-blocking for faster accept
    listener.set_nonblocking(true).ok();

    let mut backends: Vec<Option<UnixStream>> = Vec::new();
    for path in &backend_sockets {
        match connect_with_retry(path, 60, 200) {
            Ok(stream) => {
                println!("[LB] Connected to {}", path);
                backends.push(Some(stream));
            }
            Err(e) => {
                eprintln!("[LB] Failed to connect to {}: {}", path, e);
                backends.push(None);
            }
        }
    }

    let mut next_backend = 0usize;

    for stream in listener.incoming() {
        match stream {
            Ok(client) => {
                let mut attempts = 0;
                let mut sent = false;
                while attempts < backends.len() {
                    let idx = next_backend;
                    next_backend = (next_backend + 1) % backends.len();
                    attempts += 1;

                    if let Some(ref mut unix) = backends[idx] {
                        let fd = client.as_raw_fd();
                        if let Err(e) = send_fd(unix, fd) {
                            eprintln!("[LB] send_fd error: {}, reconnecting...", e);
                            // Reconnect
                            match connect_with_retry(&backend_sockets[idx], 5, 100) {
                                Ok(new_stream) => {
                                    backends[idx] = Some(new_stream);
                                    // Retry send with new connection
                                    if let Some(ref mut unix2) = backends[idx] {
                                        if let Err(e2) = send_fd(unix2, fd) {
                                            eprintln!("[LB] Retry send_fd failed: {}", e2);
                                        } else {
                                            sent = true;
                                            break;
                                        }
                                    }
                                }
                                Err(e2) => {
                                    eprintln!("[LB] Reconnect failed: {}", e2);
                                }
                            }
                            continue;
                        }
                        sent = true;
                        break;
                    }
                }

                if !sent {
                    // Drop client if couldn't send to any backend
                    drop(client);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No connections available, brief sleep to avoid busy loop
                std::thread::sleep(std::time::Duration::from_micros(10));
            }
            Err(e) => {
                eprintln!("[LB] Accept error: {}", e);
            }
        }
    }
}

fn connect_with_retry(path: &str, max_retries: usize, delay_ms: u64) -> io::Result<UnixStream> {
    for attempt in 0..max_retries {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) if attempt < max_retries - 1 => {
                eprintln!("[LB] Retry {}/{} connecting to {}: {}", attempt + 1, max_retries, path, e);
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::NotConnected, "Max retries exceeded"))
}

fn send_fd(stream: &mut UnixStream, fd: RawFd) -> io::Result<()> {
    let sock_fd = stream.as_raw_fd();
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

    let res = unsafe { libc::sendmsg(sock_fd, &msg, 0) };
    if res < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}
