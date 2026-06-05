//! IVF (Inverted File Index) for sub-linear vector search.
//! Uses pre-computed centroids and cell assignments from binary index.

use crate::dataset::{Dataset, DIMS};

pub const IVF_NPROBE_EASY: usize = 1;
pub const IVF_NPROBE_HARD: usize = 8;
pub const IVF_NPROBE_REPAIR: usize = 8;  // Expanded probe when result is ambiguous (1-4)

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

    /// k-NN search with REPAIR pattern:
    ///   1. Phase 1: scan nearest `nprobe` cells, find top-k
    ///   2. If fraud_count is 0 (all legit) or k (all fraud) — UNANIMOUS decision
    ///      Binary decision (approved < 0.6) is locked; return immediately
    ///   3. Otherwise (fraud_count in 1..k-1) — AMBIGUOUS, re-scan with `nprobe_repair` cells
    pub fn search(
        &self,
        ds: &Dataset,
        query: &[i16; DIMS],
        k: usize,
        nprobe: usize,
    ) -> usize {
        // Phase 1: scan nearest nprobe cells
        let fraud_count = self.search_phase(ds, query, k, nprobe);

        // Repair pattern: if decision is unanimous, return; else expand
        if fraud_count == 0 || fraud_count >= k {
            return fraud_count;
        }

        // Ambiguous (1..k-1): expand to more cells
        let nprobe_expanded = IVF_NPROBE_REPAIR.min(ds.num_cells);
        if nprobe_expanded <= nprobe {
            return fraud_count;
        }
        self.search_phase(ds, query, k, nprobe_expanded)
    }

    /// Single-phase IVF search. Returns the fraud count among the top-k nearest
    /// neighbours. Uses i64 accumulator to avoid overflow (max dist for 14 dims
    /// of i16 is ~6e10, which overflows i32).
    fn search_phase(
        &self,
        ds: &Dataset,
        query: &[i16; DIMS],
        k: usize,
        nprobe: usize,
    ) -> usize {
        let mut nearest_cells: [(i64, usize); 8] = [(i64::MAX, 0); 8];
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

        let mut best: [(i64, usize); 5] = [(i64::MAX, 0); 5];
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

/// L2 squared distance between two i16 vectors, accumulated in i64 to avoid
/// overflow (max value: 14 * (2*32767)^2 ≈ 6e10, exceeds i32::MAX).
#[inline(always)]
pub fn distance_i16(a: &[i16; DIMS], b: &[i16; DIMS]) -> i64 {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        #[cfg(target_arch = "x86_64")]
        if is_x86_feature_detected!("avx2") {
            return unsafe { distance_i16_avx2(a, b) };
        }
    }
    let mut sum: i64 = 0;
    for d in 0..DIMS {
        let diff = a[d] as i64 - b[d] as i64;
        sum += diff * diff;
    }
    sum
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn distance_i16_avx2(a: &[i16; DIMS], b: &[i16; DIMS]) -> i64 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;

    let a0 = _mm_loadu_si128(a.as_ptr() as *const __m128i);
    let b0 = _mm_loadu_si128(b.as_ptr() as *const __m128i);
    let a1 = _mm_loadu_si128(a.as_ptr().add(8) as *const __m128i);
    let b1 = _mm_loadu_si128(b.as_ptr().add(8) as *const __m128i);

    // Mask off i16 lanes 6,7 of a1/b1 to 0 (OOB garbage)
    let mask = _mm_set_epi16(
        0, 0,
        -1, -1, -1, -1, -1, -1
    );
    let a1_masked = _mm_and_si128(a1, mask);
    let b1_masked = _mm_and_si128(b1, mask);

    let d0 = _mm_sub_epi16(a0, b0);
    let d1 = _mm_sub_epi16(a1_masked, b1_masked);

    // Square i16 -> i32 (4 lanes at a time)
    let d0_lo = _mm_cvtepi16_epi32(d0);
    let d0_hi = _mm_cvtepi16_epi32(_mm_srli_si128(d0, 8));
    let d1_lo = _mm_cvtepi16_epi32(d1);
    let d1_hi = _mm_cvtepi16_epi32(_mm_srli_si128(d1, 8));

    // Square (i32 * i32 -> i32 via mullo)
    let d0_lo_sq = _mm_mullo_epi32(d0_lo, d0_lo);
    let d0_hi_sq = _mm_mullo_epi32(d0_hi, d0_hi);
    let d1_lo_sq = _mm_mullo_epi32(d1_lo, d1_lo);
    let d1_hi_sq = _mm_mullo_epi32(d1_hi, d1_hi);

    // Sum to i32 (still fits: 14 * (32767)^2 / 4 per quad = 4.7e9 which overflows i32)
    // Wait — full sum is 14 * 32767^2 ≈ 1.5e10, which overflows i32.
    // We need to promote to i64 BEFORE summing.
    // Use _mm256_cvtepi32_epi64 to extend 4 i32 to 4 i64.
    let sum32_0 = _mm_add_epi32(_mm_add_epi32(d0_lo_sq, d0_hi_sq), _mm_add_epi32(d1_lo_sq, d1_hi_sq));
    // sum32_0 = [s0, s1, s2, s3] i32 — each lane is sum of 4 dims squared.
    // Wait, that's wrong. Each lane is sum of 4 dims squared, but we have 14 dims.
    // Total sum = sum of 14 dims squared. Currently sum32_0 has sum of 4+4+4+2 = 14 lanes worth.
    // Hmm, actually d0_lo, d0_hi, d1_lo, d1_hi each have 4 lanes (total 16 lanes), and we're
    // squaring 14 valid + 2 garbage (masked to 0). So sum is correctly 14 dims squared.
    // But sum32_0 is the sum of the 4 i32 squared, so it's 4 lanes each containing sum of
    // 4 dims (overlapping: d0_lo covers dims 0-3, d0_hi covers 4-7, d1_lo covers 8-11,
    // d1_hi covers 12-13 plus 2 garbage = 0). So sum32_0 lane 0 = dims 0-3 + 4-7 + 8-11 + 12-13.
    // That's all 14 dims! ✓
    // Now we need to sum these 4 i32 lanes into 1 i64.
    // Hmm, but each lane can be up to 14 * 32767^2 ≈ 1.5e10 which overflows i32 (max 2.1e9).
    // So sum32_0 is OVERFLOWED as i32! ❌

    // CORRECT APPROACH: convert each i32 squared to i64 BEFORE summing.
    let s0_i64 = _mm256_cvtepi32_epi64(d0_lo_sq);  // 4 i32 -> 4 i64
    let s1_i64 = _mm256_cvtepi32_epi64(d0_hi_sq);
    let s2_i64 = _mm256_cvtepi32_epi64(d1_lo_sq);
    let s3_i64 = _mm256_cvtepi32_epi64(d1_hi_sq);

    let sum64 = _mm256_add_epi64(_mm256_add_epi64(s0_i64, s1_i64), _mm256_add_epi64(s2_i64, s3_i64));
    // sum64 = 4 i64, each is the sum of 4 dims squared. Total = sum of 14 dims squared.

    // Horizontal sum of 4 i64 lanes to 1 i64
    // Move high lane (lane 2,3) to low and add
    let shuf = _mm256_permute4x64_epi64::<0b11_10_01_00>(sum64);  // [lane3, lane2, lane1, lane0]
    let sum_pair = _mm256_add_epi64(sum64, shuf);  // [s3+s0, s2+s1, s1+s2, s0+s3]
    // Extract low 128 bits and add the two i64 lanes
    let lo128 = _mm256_castsi256_si128(sum_pair);  // [s3+s0, s2+s1]
    let sum_lo = _mm_add_epi64(lo128, _mm_srli_si128(lo128, 8));  // [(s3+s0)+(s2+s1), ...]
    _mm_cvtsi128_si64(sum_lo)  // Lane 0 = total sum as i64
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
