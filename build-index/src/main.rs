// Fast offline index builder for Rinha 2026.
// Uses Forgy (random) initialization + parallel Lloyd's algorithm.
// Optimized for speed over quality — enough for top 10, may need tuning for top 1.

use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Instant;
use rayon::prelude::*;

const DIMS: usize = 14;
const NUM_CELLS: usize = 1024;
const KMEANS_ITERATIONS: usize = 3;
const QSCALE: f32 = 10000.0;

fn main() {
    let args: Vec<String> = env::args().collect();
    let in_path = args.get(1).map(|s| s.as_str()).unwrap_or("resources/references.json.gz");
    let out_path = args.get(2).map(|s| s.as_str()).unwrap_or("resources/index_v2.bin");

    let start = Instant::now();
    println!("[build-index] Parsing {}...", in_path);

    let file = File::open(in_path).expect("Failed to open references.json.gz");
    let decoder = flate2::read::GzDecoder::new(file);
    let data = std::io::read_to_string(decoder).expect("Failed to read gz");

    let (vectors, labels) = parse_json(&data);
    let n = vectors.len();
    println!("[build-index] Parsed {} vectors in {:?}", n, start.elapsed());

    // Forgy initialization: pick evenly spaced vectors
    println!("[build-index] Forgy initialization ({} cells)...", NUM_CELLS);
    let step = n / NUM_CELLS;
    let centroids: Vec<[i16; DIMS]> = (0..NUM_CELLS)
        .map(|c| vectors[(c * step) % n])
        .collect();
    println!("[build-index] Initialized in {:?}", start.elapsed());

    // Lloyd's algorithm (parallel)
    println!("[build-index] Lloyd's algorithm ({} iterations)...", KMEANS_ITERATIONS);
    let mut assignments = vec![0usize; n];
    let mut centroids = centroids;
    
    for iter in 0..KMEANS_ITERATIONS {
        let iter_start = Instant::now();
        
        // Assign in parallel
        assignments.par_chunks_mut(10000)
            .enumerate()
            .for_each(|(chunk_idx, chunk)| {
                let offset = chunk_idx * 10000;
                for (i, assign) in chunk.iter_mut().enumerate() {
                    let vec_idx = offset + i;
                    if vec_idx >= n { break; }
                    
                    let mut best_dist = i64::MAX;
                    let mut best_c = 0;
                    for c in 0..NUM_CELLS {
                        let d = distance_i16(&centroids[c], &vectors[vec_idx]);
                        if d < best_dist {
                            best_dist = d;
                            best_c = c;
                        }
                    }
                    *assign = best_c;
                }
            });
        
        // Recompute centroids
        let mut new_centroids = vec![[0i64; DIMS]; NUM_CELLS];
        let mut counts = vec![0usize; NUM_CELLS];
        
        for i in 0..n {
            let c = assignments[i];
            for d in 0..DIMS {
                new_centroids[c][d] += vectors[i][d] as i64;
            }
            counts[c] += 1;
        }
        
        for c in 0..NUM_CELLS {
            if counts[c] > 0 {
                for d in 0..DIMS {
                    centroids[c][d] = (new_centroids[c][d] / counts[c] as i64) as i16;
                }
            }
        }
        
        println!("[build-index]   Iteration {}: {}ms", 
            iter + 1, 
            iter_start.elapsed().as_millis(),
        );
    }

    // Build cells
    println!("[build-index] Building cells...");
    let mut cells: Vec<Vec<usize>> = vec![Vec::new(); NUM_CELLS];
    for i in 0..n {
        cells[assignments[i]].push(i);
    }
    
    let min_size = cells.iter().map(|c| c.len()).min().unwrap_or(0);
    let max_size = cells.iter().map(|c| c.len()).max().unwrap_or(0);
    let avg_size = cells.iter().map(|c| c.len()).sum::<usize>() / NUM_CELLS;
    let empty_cells = cells.iter().filter(|c| c.is_empty()).count();
    println!("[build-index] Cell stats: min={}, max={}, avg={}, empty={}", 
        min_size, max_size, avg_size, empty_cells);

    // Save binary
    println!("[build-index] Saving to {}...", out_path);
    let out = File::create(out_path).expect("Failed to create output");
    let mut writer = BufWriter::new(out);
    
    // Header
    writer.write_all(b"RINHA06\x02").unwrap();
    writer.write_all(&(n as u32).to_le_bytes()).unwrap();
    writer.write_all(&(DIMS as u16).to_le_bytes()).unwrap();
    writer.write_all(&(NUM_CELLS as u32).to_le_bytes()).unwrap();
    
    // Centroids
    for c in 0..NUM_CELLS {
        for d in 0..DIMS {
            writer.write_all(&centroids[c][d].to_le_bytes()).unwrap();
        }
    }
    
    // Labels
    for i in 0..n {
        writer.write_all(&[if labels[i] { 1u8 } else { 0u8 }]).unwrap();
    }
    
    // Cell metadata
    let mut offset = 0u32;
    for c in 0..NUM_CELLS {
        writer.write_all(&offset.to_le_bytes()).unwrap();
        writer.write_all(&( cells[c].len() as u32).to_le_bytes()).unwrap();
        offset += cells[c].len() as u32;
    }
    
    // Cell indices
    for c in 0..NUM_CELLS {
        for &idx in &cells[c] {
            writer.write_all(&( idx as u32).to_le_bytes()).unwrap();
        }
    }
    
    // Vectors SoA
    for d in 0..DIMS {
        for i in 0..n {
            writer.write_all(&vectors[i][d].to_le_bytes()).unwrap();
        }
    }
    
    writer.flush().unwrap();
    let file = writer.into_inner().unwrap();
    let size = file.metadata().unwrap().len();
    println!("[build-index] Saved {} bytes", size);
    println!("[build-index] Total time: {:?}", start.elapsed());
}

