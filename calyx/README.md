# Calyx — Synapse's association-native foundation (fork-and-own)

**This is Synapse's code.** Calyx is a blueprint designed to be built on top of; these crates were absorbed from `ChrisRoyse/Calyx-Dev` @ `9894f84f` (2026-07-15) as the starting point and are **fully owned and customized by this project from here on**. There is no upstream tracking, no sync procedure, and no expectation of staying close to the original — reshape, gut, extend, and rename anything as Synapse needs. Upstream (and `C:\code\calyx-dev`) is reference material only.

Rules:
1. This directory is the **only** Calyx Synapse uses. Synapse crates depend on these crates by path (`calyx/crates/...`); never add a git/crates.io Calyx dependency.
2. Calyx-side changes (new encoders, structured-record measure pipeline, async facade, Windows hardening, GPU ops — see the `[CALYX]` issues) are normal Synapse commits, reviewed and FSV'd like everything else.
3. `calyx/` is a nested cargo workspace (`exclude = ["calyx"]` in the root manifest) so it keeps its own dependency set and lint profile while living in this repo.
4. The design doctrine is `BUILDING_ON_CALYX.md` (kept here): decompose to atoms → all base associations → differentiate (bits) → kernel → compose. Encoders for the explicit, no learned embedders, grounding mandatory, no-flatten, fail closed.

Local gates for this nested workspace are separate from the root Synapse workspace:

```powershell
cargo check --manifest-path calyx\Cargo.toml --workspace
cargo fmt --manifest-path calyx\Cargo.toml --all --check
cargo clippy --manifest-path calyx\Cargo.toml --workspace --all-targets
```

The root `.githooks/pre-push` hook runs those Calyx fmt/clippy gates automatically when a push touches `calyx/` Rust or Cargo files.

CUDA compile probes are opt-in, standalone, and package-feature scoped:

```powershell
cargo check --manifest-path calyx\Cargo.toml --workspace --features "calyx-assay/cuda calyx-loom/cuda calyx-registry/cuda calyx-search/cuda calyx-sextant/cuda"
```

On Windows, CUDA 13.x compile probes require `NVCC_CCBIN` to point at the MSVC
`Hostx64\x64` directory containing `cl.exe`, and `NVCC_APPEND_FLAGS` must include
`-Xcompiler=/Zc:preprocessor`. Configure both manually for the isolated probe.
Standard `scripts\synapse-setup.ps1` does not detect or persist either variable,
does not enable these features, and rejects accelerator requests for the
installed CPU-only/no-explicit-GPU daemon. A bad probe configuration must fail
loudly rather than silently selecting a different backend.

Kept crates (dependency-closed set): `calyx-core`, `calyx-aster` (storage), `calyx-registry` (lenses), `calyx-forge` (CPU/CUDA math), `calyx-loom` (associations), `calyx-assay` (bits), `calyx-lodestar` (kernel), `calyx-ward` (guard), `calyx-oracle` (prediction), `calyx-ledger` (provenance), `calyx-sextant`/`calyx-search` (search), `calyx-anneal` (self-optimization), `calyx-mincut`, `calyx-paths`.

Pruned at absorption (apps/servers this project doesn't need): calyx-poly, calyx-leapable, calyx-cli, calyxd, calyx-mcp, calyx-web-api, calyx-gatebrokerd, calyx-hazard-soak, calyx-buildinfo, and the upstream assets/data/datasets/docs/fuzz/infra/tools directories.

License: BSL 1.1 (see `LICENSE`); both projects are by the same author.
