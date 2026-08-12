#include <math.h>
#include <stdint.h>

#define PQ_BLOCK 256

__device__ void pq_block_hadamard(float *values, int dim) {
    const int tid = threadIdx.x;
    int offset = 0;
    while (offset < dim) {
        const int remaining = dim - offset;
        int block_len = 1;
        while ((block_len << 1) <= remaining) {
            block_len <<= 1;
        }
        for (int width = 1; width < block_len; width <<= 1) {
            const int butterflies = block_len >> 1;
            for (int pair = tid; pair < butterflies; pair += blockDim.x) {
                const int base = offset + (pair / width) * (width << 1) + (pair % width);
                const float left = values[base];
                const float right = values[base + width];
                values[base] = left + right;
                values[base + width] = left - right;
            }
            __syncthreads();
        }
        const float factor = 1.0f / sqrtf(static_cast<float>(block_len));
        for (int index = tid; index < block_len; index += blockDim.x) {
            values[offset + index] *= factor;
        }
        __syncthreads();
        offset += block_len;
    }
}

extern "C" __global__ __launch_bounds__(PQ_BLOCK) void pq_binary_encode_f32(
    const float *input,
    const float *diagonal,
    int dim,
    int rows,
    unsigned char *encoded,
    int *bad_flags) {
    extern __shared__ float values[];
    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    if (row >= rows || dim <= 0) {
        return;
    }
    if (tid == 0) {
        bad_flags[row] = 0;
    }
    __syncthreads();
    const int base = row * dim;
    for (int index = tid; index < dim; index += blockDim.x) {
        float value = input[base + index];
        if (!isfinite(value)) {
            atomicOr(&bad_flags[row], 1);
            value = 0.0f;
        }
        values[index] = value * diagonal[index];
    }
    __syncthreads();
    pq_block_hadamard(values, dim);

    const int stride = (dim + 7) >> 3;
    unsigned char *row_out = encoded + row * stride;
    for (int byte = tid; byte < stride; byte += blockDim.x) {
        unsigned int packed = 0;
        const int start = byte << 3;
        for (int bit = 0; bit < 8 && start + bit < dim; ++bit) {
            if (values[start + bit] > 0.0f) {
                packed |= 1u << bit;
            }
        }
        row_out[byte] = static_cast<unsigned char>(packed);
    }
}

extern "C" __global__ __launch_bounds__(PQ_BLOCK) void pq_binary_decode_f32(
    const unsigned char *encoded,
    const float *diagonal,
    int dim,
    int rows,
    float amplitude,
    float *output) {
    extern __shared__ float values[];
    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    if (row >= rows || dim <= 0) {
        return;
    }
    const int stride = (dim + 7) >> 3;
    const unsigned char *row_in = encoded + row * stride;
    for (int index = tid; index < dim; index += blockDim.x) {
        const bool positive = ((row_in[index >> 3] >> (index & 7)) & 1u) != 0u;
        values[index] = positive ? amplitude : -amplitude;
    }
    __syncthreads();
    pq_block_hadamard(values, dim);
    for (int index = tid; index < dim; index += blockDim.x) {
        output[row * dim + index] = values[index] * diagonal[index];
    }
}

extern "C" __global__ void pq_binary_score(
    const unsigned char *query,
    const unsigned char *candidates,
    int dim,
    int rows,
    int *mismatch_counts,
    float *scores) {
    const int row = blockIdx.x * blockDim.x + threadIdx.x;
    if (row >= rows || dim <= 0) {
        return;
    }
    const int stride = (dim + 7) >> 3;
    const int full_bytes = dim >> 3;
    const unsigned char *candidate = candidates + row * stride;
    int mismatches = 0;
    for (int index = 0; index < full_bytes; ++index) {
        mismatches += __popc(static_cast<unsigned int>(query[index] ^ candidate[index]));
    }
    const int tail = dim & 7;
    if (tail != 0) {
        const unsigned int mask = (1u << tail) - 1u;
        mismatches += __popc(static_cast<unsigned int>(query[full_bytes] ^ candidate[full_bytes]) & mask);
    }
    mismatch_counts[row] = mismatches;
    scores[row] = 1.0f - 2.0f * static_cast<float>(mismatches) / static_cast<float>(dim);
}
