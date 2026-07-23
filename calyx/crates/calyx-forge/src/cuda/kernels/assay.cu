#include <math.h>
#include <float.h>

#ifndef M_PI
#define M_PI 3.14159265358979323846264338327950288
#endif

#define ASSAY_THREADS 256
#define ASSAY_MAX_K 32
#define ASSAY_LINALG_MAX_D 64
#define ASSAY_GRANGER_MAX_LAGS 32
#define ASSAY_GRANGER_MAX_K (2 * ASSAY_GRANGER_MAX_LAGS + 1)
#define ASSAY_HAWKES_MAX_D 32
#define ASSAY_FLAG_NONFINITE 1u
#define ASSAY_FLAG_INVALID_INDEX 2u

__device__ __forceinline__ void reduce4(double *a, double *b, double *c, double *d, unsigned int *bad, int tid) {
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            a[tid] += a[tid + stride];
            b[tid] += b[tid + stride];
            c[tid] += c[tid + stride];
            d[tid] += d[tid + stride];
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
}

__device__ __forceinline__ void reduce3(double *a, double *b, double *c, unsigned int *bad, int tid) {
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            a[tid] += a[tid + stride];
            b[tid] += b[tid + stride];
            c[tid] += c[tid + stride];
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
}

__device__ __forceinline__ void reduce1(double *a, unsigned int *bad, int tid) {
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            a[tid] += a[tid + stride];
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
}

__device__ __forceinline__ double kernel_lookup(const double *k, int n, int row, int col) {
    return k[row * n + col];
}

__device__ __forceinline__ float assay_sigmoid_f32(float logit) {
    const float clamped = fminf(fmaxf(logit, -40.0f), 40.0f);
    return 1.0f / (1.0f + expf(-clamped));
}

__device__ __forceinline__ void reduce_float_sum(float *values, unsigned int *bad, int tid) {
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            values[tid] += values[tid + stride];
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
}

__device__ __forceinline__ void reduce_float_max(float *values, unsigned int *bad, int tid) {
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            values[tid] = fmaxf(values[tid], values[tid + stride]);
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
}

__device__ __forceinline__ void reduce6(
    double *a,
    double *b,
    double *c,
    double *d,
    double *e,
    double *f,
    unsigned int *bad,
    int tid) {
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            a[tid] += a[tid + stride];
            b[tid] += b[tid + stride];
            c[tid] += c[tid + stride];
            d[tid] += d[tid + stride];
            e[tid] += e[tid + stride];
            f[tid] += f[tid + stride];
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
}

