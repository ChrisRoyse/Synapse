# ADR: Synapse owns one immutable Calyx Assay backend

## Status

Accepted — 2026-08-14 (#2245).

## Context

Synapse selected and reported one Forge math backend, while generic
`calyx-assay` entry points independently consulted `CALYX_ASSAY_CUDA_STRICT`.
The daemon did not set that variable, so a CUDA-configured process silently ran
Assay work on CPU. The build feature was also split: `calyx-cuda` compiled Forge
CUDA but not Assay CUDA implementations.

Runtime environment mutation is not a sound cross-platform configuration
mechanism for a multithreaded Rust process, and a timeout increase would retain
the wrong execution backend.

## Decision

`synapse-calyx/calyx-cuda` enables both `calyx-forge/cuda` and
`calyx-assay/cuda`.

During vault open, Synapse maps its selected serving backend to a typed
`AssayComputeBackend` and configures it in a process-wide `OnceLock` before any
request can run. Repeating the same value is idempotent. A different second
value returns `CALYX_ASSAY_COMPUTE_BACKEND_CONFLICT`; an unknown Synapse backend
also refuses vault open. Generic Assay APIs consult this immutable value before
the legacy standalone environment contract.

Health reports `calyx_assay_compute_backend` independently from
`calyx_math_backend`. CUDA execution errors remain structured Calyx/Forge
errors; CPU is never selected as a runtime failure fallback.

## Consequences

- The advertised and executed Assay backend cannot silently diverge.
- Every generic Assay estimator receives the same backend law without
  duplicating selection code across its call sites.
- Changing serving backend requires a process restart, matching CUDA runtime
  and reservation lifecycle ownership.
- CUDA-enabled compile/lint must exercise the release feature graph; the
  default feature graph alone cannot validate this boundary.

## Research basis

- [Rust 2024 newly unsafe functions](https://doc.rust-lang.org/stable/edition-guide/rust-2024/newly-unsafe-functions.html): process environment mutation may be unsound in multithreaded programs and should be replaced where possible.
- [Rust `OnceLock`](https://doc.rust-lang.org/std/sync/struct.OnceLock.html): the standard library's thread-safe write-once primitive accepts a runtime-selected initialization value.
- [CUDA error checking](https://docs.nvidia.com/cuda/cuda-programming-guide/02-basics/intro-to-cuda-cpp.html): production applications must check and manage every CUDA API result, including asynchronous execution failures.
- [CUDA Best Practices](https://docs.nvidia.com/cuda/cuda-c-best-practices-guide/index.html): minimize transfer/launch overhead and batch device work instead of multiplying small operations.
