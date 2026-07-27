# Upstream Calyx-Dev Delta Audit — Phase 1 Inventory

**Issue:** [CALYX] #1760 — Audit and selectively port post-absorption Calyx-Dev production deltas
**Date:** 2026-07-23
**Author of this artifact:** automated audit agent (Phase 1 = INVENTORY ONLY; no code ported by this pass)

> This document is the executable deliverable for porting agents. Each `applicable` /
> `needs-review` row below can be actioned without re-deriving the comparison. Read the
> **Methodology** and **Confidence & limitations** sections before trusting a single row's
> verdict — the automated signal is reproducible but is corrected by hand for multi-file
> work-streams (grouped by issue number), which the file-level signal systematically mis-buckets.

---

## 1. Compared revisions (exact)

| Endpoint | Commit | Date | Subject |
|---|---|---|---|
| **Absorption baseline** (fork branch point) | `9894f84f` | 2026-07-15 | `fix(poly): reclaim fallback FSV roots at thread exit` |
| **Upstream Calyx-Dev main HEAD (compared)** | `8e1625190adaaa2f54ee2a864cb08e871310591d` (`8e162519`) | 2026-07-23 | `fix(lawdemo): remove substitute console replay (#2011)` |

**IMPORTANT — head moved past the numbers in the issue.** The issue body and the earliest
issue checkpoints compared against `313860bc` (283 commits ahead). Later checkpoints reached
`55b95e70` and ported up to `3ac0614e`/`198a6c73`. As of this audit the authenticated
`C:\code\Calyx-Dev` / `origin/main` HEAD is **`8e162519`**, which is **439 commits** ahead of
`9894f84f` (394 non-merge + 45 merges). This audit is computed against `8e162519`; any future
re-run must re-record the then-current HEAD.

```
git -C C:/code/Calyx-Dev rev-list --count 9894f84f..8e162519         # 439
git -C C:/code/Calyx-Dev rev-list --count --no-merges 9894f84f..8e162519   # 394
git -C C:/code/Calyx-Dev rev-list --count --merges    9894f84f..8e162519   # 45
```

---

## 2. Fork vs upstream crate map (what is pruned)

The Synapse-owned fork lives in-repo at `C:\code\Synapse\calyx\`. Crate membership drives the
"pruned-crate / not-applicable" exclusions.

**Kept in fork (16 crates — production surface in scope):**
`calyx-anneal`, `calyx-assay`, `calyx-aster`, `calyx-core`, `calyx-forge`, `calyx-ledger`,
`calyx-lodestar`, `calyx-loom`, `calyx-mincut`, `calyx-oracle`, `calyx-paths`, `calyx-registry`,
`calyx-search`, `calyx-sextant`, `calyx-ward`, **`calyx-lenses`** (Synapse-only, no upstream counterpart).

**Pruned from fork (11 upstream crates — commits touching only these are not-applicable):**
`calyx-buildinfo`, `calyx-cli`, `calyxd`, `calyx-fsv`, `calyx-gatebrokerd`, `calyx-hazard-soak`,
`calyx-lawdemo-web`, `calyx-mcp`, `calyx-poly`, `calyx-testkit`, `calyx-web-api`.

**CUDA is NOT pruned.** The fork retains the full CUDA surface (131 `.rs`/`.cu` files reference
CUDA; `calyx-forge/src/cuda/`, `calyx-forge/Cargo.toml` `cuda = ["dep:cudarc"]`, `cuvs-sys`).
The `9894f84f` baseline already shipped an extensive CUDA implementation, so post-baseline CUDA
commits are **refinements on an existing surface**, not new capability — they are judged on merit,
not excluded wholesale.

---

## 3. Methodology

**Step 1 — Enumerate production-only commits.** Took all 394 non-merge commits in
`9894f84f..8e162519`. For each, mapped touched files to top-level crates. Excluded a commit
when it touched **no production (`.rs`/`.cu`, non-test) file in a kept crate**. Exclusion buckets:

| Excluded bucket | Count | Meaning |
|---|---|---|
| pruned-crate-only | 66 | touches only the 11 pruned crates |
| ci/docs-only | 62 | only `.md` / `.yml` / `.github` / `docs/` |
| pruned/other-only | 48 | only pruned crates + root `Cargo.*` / workspace files |
| tests/fsv-only | 12 | only test/bench/`fsv/` files in kept crates |
| **Total excluded** | **188** | |
| **Candidates carried to Step 2** | **206** | touch kept-crate production code |

**Step 2 — Automated presence + divergence signal.** For each of the 206 candidates:
- `line-presence ratio` = fraction of the commit's **added production lines** (whitespace-normalized,
  ≥4 chars) that appear verbatim in the current fork file at the same path.
- `area status` per touched file, comparing the fork file against the `9894f84f` baseline file
  **with `#[cfg(test)]` mod blocks stripped** (the fork deleted all tests, so raw diffs are noisy):
  `BASELINE` (fork == baseline production ⇒ fork never touched this area),
  `DIVERGED` (fork rewrote it), `ABSENT` (path not present in fork ⇒ pruned/renamed).

Reproduce: scripts in the issue thread / audit scratchpad
(`classify.py`, `presence.py`, `baseline_cmp.py`, `gen_table.py`).

**Step 3 — Decision rule (automated first pass):**
- `ABSENT` touched files ⇒ **not-applicable** (surface pruned/renamed).
- line-presence ≥ 0.85 ⇒ **already-adapted**.
- `BASELINE` (fork untouched) + low presence ⇒ **applicable** (upstream delta genuinely missing).
- `DIVERGED` + low presence ⇒ **superseded** (fork owns a rewrite; confirm behavior covered).

**Step 4 — Manual work-stream correction (this is where the value is).** The file-level signal
mis-buckets multi-file features: a feature's new file may be `ABSENT` (→ wrongly not-applicable)
while its edits to a shared file read `DIVERGED` (→ wrongly superseded). Commits sharing an issue
number were regrouped and adjudicated by **reading the fork tree**. Verified corrections are in the
table's Evidence column and Section 5. Example: the `#1885` A37 stream — automation split it across
`superseded`/`not-applicable`/`applicable`; reading `calyx-assay/src/logistic/` proved the whole
stream is missing (no `conditioning.rs`, no `pipeline.rs`) ⇒ all five commits are `applicable`.

### Confidence & limitations (read this)
- **`applicable` (verified)** — evidence cites a concrete fork read (Section 5). High confidence.
- **`applicable` (signal-only)** — fork file byte-identical to baseline; the delta is provably
  absent, **but** many such rows are follow-up perf/fix commits inside a CUDA work-stream whose
  *base* feat commit is `superseded` in the fork's rewritten path. Porting them onto the fork's
  diverged base is only meaningful after diffing that path — treated as **low priority** below.
- **`superseded`** — fork DIVERGED in the touched files. This means "fork rewrote the area," **not**
  "behavior is proven equivalent." Where correctness matters, a porter should still diff.
- **`needs-review`** — fork has a *parallel* implementation (e.g. `wal/`, `plain_graph/lifecycle.rs`);
  the delta may be partially present. Requires invariant-by-invariant diff before porting.

