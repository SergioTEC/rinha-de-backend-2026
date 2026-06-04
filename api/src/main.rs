mod json_parser;
mod vectorize;
mod dataset;
mod fastpath;
mod ivf;
mod http;

use std::env;
use std::fs;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

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

fn main() {
    let port = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9001u16);

    println!("[API] Starting on port {}...", port);

    let mcc_data = fs::read("resources/mcc_risk.json").expect("mcc_risk.json not found");
    init_mcc_risk_table(&mcc_data);

    let state_lock: Arc<Mutex<Option<Arc<AppState>>>> = Arc::new(Mutex::new(None));

    let state_for_load = Arc::clone(&state_lock);
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

        let state = AppState { dataset: Arc::new(ds), ivf: Arc::new(ivf) };
        let state_arc = Arc::new(state);
        warm_up(&state_arc);

        let mut w = state_for_load.lock().unwrap();
        *w = Some(state_arc);
        println!("[API] Warm-up complete — ready for requests");
    });

    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).expect("Failed to bind TCP");
    listener.set_nonblocking(false).expect("set_nonblocking");
    println!("[API] Listening on TCP {}", addr);
    println!("[API] Ready");

    let state_arc = Arc::clone(&state_lock);
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

fn warm_up(state: &AppState) {
    println!("[API] Warming up...");
    let mut query = [0i16; 14];
    for _ in 0..256 {
        let _ = state.ivf.search(&state.dataset, &query, 5, ivf::IVF_NPROBE_EASY);
        query[0] = query[0].wrapping_add(100);
    }
}

fn handle_fraud_score(body: &[u8], state_lock: &Arc<Mutex<Option<Arc<AppState>>>>) -> FraudResult {
    let tx = parse_transaction(body);
    let mut v = [0.0f32; 14];
    vectorize(&tx, &mut v);

    match fast_path(&v) {
        FastResult::Legit => return FraudResult::Score(0),
        FastResult::Fraud => return FraudResult::Score(5),
        FastResult::Borderline => {}
    }

    let r = state_lock.lock().unwrap();
    match r.as_ref() {
        Some(state) => {
            let mut qv = [0i16; 14];
            for d in 0..14 { qv[d] = quantize(v[d]); }
            let fraud_count = state.ivf.search(&state.dataset, &qv, 5, 1);
            FraudResult::Score(fraud_count)
        }
        None => FraudResult::Error,
    }
}