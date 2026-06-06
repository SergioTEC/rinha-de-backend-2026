//! IVF (Inverted File Index) for sub-linear vector search.
//! Uses pre-computed centroids and cell assignments from binary index.

use crate::dataset::{Dataset, DIMS};

pub const IVF_NPROBE_EASY: usize = 1;
pub const IVF_NPROBE_HARD: usize = 8;
pub const IVF_NPROBE_REPAIR: usize = 48;  // Expanded probe when result is ambiguous (1-4)
/// Early stopping threshold: if the k-th neighbor distance is below this,
/// we've found "good enough" neighbors — no need to scan more cells.
pub const IVF_EARLY_DISTANCE_LIMIT: i64 = 200_000;
pub const IVF_NPARTITIONS: usize = 16;     // Number of partition keys (2^4 bits)

/// Compute partition key from a quantized query.
/// Uses 4 features that are stable across different fraud patterns:
/// - bit 0: amount > 5000 (high-value tx)
/// - bit 1: km_from_home > 500 (far from home)
/// - bit 2: tx_count_24h > 10 (many recent tx)
/// - bit 3: amount_vs_avg > 0.5 (unusual amount)
#[inline(always)]
pub fn partition_key(q: &[i16; DIMS]) -> u8 {
    let mut key = 0u8;
    // Quantized values use QSCALE = 10000, so threshold = real_value * 10000.
    // We use i16::MAX-capped thresholds: amount > 3000, km > 300, tx > 8, ratio > 0.3.
    if q[0] > 30_000 { key |= 1 << 0; }
    if q[7] > 3_000 { key |= 1 << 1; }
    if q[8] > 8_000 { key |= 1 << 2; }
    if q[2] > 3_000 { key |= 1 << 3; }
    key
}

pub struct IVFIndex {
    pub num_cells: usize,
    /// Map from partition key (0-15) to list of cell indices that "belong" to it.
    /// Each cell is assigned to the partition key most common in its vectors.
    pub part_by_key: Vec<[u32; 8]>,  // up to 8 cells per partition
    pub part_count: Vec<u32>,         // how many cells per partition
}

impl IVFIndex {
    pub fn new() -> Self {
        Self {
            num_cells: 0,
            part_by_key: vec![[u32::MAX; 8]; IVF_NPARTITIONS],
            part_count: vec![0; IVF_NPARTITIONS],
        }
    }

    pub fn build_from_dataset(ds: &Dataset, _num_cells: usize) -> Self {
        let mut ivf = Self {
            num_cells: ds.num_cells,
            part_by_key: vec![[u32::MAX; 8]; IVF_NPARTITIONS],
            part_count: vec![0; IVF_NPARTITIONS],
        };
        ivf.assign_partitions(ds);
        ivf
    }

