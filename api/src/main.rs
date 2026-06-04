mod json_parser;
mod vectorize;
mod dataset;
mod fastpath;
mod ivf;
mod http;

use std::env;
use std::fs;
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::Arc;
use std::thread;

use json_parser::{init_mcc_risk_table, parse_transaction};
use vectorize::{vectorize, quantize};
use dataset::Dataset;
use fastpath::{fast_path, FastResult};
use ivf::IVFIndex;
use http::{FraudResult, handle_connection};

fn recv_fd_raw(fd: RawFd) -> std::io::Result<RawFd> {
    let fd_size = std::mem::size_of::<RawFd>();
    let cmsg_size = unsafe { libc::CMSG_SPACE(fd_size as libc::c_uint) } as usize;
    let mut control_buf = vec![0u8; cmsg_size];
    let mut recv_buf = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: recv_buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: recv_buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_size as _;

    let result = unsafe { libc::recvmsg(fd, &mut msg, 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if result == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "LB disconnected"));
    }

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "No CMSG received"));
        }
        if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Unexpected CMSG type"));
        }
        let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
        Ok(*data_ptr)
    }
}

struct AppState {
    dataset: Arc<Dataset>,
    ivf: Arc<IVFIndex>,
}

fn create_uds_listener(path: &str) -> i32 {
    let _ = fs::remove_file(path);

    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        panic!("Failed to create SOCK_SEQPACKET socket: {}", std::io::Error::last_os_error());
    }

    let sndbuf: i32 = 256 * 1024;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &sndbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<i32>() as u32,
        );
    }

    use std::os::unix::ffi::OsStrExt;
    let path_bytes = std::path::Path::new(path).as_os_str().as_bytes();
    let mut addr: [libc::c_char; 108] = unsafe { std::mem::zeroed() };
    let len = std::cmp::min(path_bytes.len(), addr.len() - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr() as *const libc::c_char,
            addr.as_mut_ptr(),
            len,
        );
    }

    let sun = libc::sockaddr_un {
        sun_family: libc::AF_UNIX as u16,
        sun_path: unsafe { std::mem::transmute(addr) },
    };
    let sun_len = std::mem::size_of::<libc::sockaddr_un>() as u32;

    let res = unsafe { libc::bind(fd, &sun as *const _ as *const libc::sockaddr, sun_len) };
    if res < 0 {
        panic!("Failed to bind UDS {}: {}", path, std::io::Error::last_os_error());
    }

    let res = unsafe { libc::listen(fd, 64) };
    if res < 0 {
        panic!("Failed to listen UDS: {}", std::io::Error::last_os_error());
    }

    let path_cstr = std::ffi::CString::new(path).unwrap();
    unsafe { libc::chmod(path_cstr.as_ptr(), 0o777); }

    fd
}

fn accept_uds_conn(listener_fd: i32) -> Option<i32> {
    let mut client_addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut client_len = std::mem::size_of::<libc::sockaddr_un>() as u32;
    let client_fd = unsafe {
        libc::accept4(
            listener_fd,
            &mut client_addr as *mut _ as *mut libc::sockaddr,
            &mut client_len,
            libc::SOCK_CLOEXEC,
        )
    };
    if client_fd < 0 {
        return None;
    }
    Some(client_fd)
}

