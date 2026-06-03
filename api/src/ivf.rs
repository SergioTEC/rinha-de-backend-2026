// IVF (Inverted File Index) for sub-linear vector search.
// Uses pre-computed centroids and cell assignments from binary index.

use crate::dataset::{Dataset, DIMS};

/// Number of cells to probe for "easy" queries
pub const IVF_NPROBE_EASY: usize = 1;
/// Number of cells to probe for "borderline" queries
pub const IVF_NPROBE_HARD: usize = 12;

/// The IVF index (uses pre-computed data from Dataset)
pub struct IVFIndex {
    /// Number of cells
    pub num_cells: usize,
}

impl IVFIndex {
    pub fn new() -> Self {
        Self { num_cells: 0 }
    }

    /// Build from dataset (centroids already loaded)
    pub fn build_from_dataset(ds: &Dataset, _num_cells: usize) -> Self {
        Self { num_cells: ds.num_cells }
    }

    /// Search IVF: find top k vectors across nprobe cells
    /// Zero-allocation hot path using stack buffers
    #[inline(always)]
    pub fn search(
        &self,
        ds: &Dataset,
        query: &[i16; DIMS],
        k: usize,
        nprobe: usize,
    ) -> usize {
        // Find nprobe nearest cells using pre-computed centroids
        // Use a stack-allocated array for top-nprobe tracking
        let mut nearest_cells: [(i32, usize); 12] = [(i32::MAX, 0); 12];
        let effective_nprobe = nprobe.min(ds.num_cells).min(12);
        
        for c in 0..ds.num_cells {
            let dist = distance_i16(query, &ds.centroids[c]);
            
            // Insert into nearest_cells if better than current worst
            let mut insert_idx = effective_nprobe;
            for i in 0..effective_nprobe {
                if dist < nearest_cells[i].0 {
                    insert_idx = i;
                    break;
                }
            }
            
            if insert_idx < effective_nprobe {
                // Shift worse entries down
                for j in (insert_idx + 1..effective_nprobe).rev() {
                    nearest_cells[j] = nearest_cells[j - 1];
                }
                nearest_cells[insert_idx] = (dist, c);
            }
        }
        
        // Search within nprobe cells
        let mut best: [(i32, usize); 5] = [(i32::MAX, 0); 5];
        let mut best_len: usize = 0;
        
        for &(cdist, cidx) in nearest_cells.iter().take(effective_nprobe) {
            // Skip if even centroid distance is worse than current k-th
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
                    // Bubble up
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
        
        let fraud_count = best.iter().take(best_len).filter(|(_, idx)| ds.labels[*idx]).count();
        fraud_count
    }

    /// Compute fraud score (0.0 to 1.0)
    #[inline(always)]
    pub fn fraud_score(&self, ds: &Dataset, query: &[i16; DIMS], k: usize, nprobe: usize) -> f32 {
        let fraud_count = self.search(ds, query, k, nprobe);
        fraud_count as f32 / k as f32
    }
}

/// L2 distance between two int16 vectors - pure i32, no widening to i64
#[inline(always)]
fn distance_i16(a: &[i16; DIMS], b: &[i16; DIMS]) -> i32 {
    let mut sum: i32 = 0;
    let mut overflow: bool = false;
    
    for d in 0..DIMS {
        let diff = a[d] as i32 - b[d] as i32;
        let sq = diff.wrapping_mul(diff);
        let (new_sum, did_overflow) = sum.overflowing_add(sq);
        sum = new_sum;
        overflow |= did_overflow;
    }
    
    if overflow {
        i32::MAX
    } else {
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ivf_search() {
        let mut ds = Dataset::new();
        ds.count = 100;
        ds.num_cells = 4;
        ds.labels = vec![false; 100];
        for i in 0..50 { ds.labels[i] = true; }
        
        ds.centroids = vec![
            [9000i16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0i16; DIMS],
            [0i16; DIMS],
            [0i16; DIMS],
        ];
        
        ds.cell_meta = vec![(0, 50), (50, 50), (100, 0), (100, 0)];
        ds.cell_indices = (0..100).map(|x| x as u32).collect();
        
        ds.dims = Vec::with_capacity(DIMS);
        for _d in 0..DIMS {
            ds.dims.push(vec![0i16; 100]);
        }
        for i in 0..50 {
            ds.dims[0][i] = 9000i16;
        }
        for i in 50..100 {
            ds.dims[0][i] = 1000i16;
        }

        let ivf = IVFIndex::new();
        let mut query = [0i16; DIMS];
        query[0] = 8000i16;
        
        let fraud_count = ivf.search(&ds, &query, 5, 2);
        assert!(fraud_count >= 3);
    }
}
