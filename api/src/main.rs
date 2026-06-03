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
        for d in 0..ds.dims.len() {
            let ptr = ds.dims[d].as_ptr() as *const libc::c_void;
            let len = ds.dims[d].len() * 2;
            libc::mlock(ptr, len);
        }
        println!("[API] mlock applied");
    }

    let state = Arc::new(AppState {
        dataset: Arc::new(ds),
        ivf: Arc::new(ivf),
    });

    // Warm-up
    warm_up(&state);

    // Thread pool: 1 worker for minimal memory (Rinha has 1 CPU total)
    let num_workers = 1;
    let (tx, rx) = mpsc::channel();
    let rx = Arc::new(std::sync::Mutex::new(rx));
    
    for _ in 0..num_workers {
        let rx_clone = Arc::clone(&rx);
        let state_clone = Arc::clone(&state);
        thread::spawn(move || {
            loop {
                let stream_result = {
                    let rx = rx_clone.lock().unwrap();
                    rx.recv()
                };
                match stream_result {
                    Ok(stream) => {
                        handle_connection(stream, |body| {
                            handle_fraud_score(body, &state_clone)
                        });
                    }
                    Err(_) => break,
                }
            }
        });
    }

    if mode.starts_with("/") {
        let _ = fs::remove_file(&mode);
        let listener = UnixListener::bind(&mode).expect("Failed to bind Unix socket");
        println!("[API] Listening on Unix socket {}", mode);
        println!("[API] Ready");

        for stream in listener.incoming() {
            match stream {
                Ok(mut unix_stream) => {
                    match recv_fd_with_rights(&mut unix_stream) {
                        Ok(client_fd) => {
                            unsafe {
                                let tcp_stream = std::net::TcpStream::from_raw_fd(client_fd);
                                let _ = tx.send(tcp_stream);
                            }
                        }
                        Err(e) => eprintln!("SCM_RIGHTS recv error: {}", e),
                    }
                }
                Err(e) => eprintln!("Accept error: {}", e),
            }
        }
    } else {
        let port = env::var("PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9999u16);
        let addr = format!("0.0.0.0:{}", port);
        let listener = TcpListener::bind(&addr).expect("Failed to bind TCP");
        
        // Enable TCP_NODELAY to reduce latency (disable Nagle algorithm)
        listener.set_nonblocking(false).expect("set_nonblocking");
        
        println!("[API] Listening on TCP {}", addr);
        println!("[API] Ready");

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    // Disable Nagle algorithm for low latency
                    let _ = stream.set_nodelay(true);
                    let _ = tx.send(stream);
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

    match fast_path(&v) {
        FastResult::Legit => return FraudResult::Score(0),
        FastResult::Fraud => return FraudResult::Score(5),
        FastResult::Borderline => {}
    }

    let mut qv = [0i16; 14];
    for d in 0..14 {
        qv[d] = quantize(v[d]);
    }

    let nprobe = if v[2] < 0.3 && v[7] < 0.3 && v[11] < 0.5 {
        IVF_NPROBE_EASY
    } else {
        IVF_NPROBE_HARD
    };

    let fraud_count = state.ivf.search(&state.dataset, &qv, 5, nprobe);
    FraudResult::Score(fraud_count)
}