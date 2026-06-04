//! IVF (Inverted File Index) for sub-linear vector search.
//! Uses pre-computed centroids and cell assignments from binary index.

use crate::dataset::{Dataset, DIMS};

pub const IVF_NPROBE_EASY: usize = 1;
pub const IVF_NPROBE_HARD: usize = 8;

pub struct IVFIndex {
    pub num_cells: usize,
}

impl IVFIndex {
    pub fn new() -> Self {
        Self { num_cells: 0 }
    }

    pub fn build_from_dataset(ds: &Dataset, _num_cells: usize) -> Self {
        Self { num_cells: ds.num_cells }
    }

    /// Count frauds in a single cell (fastpath). Returns (fraud_count, total_count).
    #[inline(always)]
    pub fn count_frauds_in_cell(
        &self,
        ds: &Dataset,
        cell_idx: usize,
    ) -> (u8, usize) {
        let (off, len) = ds.cell_meta[cell_idx];
        if len == 0 {
            return (0, 0);
        }

        let mut fraud_count: u8 = 0;
        for i in 0..len {
            let vec_idx = ds.cell_indices[(off + i) as usize] as usize;
            if ds.labels[vec_idx] != 0 {
                fraud_count += 1;
                if fraud_count >= 5 {
                    break;
                }
            }
        }
        (fraud_count, len as usize)
    }
    pub fn search(
        &self,
        ds: &Dataset,
        query: &[i16; DIMS],
        k: usize,
        nprobe: usize,
    ) -> usize {
        let mut nearest_cells: [(i32, usize); 8] = [(i32::MAX, 0); 8];
        let effective_nprobe = nprobe.min(ds.num_cells).min(8);
        
        for c in 0..ds.num_cells {
            let dist = distance_i16(query, &ds.centroids[c]);
            
            let mut insert_idx = effective_nprobe;
            for i in 0..effective_nprobe {
                if dist < nearest_cells[i].0 {
                    insert_idx = i;
                    break;
                }
            }
            
            if insert_idx < effective_nprobe {
                for j in (insert_idx + 1..effective_nprobe).rev() {
                    nearest_cells[j] = nearest_cells[j - 1];
                }
                nearest_cells[insert_idx] = (dist, c);
            }
        }
        
        let mut best: [(i32, usize); 5] = [(i32::MAX, 0); 5];
        let mut best_len: usize = 0;
        
        for &(cdist, cidx) in nearest_cells.iter().take(effective_nprobe) {
            if best_len == k && cdist >= best[k - 1].0 {
                continue;
            }

            let (offset, len) = ds.cell_meta[cidx];
            for i in 0..len {
                let vidx = ds.cell_indices[(offset + i) as usize] as usize;
                let dist = ds.distance(query, vidx);
                
                if best_len < k {
                    best[best_len] = (dist, vidx);
                    best_len += 1;
                    let mut j = best_len - 1;
                    while j > 0 && best[j - 1].0 > best[j].0 {
                        best.swap(j - 1, j);
                        j -= 1;
                    }
                } else if dist < best[k - 1].0 {
                    best[k - 1] = (dist, vidx);
                    let mut j = k - 1;
                    while j > 0 && best[j - 1].0 > best[j].0 {
                        best.swap(j - 1, j);
                        j -= 1;
                    }
                }
            }
        }
        
        let fraud_count = best.iter().take(best_len).filter(|(_, idx)| ds.labels[*idx] != 0).count();
        fraud_count
    }
}

#[inline(always)]
pub fn distance_i16(a: &[i16; DIMS], b: &[i16; DIMS]) -> i32 {
    let mut sum: i32 = 0;
    
    for d in 0..DIMS {
        let diff = a[d] as i32 - b[d] as i32;
        sum += diff * diff;
    }
    
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ivf_search() {
        let mut ds = Dataset::new();
        ds.count = 100;
        ds.num_cells = 4;
        ds.labels = vec![0u8; 100];
        for i in 0..50 { ds.labels[i] = 1; }
        
        ds.centroids = vec![
            [9000i16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0i16; DIMS],
            [0i16; DIMS],
            [0i16; DIMS],
        ];
        
        ds.cell_meta = vec![(0, 50), (50, 50), (100, 0), (100, 0)];
        ds.cell_indices = (0..100).map(|x| x as u32).collect();
        
        // dims[dim_idx * count + vec_idx]
        ds.dims = vec![0i16; DIMS * 100];
        for i in 0..50 {
            ds.dims[0 * 100 + i] = 9000i16;
        }
        for i in 50..100 {
            ds.dims[0 * 100 + i] = 1000i16;
        }

        let ivf = IVFIndex::new();
        let mut query = [0i16; DIMS];
        query[0] = 8000i16;
        
        let fraud_count = ivf.search(&ds, &query, 5, 2);
        assert!(fraud_count >= 3);
    }
}
