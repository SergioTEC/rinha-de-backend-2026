// Dataset loading from pre-computed binary index (version 2).
// Format: magic(8) + count(4) + dims(2) + cell_count(4) + centroids + labels + cell_offsets + cell_indices + vectors_SoA
// MEMORY OPTIMIZED: single contiguous array instead of Vec<Vec<i16>>

use std::fs::File;

/// Number of dimensions per vector
pub const DIMS: usize = 14;

/// Scale factor for int16 quantization (float * 10000)
pub const QSCALE: f32 = 10000.0;

/// The full dataset loaded into memory - memory optimized version.
/// Uses a single contiguous Vec<i16> for all dimensions to reduce overhead and improve cache locality.
pub struct Dataset {
    /// Number of vectors
    pub count: usize,
    /// Number of IVF cells
    pub num_cells: usize,
    /// Labels: true = fraud, false = legit (packed as bytes)
    pub labels: Vec<u8>,
    /// IVF centroids: [cell_count][DIMS] - kept as Vec for flexibility
    pub centroids: Vec<[i16; DIMS]>,
    /// Cell metadata: (offset, len) into cell_indices
    pub cell_meta: Vec<(u32, u32)>,
    /// Flattened cell indices: all vectors grouped by cell (u32 to save memory)
    pub cell_indices: Vec<u32>,
    /// Dimensions as single contiguous array: dims[dim_idx * count + vector_idx]
    /// This reduces memory overhead from Vec<Vec<i16>> (24 bytes per Vec) to just one Vec
    pub dims: Vec<i16>,
}

impl Dataset {
    pub fn new() -> Self {
        Self {
            count: 0,
            num_cells: 0,
            labels: Vec::new(),
            centroids: Vec::new(),
            cell_meta: Vec::new(),
            cell_indices: Vec::new(),
            dims: Vec::new(),
        }
    }

    /// Load pre-computed binary index (resources/index_v2.bin, version 2).
    /// Uses mmap with populate + madvise for zero page-faults under load.
    pub fn load_from_bin(path: &str) -> Self {
        let file = File::open(path).expect("Failed to open index.bin");
        let size = file.metadata().expect("Failed to get metadata").len() as usize;
        
        println!("[Dataset] Loading binary index: {} ({} bytes)", path, size);
        
        // Memory map with populate (forces page-in) and copy to Vec
        let mmap = unsafe {
            memmap2::MmapOptions::new()
                .populate()
                .map(&file)
                .expect("Failed to mmap index")
        };

        #[cfg(target_os = "linux")]
        unsafe {
            libc::madvise(mmap.as_ptr() as *mut libc::c_void, size, libc::MADV_WILLNEED);
            libc::madvise(mmap.as_ptr() as *mut libc::c_void, size, libc::MADV_RANDOM);
            libc::madvise(mmap.as_ptr() as *mut libc::c_void, size, libc::MADV_HUGEPAGE);
        }
        
        // Prefetch: touch every page to force page-faults now (not under load)
        let mut _sum: u8 = 0;
        let page_size = 4096usize;
        for offset in (0..size).step_by(page_size) {
            _sum = _sum.wrapping_add(mmap[offset]);
        }
        
        let data = &mmap[..];
        
        // Parse header
        if data.len() < 18 || &data[0..8] != b"RINHA06\x02" {
            panic!("Invalid index file format (expected version 2)");
        }
        
        let count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
        let dims_in_file = u16::from_le_bytes([data[12], data[13]]) as usize;
        let num_cells = u32::from_le_bytes([data[14], data[15], data[16], data[17]]) as usize;
        
        assert_eq!(dims_in_file, DIMS, "Dimension mismatch");
        
        println!("[Dataset] Index: {} vectors, {} dims, {} cells", count, dims_in_file, num_cells);
        
        let mut offset = 18usize;
        
        // Read centroids
        let centroids_size = num_cells * DIMS * 2;
        let mut centroids: Vec<[i16; DIMS]> = Vec::with_capacity(num_cells);
        for c in 0..num_cells {
            let mut vec = [0i16; DIMS];
            for d in 0..DIMS {
                let idx = offset + (c * DIMS + d) * 2;
                vec[d] = i16::from_le_bytes([data[idx], data[idx + 1]]);
            }
            centroids.push(vec);
        }
        offset += centroids_size;
        
        // Read labels as u8 (1 byte each, 0 or 1)
        let mut labels: Vec<u8> = Vec::with_capacity(count);
        labels.extend((0..count).map(|i| if data[offset + i] != 0 { 1u8 } else { 0u8 }));
        offset += count;
        
        // Read cell metadata
        let mut cell_meta: Vec<(u32, u32)> = Vec::with_capacity(num_cells);
        let mut total_indices: u32 = 0;
        for c in 0..num_cells {
            let off = offset + c * 8;
            let cell_off = u32::from_le_bytes([data[off], data[off+1], data[off+2], data[off+3]]);
            let len = u32::from_le_bytes([data[off+4], data[off+5], data[off+6], data[off+7]]);
            cell_meta.push((cell_off, len));
            total_indices += len;
        }
        offset += num_cells * 8;
        
        // Read cell indices
        let mut cell_indices: Vec<u32> = Vec::with_capacity(total_indices as usize);
        for i in 0..total_indices {
            let off = offset + i as usize * 4;
            let idx = u32::from_le_bytes([data[off], data[off+1], data[off+2], data[off+3]]);
            cell_indices.push(idx);
        }
        offset += total_indices as usize * 4;
        
        // Read vectors into single contiguous array: dims[dim_idx * count + vec_idx]
        let mut dims: Vec<i16> = Vec::with_capacity(DIMS * count);
        for d in 0..DIMS {
            for i in 0..count {
                let idx = offset + (i * DIMS + d) * 2;
                let val = i16::from_le_bytes([data[idx], data[idx + 1]]);
                dims.push(val);
            }
        }
        
        println!("[Dataset] Loaded {} vectors, {} cells, {} indices", count, num_cells, total_indices);
        println!("[Dataset] Memory: labels={}KB, cell_indices={}KB, dims={}KB", 
                 labels.len() * std::mem::size_of::<u8>() / 1024,
                 cell_indices.len() * std::mem::size_of::<u32>() / 1024,
                 dims.len() * std::mem::size_of::<i16>() / 1024);
        
        Self { count, num_cells, labels, centroids, cell_meta, cell_indices, dims }
    }