fn parse_json(data: &str) -> (Vec<[i16; DIMS]>, Vec<bool>) {
    let data_bytes = data.as_bytes();
    let mut pos: usize = 0;
    
    while pos < data_bytes.len() && data_bytes[pos] != b'[' {
        pos += 1;
    }
    if pos < data_bytes.len() { pos += 1; }
    
    let mut vectors = Vec::with_capacity(3_100_000);
    let mut labels = Vec::with_capacity(3_100_000);
    
    loop {
        while pos < data_bytes.len() && (data_bytes[pos] == b' ' || data_bytes[pos] == b'\t' || data_bytes[pos] == b'\n' || data_bytes[pos] == b'\r' || data_bytes[pos] == b',') {
            pos += 1;
        }
        if pos >= data_bytes.len() || data_bytes[pos] == b']' {
            break;
        }
        
        if data_bytes[pos] == b'{' { pos += 1; }
        
        let mut vec = [0i16; DIMS];
        let mut is_fraud = false;
        
        loop {
            while pos < data_bytes.len() && (data_bytes[pos] == b' ' || data_bytes[pos] == b'\t' || data_bytes[pos] == b'\n' || data_bytes[pos] == b'\r') {
                pos += 1;
            }
            if pos >= data_bytes.len() || data_bytes[pos] == b'}' {
                break;
            }
            
            if data_bytes[pos] == b'"' { pos += 1; }
            let key_start = pos;
            while pos < data_bytes.len() && data_bytes[pos] != b'"' {
                pos += 1;
            }
            let key = &data_bytes[key_start..pos];
            if pos < data_bytes.len() && data_bytes[pos] == b'"' { pos += 1; }
            
            while pos < data_bytes.len() && (data_bytes[pos] == b' ' || data_bytes[pos] == b':' ) {
                pos += 1;
            }
            
            if key == b"vector" {
                while pos < data_bytes.len() && data_bytes[pos] != b'[' {
                    pos += 1;
                }
                if pos < data_bytes.len() { pos += 1; }
                
                let mut dim_idx: usize = 0;
                while dim_idx < DIMS && pos < data_bytes.len() {
                    while pos < data_bytes.len() && (data_bytes[pos] == b' ' || data_bytes[pos] == b'\t') {
                        pos += 1;
                    }
                    
                    let mut neg = false;
                    if data_bytes[pos] == b'-' {
                        neg = true;
                        pos += 1;
                    }
                    let mut int_part: i32 = 0;
                    while pos < data_bytes.len() && data_bytes[pos].is_ascii_digit() {
                        int_part = int_part * 10 + (data_bytes[pos] - b'0') as i32;
                        pos += 1;
                    }
                    let mut frac: f32 = 0.0;
                    let mut div: f32 = 1.0;
                    if pos < data_bytes.len() && data_bytes[pos] == b'.' {
                        pos += 1;
                        while pos < data_bytes.len() && data_bytes[pos].is_ascii_digit() {
                            frac = frac * 10.0 + (data_bytes[pos] - b'0') as f32;
                            div *= 10.0;
                            pos += 1;
                        }
                    }
                    let mut val = int_part as f32 + frac / div;
                    if neg { val = -val; }
                    
                    vec[dim_idx] = (val * QSCALE) as i16;
                    dim_idx += 1;
                    
                    while pos < data_bytes.len() && (data_bytes[pos] == b' ' || data_bytes[pos] == b'\t') {
                        pos += 1;
                    }
                    if pos < data_bytes.len() && data_bytes[pos] == b',' {
                        pos += 1;
                    }
                }
                
                while pos < data_bytes.len() && data_bytes[pos] != b']' {
                    pos += 1;
                }
                if pos < data_bytes.len() { pos += 1; }
            } else if key == b"label" {
                if data_bytes[pos] == b'"' { pos += 1; }
                let val_start = pos;
                while pos < data_bytes.len() && data_bytes[pos] != b'"' {
                    pos += 1;
                }
                let val = &data_bytes[val_start..pos];
                is_fraud = val == b"fraud";
                if pos < data_bytes.len() && data_bytes[pos] == b'"' { pos += 1; }
            }
            
            while pos < data_bytes.len() && (data_bytes[pos] == b' ' || data_bytes[pos] == b'\t' || data_bytes[pos] == b'\n' || data_bytes[pos] == b'\r' || data_bytes[pos] == b',') {
                pos += 1;
            }
        }
        
        if pos < data_bytes.len() && data_bytes[pos] == b'}' { pos += 1; }
        
        vectors.push(vec);
        labels.push(is_fraud);
        
        if vectors.len() % 500000 == 0 {
            println!("[build-index]   Parsed {} vectors...", vectors.len());
        }
        
        while pos < data_bytes.len() && (data_bytes[pos] == b' ' || data_bytes[pos] == b'\t' || data_bytes[pos] == b'\n' || data_bytes[pos] == b'\r' || data_bytes[pos] == b',') {
            pos += 1;
        }
    }
    
    (vectors, labels)
}

fn distance_i16(a: &[i16; DIMS], b: &[i16; DIMS]) -> i64 {
    let mut sum: i64 = 0;
    for d in 0..DIMS {
        let diff = a[d] as i64 - b[d] as i64;
        sum += diff * diff;
    }
    sum
}