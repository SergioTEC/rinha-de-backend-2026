#include "distance_avx2.h"
#include <immintrin.h>

// AVX2 implementation of squared L2 distance for 14 int16 dimensions.
// Returns int64_t to avoid overflow: max dist is 14 * (2*32767)^2 ≈ 6e10.
// Requires arrays of at least 16 int16 (32 bytes) with padding at end.
int64_t l2sq_int16_avx2(const int16_t *a, const int16_t *b) {
    // Load 16 int16 from each array (14 dims + 2 padding zeros)
    __m256i va = _mm256_loadu_si256((__m256i const *)a);
    __m256i vb = _mm256_loadu_si256((__m256i const *)b);

    // Subtract: diff = a - b
    __m256i diff = _mm256_sub_epi16(va, vb);

    // Multiply and horizontal add: for each pair (diff[i], diff[i+1]),
    // computes diff[i]*diff[i] + diff[i+1]*diff[i+1] as int32
    __m256i prod = _mm256_madd_epi16(diff, diff);

    // Widen to int64 BEFORE summing to avoid overflow
    __m128i low32 = _mm256_castsi256_si128(prod);
    __m128i high32 = _mm256_extracti128_si256(prod, 1);
    __m256i low64 = _mm256_cvtepi32_epi64(low32);
    __m256i high64 = _mm256_cvtepi32_epi64(high32);
    __m256i sum64 = _mm256_add_epi64(low64, high64);

    // Reduce 4 int64 to 1 int64
    __m128i lo = _mm256_castsi256_si128(sum64);
    __m128i hi = _mm256_extracti128_si256(sum64, 1);
    __m128i pair = _mm_add_epi64(lo, hi);
    __m128i pair_hi = _mm_unpackhi_epi64(pair, pair);
    return _mm_cvtsi128_si64(_mm_add_epi64(pair, pair_hi));
}

// Scalar fallback for any length.
int64_t l2sq_int16_scalar(const int16_t *a, const int16_t *b, int len) {
    int64_t sum = 0;
    for (int i = 0; i < len; i++) {
        int64_t diff = (int64_t)a[i] - (int64_t)b[i];
        sum += diff * diff;
    }
    return sum;
}
