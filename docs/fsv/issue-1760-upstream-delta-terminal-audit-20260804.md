# Issue #1760 upstream-delta terminal audit FSV — 2026-08-04

## Scope

This run closes the source-history audit from the previously terminally adjudicated Calyx-Dev
head `1fc85aa7` through the freshly fetched authenticated `origin/main` head `06508c0a`. It does
not merge or synchronize the upstream tree. The incremental range contains no reachable behavior
that requires a new native port, so no product database mutation is claimed by this change.

## Sources of truth

1. Source history: Git objects in `C:\code\Calyx-Dev\.git`, read through `origin/main`.
2. Product behavior: the Synapse-owned `calyx/` source, specifically the native atomic anchor
   batch and MVCC overlay scan implementations.
3. Host capability: Windows `Win32_VideoController` and command discovery for `nvidia-smi`.
4. Audit state: Section 10 of `docs/calyx/UPSTREAM_DELTA_AUDIT_2026-07-23.md`.

## Trigger and independent readback

Before the trigger, the durable audit ended at `1fc85aa7`; commits after that head had no terminal
rows. The trigger was an authenticated `git fetch origin main`, followed by semantic inspection of
every non-merge commit in the incremental range and publication of Section 10.

The audit document was then read independently and compared to the Git object graph:

```text
origin/main                                      06508c0a15ac6bd80130467639829e825408741f
rev-list --count 9894f84f..origin/main          530
rev-list --count --no-merges 1fc85aa7..main      24
Section 10 commit rows                           24
Section 10 applicable/needs-review rows           0
```

The row count is derived by matching only eight-hex commit cells in Section 10. It is not copied
from the prose count. Every one of the 24 Git commits has one terminal row.

Separate product-source readback:

```text
calyx-aster/src/vault/ledger_anchor_batch.rs:138
    pub fn anchors_for_many_with_ledger_entry(

calyx-aster/src/cf/router_scan.rs:27
    for table in shard.read_tables_oldest_first(cf) {

calyx-aster/src/mvcc/store/scan_pages.rs:47
    .scan_cf_range_keys_at(snapshot, cf, range, clock)
```

This proves the grounding stream's retained transaction requirement and the overlay-scan
correctness requirement already have native owners. The upstream plan importer itself has no
retained producer.

Separate OS readback:

```text
Win32_VideoController.Name    Intel(R) Iris(R) Xe Graphics
Win32_VideoController.PNP     PCI\VEN_8086...
nvidia-smi command present    false
```

The upstream resident CUDA CAGRA cache cannot execute on this host and its `cagra_serve` owner is
absent from the retained tree.

## Boundary and edge-case audit

### 1. Pruned-only delta

- Before: `5bd2c36d` was beyond the prior audit head and unclassified.
- Trigger: inspect its complete file delta.
- After: the row is `not applicable`; every changed path is under pruned `infra/aiwonder` Linux
  lawdemo deployment. No retained source changed.

### 2. Retained crate with an absent producer

- Before: `59006838` looked applicable because it adds public Aster production code.
- Trigger: trace every caller and compare its invariant with current Synapse code.
- After: its only producer is the pruned grounding-plan CLI; current Synapse separately proves an
  atomic Base + Anchors + Ledger owner at line 138 above. The delta is classified `not applicable
  to retained producer`, not silently imported as a second grounding identity protocol.

### 3. Hardware-specific retained delta

- Before: `f94b8be4` touched retained Forge/Sextant crates and could appear to be a host
  optimization candidate.
- Trigger: inspect the target module and independently query physical display hardware/runtime.
- After: the change targets absent resident `cagra_serve`; the host has Intel Iris Xe and no
  NVIDIA runtime. It is explicitly `not applicable`, while the environment-overridable safety
  floor in the same commit is independently reverted by `723b67b7`.

## Research readback

Exa MCP v3.4.0 successfully answered two real `web_search_exa` calls after initialization and
tool discovery: atomic durable batch/idempotency practice (7,240 returned characters) and CUDA
device-memory sizing (1,482 returned characters). The lane evidence was written to
`%TEMP%\synapse-research-lane-readback.json`.

Primary sources were fetched separately with HTTP 200 and inspected:

- PostgreSQL transactions: <https://www.postgresql.org/docs/current/tutorial-transactions.html>
- Rust `RwLock`: <https://doc.rust-lang.org/std/sync/struct.RwLock.html>
- NVIDIA CUDA memory API: <https://docs.nvidia.com/cuda/cuda-runtime-api/group__CUDART__MEMORY.html>

## Structural verification

`pwsh -File scripts/lint.ps1` passed all seven gates in both workspaces: shared lint contract,
toolchain agreement, lock-graph agreement, formatting, dependency policy, Clippy, and the public
Calyx API ratchet (363/363). `git diff --check` also passed.

## Verdict

The incremental source-history audit is complete through authenticated head `06508c0a`. All 24
commits have evidence-backed terminal decisions; none remains applicable or needs review, and the
change makes no unverified runtime-effect claim.
