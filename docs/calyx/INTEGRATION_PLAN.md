# Calyx Integration Plan — Synapse on an Association-Native Database

**Status:** Proposed · 2026-07-15
**Scope:** Run Synapse on Calyx as its storage layer, then unlock every Calyx capability (associations, bits, kernel, guard, oracle, provenance, self-optimization) over everything Synapse captures.
**Doctrine:** `BUILDING_ON_CALYX.md` is the build handbook. Encoders only — **no learned embedders**. CUDA is fail-closed when selected; CPU is explicit, never a fallback. Manual FSV (AGENTS.md D1) gates every step.

---

## 1. What exists today

### 1.1 Synapse persists 17 logical column families through one facade

All storage flows through `synapse_storage::Db` (`crates/synapse-storage/src/lib.rs`) — `put_batch`, `put_batch_pressure_bypass`, `put_cf_batches_pressure_bypass`, `mutate_batch_pressure_bypass`, `get_cf`, `scan_cf`, `scan_cf_prefix[_from]`, `scan_cf_from`, `scan_cf_tail`, `delete_batch`, `flush`, GC, disk-pressure, sizes/counts. ~155 files consume this facade; none touch the concrete vault directly. **This facade is the storage seam.**

| CF | Contents | Key | TTL / caps |
|---|---|---|---|
| `CF_EVENTS` | replay event log (`Event`: seq, ts, source, kind, JSON data, correlations) | `ts_ns‖seq` | 24 h / 2–4 GB |
| `CF_OBSERVATIONS` | perception snapshots (`Observation`: foreground ctx, focused element, a11y nodes, entities, HUD, audio, clipboard, fs events) | seq | 6 h / 0.5–1 GB |
| `CF_PROFILES` | profile registry rows (versions, packages, trust, quarantine…) | id | none / 20–50 MB |
| `CF_MODEL_CACHE` | ONNX model blobs | id | LRU / 1–2 GB |
| `CF_SESSIONS` | MCP session continuity | id | 30 d |
| `CF_REFLEX_AUDIT` | per-reflex audit trail | `ts_ns‖seq` | 7 d |
| `CF_OCR_CACHE` | OCR memoization | region hash | 1 h |
| `CF_TELEMETRY` | metric ring buffer | ts | 6 h |
| `CF_ACTION_LOG` | emitted actions (`Action` enums + humanize params) | `ts_ns‖seq` | 24 h |
| `CF_PROCESS_HISTORY` | process start/exit | ts | 6 h |
| `CF_KV` | generic KV + secondary indexes (e.g. transcript ts-index) | prefixed | none |
| `CF_TIMELINE` | operator activity timeline (`TimelineRecord`: kind, actor, app, payload) | `ts_ns‖seq` | 90 d / 4–8 GB |
| `CF_EPISODES` | derived episodes (`EpisodeRecord`: app, document, url, titles, row/keystroke/click counts, interruptions, boundaries) | `start_ts‖ordinal` | 90 d |
| `CF_ROUTINES` | mined routines (`RoutineRecord`: steps, dow class, minute-of-day, tolerance, support/opportunity days, confidence, evidence) | `rt1-…` id | replace-all |
| `CF_ROUTINE_STATE` | operator lifecycle (confirm/disable/label transitions, confidence history) | routine id | never expires |
| `CF_AGENT_EVENTS` | agent journal (`AgentEventRecord`: kind, spawn, OTel GenAI attrs — model, token usage, tool calls, errors, end state) | `ts_ns‖seq` | TTL+GC |
| `CF_AGENT_TRANSCRIPTS` | normalized agent transcript lines | `spawn_id‖0x00‖line_no` | TTL+GC |

Payloads are JSON (`codecs.rs`; ADR-0001 forbids binary codecs — bytes must stay inspectable). Retention/caps live in `synapse-core/src/retention.rs`. The daemon opens one DB (`daemon_lifecycle.rs`, `SCHEMA_VERSION = 1`).

