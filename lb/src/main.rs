use std::env;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

fn main() {
    let args: Vec<String> = env::args().collect();
    let port = args.get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(9999u16);

    let backend_sockets = [
        "/tmp/sockets/api1.sock",
        "/tmp/sockets/api2.sock",
    ];

    println!("[LB] Starting on port {}", port);

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
        }
    }

    let mut next_backend = 0usize;

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
                            if let Ok(new) = UnixStream::connect(backend_sockets[idx]) {
                                backends[idx] = Some(new);
                            }
                            continue;
                        }
                        break;
                    }
                }
            }
            Err(e) => {
                eprintln!("[LB] Accept error: {}", e);
            }
        }
    }
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
    msg.msg_controllen = cmsg_space as libc::socklen_t;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::new(io::ErrorKind::Other, "CMSG_FIRSTHDR failed"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_size as libc::c_uint) as u32;

        let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
        *data_ptr = fd;
    }

    let res = unsafe { libc::sendmsg(sock_fd, &msg, 0) };
    if res < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}