    /// Get a single dimension value for a vector
    #[inline(always)]
    pub fn dim(&self, dim_idx: usize, vec_idx: usize) -> i16 {
        self.dims[dim_idx * self.count + vec_idx]
    }

    /// Compute L2 distance between a query vector and a reference vector.
    #[inline(always)]
    pub fn distance(
        &self,
        query: &[i16; DIMS],
        idx: usize,
    ) -> i32 {
        distance_impl(query, idx, self.count, &self.dims)
    }

    /// Find k nearest neighbors (brute force for baseline)
    pub fn knn_brute_force(
        &self,
        query: &[i16; DIMS],
        k: usize,
    ) -> (Vec<usize>, usize) {
        let mut best: Vec<(i32, usize)> = Vec::with_capacity(k);
        
        for i in 0..self.count {
            let dist = self.distance(query, i);
            
            if best.len() < k {
                best.push((dist, i));
                let mut j = best.len() - 1;
                while j > 0 && best[j-1].0 > best[j].0 {
                    best.swap(j-1, j);
                    j -= 1;
                }
            } else if dist < best[k-1].0 {
                best[k-1] = (dist, i);
                let mut j = k - 1;
                while j > 0 && best[j-1].0 > best[j].0 {
                    best.swap(j-1, j);
                    j -= 1;
                }
            }
        }
        
        let fraud_count = best.iter().filter(|(_, idx)| self.labels[*idx] != 0).count();
        let indices = best.into_iter().map(|(_, idx)| idx).collect();
        (indices, fraud_count)
    }
}

/// L2 distance implementation with AVX2 or scalar fallback
#[inline(always)]
fn distance_impl(query: &[i16; DIMS], idx: usize, count: usize, dims: &[i16]) -> i32 {
    #[cfg(has_c_avx2)]
    {
        let mut qa = [0i16; 16];
        let mut ra = [0i16; 16];
        for d in 0..DIMS {
            qa[d] = query[d];
            ra[d] = dims[d * count + idx];
        }
        unsafe { l2sq_int16_avx2(qa.as_ptr(), ra.as_ptr()) }
    }
    #[cfg(not(has_c_avx2))]
    {
        let mut sum: i32 = 0;
        for d in 0..DIMS {
            let diff = query[d] as i32 - dims[d * count + idx] as i32;
            sum += diff * diff;
        }
        sum
    }
}

/// Normalize a float to quantized int16
#[inline(always)]
pub fn quantize(v: f32) -> i16 {
    let scaled = v * QSCALE;
    if scaled > 32767.0 {
        32767
    } else if scaled < -32768.0 {
        -32768
    } else {
        scaled as i16
    }
}

// Link C AVX2 distance function when compiled with has_c_avx2
#[cfg(has_c_avx2)]
extern "C" {
    fn l2sq_int16_avx2(a: *const i16, b: *const i16) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_and_search() {
        let mut ds = Dataset::new();
        ds.count = 3;
        ds.num_cells = 2;
        ds.labels = vec![0u8, 1u8, 0u8];
        ds.centroids = vec![[0i16; DIMS], [0i16; DIMS]];
        ds.cell_meta = vec![(0, 2), (2, 1)];
        ds.cell_indices = vec![0u32, 1, 2];
        ds.dims = vec![0i16; DIMS * 3];
        ds.dims[0 * 3 + 0] = 1000i16;
        ds.dims[0 * 3 + 1] = 9000i16;
        ds.dims[0 * 3 + 2] = 2000i16;
        
        let query = [1000i16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let (indices, fraud_count) = ds.knn_brute_force(&query, 2);
        
        assert_eq!(indices.len(), 2);
        assert!(fraud_count <= 2);
    }
}