__device__ __forceinline__ double assay_gls_power_from_sums(
    double c_sum,
    double s_sum,
    double cc_hat,
    double cs_hat,
    double yc_sum,
    double ys_sum,
    int n,
    double variance,
    unsigned int *bad) {
    if (n <= 0 || !isfinite(variance) || variance <= 0.0) {
        *bad |= ASSAY_FLAG_INVALID_INDEX;
        return 0.0;
    }
    const double weight = 1.0 / (double)n;
    const double c = c_sum * weight;
    const double s = s_sum * weight;
    const double cc = cc_hat * weight - c * c;
    const double ss = (1.0 - cc_hat * weight) - s * s;
    const double cs = cs_hat * weight - c * s;
    const double yc = yc_sum * weight;
    const double ys = ys_sum * weight;
    const double determinant = cc * ss - cs * cs;
    if (fabs(determinant) < DBL_EPSILON) {
        return 0.0;
    }
    const double power = (ss * yc * yc + cc * ys * ys - 2.0 * cs * yc * ys) /
                         (variance * determinant);
    if (!isfinite(power)) {
        *bad |= ASSAY_FLAG_NONFINITE;
        return 0.0;
    }
    return fmin(fmax(power, 0.0), 1.0);
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_gls_powers_f64(
    const double *times,
    const double *centered,
    const double *frequencies,
    int n,
    int frequency_count,
    double variance,
    double *powers,
    unsigned int *flags) {
    __shared__ double c_sums[ASSAY_THREADS];
    __shared__ double s_sums[ASSAY_THREADS];
    __shared__ double cc_sums[ASSAY_THREADS];
    __shared__ double cs_sums[ASSAY_THREADS];
    __shared__ double yc_sums[ASSAY_THREADS];
    __shared__ double ys_sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int freq_idx = blockIdx.x;
    const int tid = threadIdx.x;
    unsigned int local_bad =
        (freq_idx >= frequency_count || frequency_count <= 0 || n <= 0 || !isfinite(variance) ||
         variance <= 0.0)
            ? ASSAY_FLAG_INVALID_INDEX
            : 0u;
    double c_sum = 0.0;
    double s_sum = 0.0;
    double cc_sum = 0.0;
    double cs_sum = 0.0;
    double yc_sum = 0.0;
    double ys_sum = 0.0;
    if (local_bad == 0u) {
        const double frequency = frequencies[freq_idx];
        if (!isfinite(frequency) || frequency <= 0.0) {
            local_bad |= ASSAY_FLAG_NONFINITE;
        }
        const double omega = 2.0 * M_PI * frequency;
        for (int row = tid; row < n; row += blockDim.x) {
            const double time = times[row];
            const double value = centered[row];
            if (!(isfinite(time) && isfinite(value))) {
                local_bad |= ASSAY_FLAG_NONFINITE;
            }
            double s;
            double c;
            sincos(omega * time, &s, &c);
            c_sum += c;
            s_sum += s;
            cc_sum += c * c;
            cs_sum += c * s;
            yc_sum += value * c;
            ys_sum += value * s;
        }
    }
    c_sums[tid] = c_sum;
    s_sums[tid] = s_sum;
    cc_sums[tid] = cc_sum;
    cs_sums[tid] = cs_sum;
    yc_sums[tid] = yc_sum;
    ys_sums[tid] = ys_sum;
    bad[tid] = local_bad;
    __syncthreads();
    reduce6(c_sums, s_sums, cc_sums, cs_sums, yc_sums, ys_sums, bad, tid);
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        unsigned int power_bad = 0u;
        const double power = assay_gls_power_from_sums(
            c_sums[0], s_sums[0], cc_sums[0], cs_sums[0], yc_sums[0], ys_sums[0], n, variance,
            &power_bad);
        if (power_bad != 0u) {
            atomicOr(flags, power_bad);
            return;
        }
        powers[freq_idx] = power;
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_gls_permutation_powers_f64(
    const double *times,
    const double *centered,
    const double *frequencies,
    const int *permutations,
    int n,
    int frequency_count,
    int permutation_count,
    double variance,
    double *powers,
    unsigned int *flags) {
    __shared__ double c_sums[ASSAY_THREADS];
    __shared__ double s_sums[ASSAY_THREADS];
    __shared__ double cc_sums[ASSAY_THREADS];
    __shared__ double cs_sums[ASSAY_THREADS];
    __shared__ double yc_sums[ASSAY_THREADS];
    __shared__ double ys_sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int freq_idx = blockIdx.x;
    const int perm_idx = blockIdx.y;
    const int tid = threadIdx.x;
    unsigned int local_bad =
        (freq_idx >= frequency_count || perm_idx >= permutation_count || frequency_count <= 0 ||
         permutation_count <= 0 || n <= 0 || !isfinite(variance) || variance <= 0.0)
            ? ASSAY_FLAG_INVALID_INDEX
            : 0u;
    double c_sum = 0.0;
    double s_sum = 0.0;
    double cc_sum = 0.0;
    double cs_sum = 0.0;
    double yc_sum = 0.0;
    double ys_sum = 0.0;
    if (local_bad == 0u) {
        const double frequency = frequencies[freq_idx];
        if (!isfinite(frequency) || frequency <= 0.0) {
            local_bad |= ASSAY_FLAG_NONFINITE;
        }
        const double omega = 2.0 * M_PI * frequency;
        const int perm_base = perm_idx * n;
        for (int row = tid; row < n; row += blockDim.x) {
            const int source = permutations[perm_base + row];
            if (source < 0 || source >= n) {
                local_bad |= ASSAY_FLAG_INVALID_INDEX;
                continue;
            }
            const double time = times[row];
            const double value = centered[source];
            if (!(isfinite(time) && isfinite(value))) {
                local_bad |= ASSAY_FLAG_NONFINITE;
            }
            double s;
            double c;
            sincos(omega * time, &s, &c);
            c_sum += c;
            s_sum += s;
            cc_sum += c * c;
            cs_sum += c * s;
            yc_sum += value * c;
            ys_sum += value * s;
        }
    }
    c_sums[tid] = c_sum;
    s_sums[tid] = s_sum;
    cc_sums[tid] = cc_sum;
    cs_sums[tid] = cs_sum;
    yc_sums[tid] = yc_sum;
    ys_sums[tid] = ys_sum;
    bad[tid] = local_bad;
    __syncthreads();
    reduce6(c_sums, s_sums, cc_sums, cs_sums, yc_sums, ys_sums, bad, tid);
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        unsigned int power_bad = 0u;
        const double power = assay_gls_power_from_sums(
            c_sums[0], s_sums[0], cc_sums[0], cs_sums[0], yc_sums[0], ys_sums[0], n, variance,
            &power_bad);
        if (power_bad != 0u) {
            atomicOr(flags, power_bad);
            return;
        }
        powers[perm_idx * frequency_count + freq_idx] = power;
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_row_max_f64(
    const double *values,
    int row_len,
    int row_count,
    double *max_values,
    unsigned int *flags) {
    __shared__ double local[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];
    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    unsigned int local_bad =
        (row >= row_count || row_len <= 0 || row_count <= 0) ? ASSAY_FLAG_INVALID_INDEX : 0u;
    double best = 0.0;
    if (local_bad == 0u) {
        const int base = row * row_len;
        for (int col = tid; col < row_len; col += blockDim.x) {
            const double value = values[base + col];
            if (!isfinite(value)) {
                local_bad |= ASSAY_FLAG_NONFINITE;
            }
            best = fmax(best, value);
        }
    }
    local[tid] = best;
    bad[tid] = local_bad;
    __syncthreads();
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            local[tid] = fmax(local[tid], local[tid + stride]);
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        max_values[row] = local[0];
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_acf_slotted_f64(
    const double *times,
    const double *centered,
    int n,
    double variance,
    double slot_width,
    double max_lag,
    int slot_count,
    double *sums,
    int *counts,
    unsigned int *flags) {
    const int i = blockIdx.x;
    const int tid = threadIdx.x;
    if (i >= n || i < 0 || n <= 0 || slot_count <= 0 || !isfinite(variance) || variance <= 0.0 ||
        !isfinite(slot_width) || slot_width <= 0.0 || !isfinite(max_lag) || max_lag <= 0.0) {
        if (i == 0 && tid == 0) {
            atomicOr(flags, ASSAY_FLAG_INVALID_INDEX);
        }
        return;
    }
    const double left_time = times[i];
    const double left_value = centered[i];
    if (!(isfinite(left_time) && isfinite(left_value))) {
        if (tid == 0) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
        }
        return;
    }
    for (int j = i + 1 + tid; j < n; j += blockDim.x) {
        const double right_time = times[j];
        const double right_value = centered[j];
        if (!(isfinite(right_time) && isfinite(right_value))) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            continue;
        }
        const double lag = right_time - left_time;
        if (lag > max_lag) {
            continue;
        }
        const int slot = (int)llround(lag / slot_width);
        if (slot >= 1 && slot <= slot_count) {
            atomicAdd(&sums[slot], left_value * right_value);
            atomicAdd(&counts[slot], 1);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_cross_correlation_f32(
    const float *x,
    const float *y,
    int n,
    int max_lag,
    float *correlations,
    int *n_pairs,
    unsigned int *flags) {
    __shared__ double sx_sums[ASSAY_THREADS];
    __shared__ double sy_sums[ASSAY_THREADS];
    __shared__ double sxx_sums[ASSAY_THREADS];
    __shared__ double syy_sums[ASSAY_THREADS];
    __shared__ double sxy_sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int lag_idx = blockIdx.x;
    const int lag = lag_idx - max_lag;
    const int tid = threadIdx.x;
    const int point_count = 2 * max_lag + 1;
    unsigned int local_bad =
        (lag_idx >= point_count || n <= 0 || max_lag < 0) ? ASSAY_FLAG_INVALID_INDEX : 0u;
    int start_x = lag >= 0 ? 0 : -lag;
    int start_y = lag >= 0 ? lag : 0;
    int len = n - abs(lag);
    if (len <= 0) {
        local_bad |= ASSAY_FLAG_INVALID_INDEX;
    }
    double sx = 0.0;
    double sy = 0.0;
    double sxx = 0.0;
    double syy = 0.0;
    double sxy = 0.0;
    if (local_bad == 0u) {
        for (int row = tid; row < len; row += blockDim.x) {
            const float xf = x[start_x + row];
            const float yf = y[start_y + row];
            if (!(isfinite(xf) && isfinite(yf))) {
                local_bad |= ASSAY_FLAG_NONFINITE;
            }
            const double xd = (double)xf;
            const double yd = (double)yf;
            sx += xd;
            sy += yd;
            sxx += xd * xd;
            syy += yd * yd;
            sxy += xd * yd;
        }
    }
    sx_sums[tid] = sx;
    sy_sums[tid] = sy;
    sxx_sums[tid] = sxx;
    syy_sums[tid] = syy;
    sxy_sums[tid] = sxy;
    bad[tid] = local_bad;
    __syncthreads();
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            sx_sums[tid] += sx_sums[tid + stride];
            sy_sums[tid] += sy_sums[tid + stride];
            sxx_sums[tid] += sxx_sums[tid + stride];
            syy_sums[tid] += syy_sums[tid + stride];
            sxy_sums[tid] += sxy_sums[tid + stride];
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        const double nf = (double)len;
        const double vx = sxx_sums[0] - (sx_sums[0] * sx_sums[0]) / nf;
        const double vy = syy_sums[0] - (sy_sums[0] * sy_sums[0]) / nf;
        const double cov = sxy_sums[0] - (sx_sums[0] * sy_sums[0]) / nf;
        const double denom = sqrt(vx * vy);
        if (!(isfinite(vx) && isfinite(vy) && isfinite(cov) && isfinite(denom)) || vx <= 0.0 ||
            vy <= 0.0 || denom <= 0.0) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            return;
        }
        correlations[lag_idx] = (float)fmin(fmax(cov / denom, -1.0), 1.0);
        n_pairs[lag_idx] = len;
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_hawkes_exposures_f64(
    const double *events,
    const int *offsets,
    int d,
    double observation_end,
    double decay,
    double *exposures,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];
    const int source = blockIdx.x;
    const int tid = threadIdx.x;
    unsigned int local_bad =
        (source >= d || d <= 0 || d > ASSAY_HAWKES_MAX_D || !isfinite(observation_end) ||
         observation_end <= 0.0 || !isfinite(decay) || decay <= 0.0)
            ? ASSAY_FLAG_INVALID_INDEX
            : 0u;
    double sum = 0.0;
    if (local_bad == 0u) {
        const int start = offsets[source];
        const int end = offsets[source + 1];
        if (start < 0 || end <= start) {
            local_bad |= ASSAY_FLAG_INVALID_INDEX;
        }
        for (int idx = start + tid; idx < end && local_bad == 0u; idx += blockDim.x) {
            const double event_time = events[idx];
            if (!isfinite(event_time) || event_time < 0.0 || event_time >= observation_end) {
                local_bad |= ASSAY_FLAG_NONFINITE;
            }
            sum += 1.0 - exp(-decay * (observation_end - event_time));
        }
    }
    sums[tid] = sum;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        if (!isfinite(sums[0]) || sums[0] <= 0.0) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            return;
        }
        exposures[source] = sums[0];
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_hawkes_kernel_sums_f64(
    const double *events,
    const int *offsets,
    const int *event_process,
    int d,
    int total_events,
    double decay,
    double *kernel_sums,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];
    const int event_idx = blockIdx.x;
    const int source = blockIdx.y;
    const int tid = threadIdx.x;
    unsigned int local_bad =
        (event_idx >= total_events || source >= d || total_events <= 0 || d <= 0 ||
         d > ASSAY_HAWKES_MAX_D || !isfinite(decay) || decay <= 0.0)
            ? ASSAY_FLAG_INVALID_INDEX
            : 0u;
    double sum = 0.0;
    if (local_bad == 0u) {
        const int target = event_process[event_idx];
        if (target < 0 || target >= d || event_idx < offsets[target] || event_idx >= offsets[target + 1]) {
            local_bad |= ASSAY_FLAG_INVALID_INDEX;
        }
        const double target_time = events[event_idx];
        if (!isfinite(target_time)) {
            local_bad |= ASSAY_FLAG_NONFINITE;
        }
        const int start = offsets[source];
        const int end = offsets[source + 1];
        for (int idx = start + tid; idx < end && local_bad == 0u; idx += blockDim.x) {
            const double source_time = events[idx];
            if (!isfinite(source_time)) {
                local_bad |= ASSAY_FLAG_NONFINITE;
            }
            if (source_time >= target_time) {
                continue;
            }
            sum += decay * exp(-decay * (target_time - source_time));
        }
    }
    sums[tid] = sum;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        kernel_sums[event_idx * d + source] = sums[0];
    }
}

__device__ __forceinline__ double assay_hawkes_intensity(
    const double *kernel_sums,
    const double *baseline,
    const double *branching,
    int d,
    int target,
    int event_idx,
    unsigned int *bad) {
    double intensity = baseline[target];
    if (!isfinite(intensity)) {
        *bad |= ASSAY_FLAG_NONFINITE;
        return 0.0;
    }
    for (int source = 0; source < d; source++) {
        const double value = branching[target * d + source] * kernel_sums[event_idx * d + source];
        if (!isfinite(value)) {
            *bad |= ASSAY_FLAG_NONFINITE;
        }
        intensity += value;
    }
    if (!isfinite(intensity) || intensity <= 0.0) {
        *bad |= ASSAY_FLAG_NONFINITE;
        return 0.0;
    }
    return intensity;
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_hawkes_em_background_f64(
    const int *offsets,
    const double *kernel_sums,
    const double *baseline,
    const double *branching,
    int d,
    double *background_counts,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];
    const int target = blockIdx.x;
    const int tid = threadIdx.x;
    unsigned int local_bad =
        (target >= d || d <= 0 || d > ASSAY_HAWKES_MAX_D) ? ASSAY_FLAG_INVALID_INDEX : 0u;
    double local = 0.0;
    if (local_bad == 0u) {
        const int start = offsets[target];
        const int end = offsets[target + 1];
        if (start < 0 || end <= start) {
            local_bad |= ASSAY_FLAG_INVALID_INDEX;
        }
        for (int event_idx = start + tid; event_idx < end && local_bad == 0u; event_idx += blockDim.x) {
            unsigned int intensity_bad = 0u;
            const double intensity =
                assay_hawkes_intensity(kernel_sums, baseline, branching, d, target, event_idx, &intensity_bad);
            local_bad |= intensity_bad;
            if (intensity_bad == 0u) {
                local += baseline[target] / intensity;
            }
        }
    }
    sums[tid] = local;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        background_counts[target] = sums[0];
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_hawkes_em_triggered_f64(
    const int *offsets,
    const double *kernel_sums,
    const double *baseline,
    const double *branching,
    int d,
    double *triggered_counts,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];
    const int source = blockIdx.x;
    const int target = blockIdx.y;
    const int tid = threadIdx.x;
    unsigned int local_bad =
        (source >= d || target >= d || d <= 0 || d > ASSAY_HAWKES_MAX_D) ? ASSAY_FLAG_INVALID_INDEX
                                                                        : 0u;
    double local = 0.0;
    if (local_bad == 0u) {
        const int start = offsets[target];
        const int end = offsets[target + 1];
        if (start < 0 || end <= start) {
            local_bad |= ASSAY_FLAG_INVALID_INDEX;
        }
        for (int event_idx = start + tid; event_idx < end && local_bad == 0u; event_idx += blockDim.x) {
            unsigned int intensity_bad = 0u;
            const double intensity =
                assay_hawkes_intensity(kernel_sums, baseline, branching, d, target, event_idx, &intensity_bad);
            local_bad |= intensity_bad;
            if (intensity_bad == 0u) {
                const double contribution = branching[target * d + source] * kernel_sums[event_idx * d + source];
                if (!isfinite(contribution)) {
                    local_bad |= ASSAY_FLAG_NONFINITE;
                } else {
                    local += contribution / intensity;
                }
            }
        }
    }
    sums[tid] = local;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        triggered_counts[target * d + source] = sums[0];
    }
}

extern "C" __global__ void assay_hawkes_em_update_f64(
    const double *background_counts,
    const double *triggered_counts,
    const double *exposures,
    int d,
    double observation_end,
    double *next_baseline,
    double *next_branching,
    unsigned int *flags) {
    const int source = blockIdx.x;
    const int target = blockIdx.y;
    if (threadIdx.x != 0) {
        return;
    }
    if (source >= d || target >= d || d <= 0 || d > ASSAY_HAWKES_MAX_D || !isfinite(observation_end) ||
        observation_end <= 0.0) {
        atomicOr(flags, ASSAY_FLAG_INVALID_INDEX);
        return;
    }
    if (source == 0) {
        const double base = background_counts[target] / observation_end;
        if (!isfinite(base) || base < 0.0) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            return;
        }
        next_baseline[target] = base;
    }
    const double exposure = exposures[source];
    const double branch = triggered_counts[target * d + source] / exposure;
    if (!isfinite(exposure) || exposure <= 0.0 || !isfinite(branch) || branch < 0.0) {
        atomicOr(flags, ASSAY_FLAG_NONFINITE);
        return;
    }
    next_branching[target * d + source] = branch;
}

