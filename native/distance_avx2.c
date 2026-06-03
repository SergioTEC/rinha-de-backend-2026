#include "distance_avx2.h"
#include <immintrin.h>

// AVX2 implementation of squared L2 distance for 14 int16 dimensions.
// Requires arrays of at least 16 int16 (32 bytes) with padding at end.
int32_t l2sq_int16_avx2(const int16_t *a, const int16_t *b) {
    // Load 16 int16 from each array (14 dims + 2 padding zeros)
    __m256i va = _mm256_loadu_si256((__m256i const *)a);
    __m256i vb = _mm256_loadu_si256((__m256i const *)b);
    
    // Subtract: diff = a - b
    __m256i diff = _mm256_sub_epi16(va, vb);
    
    // Multiply and horizontal add: for each pair (diff[i], diff[i+1]),
    // computes diff[i]*diff[i] + diff[i+1]*diff[i+1] as int32
    __m256i prod = _mm256_madd_epi16(diff, diff);
    
    // Reduce 8 int32 to 1 int32
    __m128i low = _mm256_castsi256_si128(prod);
    __m128i high = _mm256_extracti128_si256(prod, 1);
    __m128i sum = _mm_add_epi32(low, high);
    sum = _mm_hadd_epi32(sum, sum);
    sum = _mm_hadd_epi32(sum, sum);
    
    return _mm_cvtsi128_si32(sum);
}

// Scalar fallback for any length.
int32_t l2sq_int16_scalar(const int16_t *a, const int16_t *b, int len) {
    int32_t sum = 0;
    for (int i = 0; i < len; i++) {
        int32_t diff = (int32_t)a[i] - (int32_t)b[i];
        sum += diff * diff;
    }
    return sum;
}
