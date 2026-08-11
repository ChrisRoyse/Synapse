# Forge GPU Routing for Synapse Workloads (#1695)

Date: 2026-07-16

## Purpose

Issue #1695 examined the PRD-listed Forge ops that Synapse will rely on during the Calyx integration. The root problem was not a blanket lack of CUDA. Forge already had working CUDA distance/top-k/KSG primitives, but two production-facing capabilities were not first-class backend operations:

- exact KNN was assembled ad hoc from lower-level distance/top-k calls, so callers could not audit backend support directly;
- Loom's original `agreement_batch_gpu` constructed a private CUDA backend and was never called by the live weave. The live materialization path now receives the owning vault's selected backend, groups scoreable agreement pairs by dimension, and issues one admitted `Backend::paired_cosine` dispatch per group.

## Source Sizing

The profile used the live Synapse daemon and current storage SoT:

| SoT | Value |
|---|---:|
| `synapse-mcp` bind | `127.0.0.1:7700` |
| MCP tool count | `40` |
| `CF_EPISODES` | `3,638` |
| `CF_TIMELINE` | `148,559` |
| CUDA device | `NVIDIA GeForce RTX 5090` |

Manual profile artifact: `%TEMP%\synapse1695-profile\profile.json`, read back separately after the real MCP `shell` trigger.

## Profile Findings

| Workload | Profile size | CPU | CUDA/current | Decision |
|---|---:|---:|---:|---|
| cosine top-k KNN | 8 queries x 3,638 candidates x dim 64, k=16 | 50.8 ms | 18.6 ms | ship first-class `Backend::knn` |
| Loom agreement pairs | 2,048 pairs x dim 64 | 2.0 ms | 202.9 ms | replace per-pair CUDA loop with `Backend::paired_cosine` |
| KSG counts | 512 rows | 118.2 ms | 2.2 ms | already shipped via assay CUDA path |
| timeline histogram NMI | 148,559 rows, 16 bins | 48.6 ms | not implemented | CPU-route until profile says otherwise |
| sparse postings | 3,638 rows, dim 4,096, row nnz 16 | 11.8 ms | not implemented | CPU-route until corpus scale requires GPU |
| ColBERT MaxSim | 3,638 docs, 4x8 tokens, dim 32 | 27.4 ms | not implemented | CPU-route until user-facing latency justifies GPU |

## Implemented Routing Contract

`calyx-forge` now exposes:

| Operation | Backend API | CPU | CUDA | Notes |
|---|---|---|---|---|
| exact KNN | `Backend::knn` | yes | yes | cosine/dot use exact top-k; L2 sorts ascending squared distance |
| paired cosine | `Backend::paired_cosine` | yes | yes | one row-pair per output scalar; Loom agreement uses this path |
| KSG counts | assay host functions | yes | yes | pre-existing CUDA path; profile confirmed parity on sampled counts |

`FORGE_DEFERRED_BACKEND_OPS` now contains only operations that remain absent as backend ops:

- `histogram_nmi`
- `spmm_sparse_ops`
- `graph_ops`
- `colbert_maxsim`

`FORGE_CPU_ROUTED_BACKEND_OPS` names the same list explicitly. This is deliberate: these operations are not silently claimed as GPU-ready, and callers can inspect the routing contract.

## Best-Practice Research Applied

- RAPIDS cuVS documents brute-force KNN as exact exhaustive nearest-neighbor search and notes that exact search is matrix-multiplication based. That supports using Forge's existing exact distance plus exact top-k primitives for profile-scale KNN instead of adding an approximate index inside Forge. Source: https://docs.rapids.ai/api/cuvs/stable/neighbors/bruteforce/
- NVIDIA cuSPARSE documents deterministic SpMV algorithms only for specific COO/CSR non-transpose modes, and recommends preprocessing when the same sparse matrix is reused. Sparse slot search currently has a small measured CPU cost; a future GPU sparse issue should use cuSPARSE-style CSR reuse and explicit determinism constraints instead of ad hoc kernels. Source: https://docs.nvidia.com/cuda/archive/13.0.2/cusparse/generic-api/generic-api-functions.html
- NVIDIA's CUDA C++ Best Practices Guide calls out that parallel floating-point operation order can change numerical results. The new Forge result constructors validate finite outputs, shape, and bounds; manual FSV compares CPU/CUDA within tolerance rather than assuming bit-identical reduction order. Source: https://docs.nvidia.com/cuda/cuda-c-best-practices-guide/index.html

## Follow-On Rule

Do not add a new Forge GPU op because it is listed in the PRD. Add one only after a real Synapse profile identifies the CPU path as a bottleneck at the relevant corpus scale, then expose it as a named backend operation with CPU/CUDA implementations, strict shape validation, finite-output checks, and manual FSV evidence at the physical SoT.
