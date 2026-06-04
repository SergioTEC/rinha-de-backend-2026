mod json_parser;
mod vectorize;
mod dataset;
mod fastpath;
mod ivf;
mod http;

use std::env;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::io::{FromRawFd, RawFd};
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread;

use json_parser::{init_mcc_risk_table, parse_transaction};
use vectorize::{vectorize, quantize};
use dataset::Dataset;
use fastpath::{fast_path, FastResult};
use ivf::IVFIndex;
use http::{FraudResult, handle_connection, HTTP_READY};

struct AppState {
    dataset: Arc<Dataset>,
    ivf: Arc<IVFIndex>,
}

static STATE: OnceLock<Arc<AppState>> = OnceLock::new();

fn main() {
    let sock_path = env::var("SOCK").unwrap_or_default();
    println!("[API] Starting...");

    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_TIMERSLACK, 1u64, 0, 0, 0);
    }

    if let Some(parent) = std::path::Path::new(&sock_path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::remove_file(&sock_path);

    let uds_fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
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

        let sndbuf: i32 = 256 * 1024;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, &sndbuf as *const _ as *const libc::c_void, 4);

        unsafe { libc::chmod(sock_path.as_ptr() as *const libc::c_char, 0o777); }
        fd
    };

    println!("[API] UDS listener at {sock_path} (fd={uds_fd})");

    let mcc_data = fs::read("resources/mcc_risk.json").expect("mcc_risk.json not found");
    init_mcc_risk_table(&mcc_data);

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
            let ptr_labels = ds.labels.as_ptr() as *const libc::c_void;
            libc::mlock(ptr_labels, ds.labels.len());
            let ptr_centroids = ds.centroids.as_ptr() as *const libc::c_void;
            libc::mlock(ptr_centroids, ds.centroids.len() * std::mem::size_of::<[i16; 14]>());
            let ptr_meta = ds.cell_meta.as_ptr() as *const libc::c_void;
            libc::mlock(ptr_meta, ds.cell_meta.len() * 8);
            let ptr_idx = ds.cell_indices.as_ptr() as *const libc::c_void;
            libc::mlock(ptr_idx, ds.cell_indices.len() * 4);
            println!("[API] mlock applied to all regions");
        }

        let loaded = AppState { dataset: Arc::new(ds), ivf: Arc::new(ivf) };
        let loaded_arc = Arc::new(loaded);
        warm_up(&loaded_arc);

        let _ = STATE.set(loaded_arc);
        println!("[API] Ready for requests");
    });

    println!("[API] Accepting LB connections...");

    loop {
        let conn_fd = unsafe {
            libc::accept4(uds_fd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC)
        };
        if conn_fd < 0 {
            std::thread::sleep(std::time::Duration::from_micros(10));
            continue;
        }

        thread::spawn(move || {
            let fdsz = std::mem::size_of::<RawFd>();
            let cs = unsafe { libc::CMSG_SPACE(fdsz as u32) } as usize;
            let mut cbuf = [0u8; 64];
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

                thread::spawn(move || {
                    unsafe {
                        let stream = std::net::TcpStream::from_raw_fd(client_fd);
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_nodelay(true);
                        let _ = stream.set_ttl(64);
                        let one: i32 = 1;
                        let fd = stream.as_raw_fd();
                        libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_QUICKACK, &one as *const _ as *const libc::c_void, 4);
                        handle_connection(stream, |body| {
                            match STATE.get() {
                                Some(state) => process(body, state),
                                None => FraudResult::Error,
                            }
                        });
                    }
                });
            }

            unsafe { libc::close(conn_fd); }
        });
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