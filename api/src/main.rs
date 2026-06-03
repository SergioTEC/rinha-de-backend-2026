// Main entry point for Rinha API with Unix socket support (SCM_RIGHTS).
// Optimized: thread pool, zero-allocation hot path, mlock on index.

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
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, mpsc};
use std::thread;

use json_parser::{init_mcc_risk_table, parse_transaction};
use vectorize::{vectorize, quantize};
use dataset::Dataset;
use fastpath::{fast_path, FastResult};
use ivf::{IVFIndex, IVF_NPROBE_EASY, IVF_NPROBE_HARD};
use http::{FraudResult, handle_connection};

fn recv_fd_with_rights(unix_stream: &mut UnixStream) -> std::io::Result<RawFd> {
    let raw_fd = unix_stream.as_raw_fd();
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
    let result = unsafe { libc::recvmsg(raw_fd, &mut msg, 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
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

fn main() {
    let mode = env::var("LISTEN_SOCKET").unwrap_or_default();
    println!("[API] Starting...");

    let mcc_data = fs::read("resources/mcc_risk.json").expect("mcc_risk.json not found");
    init_mcc_risk_table(&mcc_data);

    println!("[API] Loading index_v2.bin...");
    let ds = Dataset::load_from_bin("resources/index_v2.bin");
    println!("[API] Dataset loaded: {} vectors, {} cells", ds.count, ds.num_cells);

    println!("[API] Building IVF index...");
    let ivf = IVFIndex::build_from_dataset(&ds, ds.num_cells);
    println!("[API] IVF index built: {} cells", ivf.num_cells);

    // mlock to prevent swapping (Linux only, ignored on Mac)
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

    // Warm-up
    warm_up(&state);

    // Spawn per connection (like top 3 do)
    let state_arc = Arc::clone(&state);

    if mode.starts_with("/") {
        let _ = fs::remove_file(&mode);
        
        #[cfg(target_os = "linux")]
        {
            // Linux: raw socket + accept() loop — no UnixListener overhead
            // Note: use SOCK_STREAM to match LB's UnixStream::connect (Rust default)
            let fd = unsafe {
                libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)
            };
            if fd < 0 {
                panic!("Failed to create SOCK_SEQPACKET socket");
            }
            
            use std::os::unix::ffi::OsStrExt;
            let path_bytes = std::path::Path::new(&mode).as_os_str().as_bytes();
            let mut addr: [libc::c_char; 108] = unsafe { std::mem::zeroed() };
            addr[0] = 0;
            let len = std::cmp::min(path_bytes.len(), addr.len() - 1);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    path_bytes.as_ptr() as *const libc::c_char,
                    addr.as_mut_ptr().offset(1),
                    len,
                );
            }
            let sun = libc::sockaddr_un {
                sun_family: libc::AF_UNIX as u16,
                sun_path: unsafe { std::mem::transmute(addr) },
            };
            let sun_len = std::mem::size_of::<libc::sockaddr_un>() as u32;
            
            let res = unsafe {
                libc::bind(fd, &sun as *const _ as *const libc::sockaddr, sun_len)
            };
            if res < 0 {
                panic!("Failed to bind SOCK_SEQPACKET: {}", std::io::Error::last_os_error());
            }
            
            let res = unsafe { libc::listen(fd, 64) };
            if res < 0 {
                panic!("Failed to listen: {}", std::io::Error::last_os_error());
            }
            
            println!("[API] Listening on Unix socket {} (SOCK_SEQPACKET)", mode);
            println!("[API] Ready");

            // Raw accept loop — NO UnixListener, NO extra socket setup
            loop {
                let mut client_addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
                let mut client_len = std::mem::size_of::<libc::sockaddr_un>() as u32;
                let client_fd = unsafe {
                    libc::accept(fd, &mut client_addr as *mut _ as *mut libc::sockaddr, &mut client_len)
                };
                if client_fd < 0 {
                    continue;
                }

                // Receive client TCP FD via SCM_RIGHTS
                let cmsg_size = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as libc::c_uint) } as usize;
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
                
                let res = unsafe { libc::recvmsg(client_fd, &mut msg, 0) };
                if res < 0 {
                    unsafe { libc::close(client_fd); }
                    continue;
                }
                
                let tcp_fd = unsafe {
                    let cmsg = libc::CMSG_FIRSTHDR(&msg);
                    if cmsg.is_null() || (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
                        libc::close(client_fd);
                        -1
                    } else {
                        let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
                        let fd = *data_ptr;
                        libc::close(client_fd);  // close the Unix socket, keep the TCP FD
                        fd
                    }
                };
                
                if tcp_fd < 0 {
                    continue;
                }
                
                // Use the TCP FD directly — LB already accepted the connection!
                let state_clone = Arc::clone(&state_arc);
                thread::spawn(move || {
                    unsafe {
                        let tcp_stream = std::net::TcpStream::from_raw_fd(tcp_fd);
                        handle_connection(tcp_stream, |body| {
                            handle_fraud_score(body, &state_clone)
                        });
                    }
                });
            }
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            // macOS: Unix sockets not supported in this build, use TCP
            eprintln!("[API] Unix sockets not supported on macOS, falling back to TCP");
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
        let fraud_count = state.ivf.search(&state.dataset, &query, 5, ivf::IVF_NPROBE_EASY);
        query[0] = query[0].wrapping_add(100);
    }
    println!("[API] Warm-up complete");
}

fn handle_fraud_score(body: &[u8], state: &AppState) -> FraudResult {
    let tx = parse_transaction(body);
    let mut v = [0.0f32; 14];
    vectorize(&tx, &mut v);

    // Heuristic fastpath (computationaly free)
    match fast_path(&v) {
        FastResult::Legit => return FraudResult::Score(0),
        FastResult::Fraud => return FraudResult::Score(5),
        FastResult::Borderline => {}
    }

    let mut qv = [0i16; 14];
    for d in 0..14 {
        qv[d] = quantize(v[d]);
    }

    // === FULL IVF SEARCH ===
    // nprobe=1, k=5 for speed (check only closest cell)
    let fraud_count = state.ivf.search(&state.dataset, &qv, 5, 1);
    FraudResult::Score(fraud_count)
}