extern "C" __global__ void assay_hawkes_spectral_radius_f64(
    const double *branching,
    int d,
    double *spectral_radius,
    unsigned int *flags) {
    if (blockIdx.x != 0 || threadIdx.x != 0) {
        return;
    }
    if (d <= 0 || d > ASSAY_HAWKES_MAX_D) {
        atomicOr(flags, ASSAY_FLAG_INVALID_INDEX);
        return;
    }
    double vector[ASSAY_HAWKES_MAX_D];
    double next[ASSAY_HAWKES_MAX_D];
    for (int i = 0; i < d; i++) {
        vector[i] = 1.0 / (double)d;
    }
    double eigenvalue = 0.0;
    for (int iter = 0; iter < 100; iter++) {
        for (int row = 0; row < d; row++) {
            double sum = 0.0;
            for (int col = 0; col < d; col++) {
                const double value = branching[row * d + col];
                if (!isfinite(value)) {
                    atomicOr(flags, ASSAY_FLAG_NONFINITE);
                    return;
                }
                sum += value * vector[col];
            }
            next[row] = sum;
        }
        double norm = 0.0;
        for (int row = 0; row < d; row++) {
            norm += next[row];
        }
        if (!isfinite(norm) || norm <= 1.0e-15) {
            spectral_radius[0] = 0.0;
            return;
        }
        for (int row = 0; row < d; row++) {
            vector[row] = next[row] / norm;
        }
        eigenvalue = norm;
    }
    if (!isfinite(eigenvalue) || eigenvalue < 0.0) {
        atomicOr(flags, ASSAY_FLAG_NONFINITE);
        return;
    }
    spectral_radius[0] = eigenvalue;
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_corr_matrix_f32(
    const float *columns,
    int n,
    int d,
    double *corr,
    unsigned int *flags) {
    __shared__ double sums_x[ASSAY_THREADS];
    __shared__ double sums_y[ASSAY_THREADS];
    __shared__ double sums_xx[ASSAY_THREADS];
    __shared__ double sums_yy[ASSAY_THREADS];
    __shared__ double sums_xy[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int pair = blockIdx.x;
    const int tid = threadIdx.x;
    const int i = d > 0 ? pair / d : -1;
    const int j = d > 0 ? pair - i * d : -1;
    unsigned int local_bad =
        (n <= 0 || d <= 0 || d > ASSAY_LINALG_MAX_D || i < 0 || j < 0 || i >= d || j >= d)
            ? ASSAY_FLAG_INVALID_INDEX
            : 0u;

    double sx = 0.0;
    double sy = 0.0;
    double sxx = 0.0;
    double syy = 0.0;
    double sxy = 0.0;
    if (local_bad == 0u) {
        for (int row = tid; row < n; row += blockDim.x) {
            const float xf = columns[i * n + row];
            const float yf = columns[j * n + row];
            if (!(isfinite(xf) && isfinite(yf))) {
                local_bad |= ASSAY_FLAG_NONFINITE;
            }
            const double x = (double)xf;
            const double y = (double)yf;
            sx += x;
            sy += y;
            sxx += x * x;
            syy += y * y;
            sxy += x * y;
        }
    }
    sums_x[tid] = sx;
    sums_y[tid] = sy;
    sums_xx[tid] = sxx;
    sums_yy[tid] = syy;
    sums_xy[tid] = sxy;
    bad[tid] = local_bad;
    __syncthreads();
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            sums_x[tid] += sums_x[tid + stride];
            sums_y[tid] += sums_y[tid + stride];
            sums_xx[tid] += sums_xx[tid + stride];
            sums_yy[tid] += sums_yy[tid + stride];
            sums_xy[tid] += sums_xy[tid + stride];
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        if (i == j) {
            corr[pair] = 1.0;
            return;
        }
        const double nf = (double)n;
        const double vx = sums_xx[0] - (sums_x[0] * sums_x[0]) / nf;
        const double vy = sums_yy[0] - (sums_y[0] * sums_y[0]) / nf;
        const double cov = sums_xy[0] - (sums_x[0] * sums_y[0]) / nf;
        const double denom = sqrt(vx * vy);
        if (!(isfinite(vx) && isfinite(vy) && isfinite(cov) && isfinite(denom)) || vx <= 0.0 ||
            vy <= 0.0 || denom <= 0.0) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            return;
        }
        const double r = fmin(fmax(cov / denom, -1.0), 1.0);
        if (!isfinite(r)) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            return;
        }
        corr[pair] = r;
    }
}

extern "C" __global__ void assay_invert_symmetric_f64(
    const double *matrix,
    int d,
    double *scratch,
    double *inverse,
    unsigned int *flags) {
    if (blockIdx.x != 0 || threadIdx.x != 0) {
        return;
    }
    if (d <= 0 || d > ASSAY_LINALG_MAX_D) {
        atomicOr(flags, ASSAY_FLAG_INVALID_INDEX);
        return;
    }
    const int width = 2 * d;
    for (int row = 0; row < d; row++) {
        for (int col = 0; col < d; col++) {
            const double value = matrix[row * d + col];
            if (!isfinite(value)) {
                atomicOr(flags, ASSAY_FLAG_NONFINITE);
                return;
            }
            scratch[row * width + col] = value;
            scratch[row * width + d + col] = row == col ? 1.0 : 0.0;
        }
    }
    for (int col = 0; col < d; col++) {
        int pivot = col;
        double best = fabs(scratch[col * width + col]);
        for (int row = col + 1; row < d; row++) {
            const double candidate = fabs(scratch[row * width + col]);
            if (candidate > best) {
                best = candidate;
                pivot = row;
            }
        }
        if (!(isfinite(best)) || best < 1.0e-12) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            return;
        }
        if (pivot != col) {
            for (int idx = 0; idx < width; idx++) {
                const double tmp = scratch[col * width + idx];
                scratch[col * width + idx] = scratch[pivot * width + idx];
                scratch[pivot * width + idx] = tmp;
            }
        }
        const double pivot_value = scratch[col * width + col];
        if (!isfinite(pivot_value) || pivot_value == 0.0) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            return;
        }
        const double inv_pivot = 1.0 / pivot_value;
        for (int idx = 0; idx < width; idx++) {
            scratch[col * width + idx] *= inv_pivot;
        }
        for (int row = 0; row < d; row++) {
            if (row == col) {
                continue;
            }
            const double factor = scratch[row * width + col];
            if (factor == 0.0) {
                continue;
            }
            for (int idx = 0; idx < width; idx++) {
                scratch[row * width + idx] -= factor * scratch[col * width + idx];
            }
        }
    }
    for (int row = 0; row < d; row++) {
        for (int col = 0; col < d; col++) {
            const double value = scratch[row * width + d + col];
            if (!isfinite(value)) {
                atomicOr(flags, ASSAY_FLAG_NONFINITE);
                return;
            }
            inverse[row * d + col] = value;
        }
    }
}

__device__ __forceinline__ double granger_value_y(const float *y, int target, int col, int p) {
    return col == 0 ? 1.0 : (double)y[target - col];
}

__device__ __forceinline__ double granger_value_u(const float *x, const float *y, int target, int col, int p) {
    if (col == 0) {
        return 1.0;
    }
    if (col <= p) {
        return (double)y[target - col];
    }
    return (double)x[target - (col - p)];
}

__device__ bool assay_solve_gauss_jordan(double *a, double *rhs, int k, int row_stride) {
    double scale = 0.0;
    for (int i = 0; i < k; i++) {
        const double diag = fabs(a[i * row_stride + i]);
        if (diag > scale) {
            scale = diag;
        }
    }
    const double eps = 1.0e-12 * fmax(scale, 1.0);
    for (int col = 0; col < k; col++) {
        int pivot = col;
        double best = fabs(a[col * row_stride + col]);
        for (int row = col + 1; row < k; row++) {
            const double candidate = fabs(a[row * row_stride + col]);
            if (candidate > best) {
                best = candidate;
                pivot = row;
            }
        }
        if (!(isfinite(best)) || best < eps) {
            return false;
        }
        if (pivot != col) {
            for (int j = 0; j < k; j++) {
                const double tmp = a[col * row_stride + j];
                a[col * row_stride + j] = a[pivot * row_stride + j];
                a[pivot * row_stride + j] = tmp;
            }
            const double rhs_tmp = rhs[col];
            rhs[col] = rhs[pivot];
            rhs[pivot] = rhs_tmp;
        }
        const double pivot_value = a[col * row_stride + col];
        if (!isfinite(pivot_value) || pivot_value == 0.0) {
            return false;
        }
        const double inv_pivot = 1.0 / pivot_value;
        for (int j = 0; j < k; j++) {
            a[col * row_stride + j] *= inv_pivot;
        }
        rhs[col] *= inv_pivot;
        for (int row = 0; row < k; row++) {
            if (row == col) {
                continue;
            }
            const double factor = a[row * row_stride + col];
            if (factor == 0.0) {
                continue;
            }
            for (int j = 0; j < k; j++) {
                a[row * row_stride + j] -= factor * a[col * row_stride + j];
            }
            rhs[row] -= factor * rhs[col];
        }
    }
    return true;
}

