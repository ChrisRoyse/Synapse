#include <math.h>
#include <stdint.h>

#define POST_FLAG_NONFINITE 1u
#define POST_FLAG_EMPTY_MASK 2u
#define POST_FLAG_INVALID_INDEX 4u
#define POST_FLAG_ZERO_NORM 8u

__device__ __forceinline__ bool finite2(float a, float b) {
    return isfinite(a) && isfinite(b);
}

__device__ __forceinline__ const float *external_f32_ptr(unsigned long long ptr) {
    return reinterpret_cast<const float *>(static_cast<uintptr_t>(ptr));
}

__device__ __forceinline__ void reduce_sums(
    float *sum0,
    float *sum1,
    int *bad,
    int tid) {
    for (int stride = 128; stride > 0; stride >>= 1) {
        if (tid < stride) {
            sum0[tid] += sum0[tid + stride];
            sum1[tid] += sum1[tid + stride];
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
}

// BATCHING (#2107 H4): `blockIdx.y` is the query row. A single-query launch is
// `gridDim.y == 1`, which walks exactly the same per-block reduction tree over
// exactly the same operands in exactly the same order as the pre-batching
// kernel, so batched results are bit-identical to the per-query loop they
// replace. Only the launch geometry changed.
extern "C" __global__ __launch_bounds__(256) void cosine_batch_f32(
    const float *queries,
    const float *candidates,
    int dim,
    int n_cands,
    float *out) {
    __shared__ float dot_shared[256];
    __shared__ float norm_q_shared[256];
    __shared__ float norm_c_shared[256];
    __shared__ int bad_shared[256];

    const int cand = blockIdx.x;
    const int tid = threadIdx.x;
    if (cand >= n_cands) {
        return;
    }

    const float *query = queries + (long long)blockIdx.y * (long long)dim;
    const long long out_index = (long long)blockIdx.y * (long long)n_cands + cand;

    float dot = 0.0f;
    float norm_q = 0.0f;
    float norm_c = 0.0f;
    int bad = dim <= 0;
    // 64-bit row base: n_cands * dim overflows int32 for wide candidate panels.
    const long long base = (long long)cand * (long long)dim;

    for (int i = tid; i < dim; i += blockDim.x) {
        const float q = query[i];
        const float c = candidates[base + i];
        bad |= !finite2(q, c);
        dot += q * c;
        norm_q += q * q;
        norm_c += c * c;
    }

    dot_shared[tid] = dot;
    norm_q_shared[tid] = norm_q;
    norm_c_shared[tid] = norm_c;
    bad_shared[tid] = bad;
    __syncthreads();

    reduce_sums(dot_shared, norm_q_shared, bad_shared, tid);
    for (int stride = 128; stride > 0; stride >>= 1) {
        if (tid < stride) {
            norm_c_shared[tid] += norm_c_shared[tid + stride];
        }
        __syncthreads();
    }

    if (tid == 0) {
        const float denom = sqrtf(norm_q_shared[0]) * sqrtf(norm_c_shared[0]);
        if (bad_shared[0]) {
            out[out_index] = NAN;
        } else {
            out[out_index] = denom > 0.0f ? dot_shared[0] / denom : -2.0f;
        }
    }
}

extern "C" __global__ __launch_bounds__(256) void dot_batch_f32(
    const float *queries,
    const float *candidates,
    int dim,
    int n_cands,
    float *out) {
    __shared__ float dot_shared[256];
    __shared__ float unused_shared[256];
    __shared__ int bad_shared[256];

    const int cand = blockIdx.x;
    const int tid = threadIdx.x;
    if (cand >= n_cands) {
        return;
    }

    const float *query = queries + (long long)blockIdx.y * (long long)dim;
    const long long out_index = (long long)blockIdx.y * (long long)n_cands + cand;

    float dot = 0.0f;
    int bad = dim < 0;
    // 64-bit row base: n_cands * dim overflows int32 for wide candidate panels.
    const long long base = (long long)cand * (long long)dim;

    for (int i = tid; i < dim; i += blockDim.x) {
        const float q = query[i];
        const float c = candidates[base + i];
        bad |= !finite2(q, c);
        dot += q * c;
    }

    dot_shared[tid] = dot;
    unused_shared[tid] = 0.0f;
    bad_shared[tid] = bad;
    __syncthreads();
    reduce_sums(dot_shared, unused_shared, bad_shared, tid);

    if (tid == 0) {
        out[out_index] = bad_shared[0] ? NAN : dot_shared[0];
    }
}

extern "C" __global__ __launch_bounds__(256) void l2_batch_f32(
    const float *queries,
    const float *candidates,
    int dim,
    int n_cands,
    float *out) {
    __shared__ float l2_shared[256];
    __shared__ float unused_shared[256];
    __shared__ int bad_shared[256];

    const int cand = blockIdx.x;
    const int tid = threadIdx.x;
    if (cand >= n_cands) {
        return;
    }

    const float *query = queries + (long long)blockIdx.y * (long long)dim;
    const long long out_index = (long long)blockIdx.y * (long long)n_cands + cand;

    float l2 = 0.0f;
    int bad = dim < 0;
    // 64-bit row base: n_cands * dim overflows int32 for wide candidate panels.
    const long long base = (long long)cand * (long long)dim;

    for (int i = tid; i < dim; i += blockDim.x) {
        const float q = query[i];
        const float c = candidates[base + i];
        const float diff = q - c;
        bad |= !finite2(q, c);
        l2 += diff * diff;
    }

    l2_shared[tid] = l2;
    unused_shared[tid] = 0.0f;
    bad_shared[tid] = bad;
    __syncthreads();
    reduce_sums(l2_shared, unused_shared, bad_shared, tid);

    if (tid == 0) {
        out[out_index] = bad_shared[0] ? NAN : l2_shared[0];
    }
}

extern "C" __global__ __launch_bounds__(256) void paired_cosine_f32(
    const float *left,
    const float *right,
    int dim,
    int pair_count,
    float *out) {
    __shared__ float dot_shared[256];
    __shared__ float norm_left_shared[256];
    __shared__ float norm_right_shared[256];
    __shared__ int bad_shared[256];

    const int pair = blockIdx.x;
    const int tid = threadIdx.x;
    if (pair >= pair_count) {
        return;
    }

    float dot = 0.0f;
    float norm_left = 0.0f;
    float norm_right = 0.0f;
    int bad = dim <= 0;
    const int base = pair * dim;

    for (int i = tid; i < dim; i += blockDim.x) {
        const float left_value = left[base + i];
        const float right_value = right[base + i];
        bad |= !finite2(left_value, right_value);
        dot += left_value * right_value;
        norm_left += left_value * left_value;
        norm_right += right_value * right_value;
    }

    dot_shared[tid] = dot;
    norm_left_shared[tid] = norm_left;
    norm_right_shared[tid] = norm_right;
    bad_shared[tid] = bad;
    __syncthreads();

    reduce_sums(dot_shared, norm_left_shared, bad_shared, tid);
    for (int stride = 128; stride > 0; stride >>= 1) {
        if (tid < stride) {
            norm_right_shared[tid] += norm_right_shared[tid + stride];
        }
        __syncthreads();
    }

    if (tid == 0) {
        const float denom = sqrtf(norm_left_shared[0]) * sqrtf(norm_right_shared[0]);
        if (bad_shared[0]) {
            out[pair] = NAN;
        } else {
            out[pair] = denom > 0.0f ? dot_shared[0] / denom : -2.0f;
        }
    }
}

extern "C" __global__ __launch_bounds__(256) void normalize_rows_f32(
    float *vecs,
    int dim,
    int rows) {
    __shared__ float norm_shared[256];
    __shared__ int bad_shared[256];
    __shared__ float scale_shared;

    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    if (row >= rows) {
        return;
    }

    const int base = row * dim;
    float norm_sq = 0.0f;
    int bad = dim <= 0;

    for (int i = tid; i < dim; i += blockDim.x) {
        const float value = vecs[base + i];
        bad |= !isfinite(value);
        norm_sq += value * value;
    }

    norm_shared[tid] = norm_sq;
    bad_shared[tid] = bad;
    __syncthreads();

    for (int stride = 128; stride > 0; stride >>= 1) {
        if (tid < stride) {
            norm_shared[tid] += norm_shared[tid + stride];
            bad_shared[tid] |= bad_shared[tid + stride];
        }
        __syncthreads();
    }

    if (tid == 0) {
        const float norm = sqrtf(norm_shared[0]);
        scale_shared = (bad_shared[0] || !(norm > 0.0f) || !isfinite(norm))
            ? NAN
            : 1.0f / norm;
    }
    __syncthreads();

    for (int i = tid; i < dim; i += blockDim.x) {
        vecs[base + i] = isfinite(scale_shared) ? vecs[base + i] * scale_shared : NAN;
    }
}

extern "C" __global__ __launch_bounds__(256) void copy_dense_external_f32(
    unsigned long long values_ptr,
    int batch,
    int dim,
    float *out,
    unsigned int *flags) {
    const int idx = blockIdx.x * blockDim.x + threadIdx.x;
    const int len = batch * dim;
    if (idx >= len) {
        return;
    }
    const float *values = external_f32_ptr(values_ptr);
    const float value = values[idx];
    if (!isfinite(value)) {
        atomicOr(flags, POST_FLAG_NONFINITE);
    }
    out[idx] = value;
}

extern "C" __global__ __launch_bounds__(256) void pool_tokens_external_f32(
    unsigned long long values_ptr,
    const long long *mask,
    int batch,
    int seq,
    int dim,
    int policy,
    float *out,
    unsigned int *flags) {
    __shared__ int count_shared;
    __shared__ int selected_shared;

    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    if (row >= batch) {
        return;
    }

    if (tid == 0) {
        int count = 0;
        int selected = -1;
        const int mask_base = row * seq;
        for (int token = 0; token < seq; ++token) {
            if (mask[mask_base + token] > 0) {
                if (selected < 0 || policy == 2) {
                    selected = token;
                }
                ++count;
            }
        }
        count_shared = count;
        selected_shared = selected;
        if (count == 0 || selected < 0) {
            atomicOr(flags, POST_FLAG_EMPTY_MASK);
        }
    }
    __syncthreads();

    if (count_shared <= 0 || selected_shared < 0) {
        return;
    }

    const float *values = external_f32_ptr(values_ptr);
    const int token_base = row * seq * dim;
    const int mask_base = row * seq;
    const int out_base = row * dim;
    for (int axis = tid; axis < dim; axis += blockDim.x) {
        float value = 0.0f;
        if (policy == 0) {
            for (int token = 0; token < seq; ++token) {
                if (mask[mask_base + token] > 0) {
                    value += values[token_base + token * dim + axis];
                }
            }
            value /= static_cast<float>(count_shared);
        } else {
            value = values[token_base + selected_shared * dim + axis];
        }
        if (!isfinite(value)) {
            atomicOr(flags, POST_FLAG_NONFINITE);
        }
        out[out_base + axis] = value;
    }
}

extern "C" __global__ __launch_bounds__(256) void sparse_positive_external_f32(
    unsigned long long values_ptr,
    int batch,
    int dim,
    unsigned int *indices,
    float *out,
    int *counts,
    unsigned int *flags) {
    const int axis = blockIdx.x * blockDim.x + threadIdx.x;
    const int row = blockIdx.y;
    if (row >= batch || axis >= dim) {
        return;
    }

    const float *values = external_f32_ptr(values_ptr);
    const float value = values[row * dim + axis];
    if (!isfinite(value)) {
        atomicOr(flags, POST_FLAG_NONFINITE);
        return;
    }
    if (value > 0.0f) {
        const int slot = atomicAdd(&counts[row], 1);
        indices[row * dim + slot] = static_cast<unsigned int>(axis);
        out[row * dim + slot] = value;
    }
}

// BGE-M3 emits one lexical weight per input token, not a vocabulary-width
// row.  Compact and max-reduce duplicate token ids entirely on the device.
// One deterministic worker owns each row so the emitted indices are sorted
// and stable across CUDA schedules; seq is bounded by the frozen tokenizer
// contract (512), while the transformer forward dominates this O(seq^2)
// postprocess.
extern "C" __global__ void bgem3_sparse_compact_external_f32(
    unsigned long long values_ptr,
    const long long *token_ids,
    const long long *mask,
    int batch,
    int seq,
    int vocab_dim,
    unsigned int *indices,
    float *out,
    int *counts,
    unsigned int *flags) {
    const int row = blockIdx.x;
    if (row >= batch || threadIdx.x != 0) {
        return;
    }
    const float *values = external_f32_ptr(values_ptr);
    const int base = row * seq;
    int count = 0;
    for (int token = 0; token < seq; ++token) {
        if (mask[base + token] <= 0) {
            continue;
        }
        const long long token_id = token_ids[base + token];
        if (token_id >= 0 && token_id <= 3) {
            continue;
        }
        if (token_id < 0 || token_id >= vocab_dim) {
            atomicOr(flags, POST_FLAG_INVALID_INDEX);
            continue;
        }
        const float weight = values[base + token];
        if (!isfinite(weight)) {
            atomicOr(flags, POST_FLAG_NONFINITE);
            continue;
        }
        if (weight <= 0.0f) {
            continue;
        }
        const unsigned int id = static_cast<unsigned int>(token_id);
        int position = 0;
        while (position < count && indices[base + position] < id) {
            ++position;
        }
        if (position < count && indices[base + position] == id) {
            out[base + position] = fmaxf(out[base + position], weight);
            continue;
        }
        if (count >= seq) {
            atomicOr(flags, POST_FLAG_INVALID_INDEX);
            continue;
        }
        for (int move = count; move > position; --move) {
            indices[base + move] = indices[base + move - 1];
            out[base + move] = out[base + move - 1];
        }
        indices[base + position] = id;
        out[base + position] = weight;
        ++count;
    }
    counts[row] = count;
}

extern "C" __global__ __launch_bounds__(256) void colbert_compact_external_f32(
    unsigned long long values_ptr,
    const long long *mask,
    int batch,
    int seq,
    int dim,
    int normalize,
    float *out,
    int *counts,
    unsigned int *flags) {
    const int token = blockIdx.x;
    const int row = blockIdx.y;
    const int tid = threadIdx.x;
    if (row >= batch || token >= seq) {
        return;
    }

    const int mask_base = row * seq;
    if (token == 0 && tid == 0) {
        int count = 0;
        for (int idx = 0; idx < seq; ++idx) {
            if (mask[mask_base + idx] > 0) {
                ++count;
            }
        }
        counts[row] = count;
        if (count == 0) {
            atomicOr(flags, POST_FLAG_EMPTY_MASK);
        }
    }

    if (mask[mask_base + token] <= 0) {
        return;
    }

    int ordinal = 0;
    for (int idx = 0; idx < token; ++idx) {
        if (mask[mask_base + idx] > 0) {
            ++ordinal;
        }
    }

    const float *values = external_f32_ptr(values_ptr);
    const int input_base = row * seq * dim + token * dim;
    const int output_base = row * seq * dim + ordinal * dim;
    __shared__ float norm_shared[256];
    float norm_sq = 0.0f;
    if (normalize) {
        for (int axis = tid; axis < dim; axis += blockDim.x) {
            const float value = values[input_base + axis];
            if (!isfinite(value)) {
                atomicOr(flags, POST_FLAG_NONFINITE);
            } else {
                norm_sq += value * value;
            }
        }
        norm_shared[tid] = norm_sq;
        __syncthreads();
        for (int stride = 128; stride > 0; stride >>= 1) {
            if (tid < stride) {
                norm_shared[tid] += norm_shared[tid + stride];
            }
            __syncthreads();
        }
        if (!(norm_shared[0] > 0.0f) || !isfinite(norm_shared[0])) {
            if (tid == 0) {
                atomicOr(flags, POST_FLAG_ZERO_NORM);
            }
            return;
        }
    }
    const float inv_norm = normalize ? rsqrtf(norm_shared[0]) : 1.0f;
    for (int axis = tid; axis < dim; axis += blockDim.x) {
        const float value = values[input_base + axis];
        if (!isfinite(value)) {
            atomicOr(flags, POST_FLAG_NONFINITE);
        }
        out[output_base + axis] = value * inv_norm;
    }
}

extern "C" __global__ __launch_bounds__(256) void validate_f32_flags(
    const float *values,
    int len,
    int sentinel_mode,
    unsigned int *flags) {
    const int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx >= len) {
        return;
    }

    const float value = values[idx];
    if (!isfinite(value)) {
        atomicOr(flags, 1u);
    }
    if (sentinel_mode && value <= -1.5f) {
        atomicOr(flags, 2u);
    }
}

extern "C" __global__ __launch_bounds__(256) void validate_f32_ranges_flags(
    const float *values,
    const int *ranges,
    int range_count,
    unsigned int expected_bits,
    int expected_bits_mode,
    unsigned int *flags) {
    const int rel_idx = blockIdx.x * blockDim.x + threadIdx.x;
    const int range_idx = blockIdx.y;
    if (range_idx >= range_count) {
        return;
    }

    const int offset = ranges[range_idx * 2];
    const int len = ranges[range_idx * 2 + 1];
    if (rel_idx >= len) {
        return;
    }

    const float value = values[offset + rel_idx];
    if (expected_bits_mode) {
        if (__float_as_uint(value) != expected_bits) {
            atomicOr(flags, 4u);
        }
    } else if (!isfinite(value)) {
        atomicOr(flags, 1u);
    }
}
