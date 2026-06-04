mod json_parser;
mod vectorize;
mod dataset;
mod fastpath;
mod ivf;
mod http;

use std::env;
use std::fs;
use std::os::unix::io::{FromRawFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use json_parser::{init_mcc_risk_table, parse_transaction};
use vectorize::{vectorize, quantize};
use dataset::Dataset;
use fastpath::{fast_path, FastResult};
use ivf::IVFIndex;
use http::{FraudResult, handle_connection};

struct AppState {
    dataset: Arc<Dataset>,
    ivf: Arc<IVFIndex>,
}

const NUM_WORKERS: usize = 4;

fn main() {
    let sock_path = env::var("SOCK").unwrap_or_default();
    println!("[API] Starting...");

    if let Some(parent) = std::path::Path::new(&sock_path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::remove_file(&sock_path);

    let uds_fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if fd < 0 { panic!("socket: {}", std::io::Error::last_os_error()); }

        let pb = sock_path.as_bytes();
        let mut sun: libc::sockaddr_un = std::mem::zeroed();
        sun.sun_family = libc::AF_UNIX as _;
        let ap = sun.sun_path.as_mut_ptr();
        let len = std::cmp::min(pb.len(), sun.sun_path.len() - 1);
        for i in 0..len { *ap.add(i) = pb[i] as libc::c_char; }
        let sl = std::mem::size_of::<libc::sockaddr_un>() as u32;

        if libc::bind(fd, &sun as *const _ as *const libc::sockaddr, sl) < 0 {
            panic!("bind UDS: {}", std::io::Error::last_os_error());
        }
        if libc::listen(fd, 64) < 0 { panic!("listen UDS: {}", std::io::Error::last_os_error()); }

        let sndbuf: i32 = 1024 * 1024;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, &sndbuf as *const _ as *const libc::c_void, 4);

        unsafe { libc::chmod(sock_path.as_ptr() as *const libc::c_char, 0o777); }
        fd
    };

    println!("[API] UDS listener at {sock_path} (fd={uds_fd})");

    let mcc_data = fs::read("resources/mcc_risk.json").expect("mcc_risk.json not found");
    init_mcc_risk_table(&mcc_data);

    let state = Arc::new(std::sync::Mutex::new(None::<Arc<AppState>>));

    let state_for_load = Arc::clone(&state);
    thread::spawn(move || {
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

        let loaded = AppState { dataset: Arc::new(ds), ivf: Arc::new(ivf) };
        let loaded_arc = Arc::new(loaded);
        warm_up(&loaded_arc);

        let mut w = state_for_load.lock().unwrap();
        *w = Some(loaded_arc);
        println!("[API] Ready for requests");
    });

    let queue: Arc<std::sync::Mutex<Vec<RawFd>>> = Arc::new(std::sync::Mutex::new(Vec::with_capacity(256)));

    for w_id in 0..NUM_WORKERS {
        let q = Arc::clone(&queue);
        let s = Arc::clone(&state);
        thread::spawn(move || {
            worker_loop(w_id, q, s);
        });
    }

    println!("[API] Accepting LB connections with {NUM_WORKERS} workers...");

    loop {
        let conn_fd = unsafe {
            libc::accept4(uds_fd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC)
        };
        if conn_fd < 0 {
            std::thread::sleep(std::time::Duration::from_micros(10));
            continue;
        }

        let q = Arc::clone(&queue);
        thread::spawn(move || {
            let fdsz = std::mem::size_of::<RawFd>();
            let cs = unsafe { libc::CMSG_SPACE(fdsz as u32) } as usize;
            let mut cbuf = vec![0u8; cs];
            let mut rbuf = [0u8; 1];
            let mut iov = libc::iovec {
                iov_base: rbuf.as_mut_ptr() as *mut libc::c_void,
                iov_len: 1,
            };

            loop {
                let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
                msg.msg_iov = &mut iov;
                msg.msg_iovlen = 1;
                msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
                msg.msg_controllen = cs as _;

                let r = unsafe { libc::recvmsg(conn_fd, &mut msg, 0) };
                if r <= 0 { break; }

                let client_fd = unsafe {
                    let cm = libc::CMSG_FIRSTHDR(&msg);
                    if cm.is_null() || (*cm).cmsg_type != libc::SCM_RIGHTS { -1 }
                    else { *(libc::CMSG_DATA(cm) as *mut RawFd) }
                };
                if client_fd < 0 { continue; }

                q.lock().unwrap().push(client_fd);
            }

            unsafe { libc::close(conn_fd); }
        });
    }
}

fn worker_loop(w_id: usize, queue: Arc<std::sync::Mutex<Vec<RawFd>>>, state: Arc<std::sync::Mutex<Option<Arc<AppState>>>>) {
    loop {
        let client_fd = {
            let mut q = queue.lock().unwrap();
            if q.is_empty() { None } else { Some(q.remove(0)) }
        };

        let client_fd = match client_fd {
            Some(fd) => fd,
            None => {
                std::thread::sleep(std::time::Duration::from_micros(50));
                continue;
            }
        };

        let started = Instant::now();
        unsafe {
            let mut stream = std::net::TcpStream::from_raw_fd(client_fd);
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_nodelay(true);
            let r = state.lock().unwrap();
            let s = r.as_ref().unwrap();
            handle_connection(stream, |body| {
                process(body, s)
            });
        }
        let elapsed = started.elapsed();
        if elapsed.as_millis() > 5 {
            eprintln!("[API-w{w_id}] slow request: {} us", elapsed.as_micros());
        }
    }
}

fn warm_up(state: &AppState) {
    println!("[API] Warming up...");
    let mut q = [0i16; 14];
    for _ in 0..256 {
        let _ = state.ivf.search(&state.dataset, &q, 5, ivf::IVF_NPROBE_EASY);
        q[0] = q[0].wrapping_add(100);
    }
}

fn process(body: &[u8], state: &AppState) -> FraudResult {
    let tx = parse_transaction(body);
    let mut v = [0.0f32; 14];
    vectorize(&tx, &mut v);

    match fast_path(&v) {
        FastResult::Legit => return FraudResult::Score(0),
        FastResult::Fraud => return FraudResult::Score(5),
        FastResult::Borderline => {}
    }

    let mut qv = [0i16; 14];
    for d in 0..14 { qv[d] = quantize(v[d]); }
    let fraud_count = state.ivf.search(&state.dataset, &qv, 5, 1);
    FraudResult::Score(fraud_count)
}