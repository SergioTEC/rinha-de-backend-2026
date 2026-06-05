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

    /// k-NN search with REPAIR pattern (adapted from bmtec-rust):
    ///   1. Phase 1: scan nearest `nprobe` cells, find top-k with bits
    ///   2. If fraud_count is 0 (all legit) or k (all fraud) — UNANIMOUS
    ///      Binary decision (approved < 0.6) is locked; return immediately
    ///   3. If fraud_count in 1..k-1 AND top-5 bits form a "risky pattern" AND
    ///      centroid gap (next_probe - last_probe) is small — expand to more cells
    ///   4. Otherwise (ambiguous, low risk) — accept current result
    pub fn search(
        &self,
        ds: &Dataset,
        query: &[i16; DIMS],
        k: usize,
        nprobe: usize,
    ) -> usize {
        // Phase 1: scan nearest nprobe cells, get bits + fraud_count + gap
        let (fraud_count, bits, centroid_probe, centroid_next) =
            self.search_phase_ex(ds, query, k, nprobe);

        // Unanimous: locked, return immediately
        if fraud_count == 0 || fraud_count >= k {
            return fraud_count;
        }

        // Ambiguous 1..k-1: check if it's a risky pattern
        let nprobe_expanded = IVF_NPROBE_REPAIR.min(ds.num_cells);
        if nprobe_expanded <= nprobe {
            return fraud_count;
        }

        // Risky pattern detection: certain top-5 fraud arrangements with a
        // small centroid gap indicate the binary decision (approved: true|false)
        // is sensitive to which neighbours are picked.
        if is_risky_pattern(bits, centroid_probe, centroid_next) {
            // Re-scan with expanded nprobe
            self.search_phase(ds, query, k, nprobe_expanded)
        } else {
            fraud_count
        }
    }

    /// Single-phase IVF search. Returns the fraud count among the top-k nearest
    /// neighbours. Uses i64 accumulator to avoid overflow.
    fn search_phase(
        &self,
        ds: &Dataset,
        query: &[i16; DIMS],
        k: usize,
        nprobe: usize,
    ) -> usize {
        let (_, _, _, _) = self.search_phase_ex(ds, query, k, nprobe);
        // Re-run for the bits; not the most efficient but keeps the public API
        // stable. (Could be unified but premature optimization here.)
        let (fraud_count, _, _, _) = self.search_phase_ex(ds, query, k, nprobe);
        fraud_count
    }

    /// Extended single-phase search that also returns the centroid probe
    /// distance and the next centroid distance (for repair pattern check).
    /// Returns `(fraud_count, bits, centroid_probe_dist, centroid_next_dist)`.
    fn search_phase_ex(
        &self,
        ds: &Dataset,
        query: &[i16; DIMS],
        k: usize,
        nprobe: usize,
    ) -> (usize, u8, i64, i64) {
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

        let mut best: [(i64, u8); 5] = [(i64::MAX, 0); 5]; // (dist, label)
        let mut best_len: usize = 0;

        for &(cdist, cidx) in nearest_cells.iter().take(effective_nprobe) {
            if best_len == k && cdist >= best[k - 1].0 {
                continue;
            }

            let (offset, len) = ds.cell_meta[cidx];
            for i in 0..len {
                let vidx = ds.cell_indices[(offset + i) as usize] as usize;
                let dist = ds.distance(query, vidx);
                let label = ds.labels[vidx];

                if best_len < k {
                    best[best_len] = (dist, label);
                    best_len += 1;
                    let mut j = best_len - 1;
                    while j > 0 && best[j - 1].0 > best[j].0 {
                        best.swap(j - 1, j);
                        j -= 1;
                    }
                } else if dist < best[k - 1].0 {
                    best[k - 1] = (dist, label);
                    let mut j = k - 1;
                    while j > 0 && best[j - 1].0 > best[j].0 {
                        best.swap(j - 1, j);
                        j -= 1;
                    }
                }
            }
        }

        // Compute fraud_count and bits
        let mut fraud_count = 0usize;
        let mut bits: u8 = 0;
        for i in 0..best_len {
            if best[i].1 != 0 {
                fraud_count += 1;
                bits |= 1 << i;
            }
        }

        // Centroid distances for repair pattern check
        let centroid_probe = if effective_nprobe >= 1 {
            nearest_cells[effective_nprobe - 1].0
        } else {
            i64::MAX
        };
        let centroid_next = if effective_nprobe < nearest_cells.len()
            && nearest_cells[effective_nprobe].0 != i64::MAX
        {
            nearest_cells[effective_nprobe].0
        } else {
            i64::MAX
        };

        (fraud_count, bits, centroid_probe, centroid_next)
    }
}

