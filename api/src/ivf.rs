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
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        #[cfg(target_arch = "x86_64")]
        if is_x86_feature_detected!("avx2") {
            return unsafe { distance_i16_avx2(a, b) };
        }
    }
    let mut sum: i32 = 0;
    for d in 0..DIMS {
        let diff = a[d] as i32 - b[d] as i32;
        sum += diff * diff;
    }
    sum
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn distance_i16_avx2(a: &[i16; DIMS], b: &[i16; DIMS]) -> i32 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;

    // Load 8 + 8 = 16 i16. DIMS=14, so the last 2 elements of the second load
    // are OUT-OF-BOUNDS garbage. We must zero them out BEFORE squaring.
    let a0 = _mm_loadu_si128(a.as_ptr() as *const __m128i);              // a[0..8]
    let b0 = _mm_loadu_si128(b.as_ptr() as *const __m128i);              // b[0..8]
    let a1 = _mm_loadu_si128(a.as_ptr().add(8) as *const __m128i);        // a[8..16] (last 2 are OOB)
    let b1 = _mm_loadu_si128(b.as_ptr().add(8) as *const __m128i);        // b[8..16] (last 2 are OOB)

    // CRITICAL: mask off i16 lanes 6 and 7 of a1/b1 to 0.
    // DIMS=14, so a1/b1 (16 bytes loaded from a[8]/b[8]) contains 6 valid
    // i16 values (a[8..14] / b[8..14]) plus 2 i16 of OOB garbage.
    // __m128i has 8 i16 lanes. We want lanes 0..5 keep (-1), lanes 6..7 zero.
    // _mm_set_epi16(e7, e6, e5, e4, e3, e2, e1, e0) packs high-to-low:
    //   e0 -> lane 0, e1 -> lane 1, ..., e7 -> lane 7
    let mask = _mm_set_epi16(
        0, 0,         // lanes 7, 6: zero out (OOB garbage)
        -1, -1, -1, -1, -1, -1  // lanes 5, 4, 3, 2, 1, 0: keep
    );
    let a1_masked = _mm_and_si128(a1, mask);
    let b1_masked = _mm_and_si128(b1, mask);

    // Compute diff = a - b (per i16)
    let d0 = _mm_sub_epi16(a0, b0);
    let d1 = _mm_sub_epi16(a1_masked, b1_masked);

    // Square the diffs (i16 * i16 -> i32 via 2-step cvt + mullo)
    // First, sign-extend i16 to i32 (4 lanes at a time)
    let d0_lo = _mm_cvtepi16_epi32(d0);  // first 4 i16 -> 4 i32
    let d0_hi = _mm_cvtepi16_epi32(_mm_srli_si128(d0, 8));  // next 4 i16
    let d1_lo = _mm_cvtepi16_epi32(d1);
    let d1_hi = _mm_cvtepi16_epi32(_mm_srli_si128(d1, 8));

    let d0_lo_sq = _mm_mullo_epi32(d0_lo, d0_lo);
    let d0_hi_sq = _mm_mullo_epi32(d0_hi, d0_hi);
    let d1_lo_sq = _mm_mullo_epi32(d1_lo, d1_lo);
    let d1_hi_sq = _mm_mullo_epi32(d1_hi, d1_hi);

    let sum0 = _mm_add_epi32(d0_lo_sq, d0_hi_sq);
    let sum1 = _mm_add_epi32(d1_lo_sq, d1_hi_sq);
    let sum = _mm_add_epi32(sum0, sum1);

    // Horizontal sum of 4 i32
    // sum = [s0, s1, s2, s3]
    // shuf 0b01_00_11_10 -> [s1, s0, s3, s2] (after _MM_SHUFFLE unpack convention)
    // sums = sum + shuf = [s0+s1, s1+s0, s2+s3, s3+s2] = [p0, p0, p1, p1] where p0 = s0+s1, p1 = s2+s3
    // shuf 0b00_00_00_11 -> [s3, s0, s0, s0] (low lane 0 <- src lane 3)
    // result = sums + shuf = [p0+s3, p0+s0, p1+s0, p1+s0]
    // Hmm, that's not quite right. Let me redo carefully.
    // _mm_shuffle_epi32(a, imm) packs imm as 0bzyx where lane i = src[imm_i_bit_pair]
    // imm = 0b01_00_11_10 means:
    //   lane 0 = src[0b10] = src[2]
    //   lane 1 = src[0b11] = src[3]
    //   lane 2 = src[0b00] = src[0]
    //   lane 3 = src[0b01] = src[1]
    // So shuf = [s2, s3, s0, s1]
    // sums = sum + shuf = [s0+s2, s1+s3, s2+s0, s3+s1] = [p0, p1, p0, p1]
    // shuf2 imm = 0b00_00_00_11:
    //   lane 0 = src[0b11] = src[3]
    //   lane 1 = src[0b00] = src[0]
    //   lane 2 = src[0b00] = src[0]
    //   lane 3 = src[0b00] = src[0]
    // shuf2 = [sums[3], sums[0], sums[0], sums[0]] = [p1, p0, p0, p0]
    // result = sums + shuf2 = [p0+p1, p1+p0, p0+p0, p1+p0] = [s0+s1+s2+s3, ...]
    // Lane 0 = s0+s1+s2+s3 = full sum ✓
    let shuf = _mm_shuffle_epi32(sum, 0b01_00_11_10);
    let sums = _mm_add_epi32(sum, shuf);
    let shuf2 = _mm_shuffle_epi32(sums, 0b00_00_00_11);
    let result = _mm_add_epi32(sums, shuf2);

    _mm_cvtsi128_si32(result)
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