fn main() {
    let mode = env::var("LISTEN_SOCKET").unwrap_or_default();
    println!("[API] Starting...");

    let uds_listener_fd: Option<i32> = if mode.starts_with("/") {
        #[cfg(target_os = "linux")]
        {
            Some(create_uds_listener(&mode))
        }
        #[cfg(not(target_os = "linux"))]
        {
            eprintln!("[API] UDS requires Linux; falling back to TCP");
            None
        }
    } else {
        None
    };

    if let Some(fd) = uds_listener_fd {
        println!("[API] UDS listener created at {} (fd={})", mode, fd);
    }

    let mcc_data = fs::read("resources/mcc_risk.json").expect("mcc_risk.json not found");
    init_mcc_risk_table(&mcc_data);

    println!("[API] Loading index_v2.bin...");
    let ds = Dataset::load_from_bin("resources/index_v2.bin");
    println!("[API] Dataset loaded: {} vectors, {} cells", ds.count, ds.num_cells);

    println!("[API] Building IVF index...");
    let ivf = IVFIndex::build_from_dataset(&ds, ds.num_cells);
    println!("[API] IVF index built: {} cells", ivf.num_cells);

    #[cfg(target_os = "linux")]
    unsafe {
        let ptr = ds.dims.as_ptr() as *const libc::c_void;
        let len = ds.dims.len() * 2;
        libc::mlock(ptr, len);
        println!("[API] mlock applied");
    }

    let state = Arc::new(AppState {
        dataset: Arc::new(ds),
        ivf: Arc::new(ivf),
    });

    warm_up(&state);

    let state_arc = Arc::clone(&state);

    if let Some(fd) = uds_listener_fd {
        println!("[API] Listening on Unix socket {} (SOCK_SEQPACKET)", mode);
        println!("[API] Ready");

        loop {
            let uds_fd = match accept_uds_conn(fd) {
                Some(fd) => fd,
                None => {
                    std::thread::sleep(std::time::Duration::from_micros(10));
                    continue;
                }
            };

            println!("[API] LB connected (uds_fd={})", uds_fd);

            let state_clone = Arc::clone(&state_arc);
            thread::spawn(move || {
                let fd_size = std::mem::size_of::<RawFd>();
                let cmsg_size = unsafe { libc::CMSG_SPACE(fd_size as libc::c_uint) } as usize;
                let mut control_buf = vec![0u8; cmsg_size];
                let mut recv_buf = [0u8; 1];
                let mut iov = libc::iovec {
                    iov_base: recv_buf.as_mut_ptr() as *mut libc::c_void,
                    iov_len: recv_buf.len(),
                };

                loop {
                    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
                    msg.msg_iov = &mut iov;
                    msg.msg_iovlen = 1;
                    msg.msg_control = control_buf.as_mut_ptr() as *mut libc::c_void;
                    msg.msg_controllen = cmsg_size as _;

                    let res = unsafe { libc::recvmsg(uds_fd, &mut msg, 0) };
                    if res <= 0 {
                        if res < 0 {
                            let err = std::io::Error::last_os_error();
                            if err.kind() != std::io::ErrorKind::WouldBlock {
                                eprintln!("[API] recvmsg error: {}", err);
                            }
                        }
                        break;
                    }

                    let tcp_fd = unsafe {
                        let cmsg = libc::CMSG_FIRSTHDR(&msg);
                        if cmsg.is_null()
                            || (*cmsg).cmsg_level != libc::SOL_SOCKET
                            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
                        {
                            -1
                        } else {
                            let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
                            *data_ptr
                        }
                    };

                    if tcp_fd < 0 {
                        continue;
                    }

                    let state_spawn = Arc::clone(&state_clone);
                    thread::spawn(move || {
                        unsafe {
                            let tcp_stream = std::net::TcpStream::from_raw_fd(tcp_fd);
                            let _ = tcp_stream.set_nodelay(true);
                            handle_connection(tcp_stream, |body| {
                                handle_fraud_score(body, &state_spawn)
                            });
                        }
                    });
                }

                unsafe { libc::close(uds_fd); }
                println!("[API] LB disconnected (uds_fd={})", uds_fd);
            });
        }
    } else {
        let port = env::var("PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9999u16);
        let addr = format!("0.0.0.0:{}", port);
        let listener = TcpListener::bind(&addr).expect("Failed to bind TCP");
        listener.set_nonblocking(false).expect("set_nonblocking");
        println!("[API] Listening on TCP {}", addr);
        println!("[API] Ready");

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    let state_clone = Arc::clone(&state_arc);
                    thread::spawn(move || {
                        handle_connection(stream, |body| {
                            handle_fraud_score(body, &state_clone)
                        });
                    });
                }
                Err(e) => eprintln!("Accept error: {}", e),
            }
        }
    }
}

fn warm_up(state: &AppState) {
    println!("[API] Warming up...");
    let mut query = [0i16; 14];
    for _ in 0..256 {
        let _ = state.ivf.search(&state.dataset, &query, 5, ivf::IVF_NPROBE_EASY);
        query[0] = query[0].wrapping_add(100);
    }
    println!("[API] Warm-up complete");
}

fn handle_fraud_score(body: &[u8], state: &AppState) -> FraudResult {
    let tx = parse_transaction(body);
    let mut v = [0.0f32; 14];
    vectorize(&tx, &mut v);

    match fast_path(&v) {
        FastResult::Legit => return FraudResult::Score(0),
        FastResult::Fraud => return FraudResult::Score(5),
        FastResult::Borderline => {}
    }

    let mut qv = [0i16; 14];
    for d in 0..14 {
        qv[d] = quantize(v[d]);
    }

    let fraud_count = state.ivf.search(&state.dataset, &qv, 5, 1);
    FraudResult::Score(fraud_count)
}