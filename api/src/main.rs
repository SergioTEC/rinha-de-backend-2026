mod json_parser;
mod vectorize;
mod dataset;
mod fastpath;
mod ivf;
mod http;
mod server;
mod cache;

use std::env;
use std::fs;
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread;

use json_parser::{init_mcc_risk_table, parse_transaction};
use vectorize::{vectorize, quantize};
use dataset::{Dataset, DIMS};
use fastpath::{fast_path, FastResult};
use ivf::IVFIndex;
use http::FraudResult;

pub struct AppState {
    pub dataset: Arc<Dataset>,
    pub ivf: Arc<IVFIndex>,
}

static STATE: OnceLock<Arc<AppState>> = OnceLock::new();

fn main() {
    let sock_path = env::var("SOCK").unwrap_or_default();
    println!("[API] Starting...");

    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_TIMERSLACK, 1u64, 0, 0, 0);
        // mlockall(MCL_CURRENT | MCL_FUTURE): lock all current and future pages
        // in RAM, preventing page faults during the request hot path.
        // Best-effort: EPERM/RLIMIT_MEMLOCK errors are silent.
        libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE);
    }

    if let Some(parent) = std::path::Path::new(&sock_path).parent() {
        let _ = fs::create_dir_all(parent);
    }

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

        let loaded = AppState {
            dataset: Arc::new(ds),
            ivf: Arc::new(ivf),
        };
        let loaded_arc = Arc::new(loaded);
        warm_up(&loaded_arc);

        let _ = STATE.set(loaded_arc);
        println!("[API] Ready for requests");
    });

    println!("[API] Starting epoll servers ({} workers)...", num_workers());

    let get_state: Arc<dyn Fn() -> Option<Arc<AppState>> + Send + Sync> = Arc::new(|| STATE.get().cloned());
    let n = num_workers();
    let mut handles = Vec::with_capacity(n);
    for w in 0..n {
        let sock = if n == 1 {
            sock_path.clone()
        } else {
            format!("{}-w{}", sock_path, w)
        };
        let gs = get_state.clone();
        handles.push(std::thread::Builder::new()
            .name(format!("epoll-{}", w))
            .spawn(move || {
                if let Err(e) = server::run(&sock, gs) {
                    eprintln!("[API] worker {} server error: {}", w, e);
                }
            })
            .expect("spawn worker"));
    }
    for h in handles {
        let _ = h.join();
    }
}

fn num_workers() -> usize {
    std::env::var("API_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

fn warm_up(state: &AppState) {
    println!("[API] Warming up (varied synthetic queries)...");
    // Run 1024 varied synthetic queries to populate L1/L2 cache with centroid
    // and cell data, train the BPU on the search hot path, and pre-fault any
    // not-yet-touched pages of the dataset. Inspired by dalvorsn-cpp's 900ms
    // warmup window and bmtec-rust's pre-faulting strategy.
    // 1024 (vs 4096) keeps the startup fast enough for the bot's 60s health
    // check budget on a 2.6GHz Mac Mini.
    for i in 0..1024u32 {
        let mut q = [0i16; DIMS];
        for d in 0..DIMS {
            // Pseudo-random stride per query to touch different cache lines.
            q[d] = ((i.wrapping_mul(2654435761).wrapping_add(d as u32 * 37)) as i16).wrapping_mul(13);
        }
        let nprobe = if (i & 0xF) == 0 {
            ivf::IVF_NPROBE_REPAIR
        } else {
            ivf::IVF_NPROBE_EASY
        };
        let _ = state.ivf.search(&state.dataset, &q, 5, nprobe);
    }
    println!("[API] Warmup done");
}

pub fn process(body: &[u8], state: &AppState) -> FraudResult {
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
    // Cache disabled — was potentially poisoning results.
    // (Re-enable after verifying detection accuracy baseline.)
    let fraud_count = state.ivf.search(&state.dataset, &qv, 5, 1) as u8;
    FraudResult::Score(fraud_count as usize)
}