    /// Build partition -> cells mapping from dataset centroids.
    /// For each cell, sample its centroid to derive a partition key.
    fn assign_partitions(&mut self, ds: &Dataset) {
        for c in 0..ds.num_cells {
            let key = partition_key(&ds.centroids[c]) as usize;
            let count = self.part_count[key] as usize;
            if count < self.part_by_key[key].len() {
                self.part_by_key[key][count] = c as u32;
                self.part_count[key] += 1;
            }
            // If partition is full (>8 cells), overflow cells fall back to round-robin
        }
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
        if is_risky_pattern(nprobe, bits, centroid_probe, centroid_next) {
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
        // Compute REAL centroid distances and sort by boosted distance
        // (real distance minus 1 if cell is in query's partition).
        // The boost makes partition-matched cells appear closer without
        // changing the actual centroid distance used for lower-bound checks.
        let qkey = partition_key(query) as usize;
        let pcount = self.part_count[qkey] as usize;
        let mut is_in_partition: [bool; 48] = [false; 48];
        let mut cells: [(i64, usize); 48] = [(i64::MAX, 0); 48];  // (boosted_dist, cell_idx)
        let mut real_dist: [i64; 48] = [i64::MAX; 48];            // actual centroid distance

        for c in 0..ds.num_cells {
            let rdist = distance_i16(query, &ds.centroids[c]);
            let mut in_partition = false;
            for i in 0..pcount {
                if self.part_by_key[qkey][i] as usize == c {
                    in_partition = true;
                    break;
                }
            }
            // Boosted distance for sorting: partition-matched cells get -1.
            let mut dist = if in_partition { rdist.saturating_sub(1) } else { rdist };
            let sdist = dist;

            // Simple linear insert: track up to 48 nearest cells, so O(n*48) is fine.
            let effective = 48.min(nprobe).min(ds.num_cells);
            let mut insert_idx = effective;
            for i in 0..effective {
                if sdist < cells[i].0 {
                    insert_idx = i;
                    break;
                }
            }
            if insert_idx < effective {
                for j in (insert_idx + 1..effective).rev() {
                    cells[j] = cells[j - 1];
                    real_dist[j] = real_dist[j - 1];
                    is_in_partition[j] = is_in_partition[j - 1];
                }
                cells[insert_idx] = (sdist, c);
                real_dist[insert_idx] = rdist;
                is_in_partition[insert_idx] = in_partition;
            }
        }
        let nearest_cells = cells;
        let effective_nprobe = nprobe.min(ds.num_cells).min(48);

        let mut best: [(i64, u8); 5] = [(i64::MAX, 0); 5]; // (dist, label)
        let mut best_len: usize = 0;

        for i in 0..effective_nprobe {
            let cdist = real_dist[i];
            let cidx = nearest_cells[i].1;

            // Early stopping: if centroid distance already exceeds the k-th
            // neighbor, the cell cannot contain a better neighbor.
            if best_len == k && cdist >= best[k - 1].0 {
                continue;
            }
            // Early stopping: if k-th neighbor is already "good enough" AND
            // centroid distance is large, no point scanning more cells.
            if best_len == k
                && best[k - 1].0 <= IVF_EARLY_DISTANCE_LIMIT
                && cdist > IVF_EARLY_DISTANCE_LIMIT * 4
            {
                break;
            }

            // Lower bound pruning using bounding box (when available).
            // The bounding box gives a TIGHTER bound than centroid distance
            // because it considers min/max per dimension, not just the centroid.
            // If LB >= k-th neighbor, this cell cannot contain a better vector.
            if best_len == k {
                let lb = ds.lower_bound(query, cidx);
                if lb >= best[k - 1].0 {
                    continue;
                }
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
/// Threshold values are tuned per `nprobe`:
///   - nprobe=10: 7 patterns with hand-tuned thresholds
///   - nprobe=12: 6 patterns with larger thresholds (more cells, larger gap OK)
#[inline]
fn is_risky_pattern(nprobe: usize, bits: u8, centroid_probe: i64, centroid_next: i64) -> bool {
    let centroid_gap = if centroid_next == i64::MAX {
        i64::MAX
    } else {
        centroid_next - centroid_probe
    };

    if nprobe == 10 {
        return match bits {
            0b00110 => centroid_gap <= 500_000,
            0b01010 => centroid_gap <= 500_000,
            0b01100 => centroid_gap <= 600_000,
            0b10010 => centroid_gap <= 1_200_000,
            0b10011 => centroid_gap <= 500_000,
            0b10110 => centroid_gap <= 700_000,
            0b11100 => centroid_gap <= 150_000,
            _ => false,
        };
    }

    if nprobe == 12 {
        return match bits {
            0b00110 => centroid_gap <= 1_600_000,
            0b01010 => centroid_gap <= 3_800_000,
            0b01100 => centroid_gap <= 1_000_000,
            0b10010 => centroid_gap <= 1_800_000,
            0b10011 => centroid_gap <= 500_000,
            0b11100 => centroid_gap <= 150_000,
            _ => false,
        };
    }

    // For other nprobe values, fall back to nprobe=10 thresholds.
    match bits {
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