---

## 4. Headline result

| Class | Count (of 206 candidates) |
|---|---|
| **applicable** | **45** |
| **needs-review** (parallel fork impl; adjudicate) | **9** |
| superseded (fork rewrote the area) | 71 |
| not-applicable (pruned/renamed/formatting/test/lint/docs) | 78 |
| already-adapted | 3 |

Plus **188** commits excluded at Step 1 (tests/CI/docs/pruned-crate/workspace-only).
**394 non-merge commits audited total; 439 including merges.**

**already-adapted (3):** `edbfff6a` host-wide GPU reservations, `37109555` Base page-index v4
freshness, `198a6c73` DiskANN packed raw-vector sidecar. (All previously ported; see Section 6.)

---
## 5. Ordered applicable-port list (value / risk, with dependency chains)

Ordering is by **value ÷ risk**. Each group is a coherent work-stream and should be ported as a
unit in the listed intra-group order. "Lands in" paths are relative to `C:\code\Synapse\calyx\`.

### TIER 1 — port first (high value, verified missing)

**P1. `#1885` A37 lens-block conditioning + converged logistic fits — VERIFIED applicable.**
*Issue flag confirmed.* Correctness of the A37 assay/calibration pipeline: preserves per-lens
blocks under conditioning, isolates the planted calibration signal, normalizes total lens
variation, and refuses non-converged lens-block logistic fits. **Evidence:** fork
`calyx-assay/src/logistic/` contains only `calibration.rs`, `cuda.rs`, `train.rs` — the upstream
`conditioning.rs` (387 L) and `pipeline.rs` (304 L) are absent; the whole stream is missing.
- Intra-group order (dependency chain — `f35d2142` is foundational, adds the two new files):
  1. `f35d2142` fix: preserve lens blocks in A37 conditioning (adds `logistic/conditioning.rs`, `logistic/pipeline.rs`; rewrites `logistic/{train,calibration}.rs`, `ensemble/{compute,model,redundancy,redundancy/sketch}.rs`, `forge/src/cuda/assay/logistic.rs`, `forge/src/cuda/kernels/assay.cu`)
  2. `929a4ee8` normalize total lens variation in A37 (edits `conditioning.rs`)
  3. `320cb5db` isolate A37 planted calibration signal (edits `calibration.rs`, `ensemble/compute.rs`, `logistic/calibration.rs`)
  4. `e9af7df3` require converged lens-block logistic fits (edits `conditioning.rs`, `pipeline.rs`, `train.rs`, forge cuda logistic + `assay.cu`)
  5. `ddb1e6da` lift CUDA A37 feature ceiling (forge `cuda/assay/logistic.rs`, `kernels/assay.cu`)
- **Lands in:** `calyx-assay/src/logistic/`, `calyx-assay/src/ensemble/`, `calyx-forge/src/cuda/assay/logistic.rs`, `calyx-forge/src/cuda/kernels/assay.cu`.
- **Risk:** MEDIUM — touches assay math + a CUDA kernel; fork's `logistic/{train,calibration}.rs` diverged from baseline so `f35d2142`'s edits to them need a manual 3-way merge, not a cherry-pick. Port the whole stream together; do not split.

**P2. `71e43b1e` `#1979` device-relative GPU host-cap default — VERIFIED applicable. QUICK WIN.**
The host-reservation broker defaults `host_cap_mib` to a hardcoded 12 GiB when
`CALYX_GPU_HOST_CAP_MIB` is unset; on a >12 GiB card (e.g. 32 GiB RTX 5090) this permanently
strands ~20 GiB. Fix defaults the cap to physical device capacity minus the safety floor.
- **Evidence:** fork still has `DEFAULT_HOST_CAP_MIB = 12 * 1024` (`calyx-forge/src/vram/host_reservation.rs:33`) and returns it unconditionally when the env var is unset (`host_reservation/support.rs:154`). Device-relative default NOT applied.
- **Lands in:** `calyx-forge/src/vram/host_reservation/` (default computation) + `support.rs`.
- **Risk:** LOW — localized default-value logic; the surrounding broker (`edbfff6a`) is already adapted. Standalone, no dependencies. (`b173c72a` is a docs-only comment fix; fold its comment in or skip.)

**P5. `82be2993` `#1973` spectral v3: assign disconnected nodes instead of failing closed — applicable.**
Availability/correctness fix in Lodestar spectral embedding: v3 `assignment_method` assigns
disconnected graph nodes to a partition rather than hard-failing the whole build.
- **Evidence:** fork `calyx-lodestar` spectral file byte-identical to baseline (BASELINE signal); delta absent.
- **Lands in:** `calyx-lodestar/src/` spectral/assignment path.
- **Risk:** LOW — self-contained single-area fix. Prevents a whole class of "fail closed" build aborts on disconnected corpora.

### TIER 2 — high value, requires invariant diff (parallel fork impl exists)

**P3. `#1992` WAL open/recovery hardening — needs-review (fork has parallel `wal/`).**
Four production hardening fixes to WAL lifecycle: prune covered WAL segments after
checkpoint/flush; recover `next_seq` via header-seek instead of a full floor-0 replay; bound the
ledger-hook retained-WAL scan on open; stream WAL-tail recovery (header-only tip in
`Wal::open_after`). Directly reduces vault-open latency and WAL disk growth.
- Commits (port in order; they touch the same module): `e6655c2f` (next_seq header-seek) → `4e5ded95` (stream WAL-tail on open) → `3dc9e10e` (bound ledger-hook scan) → `75e02d0e` (prune covered segments).
- **Evidence / caveat:** fork already owns `calyx-aster/src/wal/{segment,replay,stream_replay,point_read,record,batch}.rs` — a *rewritten* WAL. Some concepts (streaming replay, point reads) likely already present. **Porter must diff each invariant against the fork's `wal/` before porting; do not blind-apply.**
- **Lands in:** `calyx-aster/src/wal/`.
- **Risk:** MEDIUM-HIGH — core storage durability; fork WAL heavily diverged.

**P4. `#2000` / `#2009` graph-generation lifecycle durability — needs-review (fork has `plain_graph/lifecycle.rs`).**
Generation-store consistency: require isolated *accepted* generations, commit-and-verify lifecycle
transitions, reclaim failed/abandoned generations (including page reclaim at the writer tip), and
publish reconstructible generations atomically.
- Commits (dependency order): `c8a2bf41` (publish reconstructible generations atomically, `#1998/2000/2003`) → `2b66b536` (require isolated accepted generations, `#2000`) → `476eb838` (commit+verify lifecycle transitions, `#2000`) → `69ed1cc2` (reclaim failed generations, `#2009`) → `0b095c88` (page generation reclaim at writer tip, `#2009`).
- **Evidence / caveat:** fork has `calyx-aster/src/plain_graph/lifecycle.rs` + `physical.rs` — a parallel generation lifecycle. Adjudicate which of these invariants the fork already enforces before porting.
- **Lands in:** `calyx-aster/src/plain_graph/`.
- **Risk:** HIGH — graph durability/consistency; parallel fork implementation.