/// Risky pattern detection (adapted from bmtec-rust index.rs `is_risky_pattern`).
///
/// When the top-5 neighbours form specific fraud arrangements and the centroid
/// probe gap is small, the binary decision (approved: true|false) is sensitive
/// to which neighbours are picked. In those cases we expand the IVF probe to
/// gather more candidates.
///
/// The threshold values were tuned by bmtec on a Xeon host; the pattern matches
/// 7 specific bit patterns observed in real fraud queries.
#[inline]
fn is_risky_pattern(bits: u8, centroid_probe: i64, centroid_next: i64) -> bool {
    let centroid_gap = if centroid_next == i64::MAX {
        i64::MAX
    } else {
        centroid_next - centroid_probe
    };

    match bits {
        // 3-of-5 arrangements where the binary decision could flip if a
        // closer neighbour is found in the next probe batch.
        0b00110 => centroid_gap <= 500_000,
        0b01010 => centroid_gap <= 500_000,
        0b01100 => centroid_gap <= 600_000,
        0b10010 => centroid_gap <= 1_200_000,
        0b10011 => centroid_gap <= 500_000,
        0b10110 => centroid_gap <= 700_000,
        0b11100 => centroid_gap <= 150_000,
        _ => false,
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

/// AVX2 squared L2 distance using `_mm256_madd_epi16` (pairwise multiply-add).
///
/// Loads both inputs as a single 32-byte SIMD load each (16 × i16). DIMS=14
/// leaves the last 2 lanes as OOB garbage — we mask them to 0 BEFORE
/// subtracting, so the multiply-add below treats them as a no-op (0²=0).
/// This is both **correct** and **faster** than our prior hand-rolled
/// cvt→mullo→cvt-to-i64 chain: `madd` does d[0]²+d[1]², d[2]²+d[3]², ... in one
/// instruction, then we widen the 8 × i32 result to i64 BEFORE summing to
/// avoid the (1.5e10) i32 overflow.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn distance_i16_avx2(a: &[i16; DIMS], b: &[i16; DIMS]) -> i64 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;

    // Mask: keep i16 lanes 0..13, zero lanes 14,15.
    // _mm256_setr_epi16 args are low-to-high: arg 0 = lane 0, arg 15 = lane 15.
    let mask = _mm256_setr_epi16(
        -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0,
    );

    // Single 32-byte load per vector (16 × i16). The last 2 lanes are
    // OOB garbage (since [i16; 14] is only 28 bytes). The mask zeroes them.
    let av = _mm256_loadu_si256(a.as_ptr() as *const __m256i);
    let bv = _mm256_loadu_si256(b.as_ptr() as *const __m256i);
    let av = _mm256_and_si256(av, mask);
    let bv = _mm256_and_si256(bv, mask);

    // diff = a - b (per i16)
    let d = _mm256_sub_epi16(av, bv);

    // madd(d, d) -> 8 × i32, where lane i = d[2i]² + d[2i+1]².
    // Each pair-sum fits in i32 (max 2 * 20000² = 8e8).
    let madd = _mm256_madd_epi16(d, d);

    // Widen to i64 BEFORE summing — otherwise 8 × 8e8 ≈ 6.4e9 overflows i32.
    let lo = _mm256_cvtepi32_epi64(_mm256_castsi256_si128(madd)); // 4 × i64
    let hi = _mm256_cvtepi32_epi64(_mm256_extracti128_si256(madd, 1)); // 4 × i64
    let sum = _mm256_add_epi64(lo, hi); // 4 × i64 (each is sum of 4 dims squared)

    // Horizontal sum 4 × i64 → 1 × i64
    // sum = [s0, s1, s2, s3]
    // extract lo (s0, s1) and hi (s2, s3), then add pairs
    let sum_lo = _mm256_castsi256_si128(sum);
    let sum_hi = _mm256_extracti128_si256(sum, 1);
    let pair = _mm_add_epi64(sum_lo, sum_hi); // [s0+s2, s1+s3] (order doesn't matter)
    let pair_hi = _mm_unpackhi_epi64(pair, pair); // [s1+s3, s1+s3]
    _mm_cvtsi128_si64(_mm_add_epi64(pair, pair_hi)) // lane 0 = s0+s1+s2+s3
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
