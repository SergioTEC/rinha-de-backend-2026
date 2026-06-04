mod json_parser;
mod vectorize;
mod dataset;
mod fastpath;
mod ivf;
mod http;
mod server;

use std::env;
use std::fs;
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread;

use json_parser::{init_mcc_risk_table, parse_transaction};
use vectorize::{vectorize, quantize};
use dataset::Dataset;
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

    println!("[API] Starting epoll server...");

    let get_state: Arc<dyn Fn() -> Option<Arc<AppState>> + Send + Sync> = Arc::new(|| STATE.get().cloned());
    if let Err(e) = server::run(&sock_path, get_state) {
        eprintln!("[API] server error: {}", e);
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
    let fraud_count = state.ivf.search(&state.dataset, &qv, 5, 1);
    FraudResult::Score(fraud_count)
}