extern "C" __global__ void assay_granger_lag_summaries_f32(
    const float *x,
    const float *y,
    const int *lags,
    int lag_count,
    int n,
    int workspace_row_stride,
    double *ar_workspace,
    double *au_workspace,
    double *br_workspace,
    double *bu_workspace,
    double *rss_restricted,
    double *rss_unrestricted,
    int *n_used,
    int *df_den,
    int *status,
    unsigned int *flags) {
    const int lag_idx = blockIdx.x;
    if (threadIdx.x != 0) {
        return;
    }
    if (lag_idx >= lag_count || lag_count <= 0 || n <= 0) {
        atomicOr(flags, ASSAY_FLAG_INVALID_INDEX);
        return;
    }
    const int p = lags[lag_idx];
    status[lag_idx] = 0;
    rss_restricted[lag_idx] = 0.0;
    rss_unrestricted[lag_idx] = 0.0;
    n_used[lag_idx] = 0;
    df_den[lag_idx] = 0;
    if (p <= 0 || p > ASSAY_GRANGER_MAX_LAGS || n < 3 * p + 2) {
        status[lag_idx] = 1;
        return;
    }
    for (int row = 0; row < n; row++) {
        if (!(isfinite(x[row]) && isfinite(y[row]))) {
            status[lag_idx] = 2;
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            return;
        }
    }

    const int t_rows = n - p;
    const int kr = 1 + p;
    const int ku = 1 + 2 * p;
    if (workspace_row_stride <= 0 || workspace_row_stride > ASSAY_GRANGER_MAX_K ||
        ku > workspace_row_stride) {
        status[lag_idx] = 1;
        atomicOr(flags, ASSAY_FLAG_INVALID_INDEX);
        return;
    }
    const size_t matrix_cells = (size_t)workspace_row_stride * (size_t)workspace_row_stride;
    const size_t matrix_offset = (size_t)lag_idx * matrix_cells;
    const size_t vector_offset = (size_t)lag_idx * (size_t)workspace_row_stride;
    double *ar = ar_workspace + matrix_offset;
    double *au = au_workspace + matrix_offset;
    double *br = br_workspace + vector_offset;
    double *bu = bu_workspace + vector_offset;
    for (size_t idx = 0; idx < matrix_cells; idx++) {
        ar[idx] = 0.0;
        au[idx] = 0.0;
    }
    for (int idx = 0; idx < workspace_row_stride; idx++) {
        br[idx] = 0.0;
        bu[idx] = 0.0;
    }

    for (int target = p; target < n; target++) {
        const double yi = (double)y[target];
        for (int c = 0; c < kr; c++) {
            const double vc = granger_value_y(y, target, c, p);
            br[c] += vc * yi;
            for (int dcol = c; dcol < kr; dcol++) {
                ar[c * workspace_row_stride + dcol] += vc * granger_value_y(y, target, dcol, p);
            }
        }
        for (int c = 0; c < ku; c++) {
            const double vc = granger_value_u(x, y, target, c, p);
            bu[c] += vc * yi;
            for (int dcol = c; dcol < ku; dcol++) {
                au[c * workspace_row_stride + dcol] += vc * granger_value_u(x, y, target, dcol, p);
            }
        }
    }
    for (int c = 0; c < kr; c++) {
        for (int dcol = c + 1; dcol < kr; dcol++) {
            ar[dcol * workspace_row_stride + c] = ar[c * workspace_row_stride + dcol];
        }
    }
    for (int c = 0; c < ku; c++) {
        for (int dcol = c + 1; dcol < ku; dcol++) {
            au[dcol * workspace_row_stride + c] = au[c * workspace_row_stride + dcol];
        }
    }
    if (!assay_solve_gauss_jordan(ar, br, kr, workspace_row_stride) ||
        !assay_solve_gauss_jordan(au, bu, ku, workspace_row_stride)) {
        status[lag_idx] = 3;
        return;
    }

    double rss_r = 0.0;
    double rss_u = 0.0;
    for (int target = p; target < n; target++) {
        const double yi = (double)y[target];
        double fit_r = 0.0;
        for (int c = 0; c < kr; c++) {
            fit_r += granger_value_y(y, target, c, p) * br[c];
        }
        double fit_u = 0.0;
        for (int c = 0; c < ku; c++) {
            fit_u += granger_value_u(x, y, target, c, p) * bu[c];
        }
        rss_r += (yi - fit_r) * (yi - fit_r);
        rss_u += (yi - fit_u) * (yi - fit_u);
    }
    if (!(isfinite(rss_r) && isfinite(rss_u))) {
        status[lag_idx] = 2;
        return;
    }
    rss_restricted[lag_idx] = rss_r;
    rss_unrestricted[lag_idx] = rss_u;
    n_used[lag_idx] = t_rows;
    df_den[lag_idx] = t_rows - ku;
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_logistic_summaries_f32(
    const float *samples,
    const int *labels,
    const int *block_value_offsets,
    const int *block_feature_offsets,
    const int *block_dims,
    const float *block_scales,
    int block_count,
    const int *train_offsets,
    const int *train_indices,
    const int *test_offsets,
    const int *test_indices,
    int fit_count,
    int n,
    int dim,
    int max_iterations,
    int convergence_check_interval,
    float relative_parameter_tolerance,
    float lr,
    float l2,
    float *weights_workspace,
    float *gradient_workspace,
    float *bits,
    float *accuracy,
    int *iterations,
    float *final_relative_parameter_changes,
    int *converged,
    unsigned int *flags) {
    __shared__ float sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];
    __shared__ float shared_error;
    __shared__ float shared_max_parameter_change;
    __shared__ float shared_max_parameter_magnitude;
    __shared__ int shared_converged;

    const int fit = blockIdx.x;
    const int tid = threadIdx.x;
    unsigned int local_bad =
        (fit >= fit_count || fit_count <= 0 || n <= 0 || dim <= 0 || block_count <= 0 ||
         max_iterations <= 0 || convergence_check_interval <= 0 ||
         convergence_check_interval > max_iterations ||
         max_iterations % convergence_check_interval != 0 ||
         !(isfinite(relative_parameter_tolerance) && relative_parameter_tolerance > 0.0f) ||
         !(isfinite(lr) && isfinite(l2)) || !(lr > 0.0f) || l2 < 0.0f)
            ? ASSAY_FLAG_INVALID_INDEX
            : 0u;
    float *weights = weights_workspace + (size_t)fit * (size_t)dim;
    float *gradient = gradient_workspace + (size_t)fit * (size_t)dim;

    for (int d = tid; d < dim; d += blockDim.x) {
        weights[d] = 0.0f;
        gradient[d] = 0.0f;
    }
    __shared__ float bias;
    if (tid == 0) {
        bias = 0.0f;
        shared_max_parameter_change = INFINITY;
        shared_max_parameter_magnitude = 0.0f;
        shared_converged = 0;
        iterations[fit] = 0;
        final_relative_parameter_changes[fit] = INFINITY;
        converged[fit] = 0;
    }
    __syncthreads();

    if (local_bad == 0u) {
        const int train_start = train_offsets[fit];
        const int train_end = train_offsets[fit + 1];
        const int test_start = test_offsets[fit];
        const int test_end = test_offsets[fit + 1];
        if (train_start < 0 || train_end <= train_start || test_start < 0 || test_end <= test_start) {
            local_bad |= ASSAY_FLAG_INVALID_INDEX;
        }
        const int train_n = train_end - train_start;

        for (int iteration = 1;
             iteration <= max_iterations && local_bad == 0u && shared_converged == 0;
             iteration++) {
            for (int d = tid; d < dim; d += blockDim.x) {
                gradient[d] = 0.0f;
            }
            float bias_grad = 0.0f;
            __syncthreads();

            for (int pos = train_start; pos < train_end; pos++) {
                const int row = train_indices[pos];
                if (row < 0 || row >= n) {
                    local_bad |= ASSAY_FLAG_INVALID_INDEX;
                }
                float partial = 0.0f;
                if (local_bad == 0u) {
                    for (int block = 0; block < block_count; block++) {
                        const int block_dim = block_dims[block];
                        const int value_base = block_value_offsets[block] + row * block_dim;
                        const int feature_base = block_feature_offsets[block];
                        const float scale = block_scales[fit * block_count + block];
                        if (block_dim <= 0 || !(isfinite(scale) && scale > 0.0f)) {
                            local_bad |= ASSAY_FLAG_NONFINITE;
                        }
                        for (int col = tid; col < block_dim; col += blockDim.x) {
                            const float value = samples[value_base + col] / scale;
                            const float weight = weights[feature_base + col];
                            if (!(isfinite(value) && isfinite(weight))) {
                                local_bad |= ASSAY_FLAG_NONFINITE;
                            }
                            partial += value * weight;
                        }
                    }
                }
                sums[tid] = partial;
                bad[tid] = local_bad;
                __syncthreads();
                reduce_float_sum(sums, bad, tid);
                if (tid == 0) {
                    const float logit = sums[0] + bias;
                    const int label = (row >= 0 && row < n) ? labels[row] : -1;
                    if (label != 0 && label != 1) {
                        bad[0] |= ASSAY_FLAG_INVALID_INDEX;
                    }
                    shared_error = assay_sigmoid_f32(logit) - (label != 0 ? 1.0f : 0.0f);
                    if (!isfinite(shared_error)) {
                        bad[0] |= ASSAY_FLAG_NONFINITE;
                    }
                }
                __syncthreads();
                local_bad |= bad[0];
                if (local_bad == 0u) {
                    for (int block = 0; block < block_count; block++) {
                        const int block_dim = block_dims[block];
                        const int value_base = block_value_offsets[block] + row * block_dim;
                        const int feature_base = block_feature_offsets[block];
                        const float scale = block_scales[fit * block_count + block];
                        for (int col = tid; col < block_dim; col += blockDim.x) {
                            gradient[feature_base + col] +=
                                shared_error * (samples[value_base + col] / scale);
                        }
                    }
                    if (tid == 0) {
                        bias_grad += shared_error;
                    }
                }
                __syncthreads();
            }

            if (local_bad == 0u) {
                const float inv_n = 1.0f / (float)train_n;
                float local_max_parameter_change = 0.0f;
                float local_max_parameter_magnitude = 0.0f;
                for (int d = tid; d < dim; d += blockDim.x) {
                    const float regularized_gradient = gradient[d] * inv_n + l2 * weights[d];
                    gradient[d] = regularized_gradient;
                    if (!isfinite(regularized_gradient)) {
                        local_bad |= ASSAY_FLAG_NONFINITE;
                    }
                    const float parameter_change = lr * regularized_gradient;
                    weights[d] -= parameter_change;
                    if (!isfinite(weights[d])) {
                        local_bad |= ASSAY_FLAG_NONFINITE;
                    }
                    local_max_parameter_change =
                        fmaxf(local_max_parameter_change, fabsf(parameter_change));
                    local_max_parameter_magnitude =
                        fmaxf(local_max_parameter_magnitude, fabsf(weights[d]));
                }
                if (tid == 0) {
                    const float normalized_bias_gradient = bias_grad * inv_n;
                    if (!isfinite(normalized_bias_gradient)) {
                        local_bad |= ASSAY_FLAG_NONFINITE;
                    }
                    const float bias_change = lr * normalized_bias_gradient;
                    bias -= bias_change;
                    if (!isfinite(bias)) {
                        local_bad |= ASSAY_FLAG_NONFINITE;
                    }
                    local_max_parameter_change =
                        fmaxf(local_max_parameter_change, fabsf(bias_change));
                    local_max_parameter_magnitude =
                        fmaxf(local_max_parameter_magnitude, fabsf(bias));
                }
                sums[tid] = local_max_parameter_change;
                bad[tid] = local_bad;
                __syncthreads();
                reduce_float_max(sums, bad, tid);
                if (tid == 0) {
                    shared_max_parameter_change = sums[0];
                }
                __syncthreads();
                local_bad |= bad[0];
                sums[tid] = local_max_parameter_magnitude;
                bad[tid] = local_bad;
                __syncthreads();
                reduce_float_max(sums, bad, tid);
                if (tid == 0) {
                    shared_max_parameter_magnitude = sums[0];
                    const float relative_change =
                        shared_max_parameter_magnitude > 0.0f
                            ? shared_max_parameter_change / shared_max_parameter_magnitude
                            : (shared_max_parameter_change == 0.0f ? 0.0f : INFINITY);
                    iterations[fit] = iteration;
                    final_relative_parameter_changes[fit] = relative_change;
                    if (!isfinite(relative_change)) {
                        bad[0] |= ASSAY_FLAG_NONFINITE;
                    } else {
                        const bool checkpoint =
                            iteration % convergence_check_interval == 0 ||
                            iteration == max_iterations;
                        if (checkpoint &&
                            relative_change <= relative_parameter_tolerance) {
                            shared_converged = 1;
                            converged[fit] = 1;
                        }
                    }
                }
                __syncthreads();
                local_bad |= bad[0];
            }
            sums[tid] = 0.0f;
            bad[tid] = local_bad;
            __syncthreads();
            reduce_float_sum(sums, bad, tid);
            local_bad |= bad[0];
            __syncthreads();
        }

        int correct = 0;
        int joint00 = 0;
        int joint01 = 0;
        int joint10 = 0;
        int joint11 = 0;
        const int test_n = test_end - test_start;
        for (int pos = test_start;
             pos < test_end && local_bad == 0u && shared_converged != 0;
             pos++) {
            const int row = test_indices[pos];
            if (row < 0 || row >= n) {
                local_bad |= ASSAY_FLAG_INVALID_INDEX;
            }
            float partial = 0.0f;
            if (local_bad == 0u) {
                for (int block = 0; block < block_count; block++) {
                    const int block_dim = block_dims[block];
                    const int value_base = block_value_offsets[block] + row * block_dim;
                    const int feature_base = block_feature_offsets[block];
                    const float scale = block_scales[fit * block_count + block];
                    if (block_dim <= 0 || !(isfinite(scale) && scale > 0.0f)) {
                        local_bad |= ASSAY_FLAG_NONFINITE;
                    }
                    for (int col = tid; col < block_dim; col += blockDim.x) {
                        const float value = samples[value_base + col] / scale;
                        const float weight = weights[feature_base + col];
                        if (!(isfinite(value) && isfinite(weight))) {
                            local_bad |= ASSAY_FLAG_NONFINITE;
                        }
                        partial += value * weight;
                    }
                }
            }
            sums[tid] = partial;
            bad[tid] = local_bad;
            __syncthreads();
            reduce_float_sum(sums, bad, tid);
            if (tid == 0) {
                const int label = (row >= 0 && row < n) ? labels[row] : -1;
                if (label != 0 && label != 1) {
                    bad[0] |= ASSAY_FLAG_INVALID_INDEX;
                } else {
                    const bool prediction = assay_sigmoid_f32(sums[0] + bias) >= 0.5f;
                    const bool truth = label != 0;
                    correct += prediction == truth ? 1 : 0;
                    if (!truth && !prediction) {
                        joint00++;
                    } else if (!truth && prediction) {
                        joint01++;
                    } else if (truth && !prediction) {
                        joint10++;
                    } else {
                        joint11++;
                    }
                }
            }
            __syncthreads();
            local_bad |= bad[0];
        }
        if (tid == 0) {
            const double nf = (double)test_n;
            const double j00 = (double)joint00 / nf;
            const double j01 = (double)joint01 / nf;
            const double j10 = (double)joint10 / nf;
            const double j11 = (double)joint11 / nf;
            const double py0 = j00 + j01;
            const double py1 = j10 + j11;
            const double pp0 = j00 + j10;
            const double pp1 = j01 + j11;
            double mi = 0.0;
            if (j00 > 0.0 && py0 > 0.0 && pp0 > 0.0) mi += j00 * (log(j00 / (py0 * pp0)) / log(2.0));
            if (j01 > 0.0 && py0 > 0.0 && pp1 > 0.0) mi += j01 * (log(j01 / (py0 * pp1)) / log(2.0));
            if (j10 > 0.0 && py1 > 0.0 && pp0 > 0.0) mi += j10 * (log(j10 / (py1 * pp0)) / log(2.0));
            if (j11 > 0.0 && py1 > 0.0 && pp1 > 0.0) mi += j11 * (log(j11 / (py1 * pp1)) / log(2.0));
            if (!isfinite(mi)) {
                bad[0] |= ASSAY_FLAG_NONFINITE;
            }
            bits[fit] = (float)fmax(mi, 0.0);
            accuracy[fit] = (float)correct / (float)test_n;
            if (!(isfinite(bits[fit]) && isfinite(accuracy[fit]))) {
                bad[0] |= ASSAY_FLAG_NONFINITE;
            }
            if (bad[0] != 0u) {
                atomicOr(flags, bad[0]);
            }
        }
    } else if (tid == 0) {
        atomicOr(flags, local_bad);
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_linear_cka_energy_f32(
    const float *values,
    const int *lens_offsets,
    const int *dimensions,
    int lens_count,
    int row_count,
    double *inverse_energy,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];
    const int lens = blockIdx.x;
    const int tid = threadIdx.x;
    unsigned int local_bad =
        (lens >= lens_count || lens_count <= 0 || row_count <= 0) ? ASSAY_FLAG_INVALID_INDEX : 0u;
    double local = 0.0;
    if (local_bad == 0u) {
        const int dim = dimensions[lens];
        const int offset = lens_offsets[lens];
        const int end = lens_offsets[lens + 1];
        if (dim <= 0 || offset < 0 || end <= offset || end - offset != row_count * dim) {
            local_bad |= ASSAY_FLAG_INVALID_INDEX;
        } else {
            for (int col = tid; col < dim; col += blockDim.x) {
                double sum = 0.0;
                double sum_sq = 0.0;
                for (int row = 0; row < row_count; row++) {
                    const float value = values[offset + row * dim + col];
                    if (!isfinite(value)) {
                        local_bad |= ASSAY_FLAG_NONFINITE;
                    }
                    const double x = (double)value;
                    sum += x;
                    sum_sq += x * x;
                }
                local += sum_sq - (sum * sum) / (double)row_count;
            }
        }
    }
    sums[tid] = local;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        const double energy = sums[0];
        if (!isfinite(energy) || energy <= 0.0) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            return;
        }
        inverse_energy[lens] = 1.0 / energy;
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_linear_cka_sketch_f32(
    const float *values,
    const int *lens_offsets,
    const int *dimensions,
    const int *tuples,
    const double *inverse_energy,
    int lens_count,
    int row_count,
    int tuple_count,
    double *sketch,
    unsigned int *flags) {
    __shared__ double sums_r[ASSAY_THREADS];
    __shared__ double sums_s[ASSAY_THREADS];
    __shared__ double dummy[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];
    const int linear_block = blockIdx.x;
    const int lens = tuple_count > 0 ? linear_block / tuple_count : -1;
    const int tuple_idx = tuple_count > 0 ? linear_block - lens * tuple_count : -1;
    const int tid = threadIdx.x;
    unsigned int local_bad =
        (lens >= lens_count || tuple_idx >= tuple_count || lens_count <= 0 || row_count <= 0 ||
         tuple_count <= 0)
            ? ASSAY_FLAG_INVALID_INDEX
            : 0u;
    double r = 0.0;
    double s = 0.0;
    if (local_bad == 0u) {
        const int dim = dimensions[lens];
        const int offset = lens_offsets[lens];
        const int end = lens_offsets[lens + 1];
        const int tuple_base = tuple_idx * 4;
        const int row0 = tuples[tuple_base + 0];
        const int row1 = tuples[tuple_base + 1];
        const int row2 = tuples[tuple_base + 2];
        const int row3 = tuples[tuple_base + 3];
        if (dim <= 0 || offset < 0 || end <= offset || end - offset != row_count * dim ||
            row0 < 0 || row1 < 0 || row2 < 0 || row3 < 0 || row0 >= row_count ||
            row1 >= row_count || row2 >= row_count || row3 >= row_count || !(row0 < row1) ||
            !(row1 < row2) || !(row2 < row3) || !isfinite(inverse_energy[lens])) {
            local_bad |= ASSAY_FLAG_INVALID_INDEX;
        } else {
            const int base0 = offset + row0 * dim;
            const int base1 = offset + row1 * dim;
            const int base2 = offset + row2 * dim;
            const int base3 = offset + row3 * dim;
            for (int col = tid; col < dim; col += blockDim.x) {
                const float af = values[base0 + col];
                const float bf = values[base1 + col];
                const float cf = values[base2 + col];
                const float df = values[base3 + col];
                if (!(isfinite(af) && isfinite(bf) && isfinite(cf) && isfinite(df))) {
                    local_bad |= ASSAY_FLAG_NONFINITE;
                }
                const double a = (double)af;
                const double b = (double)bf;
                const double c = (double)cf;
                const double d = (double)df;
                r += (a - d) * (b - c);
                s += (a - c) * (b - d);
            }
        }
    }
    sums_r[tid] = r;
    sums_s[tid] = s;
    dummy[tid] = 0.0;
    bad[tid] = local_bad;
    __syncthreads();
    reduce3(sums_r, sums_s, dummy, bad, tid);
    if (tid == 0) {
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
            return;
        }
        const double inv = inverse_energy[lens];
        const double rr = sums_r[0] * inv;
        const double ss = sums_s[0] * inv;
        const double z0 = (rr + ss) / 6.0;
        const double z1 = (-2.0 * rr + ss) / 6.0;
        const double z2 = (rr - 2.0 * ss) / 6.0;
        if (!(isfinite(z0) && isfinite(z1) && isfinite(z2))) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
            return;
        }
        const int out = (lens * tuple_count + tuple_idx) * 3;
        sketch[out + 0] = z0;
        sketch[out + 1] = z1;
        sketch[out + 2] = z2;
    }
}

