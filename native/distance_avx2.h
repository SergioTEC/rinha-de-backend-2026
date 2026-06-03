#ifndef DISTANCE_AVX2_H
#define DISTANCE_AVX2_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// Compute squared L2 distance between two int16 vectors of 14 dimensions.
// Uses AVX2 to process 8 elements at a time.
// int32_t l2sq_int16_avx2(const int16_t *a, const int16_t *b, int len);
int32_t l2sq_int16_avx2(const int16_t *a, const int16_t *b);

// Scalar fallback for non-AVX2 or tail processing.
int32_t l2sq_int16_scalar(const int16_t *a, const int16_t *b, int len);

#ifdef __cplusplus
}
#endif

#endif // DISTANCE_AVX2_H