### TIER 3 — genuine but lower value / self-contained correctness (verify, then port opportunistically)
- `82be2993` is P5 above (promoted). Others (BASELINE signal, standalone-ish):
- `004d6fbc` `calyx-core` ingest: exempt compute-on-recall sidecars from the frozen-lens requirement (ingest correctness).
- `974b1abd` `calyx-core`: unify the `indexable()` slot-vector predicate (dedup + consistency; touches `calyx-search` caller).
- `b8dff972` `calyx-sextant`: vectorize DiskANN i8 candidate scoring (perf; fork has i8 infra at baseline, SIMD scoring not applied).
- `e120057d` `calyx-aster` law: gate canonical rebuild headroom.
- `afc723ca` `#2004` `calyx-aster` erase: remove cx rows from every graph generation (erase completeness — pairs with P4).
- `19dcc189` `#1902` `calyx-registry`: bind TEI clients to no-truncate meaning (only if fork's TEI path is not already superseded — registry is heavily forked; likely low yield).

### TIER 4 — applicable-by-signal but LOW PRIORITY (CUDA follow-ups on a superseded base)
These are `BASELINE`-signal `applicable` rows that are **perf/fix follow-ups inside CUDA search /
partition work-streams whose base feat commits are `superseded`** (the fork rewrote search & GPU
partition serving). Porting them in isolation onto the fork's diverged CUDA path is likely
meaningless; only revisit if a porter deliberately re-aligns the fork's CUDA serving to upstream.
`b189b49d`, `d6c8c730`, `82737827`, `4b75bebe`, `125b1673`, `20cae864`, `ed689747`, `3b3a1092`,
`90a897fc`, `4b53b226`, `9d65c5ee`, `4e87a821`, `6ee17120`, `67434bad`, `21c34a01`, `755aa6e7`,
`7c67bf83`, `d7c4821b`, `ec36b8ce`, `24acd496`, `b4e2780c`, `9b591c3e`, `f58e6365`, `8d074eae`,
`e0e4567f`, `edea6026`, `36054dc3`, `3bf22157`, `b3048abc`, `#1980` registry pooled-paragraph TEI
(`278ab569`,`12b0e704`,`1c517540`).

> **Note on `#1980` (registry segmented/pooled TEI):** a large, valuable-sounding stream, but
> `calyx-registry` + the Synapse-only `calyx-lenses` crate are among the most heavily rewritten
> areas of the fork. Most of the stream reads `DIVERGED`/`superseded`; the three `applicable` rows
> are fragments. Treat as **superseded pending a dedicated registry/lenses re-alignment**, not a
> mechanical port.

---

## 6. Already-adapted (do NOT re-port — confirmed present)

| Commit | What | Fork evidence |
|---|---|---|
| `edbfff6a` | host-wide GPU reservations (`#1890`) | `calyx-forge/src/vram/host_reservation/{state,support}.rs`, `budgeted_backend.with_host_dispatch_reservations` / `acquire_host_dispatch` present (line-presence 0.98) |
| `37109555` | Base page-index v4 freshness bound to Base CF (`#1861`) | `calyx-aster/src/base_page_index/` v4 digest; ported + locally hardened per issue #1760 checkpoints |
| `198a6c73` | DiskANN packed raw-vector sidecar (`#1990`) | `calyx-sextant` diskann single-file v2 sidecar; ported at checkpoint `3ac0614e` (line-presence 0.91) |

---

## 7. Per-commit table (all 206 candidates)

Verdict legend: **applicable** (port), **needs-review** (parallel fork impl — diff first),
**superseded** (fork rewrote area), **already-adapted**, **not-applicable**.
"Evidence" gives the fork-side proof or the automated signal (line-presence ratio / area status).

| Commit | Date | Subject | Verdict | Evidence |
|---|---|---|---|---|
| `7cc232be` | 2026-07-15 | fix(assay): accelerate dependence estimators on CUDA (#1797) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.09); fork owns rewritten area — confirm behavior covered |
| `3247e1b6` | 2026-07-15 | feat(forge): batch unique-seed TurboQuant rows on CUDA | superseded | fork DIVERGED from baseline in touched files (line-presence 0.17); fork owns rewritten area — confirm behavior covered |
| `0f7d3993` | 2026-07-15 | feat(aster): quantize streaming microbatches on CUDA | superseded | fork DIVERGED from baseline in touched files (line-presence 0.01); fork owns rewritten area — confirm behavior covered |
| `d015747c` | 2026-07-15 | fix(aster): clean batched ingest borrow | superseded | fork DIVERGED from baseline in touched files (line-presence 0.00); fork owns rewritten area — confirm behavior covered |
| `e72e8e85` | 2026-07-15 | feat(forge): add CUDA OLAP and tiled transpose | superseded | fork DIVERGED from baseline in touched files (line-presence 0.00); fork owns rewritten area — confirm behavior covered |
| `2aab0e60` | 2026-07-15 | feat(aster): dispatch OLAP and transpose to CUDA | superseded | fork DIVERGED from baseline in touched files (line-presence 0.02); fork owns rewritten area — confirm behavior covered |
| `a4469fc5` | 2026-07-15 | fix(forge): discard OLAP launch timing metadata | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `9263aa3b` | 2026-07-15 | chore(forge): format merged CUDA exports | not-applicable | chore: formatting only |
| `be0d8083` | 2026-07-15 | chore(forge): format merged CUDA exports | not-applicable | chore: formatting only |
| `43e92659` | 2026-07-15 | feat(sextant): route large skill clustering to CUDA | superseded | fork DIVERGED from baseline in touched files (line-presence 0.04); fork owns rewritten area — confirm behavior covered |
| `b75026cb` | 2026-07-15 | fix(forge): retain CUDA stream during skill launches | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `d3c46a0e` | 2026-07-15 | fix(forge): use portable infinity in skill kernel | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `8e87cf59` | 2026-07-15 | feat(forge): add resident CUDA energy descent | superseded | fork DIVERGED from baseline in touched files (line-presence 0.01); fork owns rewritten area — confirm behavior covered |
| `32a93535` | 2026-07-15 | feat(oracle): dispatch large energy descent to CUDA | superseded | fork DIVERGED from baseline in touched files (line-presence 0.49); fork owns rewritten area — confirm behavior covered |
| `f54d1eca` | 2026-07-15 | fix(forge): use portable infinity in energy kernel | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `894a6281` | 2026-07-15 | fix(forge): discard CUDA launch timing metadata | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `67c1bc37` | 2026-07-15 | fix(forge): retain CUDA stream during energy launches | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `8f7f40e0` | 2026-07-15 | test(oracle): benchmark CUDA complete against CPU | not-applicable | test/bench-only |
| `b4e2780c` | 2026-07-15 | feat(oracle): export energy CUDA crossover | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.50) |
| `73ee81da` | 2026-07-15 | chore(forge): format merged CUDA exports | not-applicable | chore: formatting only |
| `9d352653` | 2026-07-15 | chore(forge): format merged CUDA exports | not-applicable | chore: formatting only |
| `2e42af13` | 2026-07-15 | feat(sextant): add strict GPU partition phases | superseded | fork DIVERGED from baseline in touched files (line-presence 0.05); fork owns rewritten area — confirm behavior covered |
| `ade1916f` | 2026-07-15 | fix(sextant): compile CUDA partition path | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `53b170ee` | 2026-07-15 | fix(sextant): release CUDA query guard before accounting | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `97f64c3d` | 2026-07-15 | fix(sextant): satisfy cuVS kmeans tolerance contract | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `61c37e8c` | 2026-07-15 | fix(sextant): use stable cuVS routing id ABI | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `fc7d9afb` | 2026-07-15 | fix(sextant): widen CUDA routing buffer contract | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `df1abb33` | 2026-07-15 | test(sextant): cover strict CUDA partition phases | not-applicable | test-only |
| `b189b49d` | 2026-07-15 | perf(sextant): plan GPU partitions toward balance cap | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `d6c8c730` | 2026-07-15 | perf(sextant): batch strict GPU partition balance | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.13) |
| `82737827` | 2026-07-15 | perf(sextant): amortize streaming partition scans | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `d6d6570a` | 2026-07-15 | fix(sextant): bound GPU k-means workspace | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `92e09cfe` | 2026-07-15 | fix(sextant): stream GPU k-means samples | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `2f045e54` | 2026-07-15 | fix(sextant): scale strict GPU balance depth | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `183e86f4` | 2026-07-15 | fix(sextant): allow bounded GPU balance rounds | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `f0258989` | 2026-07-15 | fix(sextant): terminate strict GPU cap balancing | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `9835f182` | 2026-07-15 | perf(sextant): reuse cap-headroom GPU centroids | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `4b75bebe` | 2026-07-15 | perf(sextant): skip redundant GPU provisional scan | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.17) |
| `125b1673` | 2026-07-15 | fix(sextant): extend GPU assignment capacity probe | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `20cae864` | 2026-07-15 | fix(sextant): preserve GPU closure radius under cap pressure | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `d0796672` | 2026-07-15 | fix(sextant): balance GPU partition centroids | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `ed689747` | 2026-07-15 | fix(sextant): calibrate one-pass GPU closure support | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.10) |
| `108d0144` | 2026-07-15 | fix(sextant): calibrate sparse partition closure | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `421fb3d9` | 2026-07-15 | fix(sextant): retain one-pass partition headroom | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `3b3a1092` | 2026-07-15 | refactor(sextant): isolate partition build validation | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `c559669a` | 2026-07-15 | fix(sextant): calibrate one-pass replica closure | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `ef0b886e` | 2026-07-15 | Route Loom production weaving through CUDA batches (#1804) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.06); fork owns rewritten area — confirm behavior covered |
| `4a54d03b` | 2026-07-15 | test(olap): report issue 1519 phase timings | not-applicable | test-only |
| `4b2a57f3` | 2026-07-15 | perf(forge): reuse bounded OLAP transpose buffers | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `77b0f2c2` | 2026-07-15 | perf(forge): read transpose output through cacheable memory | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `4b53b226` | 2026-07-15 | perf(aster): append CUDA column payload in one pass | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `0ba5cb6c` | 2026-07-15 | perf(forge): read transpose columns into final output | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `e394003c` | 2026-07-15 | perf(forge): parallelize OLAP pinned staging | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `89b88f42` | 2026-07-15 | test(aster): benchmark OLAP medians with parity samples | not-applicable | test/bench-only |
| `166804dc` | 2026-07-15 | feat(search): add CUDA dense serving | superseded | fork DIVERGED from baseline in touched files (line-presence 0.06); fork owns rewritten area — confirm behavior covered |
| `7e4c9d2a` | 2026-07-15 | test(search): cover CUDA dense serving parity | not-applicable | test-only |
| `9d65c5ee` | 2026-07-15 | fix(search): keep CAGRA filters in device kernel | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.05) |
| `dde51c4e` | 2026-07-15 | fix(search): reconcile bounded device caches | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `4e87a821` | 2026-07-15 | fix(search): honor strided CAGRA datasets | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `6ee17120` | 2026-07-15 | fix(search): create CAGRA asset directories | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `fcbeb1f5` | 2026-07-15 | perf(search): batch partitioned CUDA serving | superseded | fork DIVERGED from baseline in touched files (line-presence 0.02); fork owns rewritten area — confirm behavior covered |
| `9826a21b` | 2026-07-15 | perf(search): parallelize partitioned CUDA scans | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `2fe2b127` | 2026-07-15 | fix(search): match partitioned CUDA launch geometry | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `ee6960dc` | 2026-07-15 | perf(search): snapshot partitioned CUDA assets | superseded | fork DIVERGED from baseline in touched files (line-presence 0.07); fork owns rewritten area — confirm behavior covered |
| `7664f14a` | 2026-07-15 | perf(search): expose bounded CUDA batch timing | superseded | fork DIVERGED from baseline in touched files (line-presence 0.00); fork owns rewritten area — confirm behavior covered |
| `548eae79` | 2026-07-15 | perf(search): persist compact CUDA region datasets | superseded | fork DIVERGED from baseline in touched files (line-presence 0.01); fork owns rewritten area — confirm behavior covered |
| `8a97d09a` | 2026-07-15 | fix(search): import compact dataset build metric | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `25cdad31` | 2026-07-15 | perf(search): share compact dataset CUDA stream | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `67434bad` | 2026-07-15 | fix(search): bind compact cache to payload identity | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `e91e71c9` | 2026-07-15 | perf(search): index CUDA cache generations | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `3bf268c4` | 2026-07-15 | perf(search): bound CUDA cache eviction lookup | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `b0246dae` | 2026-07-15 | perf(search): coalesce compact asset uploads | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `ee4f74c6` | 2026-07-15 | fix(search): bind compact cache to id maps | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `45b889a6` | 2026-07-15 | style(search): format CUDA serving changes | not-applicable | style: formatting only |
| `9cbc2731` | 2026-07-15 | fix(search): key compact id generations | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `90a897fc` | 2026-07-15 | fix(sextant): balance GPU primary regions | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.10) |
| `f5f4686b` | 2026-07-15 | fix(sextant): calibrate balanced GPU closure | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `1b3c9985` | 2026-07-15 | fix(search): reclaim evicted CUDA pool pages | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `d052897d` | 2026-07-15 | fix(search): bound CUDA pool across invalidations | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `6b033f0e` | 2026-07-15 | style(search): format CUDA pool helpers | not-applicable | style: formatting only |
| `0cad9572` | 2026-07-15 | Fix issue1510 clippy gate warnings | not-applicable | clippy gate fix only |
| `9fa295e9` | 2026-07-15 | Fix issue1510 search clippy gate warning | not-applicable | clippy gate fix only |
| `ac0dffaa` | 2026-07-15 | Implement sparse BM25 CUDA top-k | superseded | fork DIVERGED from baseline in touched files (line-presence 0.06); fork owns rewritten area — confirm behavior covered |
| `522d62c9` | 2026-07-15 | Add chunked CUDA MaxSim serving | superseded | fork DIVERGED from baseline in touched files (line-presence 0.05); fork owns rewritten area — confirm behavior covered |
| `21c34a01` | 2026-07-15 | fix(search): overlap frozen query lenses (#1816) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.16) |
| `d106bf14` | 2026-07-15 | Accelerate canonical MaxSim search for #1813 | superseded | fork DIVERGED from baseline in touched files (line-presence 0.23); fork owns rewritten area — confirm behavior covered |
| `f1895622` | 2026-07-15 | Reuse stable vault snapshots for search provenance | superseded | fork DIVERGED from baseline in touched files (line-presence 0.45); fork owns rewritten area — confirm behavior covered |
| `920cd3ad` | 2026-07-15 | Split merged CUDA modules under line limit | superseded | fork DIVERGED from baseline in touched files (line-presence 0.00); fork owns rewritten area — confirm behavior covered |
| `cc42ebac` | 2026-07-15 | Preserve physical ledger verification in cached view for #1818 | superseded | fork DIVERGED from baseline in touched files (line-presence 0.49); fork owns rewritten area — confirm behavior covered |
| `87286ff4` | 2026-07-16 | Bound candidate MaxSim CUDA serving for #1820 | superseded | fork DIVERGED from baseline in touched files (line-presence 0.02); fork owns rewritten area — confirm behavior covered |
| `b3421e15` | 2026-07-16 | Repair CUDA lint gate for #1822 | not-applicable | CUDA lint gate fix only |
| `2a63b0ef` | 2026-07-16 | Fix CUDA telemetry lint for #1820 | not-applicable | CUDA telemetry lint fix only |
| `4927c57f` | 2026-07-16 | Correct MaxSim telemetry visibility for #1820 | not-applicable | telemetry visibility lint fix only |
| `755aa6e7` | 2026-07-16 | perf(search): pin candidate MaxSim CUDA staging | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `9aca64df` | 2026-07-16 | fix(search): normalize async CUDA launch results | not-applicable | async CUDA launch lint fix only |
| `2b93194f` | 2026-07-16 | fix(search): satisfy CUDA release lint | not-applicable | CUDA release lint fix only |
| `7918aafe` | 2026-07-16 | fix(search): export CUDA host staging buffer | superseded | CUDA host staging export; fork rewrote search CUDA serving |
| `5693a30c` | 2026-07-16 | fix(search): remove obsolete CUDA chunk accessors | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `b2c01cb1` | 2026-07-16 | perf(search): parallelize resident CUDA stage one | superseded | fork DIVERGED from baseline in touched files (line-presence 0.06); fork owns rewritten area — confirm behavior covered |
| `e79fa35f` | 2026-07-16 | fix(search): satisfy CUDA resident lint | not-applicable | CUDA resident lint fix only |
| `7c67bf83` | 2026-07-16 | perf(search): score admitted CUDA rows only | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `78b8b85c` | 2026-07-16 | Revert "perf(search): score admitted CUDA rows only" | not-applicable | Revert of a same-day perf commit; net no-op |
| `1a83e7bd` | 2026-07-16 | perf(search): route small persisted slots explicitly | superseded | fork DIVERGED from baseline in touched files (line-presence 0.03); fork owns rewritten area — confirm behavior covered |
| `d7c4821b` | 2026-07-16 | fix(search): evict changed MaxSim generations | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `ec36b8ce` | 2026-07-16 | fix(search): invalidate MaxSim during bounded preflight | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `49080de5` | 2026-07-16 | Add explicit law search reranking | superseded | fork DIVERGED from baseline in touched files (line-presence 0.15); fork owns rewritten area — confirm behavior covered |
| `3b8ba000` | 2026-07-16 | Add resident CLI reranking with recall headroom | superseded | fork DIVERGED from baseline in touched files (line-presence 0.00); fork owns rewritten area — confirm behavior covered |
| `24acd496` | 2026-07-16 | Wire search CUDA into unified GPU builds | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `2fcc99e6` | 2026-07-16 | Preserve lexical specificity in semantic reranking | superseded | fork DIVERGED from baseline in touched files (line-presence 0.02); fork owns rewritten area — confirm behavior covered |
| `1f7ed617` | 2026-07-16 | Stabilize public CUDA reranker scores | not-applicable | stabilize public CUDA reranker scores; part of superseded rerank stream |
| `2dccad5d` | 2026-07-16 | Preserve coherent phrases in semantic reranking | superseded | fork DIVERGED from baseline in touched files (line-presence 0.02); fork owns rewritten area — confirm behavior covered |
| `94c1523f` | 2026-07-16 | Speed exact metadata reranking | superseded | fork DIVERGED from baseline in touched files (line-presence 0.03); fork owns rewritten area — confirm behavior covered |
| `8cc60356` | 2026-07-16 | Bound exact metadata retrieval work | superseded | fork DIVERGED from baseline in touched files (line-presence 0.00); fork owns rewritten area — confirm behavior covered |
| `27f7c730` | 2026-07-16 | fix(lodestar): normalize multiway spectral embedding (#1749) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.31); fork owns rewritten area — confirm behavior covered |
| `5f3913b2` | 2026-07-16 | fix(law): make kernel Answer ledger-safe (#1855) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `08603590` | 2026-07-17 | fix: preserve constellation panels in kernels (#1857) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.34); fork owns rewritten area — confirm behavior covered |
| `fb6cbf18` | 2026-07-17 | fix: publish kernel answer ledgers atomically (#1856) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.28); fork owns rewritten area — confirm behavior covered |
| `f4fa3164` | 2026-07-17 | fix: ground kernel answers in retained sources (#1859) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.22); fork owns rewritten area — confirm behavior covered |
| `0d382d80` | 2026-07-17 | feat: add typed citation kernel answers (#1858) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.00); fork owns rewritten area — confirm behavior covered |
| `e646f612` | 2026-07-18 | perf(aster): port Poly vault engine improvements | superseded | fork DIVERGED from baseline in touched files (line-presence 0.46); fork owns rewritten area — confirm behavior covered |
| `b8dff972` | 2026-07-18 | perf(sextant): vectorize DiskANN i8 candidate scoring | applicable | perf: vectorize DiskANN i8 candidate scoring; fork graph.rs at baseline (i8 infra present, SIMD scoring not applied) |
| `e120057d` | 2026-07-18 | feat(law): gate canonical rebuild headroom | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.38) |
| `9b591c3e` | 2026-07-18 | feat(registry): TEI cross-encoder rerank runtime and lens runtime provenance | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.26) |
| `31240f99` | 2026-07-18 | fix(registry): attest physical TEI model and runtime identity (#1831) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.33); fork owns rewritten area — confirm behavior covered |
| `e1e8767c` | 2026-07-18 | fix: honor frozen TEI batch limits (#1891) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.07); fork owns rewritten area — confirm behavior covered |
| `1ce8ca35` | 2026-07-18 | fix(aster,weave): stop materializing every MVCC row on vault open; adopt in weave-loom (#1862) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.03); fork owns rewritten area — confirm behavior covered |
| `f58e6365` | 2026-07-18 | fix(lodestar,cli): bound kernel-build/kernel-answer memory; stream panel index IO (#1863, #1864) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.14) |
| `4ee8dca2` | 2026-07-18 | fix(search): bound MaxSim serving working set; stop pinning whole lenses resident (#1845) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.14); fork owns rewritten area — confirm behavior covered |
| `33f65a19` | 2026-07-18 | fix(search,probe-matrix): enforce caller top-k on final fused results (#1846) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.11); fork owns rewritten area — confirm behavior covered |
| `ed9bed3d` | 2026-07-18 | fix(lodestar): teach summarize recall the sealed panel contract (#1860) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.24); fork owns rewritten area — confirm behavior covered |
| `37109555` | 2026-07-18 | fix(aster): bind Base page index freshness to Base CF content, not global ledger head (#1861) | already-adapted | fork aster base_page_index/ implements Base-CF-bound v4 freshness digest (issue #1760 checkpoint + local hardening) |
| `8d074eae` | 2026-07-18 | fix(core,cli,mcp): report missing reproduce evidence as CALYX_REPRODUCE_INSUFFICIENT (#1865) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `b14384ad` | 2026-07-18 | chore: restore canonical Rust formatting (#1900) | not-applicable | chore: canonical formatting only |
| `e0e4567f` | 2026-07-18 | fix: remove retired resident MaxSim scorer (#1901) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `19dcc189` | 2026-07-18 | fix: bind TEI clients to no-truncate meaning (#1902) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `d33141f9` | 2026-07-18 | fix: preserve full StaticLookup document meaning (#1904) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.22); fork owns rewritten area — confirm behavior covered |
| `76eed053` | 2026-07-18 | fix: add full-document legal structure and raw TF lenses (#1886 #1887 #1905) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.14); fork owns rewritten area — confirm behavior covered |
| `75e8a64f` | 2026-07-18 | fix: execute frozen algorithmic lenses in migration and assays (#1905) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.11); fork owns rewritten area — confirm behavior covered |
| `f35d2142` | 2026-07-18 | fix: preserve lens blocks in A37 conditioning (#1885) | applicable | Introduces logistic/conditioning.rs (387L) + logistic/pipeline.rs (304L); fork logistic/ has only calibration.rs,cuda.rs,train.rs -> stream absent |
| `ddb1e6da` | 2026-07-18 | fix: lift CUDA A37 feature ceiling (#1885) | applicable | Lifts CUDA A37 feature ceiling in forge cuda/assay/logistic; part of #1885 stream |
| `929a4ee8` | 2026-07-18 | fix: normalize total lens variation in A37 (#1885) | applicable | Edits conditioning.rs which is absent from fork; part of #1885 stream |
| `320cb5db` | 2026-07-18 | fix: isolate A37 planted calibration signal (#1885) | applicable | A37 planted-calibration isolation; conditioning/pipeline absent in fork |
| `e9af7df3` | 2026-07-18 | fix(assay): require converged lens-block logistic fits (#1885) | applicable | Requires converged lens-block logistic fits; conditioning/pipeline absent in fork |
| `edbfff6a` | 2026-07-18 | fix(gpu): enforce host-wide reservations (#1890) | already-adapted | fork forge vram/host_reservation/{state,support}.rs + budgeted_backend.with_host_dispatch_reservations present |
| `313860bc` | 2026-07-19 | Modularize algorithmic registry runtime (#1916) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `e625d6c4` | 2026-07-19 | Fix LAW structured ingest transaction boundaries | superseded | fork DIVERGED from baseline in touched files (line-presence 0.07); fork owns rewritten area — confirm behavior covered |
| `1475a8ce` | 2026-07-19 | Fix segmented TEI measurement and complete no-flatten typed slot path | superseded | fork DIVERGED from baseline in touched files (line-presence 0.08); fork owns rewritten area — confirm behavior covered |
| `edea6026` | 2026-07-19 | fix(registry): propagate encoder error in CUDA byte-batch closure | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.29) |
| `93fa52b1` | 2026-07-19 | feat(perms): route vault + evidence writes through owner-only private-fs | superseded | fork DIVERGED from baseline in touched files (line-presence 0.02); fork owns rewritten area — confirm behavior covered |
| `9d6a5bdf` | 2026-07-19 | feat(perms): route calyx-search persisted index writers through owner-only private-fs | superseded | fork DIVERGED from baseline in touched files (line-presence 0.00); fork owns rewritten area — confirm behavior covered |
| `95069f0a` | 2026-07-20 | feat(registry): commission long-document TEI lenses as segmented (#1944) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.17); fork owns rewritten area — confirm behavior covered |
| `004d6fbc` | 2026-07-20 | fix(ingest): exempt compute-on-recall sidecars from the frozen-lens requirement | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `974b1abd` | 2026-07-20 | Unify the indexable() slot-vector predicate in calyx-core | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.04) |
| `00fb945a` | 2026-07-20 | Add unit coverage for the resident-lease refusal codes | not-applicable | test-only (unit coverage) |
| `9fea912a` | 2026-07-20 | fix(aster): harden through a symlinked vault root | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `c761536f` | 2026-07-20 | feat(registry): typed, checkable lens-derivation versioning | superseded | fork DIVERGED from baseline in touched files (line-presence 0.05); fork owns rewritten area — confirm behavior covered |
| `58c22da5` | 2026-07-20 | fix(cli): route registry-audit failures through the derivation classifier | superseded | fork DIVERGED from baseline in touched files (line-presence 0.33); fork owns rewritten area — confirm behavior covered |
| `21371e7a` | 2026-07-20 | test(registry): derive slot/layout expectations from fixtures | not-applicable | test-only |
| `e895729b` | 2026-07-20 | Fragment oversized WAL records and byte-budget weave commits | superseded | fork DIVERGED from baseline in touched files (line-presence 0.15); fork owns rewritten area — confirm behavior covered |
| `e18b2d3d` | 2026-07-20 | Speed up token-level Multi slot similarity with bit-identical ILP | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `fc3e2ddc` | 2026-07-20 | feat(weave): explicit --candidate-device gpu for between-doc candidate search (#1963) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.39); fork owns rewritten area — confirm behavior covered |
| `82be2993` | 2026-07-20 | fix(spectral): v3 assignment_method assigns disconnected nodes instead of failing closed (#1973) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.12) |
| `71e43b1e` | 2026-07-21 | fix(gpu): device-relative default reservation cap (#1979) | applicable | fork still defaults DEFAULT_HOST_CAP_MIB=12*1024 and returns it when env unset (support.rs:154); device-relative cap NOT applied -> strands VRAM on >12GiB cards |
| `b173c72a` | 2026-07-21 | docs: correct issue ref to #1979 in host cap comment | not-applicable | docs-only |
| `b3048abc` | 2026-07-21 | fix(lawdemo-web): citation_graph collection drift, frontier-ref disambiguation, chip→card linking, non-contentless source cards; honest graph-rebuild error class (#1977) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.08) |
| `278ab569` | 2026-07-21 | feat(registry): pooled paragraph-aware TEI representation for full-length corpora (#1980) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `eae7ff2d` | 2026-07-21 | feat(registry): drive the pooled paragraph representation from the commission path (#1980) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.06); fork owns rewritten area — confirm behavior covered |
| `12b0e704` | 2026-07-21 | fix(commission): a pooled segmented manifest derives a dense output shape (#1980) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.32) |
| `f4819e1c` | 2026-07-21 | feat(registry): controlled chunk-B policy on the same paragraph boundaries (#1980) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.00); fork owns rewritten area — confirm behavior covered |
| `1ef1fc39` | 2026-07-21 | feat(commission): measure a segmented TEI lane's special-token count instead of assuming it (#1980) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.02); fork owns rewritten area — confirm behavior covered |
| `5c2be62d` | 2026-07-21 | fix(registry): accept zero-width content tokens from real tokenizers (#1980) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `87203ffc` | 2026-07-21 | fix(registry): segment long documents without ever tokenizing the whole document (#1980) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `1c517540` | 2026-07-21 | fix(registry): bound segmented embed requests by tokens, not just segment count (#1980) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `6c581f17` | 2026-07-21 | fix(registry): clamp the segmented embed token budget for a shared device (#1980) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `1c18b896` | 2026-07-21 | fix(registry): segment token indices must be the running measured-token count (#1980) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `6576c494` | 2026-07-21 | fix(registry): bound TEI segment windows by characters, not only tokens (#1983) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `d948626f` | 2026-07-22 | fix(aster): zero-syscall SST reader-cache hit path + anchors preflight point-reads (#1986) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.02); fork owns rewritten area — confirm behavior covered |
| `989741b4` | 2026-07-22 | fix(ingest): compact per-CF flush-SST debt during batch ingest (#1985) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.15); fork owns rewritten area — confirm behavior covered |
| `9f2058b3` | 2026-07-22 | fix(sextant): create diskann/spann index artifacts owner-only at write time (#1989) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.01); fork owns rewritten area — confirm behavior covered |
| `198a6c73` | 2026-07-22 | fix(sextant): pack diskann raw-vector sidecar into one durable file (#1990) | already-adapted | fork sextant diskann packs raw-vector sidecar into one durable v2 file (ported per checkpoint 3ac0614e; r=0.91) |
| `75e02d0e` | 2026-07-22 | fix(aster): prune covered WAL segments after checkpoint/flush coverage (#1992) | needs-review | WAL segment pruning after checkpoint (#1992); fork wal/ has segment.rs+stream_replay.rs -> adjudicate pruning invariant |
| `e6655c2f` | 2026-07-22 | fix(aster): recover WAL next_seq via header-seek, not a full floor-0 replay (#1992) | needs-review | WAL next_seq via header-seek (#1992); fork wal/ diverged -> confirm recovery path |
| `3dc9e10e` | 2026-07-22 | perf(aster): bound the ledger-hook retained-WAL scan on open (#1992) | needs-review | bound ledger-hook retained-WAL scan on open (#1992) |
| `4e5ded95` | 2026-07-22 | fix(aster): stream WAL-tail recovery on open; header-only tip in Wal::open_after (#1992) | needs-review | stream WAL-tail recovery on open (#1992); fork has stream_replay.rs -> likely partial |
| `077847f0` | 2026-07-22 | fix(cli,aster): bound weave-loom write side — drop latest-readback MVCC row retention + periodic flush (#1991) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.00); fork owns rewritten area — confirm behavior covered |
| `ad7d3e6c` | 2026-07-22 | chore(workspace): disable all default test/bench execution across every crate | not-applicable | chore(workspace): disable default test/bench exec; fork already tests-removed |
| `accff466` | 2026-07-22 | fix(weave): pin and batch persisted candidate search (#1997) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.18); fork owns rewritten area — confirm behavior covered |
| `b9614f87` | 2026-07-22 | fix(weave): load authenticated stored vectors without live runtimes (#2002) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.33); fork owns rewritten area — confirm behavior covered |
| `c8a2bf41` | 2026-07-22 | fix(weave): publish reconstructible generations atomically (#1998 #2000 #2003) | needs-review | weave: publish reconstructible generations atomically (#1998/2000/2003) |
| `afc723ca` | 2026-07-22 | fix(erase): remove cx rows from every graph generation (#2004) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.14) |
| `f3d91865` | 2026-07-22 | fix(aster): stream latest range pages once (#2005) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.33); fork owns rewritten area — confirm behavior covered |
| `dc89955c` | 2026-07-22 | fix(aster): fence WAL-free checkpoint sequences (#1998) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.13); fork owns rewritten area — confirm behavior covered |
| `27ee1fe0` | 2026-07-22 | fix(search): index late-interaction candidates with token ANN (#1997) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.08); fork owns rewritten area — confirm behavior covered |
| `85cdadab` | 2026-07-22 | perf(runtime): use mimalloc in production executables (#1999) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `36054dc3` | 2026-07-22 | fix(search): compile CUDA batch scoring surface (#1997) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `3bf22157` | 2026-07-22 | refactor(search): remove superseded MaxSim candidate scans (#1997) | applicable | fork files byte-identical to baseline (untouched); upstream delta absent (line-presence 0.00) |
| `5ac98278` | 2026-07-22 | fix(search): rebuild from stored-vector capability (#2002) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.50); fork owns rewritten area — confirm behavior covered |
| `870bf60a` | 2026-07-23 | fix(search): publish DiskANN metric with every graph | superseded | fork DIVERGED from baseline in touched files (line-presence 0.15); fork owns rewritten area — confirm behavior covered |
| `15ba4c00` | 2026-07-23 | fix(search): restore cuVS auto planner contract (#1997) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `128dcf6c` | 2026-07-23 | fix(aster): bound SST graph reads and derived debt (#2007) | superseded | fork DIVERGED from baseline in touched files (line-presence 0.25); fork owns rewritten area — confirm behavior covered |
| `2b66b536` | 2026-07-23 | fix(graph): require isolated accepted generations (#2000) | needs-review | graph: require isolated accepted generations (#2000); fork plain_graph/lifecycle.rs present |
| `5d0d06c5` | 2026-07-23 | fix(weave): align corpus batches with CAGRA capacity (#1997) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `476eb838` | 2026-07-23 | fix(graph): commit and verify lifecycle transitions (#2000) | needs-review | graph: commit+verify lifecycle transitions (#2000); fork lifecycle.rs present |
| `3ed83dc6` | 2026-07-23 | fix(search): batch token ANN by CAGRA query rows (#1997) | not-applicable | touched production files absent from fork tree (pruned/renamed surface) |
| `69ed1cc2` | 2026-07-23 | fix(aster): reclaim failed graph generations (#2009) | needs-review | aster: reclaim failed graph generations (#2009); fork lifecycle.rs present |
| `0b095c88` | 2026-07-23 | fix(aster): page generation reclaim at writer tip (#2009) | needs-review | aster: page generation reclaim at writer tip (#2009) |

---

## 8. Reproduction commands

```bash
# Range and endpoints
git -C C:/code/Calyx-Dev log --oneline -1 8e162519
git -C C:/code/Calyx-Dev rev-list --count --no-merges 9894f84f..8e162519

# Per-commit files (drives crate bucketing)
git -C C:/code/Calyx-Dev log --no-merges --reverse --format='COMMIT|%H|%cd|%s' \
    --date=short --name-only 9894f84f..8e162519

# Baseline tree for divergence comparison
mkdir baseline && cd baseline && git -C C:/code/Calyx-Dev archive 9894f84f crates/ | tar -x

# Inspect any commit's production delta
git -C C:/code/Calyx-Dev show <hash>
```

Analysis scripts (`classify.py`, `presence.py`, `baseline_cmp.py`, `gen_table.py`, `gen_doc.py`)
were run from the audit scratchpad; the verdict overrides (Section 5 / table) encode the manual
work-stream corrections and are the authoritative layer over the automated signal.

---

## 9. Terminal adjudication and incremental audit through `1fc85aa7`

This section supersedes the provisional `applicable` / `needs-review` language in Section 5.
The product Source of Truth is the Synapse-owned `calyx/` tree; the separate Calyx-Dev checkout
was inspected read-only and was neither merged nor synchronized.

### 9.1 Previously queued work streams

| Upstream stream | Terminal decision | Native evidence / action |
|---|---|---|
| `f35d2142`,`929a4ee8`,`320cb5db`,`e9af7df3`,`ddb1e6da` A37 | ported | Full coherent conditioning/convergence stream landed natively in `75373597`; no upstream tests or harnesses were imported. |
| `71e43b1e` device-relative host cap | ported | Native device-capacity default landed in `0779d485`; the physical-free safety floor remains authoritative. |
| `82be2993` spectral disconnected nodes | not applicable | The fork never adopted `27f7c730` row normalization or its zero-norm failure; importing the follow-up would change the fork's spectral algorithm rather than repair it. |
| `e6655c2f`,`4e5ded95`,`3dc9e10e` WAL recovery | adapted and ported | Header-only tip/torn-tail recovery, reusable-buffer CRC-validating replay, reverse bounded point reads, and streamed latest-write recovery are now native. Recovery preserves ordered WAL semantics and advances content watermarks before each recovered chunk becomes durable. |
| `75e02d0e` WAL pruning | already superseded | Synapse's bounded recycler already flushes, verifies the durable manifest floor, and truncates eligible non-active segments. Deleting through a second upstream API would duplicate the durability owner. |
| `c8a2bf41`,`2b66b536`,`476eb838`,`69ed1cc2`,`0b095c88` graph-generation lifecycle | not applicable to the retained architecture | These commits require the pruned Weave CLI producer plus generation-isolated CF/XTerm publication. No retained producer constructs or writes generation lifecycle state; Synapse serves direct `PlainGraph` collections. Importing only the reader/reclaimer would create an unreachable partial protocol. |
| `004d6fbc` compute-on-recall ingest sidecars | not applicable | Its only callers are in the pruned publish/ingest CLI surface; the retained core helper would be unreachable. |
| `974b1abd` indexability | adapted and ported | `SlotVector::is_indexable` is now the exhaustive core predicate and the search caller no longer owns a divergent duplicate. |
| `b8dff972` DiskANN i8 scoring | ported | Runtime-checked AVX2 dot/norm scoring with an f64-accumulating scalar path preserves the existing numerical contract on unsupported hosts. |
| `e120057d` rebuild disk headroom | not applicable | The upstream implementation is Unix `statvfs` policy for the pruned law rebuild CLI; it is not a Windows Synapse runtime boundary. |
| `afc723ca` graph erasure | adapted and ported | Per-context erasure now discovers every physical plain-graph collection, removes node plus both incident-edge directions, and invalidates CSR segments and metadata. This closes the retained legacy/direct graph privacy gap without importing the absent generation protocol. |
| `19dcc189` TEI no-truncate meaning | adapted and ported | TEI requests now send `truncate:false`; the request policy is part of the frozen corpus identity so old truncated meaning cannot share a LensId. |
| Tier-4 CUDA fragments | superseded | They are follow-ups to CUDA serving bases that the fork rewrote or never absorbed. No isolated fragment has a reachable, contract-compatible landing point. |

### 9.2 Incremental range after the Phase-1 head

- Prior audited head: `8e1625190adaaa2f54ee2a864cb08e871310591d`
- Authenticated Calyx-Dev head inspected: `1fc85aa7b435cff80eae8e5f2b632a4d28025a51`
- Range: `8e162519..1fc85aa7`, **67 non-merge commits**
- Excluded as docs/web/deploy/law-demo/pruned-surface only: **50**
- Retained-crate production candidates manually inspected: **17**

| Commit | Subject | Terminal decision | Evidence / native action |
|---|---|---|---|
| `4d684755` | stream physical graph acceptance counts | not applicable | Depends on the absent generation writer and pruned Weave acceptance producer described above. |
| `6e6ea46c` | bound exact recall ranking state | not applicable | Operates on `typed_kernel_index.rs`, which the fork never absorbed. |
| `b8747394` | parallelize exact total-order sorts | not applicable | Operates only on absent typed-kernel ranking. |
| `99e82b8e` | make completed kernel identity reproducible | adapted and ported | Completed identity now serializes a v2 semantic projection that excludes `kernel_id` and wall-clock `built_at_millis`; equal completed meaning is content-addressed equally. |
| `3c5db4d5` | prepare exact recall scoring | not applicable | Requires absent `slot_similarity` and typed-kernel-index surfaces. |
| `7de5883d` | stream terminal index lifecycle | not applicable | Requires absent typed-kernel immutable installer. |
| `f896477c` | reuse exact rank workspace | not applicable | Requires absent typed-kernel ranking. |
| `69a42445` | borrow recall member panels | not applicable | Requires absent typed-kernel recall path. |
| `19038cdf` | rank kernel answers without evidence churn | not applicable | Requires absent typed-kernel answer-ranking pipeline. |
| `948ca1e0` | reuse exact recall reference | not applicable | Requires absent typed-kernel recall path. |
| `cff85faf` | compare immutable streams in place | not applicable | Modifies the upstream streaming immutable installer, absent from the retained fork. |
| `48311ac0` | pin exact kernel ranking in resident | not applicable | Resident typed-kernel ranking is a pruned CLI/service contract. |
| `2298df69` | traverse bounded grounded associations | not applicable | Couples absent typed law answers to the generation-isolated graph protocol that Synapse does not produce. |
| `8290bce9` | ground inflected doctrinal queries | not applicable | Extends a provenance context-validation method/version absent from the retained provenance contract. |
| `bdd4151c` | reject incomplete injection scoring | adapted and ported | Hidden tokenizer truncation is disabled; empty, overflowed, mismatched, or >512-token encodings fail explicitly. The complete-input policy and boundary are folded into LensId. |
| `4cb88e52` | disable hidden style truncation | adapted and ported | Style tokenization disables truncation and rejects overflow; its complete-input policy and boundary are folded into LensId. |
| `fbaeb8bb` | migrate relational backend to Turso | not applicable | Migrates the pruned SQLite/law CLI backend. Synapse owns Aster-native Relational CF storage; accompanying search changes depend on absent segmented-multi/law serving surfaces. |

The 17 candidate rows above are exhaustive for retained production paths in the incremental
range. No candidate remains in `applicable` or `needs-review` state after this adjudication.
