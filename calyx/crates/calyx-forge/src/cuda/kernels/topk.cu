#include <math.h>

#define TOPK_BLOCK 1024
// One merge block owns two shared-memory slots per thread, so a single pass
// collapses 2048 candidate pairs into `k`. That is what makes the device-side
// merge terminate for every admissible k: the reduction factor is 2048/k,
// which is >= 2 for the whole supported range k <= 1024.
#define TOPK_MERGE_TILE 2048
#define TOPK_PAD_INDEX 2147483647
#define TOPK_FLAG_NONFINITE 1u

// Total order over f32 that matches Rust's `f32::total_cmp` for every
// non-NaN value, including the -0.0 < +0.0 distinction that a plain `>`
// comparison cannot see. The host merge this kernel replaced used
// `total_cmp`, and `cpu::topk_f32` still does, so the device path has to
// order identically or the two backends disagree on exact ties.
__device__ __forceinline__ int forge_total_order_key(float value) {
    const unsigned int bits = __float_as_uint(value);
    const unsigned int mask = (bits & 0x80000000u) ? 0x7FFFFFFFu : 0u;
    return (int)(bits ^ mask);
}

__device__ __forceinline__ bool forge_higher_priority(
    float left_score,
    int left_index,
    float right_score,
    int right_index) {
    const int left_key = forge_total_order_key(left_score);
    const int right_key = forge_total_order_key(right_score);
    if (left_key != right_key) {
        return left_key > right_key;
    }
    return left_index < right_index;
}

__device__ __forceinline__ void forge_compare_swap(
    float *values,
    int *indices,
    unsigned int slot,
    unsigned int stride,
    unsigned int size) {
    const unsigned int partner = slot ^ stride;
    if (partner <= slot) {
        return;
    }
    const bool descending = (slot & size) == 0;
    const bool left_wins =
        forge_higher_priority(values[slot], indices[slot], values[partner], indices[partner]);
    // DETERMINISM: ties broken by index (lower index wins) after the total-order
    // key comparison; no warp-divergent paths on the index comparison.
    const bool should_swap = descending ? !left_wins : left_wins;
    if (should_swap) {
        const float tmp_value = values[slot];
        const int tmp_index = indices[slot];
        values[slot] = values[partner];
        indices[slot] = indices[partner];
        values[partner] = tmp_value;
        indices[partner] = tmp_index;
    }
}

// Pass 1: one block per (row, 1024-score chunk). `sign` is +1 for max-k
// (cosine/dot) and -1 for min-k (L2 squared), which reproduces exactly what
// `cpu::rank_scores` does for `KnnMetric::L2Squared` — negate, take the top-k,
// negate back — without a host sort.
//
// Rows are score matrices laid out [row][count]; emitted indices are relative
// to the row, matching the single-query contract.
extern "C" __global__ __launch_bounds__(TOPK_BLOCK) void bitonic_topk_f32(
    const float *scores,
    int count,
    int k,
    float sign,
    int *out_indices,
    float *out_scores,
    unsigned int *flags) {
    __shared__ float values[TOPK_BLOCK];
    __shared__ int indices[TOPK_BLOCK];

    const int tid = threadIdx.x;
    const int chunk = blockIdx.x;
    const int row = blockIdx.y;
    const int chunks = gridDim.x;
    const int chunk_start = chunk * TOPK_BLOCK;
    const int chunk_count = max(0, min(TOPK_BLOCK, count - chunk_start));

    if (count <= 0 || k <= 0 || chunk_count <= 0) {
        return;
    }

    const long long row_base = (long long)row * (long long)count;
    const long long out_base =
        ((long long)row * (long long)chunks + (long long)chunk) * (long long)k;

    if (tid < chunk_count) {
        const float raw = scores[row_base + (long long)chunk_start + tid];
        if (!isfinite(raw)) {
            // Fail closed: record the refusal and park the lane at the bottom of
            // the order so the sort still terminates deterministically. The host
            // reads `flags` before it reads any result.
            atomicOr(flags, TOPK_FLAG_NONFINITE);
            values[tid] = -INFINITY;
            indices[tid] = TOPK_PAD_INDEX;
        } else {
            values[tid] = sign * raw;
            indices[tid] = chunk_start + tid;
        }
    } else {
        values[tid] = -INFINITY;
        indices[tid] = TOPK_PAD_INDEX;
    }
    __syncthreads();

    for (unsigned int size = 2; size <= TOPK_BLOCK; size <<= 1) {
        for (unsigned int stride = size >> 1; stride > 0; stride >>= 1) {
            forge_compare_swap(values, indices, (unsigned int)tid, stride, size);
            __syncthreads();
        }
    }

    if (tid < k) {
        out_indices[out_base + tid] = indices[tid];
        out_scores[out_base + tid] = values[tid];
    }
}

// Merge pass: collapses `TOPK_MERGE_TILE` already-ranked (score, index) pairs
// into the block's own top-k. Applied repeatedly until one chunk per row
// remains, so only `rows * k` entries ever cross PCIe.
extern "C" __global__ __launch_bounds__(TOPK_BLOCK) void bitonic_topk_merge_f32(
    const float *in_scores,
    const int *in_indices,
    int count,
    int k,
    int *out_indices,
    float *out_scores) {
    __shared__ float values[TOPK_MERGE_TILE];
    __shared__ int indices[TOPK_MERGE_TILE];

    const int tid = threadIdx.x;
    const int chunk = blockIdx.x;
    const int row = blockIdx.y;
    const int chunks = gridDim.x;

    if (count <= 0 || k <= 0) {
        return;
    }

    const long long row_base = (long long)row * (long long)count;
    const int tile_start = chunk * TOPK_MERGE_TILE;

    for (int slot = tid; slot < TOPK_MERGE_TILE; slot += TOPK_BLOCK) {
        const int src = tile_start + slot;
        if (src < count) {
            values[slot] = in_scores[row_base + src];
            indices[slot] = in_indices[row_base + src];
        } else {
            values[slot] = -INFINITY;
            indices[slot] = TOPK_PAD_INDEX;
        }
    }
    __syncthreads();

    for (unsigned int size = 2; size <= TOPK_MERGE_TILE; size <<= 1) {
        for (unsigned int stride = size >> 1; stride > 0; stride >>= 1) {
            for (unsigned int slot = tid; slot < TOPK_MERGE_TILE; slot += TOPK_BLOCK) {
                forge_compare_swap(values, indices, slot, stride, size);
            }
            __syncthreads();
        }
    }

    const long long out_base =
        ((long long)row * (long long)chunks + (long long)chunk) * (long long)k;
    for (int slot = tid; slot < k; slot += TOPK_BLOCK) {
        out_indices[out_base + slot] = indices[slot];
        out_scores[out_base + slot] = values[slot];
    }
}