### 1.2 Calyx (C:\code\calyx-dev, github.com/ChrisRoyse/Calyx-Dev)

Rust 2024 workspace, **compiles clean on Windows** (verified 2026-07-15: `cargo check` on aster/registry/search/forge/assay/lodestar/ward/oracle/ledger/loom/sextant — 53 s, zero errors; aster has explicit `windows-sys` fsync/mmap/pressure paths).

| Crate | Role |
|---|---|
| `calyx-core` | ids (128-bit content addresses), constellation/slot/anchor model, engine traits (`Lens`, `Index`, `VaultStore`, `Estimator`), temporal policy, errors (`CALYX_*` closed catalog) |
| `calyx-aster` | LSM storage engine: WAL + group commit, memtables, SSTs, **MVCC snapshots/time-travel**, manifest, GC, compaction, retention, disk **pressure**, redaction/erase, vault (`AsterVault`: `write_cf_batch`, `read_cf_at`, `scan_cf_range_*`, `pin_reader`) |
| `calyx-aster::cf` | closed `ColumnFamily` enum: `Base, Collections, Relational, Document, Kv, TimeSeries, Blob, Slot, XTerm, TemporalXTerm, Scalars, Anchors, Assay, Ledger, Kernel, Guard, Recurrence, Graph, Online, Reactive, Anneal*` — `Kv` rows carry `version ‖ expires_at ‖ payload` (native TTL) |
| `calyx-registry` | lens registry: frozen contracts, determinism probes, capability cards, admit/park/retire gate, lazy backfill, placement (CPU/GPU), quantization; runtimes: **algorithmic (weightless encoders)**, static-lookup, candle/onnx/tei (embedders — unused here), commissioned |
| `calyx-forge` | math runtime: `Backend` trait (gemm/cosine/dot/l2/normalize/topk), **`CpuBackend` (AVX-512 aware) + `CudaBackend` behind `cuda` feature** (cudarc dynamic-linking), autotune, VRAM budget, quantization |
| `calyx-loom` | cross-terms, agreement graph, abundance, blind-spot detector, reactive triggers, recurrence |
| `calyx-assay` | bits engine: KSG mutual information, sufficiency, redundancy/effective rank, transfer entropy, interaction info, Lomb–Scargle periodicity, CUSUM, MMD drift, Bayesian posteriors, Granger/CCM, admission contract |
| `calyx-lodestar` | grounding kernel: SCC → betweenness → greedy FVS, kernel index/answer, recall gate, grounding gaps, domain bridges |
| `calyx-ward` | fail-closed per-slot guard: conformal calibration, verdicts (accept/new-region/quarantine/refuse), identity-lock, drift, injection lens |
| `calyx-oracle` | prediction: forward/butterfly/reverse walks, energy-descent completion, honesty gate, time-of-next-occurrence, readiness predicate |
| `calyx-ledger` | append-only hash chain, Merkle checkpoints, verify/reproduce, redaction tombstones |
| `calyx-anneal` | reversible shadow-tested self-optimization + tripwires + rollback |
| `calyx-sextant` / `calyx-search` | per-slot indexes (HNSW/DiskANN/SPANN/BM25/MaxSim), RRF fusion, planner, guarded search |
| `calyx-mincut`, `calyx-paths` | graph/path primitives |

`calyx-leapable` proves the intended embedding pattern: an application crate that depends directly on `calyx-aster + calyx-core + calyx-ledger` in-process. Synapse follows the same pattern.

---

## 2. Core decisions

