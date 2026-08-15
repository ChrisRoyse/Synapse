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

The immutable selector decides *which* backend an Assay request must use; it
does not grant Assay independent ownership of that backend. Synapse-integrated
MMD now has two explicit execution classes:

- unattended post-ingest drift declares `BackgroundCpu`, proves the dedicated
  CPU backend, and calls a CPU-strict Assay entry point that never consults or
  replaces the configured backend;
- an explicit `Configured` request holds one `SynapseCalyxMathRuntime` lease
  across the complete panel pass. If CUDA is selected, Assay borrows the
  lease's `VramBudgetedCudaBackend`; it cannot create a raw CUDA context.

The budgeted backend admits Gaussian MMD through both the process-local VRAM
gate and the host-wide reservation ledger. Admission and the concrete kernel
share one exact device-buffer shape function. After the final lens, Synapse
drops the lease and refuses success unless CUDA reads back as
`dormant_verified` with no live host reservation. The report and structured
events expose the requested execution class and actual backend.

## Consequences

- The advertised and executed Assay backend cannot silently diverge.
- Every generic Assay estimator receives the same backend law without
  duplicating selection code across its call sites.
- Changing serving backend requires a process restart, matching CUDA runtime
  and reservation lifecycle ownership.
- Idle scheduled maintenance cannot create CUDA contexts, driver threads, or
  VRAM reservations behind foreground games.
- Explicit CUDA MMD uses one application-owned primary-context lifetime per
  pass, one shared module cache, and measured dispatch admission. Health can no
  longer claim the runtime is dormant while Assay is using an untracked CUDA
  wrapper.
- CUDA-enabled compile/lint must exercise the release feature graph; the
  default feature graph alone cannot validate this boundary.

## Research basis

- [Rust 2024 newly unsafe functions](https://doc.rust-lang.org/stable/edition-guide/rust-2024/newly-unsafe-functions.html): process environment mutation may be unsound in multithreaded programs and should be replaced where possible.
- [Rust `OnceLock`](https://doc.rust-lang.org/std/sync/struct.OnceLock.html): the standard library's thread-safe write-once primitive accepts a runtime-selected initialization value.
- [CUDA error checking](https://docs.nvidia.com/cuda/cuda-programming-guide/02-basics/intro-to-cuda-cpp.html): production applications must check and manage every CUDA API result, including asynchronous execution failures.
- [CUDA Best Practices](https://docs.nvidia.com/cuda/cuda-c-best-practices-guide/index.html): minimize transfer/launch overhead and batch device work instead of multiplying small operations.
- [CUDA Driver API primary contexts](https://docs.nvidia.com/cuda/cuda-driver-api/group__CUDA__PRIMARY__CTX.html): a device has one reference-counted primary context shared with the runtime API.
- [CUDA Driver vs. Runtime API](https://docs.nvidia.com/cuda/cuda-driver-api/driver-vs-runtime-api.html): libraries should use the application-owned current context, and multiple contexts per device are strongly discouraged because each context carries separate resources and scheduling overhead.