__device__ __forceinline__ void assay_pair_from_index(int pair_idx, int lens_count, int *left, int *right) {
    int remaining = pair_idx;
    for (int a = 0; a < lens_count - 1; a++) {
        const int count = lens_count - a - 1;
        if (remaining < count) {
            *left = a;
            *right = a + 1 + remaining;
            return;
        }
        remaining -= count;
    }
    *left = -1;
    *right = -1;
}

__device__ __forceinline__ double assay_checked_cosine(double cross, double self_a, double self_b, unsigned int *bad) {
    if (!(isfinite(cross) && isfinite(self_a) && isfinite(self_b)) || self_a <= 0.0 || self_b <= 0.0) {
        *bad |= ASSAY_FLAG_NONFINITE;
        return 0.0;
    }
    const double value = cross / sqrt(self_a * self_b);
    if (!isfinite(value)) {
        *bad |= ASSAY_FLAG_NONFINITE;
        return 0.0;
    }
    return fmin(fmax(value, -1.0), 1.0);
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_linear_cka_pairs_f32(
    const double *sketch,
    int lens_count,
    int tuple_count,
    int exact,
    float *raw_signed,
    float *redundancy,
    float *standard_error,
    float *gate_upper,
    unsigned int *flags) {
    __shared__ double sums_cross[ASSAY_THREADS];
    __shared__ double sums_a[ASSAY_THREADS];
    __shared__ double sums_b[ASSAY_THREADS];
    __shared__ double block_cross[32];
    __shared__ double block_a[32];
    __shared__ double block_b[32];
    __shared__ unsigned int bad[ASSAY_THREADS];
    const int pair_idx = blockIdx.x;
    const int tid = threadIdx.x;
    const int pair_count = lens_count * (lens_count - 1) / 2;
    unsigned int local_bad =
        (pair_idx >= pair_count || lens_count < 2 || tuple_count <= 0) ? ASSAY_FLAG_INVALID_INDEX
                                                                       : 0u;
    if (tid < 32) {
        block_cross[tid] = 0.0;
        block_a[tid] = 0.0;
        block_b[tid] = 0.0;
    }
    __syncthreads();
    int left = -1;
    int right = -1;
    assay_pair_from_index(pair_idx, lens_count, &left, &right);
    if (left < 0 || right <= left || right >= lens_count) {
        local_bad |= ASSAY_FLAG_INVALID_INDEX;
    }

    for (int block = 0; block < 32; block++) {
        const int start = (int)(((long long)block * (long long)tuple_count) / 32ll);
        const int end = (int)(((long long)(block + 1) * (long long)tuple_count) / 32ll);
        double cross = 0.0;
        double self_a = 0.0;
        double self_b = 0.0;
        if (local_bad == 0u) {
            for (int tuple_idx = start + tid; tuple_idx < end; tuple_idx += blockDim.x) {
                const int lo = (left * tuple_count + tuple_idx) * 3;
                const int ro = (right * tuple_count + tuple_idx) * 3;
                const double a0 = sketch[lo + 0];
                const double a1 = sketch[lo + 1];
                const double a2 = sketch[lo + 2];
                const double b0 = sketch[ro + 0];
                const double b1 = sketch[ro + 1];
                const double b2 = sketch[ro + 2];
                if (!(isfinite(a0) && isfinite(a1) && isfinite(a2) && isfinite(b0) &&
                      isfinite(b1) && isfinite(b2))) {
                    local_bad |= ASSAY_FLAG_NONFINITE;
                }
                cross += a0 * b0 + a1 * b1 + a2 * b2;
                self_a += a0 * a0 + a1 * a1 + a2 * a2;
                self_b += b0 * b0 + b1 * b1 + b2 * b2;
            }
        }
        sums_cross[tid] = cross;
        sums_a[tid] = self_a;
        sums_b[tid] = self_b;
        bad[tid] = local_bad;
        __syncthreads();
        reduce3(sums_cross, sums_a, sums_b, bad, tid);
        if (tid == 0) {
            block_cross[block] = sums_cross[0];
            block_a[block] = sums_a[0];
            block_b[block] = sums_b[0];
        }
        __syncthreads();
        local_bad |= bad[0];
    }

    if (tid == 0) {
        if (local_bad != 0u) {
            atomicOr(flags, local_bad);
            return;
        }
        double total_cross = 0.0;
        double total_a = 0.0;
        double total_b = 0.0;
        for (int block = 0; block < 32; block++) {
            total_cross += block_cross[block];
            total_a += block_a[block];
            total_b += block_b[block];
        }
        unsigned int out_bad = 0u;
        const double raw = assay_checked_cosine(total_cross, total_a, total_b, &out_bad);
        const double point = fmax(raw, 0.0);
        double se = 0.0;
        if (exact == 0) {
            double leave_out[32];
            double mean = 0.0;
            for (int block = 0; block < 32; block++) {
                leave_out[block] = assay_checked_cosine(
                    total_cross - block_cross[block],
                    total_a - block_a[block],
                    total_b - block_b[block],
                    &out_bad);
                mean += leave_out[block];
            }
            mean /= 32.0;
            double squared = 0.0;
            for (int block = 0; block < 32; block++) {
                const double delta = leave_out[block] - mean;
                squared += delta * delta;
            }
            const double variance = (31.0 / 32.0) * squared;
            if (!isfinite(variance) || variance < 0.0) {
                out_bad |= ASSAY_FLAG_NONFINITE;
            } else {
                se = sqrt(variance);
            }
        }
        const double gate = fmin(point + 4.0 * se, 1.0);
        if (out_bad != 0u || !(isfinite(raw) && isfinite(point) && isfinite(se) && isfinite(gate))) {
            atomicOr(flags, out_bad | ASSAY_FLAG_NONFINITE);
            return;
        }
        raw_signed[pair_idx] = (float)raw;
        redundancy[pair_idx] = (float)point;
        standard_error[pair_idx] = (float)se;
        gate_upper[pair_idx] = (float)gate;
    }
}

__device__ __forceinline__ float chebyshev_f32(const float *values, int left, int right, int dim, unsigned int *bad) {
    float max_abs = 0.0f;
    const int left_base = left * dim;
    const int right_base = right * dim;
    for (int d = 0; d < dim; d++) {
        const float a = values[left_base + d];
        const float b = values[right_base + d];
        if (!isfinite(a) || !isfinite(b)) {
            *bad |= ASSAY_FLAG_NONFINITE;
        }
        const float diff = fabsf(a - b);
        max_abs = fmaxf(max_abs, diff);
    }
    return max_abs;
}

__device__ __forceinline__ float joint_chebyshev_f32(
    const float *x,
    const float *y,
    int left,
    int right,
    int dim_x,
    int dim_y,
    unsigned int *bad) {
    return fmaxf(
        chebyshev_f32(x, left, right, dim_x, bad),
        chebyshev_f32(y, left, right, dim_y, bad));
}

__device__ __forceinline__ float euclidean_f32(const float *values, int left, int right, int dim, unsigned int *bad) {
    double sum = 0.0;
    const int left_base = left * dim;
    const int right_base = right * dim;
    for (int d = 0; d < dim; d++) {
        const float a = values[left_base + d];
        const float b = values[right_base + d];
        if (!isfinite(a) || !isfinite(b)) {
            *bad |= ASSAY_FLAG_NONFINITE;
        }
        const double diff = (double)a - (double)b;
        sum += diff * diff;
    }
    const double dist = sqrt(sum);
    if (!isfinite(dist)) {
        *bad |= ASSAY_FLAG_NONFINITE;
    }
    return (float)dist;
}

__device__ __forceinline__ bool better_neighbor(float dist, int idx, float current, int current_idx) {
    return dist < current || (dist == current && idx < current_idx);
}

__device__ __forceinline__ void insert_smallest(float dist, int idx, int k, float *best, int *best_idx) {
    if (k <= 0 || k > ASSAY_MAX_K || !isfinite(dist)) {
        return;
    }
    if (!better_neighbor(dist, idx, best[k - 1], best_idx[k - 1])) {
        return;
    }
    int pos = k - 1;
    while (pos > 0 && better_neighbor(dist, idx, best[pos - 1], best_idx[pos - 1])) {
        best[pos] = best[pos - 1];
        best_idx[pos] = best_idx[pos - 1];
        pos--;
    }
    best[pos] = dist;
    best_idx[pos] = idx;
}

__device__ __forceinline__ void insert_smallest_value(float dist, int k, float *best) {
    if (k <= 0 || k > ASSAY_MAX_K || !isfinite(dist) || dist >= best[k - 1]) {
        return;
    }
    int pos = k - 1;
    while (pos > 0 && dist < best[pos - 1]) {
        best[pos] = best[pos - 1];
        pos--;
    }
    best[pos] = dist;
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_ksg_continuous_counts_f32(
    const float *x,
    const float *y,
    int n,
    int dim_x,
    int dim_y,
    int k,
    float *radii,
    int *nx,
    int *ny,
    unsigned int *flags) {
    __shared__ float candidates[ASSAY_THREADS * ASSAY_MAX_K];
    __shared__ unsigned int count_x[ASSAY_THREADS];
    __shared__ unsigned int count_y[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];
    __shared__ float radius_shared;

    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    unsigned int local_bad = (n <= 0 || dim_x <= 0 || dim_y <= 0 || k <= 0 || k > ASSAY_MAX_K || k >= n)
        ? ASSAY_FLAG_INVALID_INDEX
        : 0u;
    float local_best[ASSAY_MAX_K];
    for (int q = 0; q < ASSAY_MAX_K; q++) {
        local_best[q] = INFINITY;
    }
    if (row < n && local_bad == 0u) {
        for (int col = tid; col < n; col += blockDim.x) {
            if (col != row) {
                const float dist = joint_chebyshev_f32(x, y, row, col, dim_x, dim_y, &local_bad);
                insert_smallest_value(dist, k, local_best);
            }
        }
    }
    for (int q = 0; q < ASSAY_MAX_K; q++) {
        const int offset = tid * ASSAY_MAX_K + q;
        candidates[offset] = local_best[q];
    }
    bad[tid] = local_bad;
    __syncthreads();

    if (tid == 0) {
        float merged[ASSAY_MAX_K];
        for (int q = 0; q < ASSAY_MAX_K; q++) {
            merged[q] = INFINITY;
        }
        for (int t = 0; t < blockDim.x; t++) {
            for (int q = 0; q < k && q < ASSAY_MAX_K; q++) {
                const int offset = t * ASSAY_MAX_K + q;
                insert_smallest_value(candidates[offset], k, merged);
            }
            local_bad |= bad[t];
        }
        radius_shared = merged[k - 1];
        radii[row] = radius_shared;
        if (!isfinite(radius_shared)) {
            local_bad |= ASSAY_FLAG_NONFINITE;
        }
        bad[0] = local_bad;
    }
    __syncthreads();

    unsigned int cx = 0u;
    unsigned int cy = 0u;
    const float radius = radius_shared;
    if (row < n && local_bad == 0u && isfinite(radius)) {
        for (int col = tid; col < n; col += blockDim.x) {
            if (col != row) {
                const float dx = chebyshev_f32(x, row, col, dim_x, &local_bad);
                const float dy = chebyshev_f32(y, row, col, dim_y, &local_bad);
                cx += dx < radius ? 1u : 0u;
                cy += dy < radius ? 1u : 0u;
            }
        }
    }
    count_x[tid] = cx;
    count_y[tid] = cy;
    bad[tid] |= local_bad;
    __syncthreads();
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            count_x[tid] += count_x[tid + stride];
            count_y[tid] += count_y[tid + stride];
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
    if (tid == 0) {
        nx[row] = (int)count_x[0];
        ny[row] = (int)count_y[0];
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_entropy_radii_f32(
    const float *values,
    int n,
    int dim,
    int k,
    float *radii,
    unsigned int *flags) {
    __shared__ float candidates[ASSAY_THREADS * ASSAY_MAX_K];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    unsigned int local_bad = (n <= 0 || dim <= 0 || k <= 0 || k > ASSAY_MAX_K || k >= n)
        ? ASSAY_FLAG_INVALID_INDEX
        : 0u;
    float local_best[ASSAY_MAX_K];
    for (int q = 0; q < ASSAY_MAX_K; q++) {
        local_best[q] = INFINITY;
    }
    if (row < n && local_bad == 0u) {
        for (int col = tid; col < n; col += blockDim.x) {
            if (col != row) {
                const float dist = chebyshev_f32(values, row, col, dim, &local_bad);
                insert_smallest_value(dist, k, local_best);
            }
        }
    }
    for (int q = 0; q < ASSAY_MAX_K; q++) {
        const int offset = tid * ASSAY_MAX_K + q;
        candidates[offset] = local_best[q];
    }
    bad[tid] = local_bad;
    __syncthreads();
    if (tid == 0) {
        float merged[ASSAY_MAX_K];
        for (int q = 0; q < ASSAY_MAX_K; q++) {
            merged[q] = INFINITY;
        }
        for (int t = 0; t < blockDim.x; t++) {
            for (int q = 0; q < k && q < ASSAY_MAX_K; q++) {
                const int offset = t * ASSAY_MAX_K + q;
                insert_smallest_value(candidates[offset], k, merged);
            }
            local_bad |= bad[t];
        }
        const float radius = merged[k - 1];
        radii[row] = radius;
        if (!isfinite(radius)) {
            local_bad |= ASSAY_FLAG_NONFINITE;
        }
        if (local_bad != 0u) {
            atomicOr(flags, local_bad);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_mixed_ksg_counts_f32(
    const float *x,
    const int *labels,
    int n,
    int dim,
    int k,
    float *radii,
    int *same_counts,
    int *full_counts,
    unsigned int *flags) {
    __shared__ float candidates[ASSAY_THREADS * ASSAY_MAX_K];
    __shared__ unsigned int same_count_shared[ASSAY_THREADS];
    __shared__ unsigned int full_count_shared[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];
    __shared__ float radius_shared;

    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    unsigned int local_bad = (n <= 0 || dim <= 0 || k <= 0 || k > ASSAY_MAX_K || k >= n)
        ? ASSAY_FLAG_INVALID_INDEX
        : 0u;
    float local_best[ASSAY_MAX_K];
    for (int q = 0; q < ASSAY_MAX_K; q++) {
        local_best[q] = INFINITY;
    }
    const int row_label = row < n ? labels[row] : -1;
    if (row < n && local_bad == 0u) {
        for (int col = tid; col < n; col += blockDim.x) {
            if (col != row && labels[col] == row_label) {
                const float dist = chebyshev_f32(x, row, col, dim, &local_bad);
                insert_smallest_value(dist, k, local_best);
            }
        }
    }
    for (int q = 0; q < ASSAY_MAX_K; q++) {
        const int offset = tid * ASSAY_MAX_K + q;
        candidates[offset] = local_best[q];
    }
    bad[tid] = local_bad;
    __syncthreads();
    if (tid == 0) {
        float merged[ASSAY_MAX_K];
        for (int q = 0; q < ASSAY_MAX_K; q++) {
            merged[q] = INFINITY;
        }
        for (int t = 0; t < blockDim.x; t++) {
            for (int q = 0; q < k && q < ASSAY_MAX_K; q++) {
                const int offset = t * ASSAY_MAX_K + q;
                insert_smallest_value(candidates[offset], k, merged);
            }
            local_bad |= bad[t];
        }
        radius_shared = merged[k - 1];
        radii[row] = radius_shared;
        if (!isfinite(radius_shared)) {
            local_bad |= ASSAY_FLAG_NONFINITE;
        }
        bad[0] = local_bad;
    }
    __syncthreads();

    unsigned int same = 0u;
    unsigned int full = 0u;
    const float radius = radius_shared;
    if (row < n && local_bad == 0u && isfinite(radius)) {
        for (int col = tid; col < n; col += blockDim.x) {
            if (col != row) {
                const float dist = chebyshev_f32(x, row, col, dim, &local_bad);
                const unsigned int inside = dist <= radius ? 1u : 0u;
                full += inside;
                same += (labels[col] == row_label) ? inside : 0u;
            }
        }
    }
    same_count_shared[tid] = same;
    full_count_shared[tid] = full;
    bad[tid] |= local_bad;
    __syncthreads();
    for (int stride = ASSAY_THREADS / 2; stride > 0; stride >>= 1) {
        if (tid < stride) {
            same_count_shared[tid] += same_count_shared[tid + stride];
            full_count_shared[tid] += full_count_shared[tid + stride];
            bad[tid] |= bad[tid + stride];
        }
        __syncthreads();
    }
    if (tid == 0) {
        same_counts[row] = (int)same_count_shared[0];
        full_counts[row] = (int)full_count_shared[0];
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
        }
    }
}

extern "C" __global__ void assay_ccm_simplex_predict_f32(
    const float *embedding,
    const float *target,
    const int *library_offsets,
    int library_count,
    int dim,
    int neighbor_count,
    float *predictions,
    unsigned int *flags) {
    const int task = blockIdx.x * blockDim.x + threadIdx.x;
    if (library_count <= 0 || dim <= 0 || neighbor_count <= 0 || neighbor_count > ASSAY_MAX_K) {
        if (task == 0) {
            atomicOr(flags, ASSAY_FLAG_INVALID_INDEX);
        }
        return;
    }
    int lib = -1;
    int begin = 0;
    int end = 0;
    for (int candidate = 0; candidate < library_count; candidate++) {
        begin = library_offsets[candidate];
        end = library_offsets[candidate + 1];
        if (task >= begin && task < end) {
            lib = candidate;
            break;
        }
    }
    if (lib < 0) {
        return;
    }
    const int library_size = end - begin;
    const int row = task - begin;
    if (library_size <= neighbor_count || row >= library_size) {
        atomicOr(flags, ASSAY_FLAG_INVALID_INDEX);
        return;
    }
    float best[ASSAY_MAX_K];
    int best_idx[ASSAY_MAX_K];
    unsigned int local_bad = 0u;
    for (int q = 0; q < ASSAY_MAX_K; q++) {
        best[q] = INFINITY;
        best_idx[q] = 2147483647;
    }
    for (int col = 0; col < library_size; col++) {
        if (col != row) {
            const float dist = euclidean_f32(embedding, row, col, dim, &local_bad);
            insert_smallest(dist, col, neighbor_count, best, best_idx);
        }
    }
    const double eps = 1.0e-12;
    const double d1 = (double)best[0];
    double prediction = 0.0;
    if (d1 <= eps) {
        double sum = 0.0;
        int count = 0;
        for (int q = 0; q < neighbor_count; q++) {
            if ((double)best[q] <= eps) {
                sum += (double)target[best_idx[q]];
                count++;
            }
        }
        if (count == 0) {
            local_bad |= ASSAY_FLAG_NONFINITE;
        } else {
            prediction = sum / (double)count;
        }
    } else {
        double weighted_sum = 0.0;
        double weight_sum = 0.0;
        for (int q = 0; q < neighbor_count; q++) {
            const double weight = exp(-((double)best[q]) / d1);
            weighted_sum += weight * (double)target[best_idx[q]];
            weight_sum += weight;
        }
        if (!(weight_sum > 0.0) || !isfinite(weight_sum)) {
            local_bad |= ASSAY_FLAG_NONFINITE;
        } else {
            prediction = weighted_sum / weight_sum;
        }
    }
    if (!isfinite(prediction)) {
        local_bad |= ASSAY_FLAG_NONFINITE;
    }
    predictions[task] = (float)prediction;
    if (local_bad != 0u) {
        atomicOr(flags, local_bad);
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_pairwise_abs_1d_f32(
    const float *values,
    int n,
    double *matrix,
    double *row_sums,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    double sum = 0.0;
    unsigned int local_bad = 0;
    if (row >= n || n <= 0) {
        return;
    }
    const float left = values[row];
    local_bad |= isfinite(left) ? 0u : ASSAY_FLAG_NONFINITE;

    for (int col = tid; col < n; col += blockDim.x) {
        const float right = values[col];
        local_bad |= isfinite(right) ? 0u : ASSAY_FLAG_NONFINITE;
        const double value = fabs((double)left - (double)right);
        matrix[row * n + col] = value;
        sum += value;
    }

    sums[tid] = sum;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);

    if (tid == 0) {
        row_sums[row] = sums[0];
        if (bad[0] != 0u || !isfinite(sums[0])) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_pairwise_rbf_1d_f32(
    const float *values,
    int n,
    double sigma,
    double *matrix,
    double *row_sums,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    double sum = 0.0;
    unsigned int local_bad = (!(sigma > 0.0) || !isfinite(sigma)) ? ASSAY_FLAG_NONFINITE : 0u;
    if (row >= n || n <= 0) {
        return;
    }
    const float left = values[row];
    local_bad |= isfinite(left) ? 0u : ASSAY_FLAG_NONFINITE;
    const double denom = 2.0 * sigma * sigma;

    for (int col = tid; col < n; col += blockDim.x) {
        const float right = values[col];
        local_bad |= isfinite(right) ? 0u : ASSAY_FLAG_NONFINITE;
        const double diff = (double)left - (double)right;
        const double value = exp(-(diff * diff) / denom);
        matrix[row * n + col] = value;
        sum += value;
    }

    sums[tid] = sum;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);

    if (tid == 0) {
        row_sums[row] = sums[0];
        if (bad[0] != 0u || !isfinite(sums[0])) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_pairwise_rbf_f64(
    const double *values,
    int n,
    int dim,
    double sigma,
    double *matrix,
    double *row_sums,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int row = blockIdx.x;
    const int tid = threadIdx.x;
    double sum = 0.0;
    unsigned int local_bad = (!(sigma > 0.0) || !isfinite(sigma) || dim <= 0) ? ASSAY_FLAG_NONFINITE : 0u;
    if (row >= n || n <= 0) {
        return;
    }
    const int row_base = row * dim;
    const double denom = 2.0 * sigma * sigma;

    for (int col = tid; col < n; col += blockDim.x) {
        const int col_base = col * dim;
        double dist2 = 0.0;
        for (int k = 0; k < dim; k++) {
            const double left = values[row_base + k];
            const double right = values[col_base + k];
            local_bad |= (isfinite(left) && isfinite(right)) ? 0u : ASSAY_FLAG_NONFINITE;
            const double diff = left - right;
            dist2 += diff * diff;
        }
        const double value = exp(-dist2 / denom);
        matrix[row * n + col] = value;
        sum += value;
    }

    sums[tid] = sum;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);

    if (tid == 0) {
        row_sums[row] = sums[0];
        if (bad[0] != 0u || !isfinite(sums[0])) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_center_symmetric_f64(
    double *matrix,
    const double *row_sums,
    double total_sum,
    int n,
    unsigned int *flags) {
    const int idx = blockIdx.x * blockDim.x + threadIdx.x;
    const int len = n * n;
    if (idx >= len) {
        return;
    }
    const int row = idx / n;
    const int col = idx - row * n;
    const double nf = (double)n;
    const double value = matrix[idx] - row_sums[row] / nf - row_sums[col] / nf + total_sum / (nf * nf);
    matrix[idx] = value;
    if (!isfinite(value)) {
        atomicOr(flags, ASSAY_FLAG_NONFINITE);
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_reduce_sum_f64(
    const double *values,
    int len,
    double *partials,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int tid = threadIdx.x;
    const int stride = blockDim.x * gridDim.x;
    double sum = 0.0;
    unsigned int local_bad = 0;
    for (int idx = blockIdx.x * blockDim.x + tid; idx < len; idx += stride) {
        const double value = values[idx];
        local_bad |= isfinite(value) ? 0u : ASSAY_FLAG_NONFINITE;
        sum += value;
    }
    sums[tid] = sum;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);

    if (tid == 0) {
        partials[blockIdx.x] = sums[0];
        if (bad[0] != 0u || !isfinite(sums[0])) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_dcor_stats_f64(
    const double *a,
    const double *b,
    int n,
    double *partial_dcov,
    double *partial_vx,
    double *partial_vy,
    unsigned int *flags) {
    __shared__ double s0[ASSAY_THREADS];
    __shared__ double s1[ASSAY_THREADS];
    __shared__ double s2[ASSAY_THREADS];
    __shared__ double s3[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int tid = threadIdx.x;
    const int len = n * n;
    const int stride = blockDim.x * gridDim.x;
    double dcov = 0.0;
    double vx = 0.0;
    double vy = 0.0;
    unsigned int local_bad = 0;
    for (int idx = blockIdx.x * blockDim.x + tid; idx < len; idx += stride) {
        const double av = a[idx];
        const double bv = b[idx];
        local_bad |= (isfinite(av) && isfinite(bv)) ? 0u : ASSAY_FLAG_NONFINITE;
        dcov += av * bv;
        vx += av * av;
        vy += bv * bv;
    }
    s0[tid] = dcov;
    s1[tid] = vx;
    s2[tid] = vy;
    s3[tid] = 0.0;
    bad[tid] = local_bad;
    __syncthreads();
    reduce4(s0, s1, s2, s3, bad, tid);

    if (tid == 0) {
        partial_dcov[blockIdx.x] = s0[0];
        partial_vx[blockIdx.x] = s1[0];
        partial_vy[blockIdx.x] = s2[0];
        if (bad[0] != 0u || !isfinite(s0[0]) || !isfinite(s1[0]) || !isfinite(s2[0])) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_hsic_stats_f64(
    const double *kc,
    const double *lc,
    const double *row_k,
    const double *row_l,
    int n,
    double *partial_tr,
    double *partial_sq_offdiag,
    double *partial_one_kl_one,
    unsigned int *flags) {
    __shared__ double s0[ASSAY_THREADS];
    __shared__ double s1[ASSAY_THREADS];
    __shared__ double s2[ASSAY_THREADS];
    __shared__ double s3[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int tid = threadIdx.x;
    const int len = n * n;
    const int stride = blockDim.x * gridDim.x;
    double tr = 0.0;
    double sq = 0.0;
    unsigned int local_bad = 0;
    for (int idx = blockIdx.x * blockDim.x + tid; idx < len; idx += stride) {
        const int row = idx / n;
        const int col = idx - row * n;
        const double kcv = kc[idx];
        const double lcv = lc[idx];
        local_bad |= (isfinite(kcv) && isfinite(lcv)) ? 0u : ASSAY_FLAG_NONFINITE;
        const double prod = kcv * lcv;
        tr += prod;
        if (row != col) {
            sq += prod * prod;
        }
    }

    s0[tid] = tr;
    s1[tid] = sq;
    s2[tid] = 0.0;
    s3[tid] = 0.0;
    bad[tid] = local_bad;
    __syncthreads();
    reduce4(s0, s1, s2, s3, bad, tid);

    if (tid == 0) {
        partial_tr[blockIdx.x] = s0[0];
        partial_sq_offdiag[blockIdx.x] = s1[0];
        if (bad[0] != 0u || !isfinite(s0[0]) || !isfinite(s1[0])) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
        }
    }

    double one = 0.0;
    unsigned int one_bad = 0;
    for (int row = blockIdx.x * blockDim.x + tid; row < n; row += stride) {
        const double rk = row_k[row] - 1.0;
        const double rl = row_l[row] - 1.0;
        one_bad |= (isfinite(rk) && isfinite(rl)) ? 0u : ASSAY_FLAG_NONFINITE;
        one += rk * rl;
    }
    s0[tid] = one;
    s1[tid] = 0.0;
    s2[tid] = 0.0;
    s3[tid] = 0.0;
    bad[tid] = one_bad;
    __syncthreads();
    reduce4(s0, s1, s2, s3, bad, tid);

    if (tid == 0) {
        partial_one_kl_one[blockIdx.x] = s0[0];
        if (bad[0] != 0u || !isfinite(s0[0])) {
            atomicOr(flags, ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_dcor_permutations_f64(
    const double *a,
    const double *b,
    const int *perms,
    int n,
    int permutations,
    double *out,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int perm_id = blockIdx.x;
    const int tid = threadIdx.x;
    if (perm_id >= permutations) {
        return;
    }
    const int *perm = perms + perm_id * n;
    const double denom = (double)n * (double)n;
    double sum = 0.0;
    unsigned int local_bad = 0;
    const int len = n * n;
    for (int idx = tid; idx < len; idx += blockDim.x) {
        const int row = idx / n;
        const int col = idx - row * n;
        const int prow = perm[row];
        const int pcol = perm[col];
        if (prow < 0 || prow >= n || pcol < 0 || pcol >= n) {
            local_bad |= ASSAY_FLAG_INVALID_INDEX;
            continue;
        }
        const double value = a[idx] * b[prow * n + pcol];
        local_bad |= isfinite(value) ? 0u : ASSAY_FLAG_NONFINITE;
        sum += value;
    }
    sums[tid] = sum;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);

    if (tid == 0) {
        out[perm_id] = sums[0] / denom;
        if (bad[0] != 0u || !isfinite(out[perm_id])) {
            atomicOr(flags, bad[0] != 0u ? bad[0] : ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_hsic_permutations_f64(
    const double *kc,
    const double *lc,
    const int *perms,
    int n,
    int permutations,
    double *out,
    unsigned int *flags) {
    __shared__ double sums[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int perm_id = blockIdx.x;
    const int tid = threadIdx.x;
    if (perm_id >= permutations) {
        return;
    }
    const int *perm = perms + perm_id * n;
    double sum = 0.0;
    unsigned int local_bad = 0;
    const int len = n * n;
    for (int idx = tid; idx < len; idx += blockDim.x) {
        const int row = idx / n;
        const int col = idx - row * n;
        const int prow = perm[row];
        const int pcol = perm[col];
        if (prow < 0 || prow >= n || pcol < 0 || pcol >= n) {
            local_bad |= ASSAY_FLAG_INVALID_INDEX;
            continue;
        }
        const double value = kc[idx] * lc[prow * n + pcol];
        local_bad |= isfinite(value) ? 0u : ASSAY_FLAG_NONFINITE;
        sum += value;
    }
    sums[tid] = sum;
    bad[tid] = local_bad;
    __syncthreads();
    reduce1(sums, bad, tid);

    if (tid == 0) {
        out[perm_id] = sums[0];
        if (bad[0] != 0u || !isfinite(out[perm_id])) {
            atomicOr(flags, bad[0] != 0u ? bad[0] : ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_count_ge_f64(
    const double *values,
    int len,
    double observed,
    double tolerance,
    unsigned int *count,
    unsigned int *flags) {
    __shared__ unsigned int counts[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int tid = threadIdx.x;
    const int stride = blockDim.x * gridDim.x;
    unsigned int local = 0u;
    unsigned int local_bad = (!(isfinite(observed) && isfinite(tolerance))) ? ASSAY_FLAG_NONFINITE : 0u;
    for (int idx = blockIdx.x * blockDim.x + tid; idx < len; idx += stride) {
        const double value = values[idx];
        local_bad |= isfinite(value) ? 0u : ASSAY_FLAG_NONFINITE;
        if (value >= observed - tolerance) {
            local += 1u;
        }
    }
    counts[tid] = local;
    bad[tid] = local_bad;
    __syncthreads();
    for (int stride2 = ASSAY_THREADS / 2; stride2 > 0; stride2 >>= 1) {
        if (tid < stride2) {
            counts[tid] += counts[tid + stride2];
            bad[tid] |= bad[tid + stride2];
        }
        __syncthreads();
    }
    if (tid == 0) {
        atomicAdd(count, counts[0]);
        if (bad[0] != 0u) {
            atomicOr(flags, bad[0]);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_mmd_observed_f64(
    const double *kernel,
    int n,
    int n_a,
    double *out,
    unsigned int *flags) {
    __shared__ double s0[ASSAY_THREADS];
    __shared__ double s1[ASSAY_THREADS];
    __shared__ double s2[ASSAY_THREADS];
    __shared__ double s3[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int tid = threadIdx.x;
    const int n_b = n - n_a;
    const int len = n * n;
    double aa = 0.0;
    double bb = 0.0;
    double ab = 0.0;
    unsigned int local_bad = (n_a <= 0 || n_b <= 0) ? ASSAY_FLAG_INVALID_INDEX : 0u;
    for (int idx = tid; idx < len; idx += blockDim.x) {
        const int row = idx / n;
        const int col = idx - row * n;
        const double value = kernel_lookup(kernel, n, row, col);
        local_bad |= isfinite(value) ? 0u : ASSAY_FLAG_NONFINITE;
        if (row < n_a && col < n_a) {
            aa += value;
        } else if (row >= n_a && col >= n_a) {
            bb += value;
        } else if (row < n_a && col >= n_a) {
            ab += value;
        }
    }
    s0[tid] = aa;
    s1[tid] = bb;
    s2[tid] = ab;
    s3[tid] = 0.0;
    bad[tid] = local_bad;
    __syncthreads();
    reduce4(s0, s1, s2, s3, bad, tid);

    if (tid == 0) {
        const double naf = (double)n_a;
        const double nbf = (double)n_b;
        out[0] = s0[0] / (naf * naf) + s1[0] / (nbf * nbf) - 2.0 * s2[0] / (naf * nbf);
        if (bad[0] != 0u || !isfinite(out[0])) {
            atomicOr(flags, bad[0] != 0u ? bad[0] : ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_mmd_permutations_f64(
    const double *kernel,
    const int *perms,
    int n,
    int n_a,
    int permutations,
    double *out,
    unsigned int *flags) {
    __shared__ double s0[ASSAY_THREADS];
    __shared__ double s1[ASSAY_THREADS];
    __shared__ double s2[ASSAY_THREADS];
    __shared__ double s3[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int perm_id = blockIdx.x;
    const int tid = threadIdx.x;
    if (perm_id >= permutations) {
        return;
    }
    const int *perm = perms + perm_id * n;
    const int n_b = n - n_a;
    const int len = n * n;
    double aa = 0.0;
    double bb = 0.0;
    double ab = 0.0;
    unsigned int local_bad = (n_a <= 0 || n_b <= 0) ? ASSAY_FLAG_INVALID_INDEX : 0u;

    for (int idx = tid; idx < len; idx += blockDim.x) {
        const int rel_row = idx / n;
        const int rel_col = idx - rel_row * n;
        const int row = perm[rel_row];
        const int col = perm[rel_col];
        if (row < 0 || row >= n || col < 0 || col >= n) {
            local_bad |= ASSAY_FLAG_INVALID_INDEX;
            continue;
        }
        const double value = kernel_lookup(kernel, n, row, col);
        local_bad |= isfinite(value) ? 0u : ASSAY_FLAG_NONFINITE;
        if (rel_row < n_a && rel_col < n_a) {
            aa += value;
        } else if (rel_row >= n_a && rel_col >= n_a) {
            bb += value;
        } else if (rel_row < n_a && rel_col >= n_a) {
            ab += value;
        }
    }
    s0[tid] = aa;
    s1[tid] = bb;
    s2[tid] = ab;
    s3[tid] = 0.0;
    bad[tid] = local_bad;
    __syncthreads();
    reduce4(s0, s1, s2, s3, bad, tid);

    if (tid == 0) {
        const double naf = (double)n_a;
        const double nbf = (double)n_b;
        out[perm_id] = s0[0] / (naf * naf) + s1[0] / (nbf * nbf) - 2.0 * s2[0] / (naf * nbf);
        if (bad[0] != 0u || !isfinite(out[perm_id])) {
            atomicOr(flags, bad[0] != 0u ? bad[0] : ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_mmd_change_observed_f64(
    const double *kernel,
    int n,
    int min_window,
    double *best_value_out,
    int *best_split_out,
    unsigned int *flags) {
    __shared__ double s0[ASSAY_THREADS];
    __shared__ double s1[ASSAY_THREADS];
    __shared__ double s2[ASSAY_THREADS];
    __shared__ double s3[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int tid = threadIdx.x;
    const int len = n * n;
    double best_value = -INFINITY;
    int best_split = min_window;
    unsigned int terminal_bad = (min_window < 2 || n < min_window * 2) ? ASSAY_FLAG_INVALID_INDEX : 0u;

    for (int split = min_window; split <= n - min_window; split++) {
        double aa = 0.0;
        double bb = 0.0;
        double ab = 0.0;
        unsigned int local_bad = 0u;
        for (int idx = tid; idx < len; idx += blockDim.x) {
            const int row = idx / n;
            const int col = idx - row * n;
            const double value = kernel_lookup(kernel, n, row, col);
            local_bad |= isfinite(value) ? 0u : ASSAY_FLAG_NONFINITE;
            if (row < split && col < split && row != col) {
                aa += value;
            } else if (row >= split && col >= split && row != col) {
                bb += value;
            } else if (row < split && col >= split) {
                ab += value;
            }
        }
        s0[tid] = aa;
        s1[tid] = bb;
        s2[tid] = ab;
        s3[tid] = 0.0;
        bad[tid] = local_bad;
        __syncthreads();
        reduce4(s0, s1, s2, s3, bad, tid);
        if (tid == 0) {
            const double left = (double)split;
            const double right = (double)(n - split);
            const double value =
                s0[0] / (left * (left - 1.0)) +
                s1[0] / (right * (right - 1.0)) -
                2.0 * s2[0] / (left * right);
            terminal_bad |= bad[0];
            if (isfinite(value) && value > best_value) {
                best_value = value;
                best_split = split;
            } else if (!isfinite(value)) {
                terminal_bad |= ASSAY_FLAG_NONFINITE;
            }
        }
        __syncthreads();
    }

    if (tid == 0) {
        best_value_out[0] = best_value;
        best_split_out[0] = best_split;
        if (terminal_bad != 0u || !isfinite(best_value)) {
            atomicOr(flags, terminal_bad != 0u ? terminal_bad : ASSAY_FLAG_NONFINITE);
        }
    }
}

extern "C" __global__ __launch_bounds__(ASSAY_THREADS) void assay_mmd_change_permutations_f64(
    const double *kernel,
    const int *perms,
    int n,
    int min_window,
    int permutations,
    double *out,
    unsigned int *flags) {
    __shared__ double s0[ASSAY_THREADS];
    __shared__ double s1[ASSAY_THREADS];
    __shared__ double s2[ASSAY_THREADS];
    __shared__ double s3[ASSAY_THREADS];
    __shared__ unsigned int bad[ASSAY_THREADS];

    const int perm_id = blockIdx.x;
    const int tid = threadIdx.x;
    if (perm_id >= permutations) {
        return;
    }
    const int *perm = perms + perm_id * n;
    const int len = n * n;
    double best_value = -INFINITY;
    unsigned int terminal_bad = (min_window < 2 || n < min_window * 2) ? ASSAY_FLAG_INVALID_INDEX : 0u;

    for (int split = min_window; split <= n - min_window; split++) {
        double aa = 0.0;
        double bb = 0.0;
        double ab = 0.0;
        unsigned int local_bad = 0u;
        for (int idx = tid; idx < len; idx += blockDim.x) {
            const int rel_row = idx / n;
            const int rel_col = idx - rel_row * n;
            const int row = perm[rel_row];
            const int col = perm[rel_col];
            if (row < 0 || row >= n || col < 0 || col >= n) {
                local_bad |= ASSAY_FLAG_INVALID_INDEX;
                continue;
            }
            const double value = kernel_lookup(kernel, n, row, col);
            local_bad |= isfinite(value) ? 0u : ASSAY_FLAG_NONFINITE;
            if (rel_row < split && rel_col < split && rel_row != rel_col) {
                aa += value;
            } else if (rel_row >= split && rel_col >= split && rel_row != rel_col) {
                bb += value;
            } else if (rel_row < split && rel_col >= split) {
                ab += value;
            }
        }
        s0[tid] = aa;
        s1[tid] = bb;
        s2[tid] = ab;
        s3[tid] = 0.0;
        bad[tid] = local_bad;
        __syncthreads();
        reduce4(s0, s1, s2, s3, bad, tid);
        if (tid == 0) {
            const double left = (double)split;
            const double right = (double)(n - split);
            const double value =
                s0[0] / (left * (left - 1.0)) +
                s1[0] / (right * (right - 1.0)) -
                2.0 * s2[0] / (left * right);
            terminal_bad |= bad[0];
            if (isfinite(value) && value > best_value) {
                best_value = value;
            } else if (!isfinite(value)) {
                terminal_bad |= ASSAY_FLAG_NONFINITE;
            }
        }
        __syncthreads();
    }

    if (tid == 0) {
        out[perm_id] = best_value;
        if (terminal_bad != 0u || !isfinite(best_value)) {
            atomicOr(flags, terminal_bad != 0u ? terminal_bad : ASSAY_FLAG_NONFINITE);
        }
    }
}