1. **In-process embedding, one durable vault.** `synapse-storage` owns the sole `AsterVault` at the configured daemon DB path (default `%LOCALAPPDATA%\synapse\db-daemon\`). M3 lifecycle and health read that exact storage-owned handle; no side vault exists. No daemon-to-daemon hop; Calyx runs inside `synapse-mcp.exe`.
2. **Fork-and-own — Calyx is a blueprint, absorbed as Synapse's code.** The dependency-closed 17-crate set is absorbed into this repo at `calyx/` (nested workspace, `exclude = ["calyx"]`; apps/servers pruned at absorption — see `calyx/README.md`). It is Synapse-owned from that point: fully customizable, expected to diverge, no upstream tracking. `C:\code\calyx-dev` / upstream is reference-only; this repo never builds against it. Calyx-side work (new encoders, pipelines, hardening) is written in `calyx/crates/` as normal Synapse commits.
3. **Keep the `Db` facade, replace its guts.** `synapse-storage` keeps its exact public API. All 17 CFs are **Kv-family collections** in the vault — keys byte-identical to the original codecs, values stay JSON (ADR-0001 inspectability preserved). TTL maps to Kv `expires_at`; caps map to aster retention/GC; pressure levels map to aster pressure.
4. **Raw row + constellation, dual representation.** The verbatim JSON row remains the replay/audit source of truth. Alongside it, each intelligence-bearing record is *measured* into a constellation (slots + exact `scalars` + verbatim `metadata` + anchors + provenance) with content-addressed ids (idempotent re-ingest). This is the handbook's Phase 2 "no side store" satisfied in one engine: both live in the same vault.
5. **Encoders only — never embed the explicit.** Every lens is a frozen deterministic `AlgorithmicEncoder`-style instrument (the `Gdelt*` family in `calyx-registry` is the template for adding a `Syn*` family). No candle/onnx/tei lens is ever registered. Structured meaning is measured in **bits**, not cosine-over-embeddings.
6. **Fail-closed CUDA, explicit CPU, measured parity.** The production build enables `calyx-forge/cuda`. `math_backend = "auto"` and `"cuda"` both require a working CUDA/NVML path and fail startup on unknown, corrupt, or contended GPU state; `"cpu"` is the only CPU selection. A conservative 4 GiB bootstrap envelope (bounded by a smaller configured runtime ceiling) covers CUDA initialization, the measured retained context footprint must fit that admitted envelope, the same lease is atomically resized to the measurement, and every Forge dispatch separately reserves its concrete device-buffer shape through both the process-local budget and `%ProgramData%\Calyx\gpu-reservations\device-0`. The runtime ceiling is not held while idle, so candidate-before-handoff validation remains possible. The live ledger, exact owner/command/PID/MiB row, counters, SHA-256, physical free VRAM, and startup parity probe are exposed in `health`.
7. **Manual FSV at every step (AGENTS.md D1).** Every issue's acceptance is a manual FSV: real repo-built daemon, real MCP `tools/call` trigger, then a direct read of the physical SoT (vault bytes / ledger chain / assay rows), with evidence under `docs/fsv/`. The repository carries no automated tests, benchmarks, or FSV harnesses.

## 2.5 Adaptation gap analysis — Calyx is not integration-ready as absorbed

The absorbed crates compile and their engines are real, but a working Synapse integration requires substantial new code (the `[CALYX][ADAPT]` issues):

| Gap | Evidence | Work |
|---|---|---|
| Sync vault vs tokio daemon | `AsterVault` API is synchronous; Synapse's storage contract is an async facade with a WAL-synced group-commit batcher | async facade + batcher (#1692) |
| Windows has never carried a production workload | aster has `cfg(windows)` paths but no daemon soak; AV handles, long paths, lock semantics, sleep cycles untested | durability audit + crash-consistency soak (#1693) |
| No structured-record ingestion | registry ingest paths are text/GDELT-shaped; no JSON-record → typed-fields → panel pipeline, no declarative field schemas | structured measurement pipeline (#1694) |
| Encoder catalog mostly missing | `AlgorithmicEncoder` ships only ByteFeatures/Scalar/OneHot/AstStyle/SparseKeywords/TokenHash + GDELT; cyclic, scalar variants, multi-hot, record-vector, ordinal, binning, frequency, target/mean, lag/delta, interactions, aggregations, graph-structural do not exist | `Syn*` encoders as new calyx-registry code (#1663 + #1685) |
| GPU coverage partial | #1695 ships first-class `Backend::knn` and `Backend::paired_cosine` for Loom agreement batching; `FORGE_DEFERRED_BACKEND_OPS` now names only measured CPU-routed ops (`histogram_nmi`, `spmm_sparse_ops`, `graph_ops`, `colbert_maxsim`) | use the #1695 profile/routing contract; add more GPU ops only after a real Synapse profile proves CPU is the bottleneck |
| No Synapse bridges | CALYX_* errors, config knobs, and clock injection are not wired to Synapse's error-code regime, config, or deterministic test clock | error/config/clock bridges (#1696) |

## 3. Domain → panel → anchor mapping (the atoms)

Per the handbook's foundational principle: decompose to atoms, compute **all** base associations, anchor real outcomes.

### 3.1 Panels (all deterministic encoder lenses)

**Shared `Syn*` encoder family** (new algorithmic encoders, each a frozen content-addressed lens with a determinism probe):
- `syn-cyclic-time` — hour-of-day, day-of-week, day-of-month → sin/cos pairs
- `syn-scalar-{raw,log1p,zscore,rank}` — one lens per normalization per numeric field
- `syn-onehot-{kind,actor,boundary,endstate,errortype,level}` — low-cardinality enums
- `syn-hash-{app,process,document,urlhost,tool,model}` — high-cardinality identity via feature hashing
- `syn-sparse-title` / `syn-sparse-text` — `SparseKeywords` over window titles / message text (BM25-able)
- `syn-multihot-flags` — boolean sets (fullscreen, dwm, patterns…)
- `syn-record-vector` — unit-normed assembly of all numeric fields (find-similar without embedding)
- `syn-temporal-{recency,periodic,positional}` — calyx-core temporal lenses, retrieval-only boosts
- derived features: `syn-rate-*` (keystrokes/min, clicks/min, tokens/sec, cost/task), `syn-duration`, `syn-interruption-ratio`

**Panels per domain:**

| Domain (source CF) | Slots (examples) | Scalars (exact) | Metadata (verbatim) |
|---|---|---|---|
| Timeline events | kind one-hot, app hash, title sparse, cyclic time, actor one-hot, recency | ts | app, title excerpt |
| Episodes | app hash, document hash, url-host hash, title sparse, cyclic start-time, duration log1p+rank, counts z-score, boundary one-hots, interruption ratio, record-vector | duration_ms, keystrokes, clicks, interruptions, row_count | app, document, url, titles |
| Routines | step-app hashes, dow-class one-hot, minute-of-day cyclic, support/opportunity scalars, cadence | confidence, support_days, occurrence_count, tolerance | schedule_label, step docs |
| Agent events | kind one-hot, model hash, tool hash, error one-hot, token-usage log-scalars, cost rate, cyclic time | input/output/cache tokens, duration | model, tool_name, spawn_id |
| Agent transcript lines | role one-hot, text sparse, token scalars, positional | line_no, ts | spawn_id |
| Actions | action-kind one-hot, target hash, params record-vector, cyclic time | magnitudes, timings | profile id |
| Process history | process hash, event one-hot, cyclic time, recency | pid, uptime | path |
| Reflex audit | reflex hash, outcome one-hot, latency scalars | latency | reflex id |
| Observations (sampled) | app hash, role histogram, entity multi-hot, HUD scalars, flags | element counts, dpi | process, title |

Cache/infra CFs (`CF_MODEL_CACHE`, `CF_OCR_CACHE`, `CF_PROFILES`, `CF_SESSIONS`, `CF_KV`) are stored as plain Kv/Blob collections — no panel (no intelligence question asked of them).

### 3.2 Anchors (grounded real outcomes — bits are measured about these)

| Anchor | Kind | Source |
|---|---|---|
| Routine confirmed / disabled / labeled | label | operator via `routine` tool (CF_ROUTINE_STATE transitions) |
| Approval granted / rejected | pass/fail | `approval` tool decisions |
| Agent end state (completed/failed/killed) | label | agent journal terminal events |
| Tool call error vs success | pass/fail | agent event error_type presence |
| Verification outcome | pass/fail | `verification` tool results |
| Episode interrupted vs completed | label | boundary + interruption fields |
| Escalation raised | label | `escalation` tool |
| Token cost bucket | reward | transcript usage rollups |
| Recurrence (routine occurred on schedule) | recurrence | episode evidence matcher |

Everything not yet anchored is reported **provisional** — the grounding-gap report names where anchors are missing.

## 4. Capability build-out (Compose — no blind spots)

| Calyx subsystem | Synapse surface | What it does |
|---|---|---|
| Loom cross-terms + agreement/between-record graphs | `timeline`/`episode`/`find` | associations among apps, documents, times, agents; abundance report `n·(N + C(N,2) + 1)` |
| Assay bits/sufficiency/redundancy | new `intelligence` surface (extends `storage`/`hygiene`) | which captured fields actually predict outcomes, in bits; panel sufficiency deficits → propose-lens |
| Assay transfer entropy / Granger | `timeline` causality | "app A drives app B", agent-tool → failure arrows |
| Assay periodicity (Lomb–Scargle) + hazard | `routine` | rigorous cadence, overdue-ness, false-alarm probability — upgrades the hand-rolled miner |
| Lodestar kernel + kernel_answer | `episode`/`timeline` query, `find` | the ~1% of records that explain the corpus; grounded answers with hop-scored evidence paths |
| Sextant RRF fusion + BM25 + per-slot indexes | `find` | find-similar episodes/agent-runs/actions across all slots at once, temporal-boosted |
| Ward guard (conformal, per-slot, fail-closed) | `reality`/`verification`/`escalation` | out-of-distribution detection on observations and agent behavior; identity-lock confirmed routines; quarantine verdicts |
| Oracle predict / butterfly / reverse / completion | `assist`/`act`/`routine` | next-occurrence forecasts, consequence what-if before risky actions, root-cause abduction, honesty-gated imputation |
| Oracle readiness predicate | `health` | falsifiable per-domain "is this system ready" |
| Ledger hash chain + reproduce | `audit`/`privacy` | tamper-evident provenance for every mutation; `verify_chain`; redaction/erase honoring privacy |
| Loom reactive triggers | `subscribe` | new-region / recurs / drift events pushed to subscribers |
| Anneal | background | shadow-tested reversible tuning of fusion weights, quantization, thresholds; tripwires + rollback |
| Aster MVCC time-travel | `replay`/`storage` | consistent historical snapshots for replay debugging |
| TimeSeries rollups + OLAP columns | `cost`/`telemetry` | bounded-read cost/metric analytics at any corpus size (retires scan-bound cost queries, #1640 class) |

### 4.1 Control & steering (Calyx drives Synapse and the agents using it)

Beyond storing and analyzing, Calyx **controls** what it can do so with grounding, and steers everything else:

| Steered surface | Mechanism | Doctrine |
|---|---|---|
| Model routing (`model` tool) | bits + cost/success posteriors per model × task class | grounded ⇒ recommend with CI; ledger-logged |
| Tool selection at agent spawn (`agent` tool) | per-tool success bits + failure arrows (transfer entropy) | recommended/discouraged sets with evidence |
| Risky tool calls (destructive shell, deletes, sends) | pre-flight oracle what-if + ward OOD check | warn by default, deny per policy; honesty-gated; Insufficient ⇒ warn-only |
| Running agents | quarantine-grade drift verdicts → pause/kill recommendation via `escalation` | per-slot evidence attached |
| Routine arming / autonomy tiers | identity-lock + readiness predicate + periodicity confidence | fail closed: unready domains cannot arm |
| Proactive assist | kernel + graph next-step suggestions, overdue-hazard prompts | silent below the evidence floor |
| The MCP surface itself (`syn-mcp-usage-v1` panel) | Synapse measures its own tool calls; in-band `steering` hints in tool responses; `guide` action; annealed tool budgets & defaults | grounded+calibrated ⇒ may control; provisional ⇒ advise only; overrides become anchors |
| Hot paths (reflex/capture) | **lowering only**: frozen fingerprinted artifacts, async refresh | zero live Calyx calls in a tick |

## 5. Migration & cutover

1. Backend seam landed and now accepts only `storage_backend = "calyx"`.
2. Cutover verification reads every storage-touching tool at the Calyx vault SoT.
3. Shadow phase: daemon runs on calyx backend; FSV compares live behavior (timeline queries, episode segmentation, cost queries, replay) against expectations at the SoT.
4. Cutover: default is Calyx and the retired source directory is kept until operator confirmation.
5. Decommission: the legacy dependency and backend enum are removed; retired source data is deleted only after operator confirmation.

## 6. Verification doctrine

Every issue ends with manual FSV (never automated, AGENTS.md D1): daemon PID/bind named, real MCP `tools/call` trigger, SoT read *after* the call (vault scan / ledger `verify_chain` / assay rows / kernel report - the actual bytes), evidence doc `docs/fsv/issue-<n>-...md`. Structural support is limited to `cargo check`, `cargo fmt --check`, and clippy gates; behavioral acceptance comes only from manual FSV against the physical Source of Truth.

## 7. Issue graph (filed 2026-07-15, extended same day — epic #1684)

- **Phase 0 — Foundation & absorption:** #1652 absorb Calyx fork-and-own (landed in tree) → #1653 `synapse-calyx` vault crate → #1654 initial math runtime → #1753 fail-closed measured process/host VRAM admission.
- **Phase 0.5 — Calyx adaptation (new code — §2.5):** #1692 async vault facade + WAL-synced batcher → #1693 Windows durability hardening + crash soak → #1694 structured-record measurement pipeline → #1695 deferred forge GPU ops (profile-driven) → #1696 error/config/clock bridges.
- **Phase 1 — Parity swap:** #1655 backend seam → #1656 Kv backend (byte-identical Db API; needs #1692) → #1657 retention/TTL, #1658 disk pressure, #1659 GC, #1660 inspect/dump → #1661 migration (byte-exact verified) → #1662 Calyx-only cutover.
- **Phase 2 — Measure:** #1663 `Syn*` encoder family (new calyx-registry code; needs #1694) → #1664 timeline/episode panels, #1665 agent panels, #1666 action/reflex/process/observation panels → #1667 temporal lenses + recurrence → #1668 panel lifecycle/backfill/admission gate → #1685 graph-structural + hierarchy lenses.
- **Phase 3 — Ground:** #1669 anchor writers → #1670 grounding-gap report.
- **Phase 4 — Count/Differentiate:** #1671 loom weave/abundance → #1672 assay bits/sufficiency/synergy → #1673 transfer entropy/periodicity/CUSUM/hazard → #1674 blind spots + drift.
- **Phase 5 — Compose:** #1675 kernel + kernel_answer → #1676 fused find-similar search (+agree/disagree, MaxSim) → #1677 ward guard + identity lock → #1678 oracle + readiness → #1679 ledger provenance + erase → #1680 reactive triggers → #1681 anneal self-optimizer → #1688 TimeSeries/OLAP analytics.
- **Phase 5.5 — Control & steering (§4.1):** #1686 hot-path lowering → #1689 grounded agent/model/tool steering + risky-call gating → #1690 proactive daemon steering + readiness-gated autonomy → #1691 Calyx-steered MCP surface.
- **Phase 6 — Ops & prove:** #1687 backup/verify_restore + encryption decision → #1682 end-to-end FSV sweep + performance acceptance → #1683 documentation.
- **Tracker:** #1684 `[CALYX][EPIC]` — dependency-ordered checklist of all of the above.

When all are closed, Synapse runs fully on Calyx — as its database, its intelligence, and its grounded controller — with every capability live and FSV-verified.
