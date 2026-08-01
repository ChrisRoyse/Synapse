# Synapse on Calyx — Everything the Integrated System Can Do

**Status:** Target state · 2026-07-15 · realized when the `[CALYX]` issue graph is closed
**Companion:** `docs/calyx/INTEGRATION_PLAN.md` (how) · this document (what it makes possible)

Synapse is a Windows-native perception/action/autonomy daemon that *captures* everything — operator activity timeline, derived episodes, mined routines, agent journals and transcripts, emitted actions, reflex audits, observations, process history — and stores it in a Calyx **association-native database** where the relationships between everything it captures are first-class, measured, grounded, and queryable. No learned embedders anywhere: every measurement is a deterministic encoder, every insight is information-theoretic (bits), every claim is anchored to a real outcome or explicitly tagged provisional. CUDA math is fail-closed when selected; CPU execution is an explicit operator choice, never an automatic degradation.

---

## 1. A single universal store

- **One vault, one engine.** All 17 logical data classes and every native intelligence column family live in the Calyx vault at the configured daemon DB path (default `%LOCALAPPDATA%\synapse\db-daemon\`): LSM core, WAL + group commit, MVCC snapshots, crash-safe manifest — embedded in-process in `synapse-mcp.exe`. Health, maintenance, shutdown, and storage inspection all use this same process-local owner.
- **Bounded physical maintenance.** Every storage-GC pass first proves the locked manifest coverage, then asks the one live group-commit WAL owner to recycle only fully durable, non-active segments under explicit segment/fsync budgets. The returned inventory is physical evidence (before/after bytes, candidates, exact recycled paths); zero budgets, coverage drift, and I/O failures are hard errors.
- **File-count-aware native compaction.** Maintenance measures both byte debt and SST-count debt, selects the worst native column families, and merges an oldest contiguous prefix of at most 2,048 files per CF. This preserves newest-wins ordering while draining tiny-file read amplification. Output is named in the selected commit domain; manifest coverage, exact input deletion, cache invalidation, and an exclusive affected-CF router refresh are mandatory. A missing, aliased, malformed, or undeletable input is a hard error.
- **Complete logical maintenance API.** `compact_cf` and `compact_cf_range` are implemented for Calyx. Because Synapse column families are namespaced inside physical Aster `Kv`, the engine validates the requested logical namespace/range and compacts the authoritative physical KV surface, including tombstone pruning; invalid ranges fail before mutation.
- **Observable crash recovery.** WAL replay publishes physical bytes replayed versus total bytes at open, including start, 4 MiB progress boundaries, and completion, so a large recovery can be distinguished from a frozen daemon without bypassing the real runtime.
- **Concurrent by engine authority.** Synapse clones the process-local vault handle under a lifecycle-only read lock, then relies on Calyx's own commit/router locks for physical work. Long maintenance no longer holds an unrelated outer mutex across all MCP reads and writes.
- **Everything Synapse's storage did before, preserved exactly**: byte-identical keys, JSON values you can inspect, per-class TTLs (24 h events → 90 d timeline → never-expiring operator decisions), soft/hard byte caps, oldest-first GC, 4-level disk-pressure shedding, schema versioning, dump/inspect tooling with redaction.
- **Calyx adds:**
  - **Time-travel reads** — MVCC snapshots let `replay` and debugging read the store *as it was*, consistently.
  - **Tamper-evident history** — intelligence-bearing mutations are entries in an append-only hash chain; high-frequency raw CF commits atomically retain fixed-size commitment rows that are Merkle-sealed into that chain at checkpoint cohorts. `audit` `verify_chain` independently reads both physical CFs and fails closed on a chain or cohort mismatch. See [RAW_BATCH_PROVENANCE.md](RAW_BATCH_PROVENANCE.md).
  - **Provable erasure** — `privacy` erase uses redaction tombstones: the content is unrecoverable, yet the provenance chain still verifies. Deletion you can audit.
  - **Reproducibility** — derived artifacts (kernels, calibrations) can be re-derived on demand with a bounded drift check: the system can *prove* its own outputs.

## 2. Every record is measured, not just stored (constellations)

Each intelligence-bearing record — timeline event, episode, routine, agent event, transcript line, action, reflex fire, process event, sampled observation — is measured through a panel of frozen deterministic lenses into a **constellation**:

- **Exact scalars** (durations, keystroke/click counts, token usage, costs, latencies) kept verbatim — auditable, filterable, never blurred into a vector.
- **Typed slots** from the `Syn*` encoder family: cyclic time-of-day/day-of-week, multiple scalar normalizations, one-hot enums, feature-hashed identities (app, document, URL host, tool, model), sparse keyword vectors over titles/text, multi-hot flags, unit-normed record vectors, derived rates.
- **Verbatim metadata** (app names, titles, models, spawn ids) for exact filtering and display.
- **Idempotent by construction** — content-addressed ids mean re-ingestion and re-segmentation never duplicate.
- **Hot-swappable panels** — adding a new measurement to the whole system is *one call*: new records measure immediately, history backfills lazily in the background, and the capability gate admits/parks/retires lenses by *measured* signal, not opinion.

## 3. The system knows what actually matters (bits, not vibes)

Because real outcomes are attached as **anchors** — routine confirmations and disables, approval grants/rejections, agent end states, tool-call failures, verification results, episode interruptions, escalations — Synapse can answer, with confidence intervals:

- **Which captured signals carry real information**: mutual information in bits between any lens/field and any outcome ("does time-of-day actually predict which routines you confirm?", "which factors predict agent run failure?").
- **Which signals are redundant** — pairwise redundancy and effective rank say how many *truly independent* measurements exist; redundant lenses get parked automatically.
- **Whether the panel is sufficient** — `I(panel; outcome) ≥ H(outcome)`: can the captured data explain the outcome at all? If not, the deficit names which measurement is short and by how many bits — a concrete to-do list for new lenses.
- **Honesty by default** — below the sample floor, results are tagged provisional; the system never dresses up thin evidence as knowledge.
- **The right instrument per lens** — Synapse's panels deliberately mix explicit encoders (one-hot, hash, cyclic, ordinal) with continuous ones (record vectors, rank scalars), and the two need different estimators. A k-nearest-neighbour estimator is *undefined* on a categorical column, because many samples sit at exactly the same coordinate and its k-th neighbour radius is zero; a contingency-table estimator is exact there but needs a bias correction to be honest. Every bits result therefore names the instrument that produced it, the rule that chose it, and the column cardinality the rule keyed on — so a number is comparable across a mixed panel instead of silently meaning two different things (#1672).

## 4. Temporal and causal understanding of the operator's world

- **Direction, not just correlation** — transfer entropy with lag sweeps turns "Slack and the IDE co-occur" into "Slack activity *precedes and drives* IDE context switches", including agent-tool → failure arrows.
- **Rigorous rhythm detection** — Lomb–Scargle periodograms with permutation false-alarm probabilities replace hand-rolled cadence stats in routine mining: real periods, honest confidence.
- **Overdue awareness** — renewal hazard per confirmed routine: "the Tuesday report routine is now 40 minutes overdue against its historical cadence."
- **Change detection** — CUSUM change-points and MMD drift alarms notice when behavior *shifts* (new job rhythm, new tool habits) and trigger guard recalibration.
- **Next-occurrence prediction** — the oracle forecasts when a routine will next fire (cadence median, regularity-weighted confidence, interval), feeding proactive `assist`.

## 5. Grounded answers over operator history (the kernel)

- **The ~1% that explains everything** — per-domain grounding kernels distill episodes/timeline/agent history to a minimal generating core, verified by a recall gate (~0.95) — simultaneously an index, a summary, and an answer path.
- **`kernel_answer`** — grounded question-answering over your own history: "what explains my Friday-afternoon context switching?", "which agent runs explain this week's token spend?" Every answer carries its evidence path (hop-scored graph walk) and grounding tags; nothing is asserted without a traceable basis.
- **Grounding-gap reports** — the system names the regions of its own corpus where it *lacks* outcomes to learn from.

## 6. Find-anything, explainably (fused structured search — no embeddings)

- **Query by example**: "find episodes like this one" — fused across *all* slots at once (Reciprocal Rank Fusion): similar numbers AND similar title keywords AND similar time-of-day, each slot's contribution reported (explainable ranking).
- **Query by fields**: partial field maps ("app≈chrome, duration long, evening") measured into query vectors.
- **BM25 lexical search** over title/text sparse slots; **HNSW** neighbors over record vectors; **temporal boosts** that nudge but never dominate.
- **Deduplication and near-duplicate detection** by meaning-of-structure, not string equality.

## 7. An immune system (the fail-closed guard)

- **Out-of-distribution detection, per slot, never averaged** — conformally calibrated thresholds from real anchored bad cases (failed runs, rejected approvals). Verdicts: accept / new-region / quarantine / refuse — always a structured answer, never a silent wrong one.
- **Agent supervision** — an agent whose event stream drifts outside the trusted region is quarantined and escalated *while it runs*.
- **Reality checking** — observations that don't fit any known region of operator behavior are flagged before autonomy acts on them.
- **Identity-locked routines** — an operator-confirmed routine's canonical form cannot silently drift; an impostor pattern is refused and surfaced.
- **Blind-spot detection** — records where one lens is confident while its neighbors disagree (mislabeled, anomalous, drifting) surface in `hygiene`.

## 8. Foresight before action (the oracle)

- **Consequence what-if** — before a risky or novel action, a butterfly tree of grounded consequences from what historically followed similar actions (bounded depth, attenuated confidence, honesty-gated).
- **Root-cause abduction** — reverse walks from an outcome to its likely causes with grounded `n/(n+1)` confidence: "why did this evening's session go sideways?"
- **Honesty gate everywhere** — if the panel's bits cannot carry the outcome's entropy, the oracle answers *Insufficient*, with the per-sensor deficit — never a confident guess.
- **Field completion** — missing episode/agent fields imputed from trusted-region attractors, explicitly tagged inferred/provisional.
- **Readiness predicate** — a falsifiable, multi-tier "is this domain ready for autonomy" gate surfaced in `health`.

## 9. A system that improves itself, reversibly

- **Anneal self-optimization** — fusion weights, quantization levels, guard thresholds, index parameters tuned by shadow-testing on held-out replay, gated by tripwires and per-metric non-regression, promoted by pointer swap with ledger record — and every promotion can be rolled back byte-identically.
- **Measured compression** — slots quantize only as far as recall/bits/false-accept hold; the store refuses to trade intelligence for space silently.
- **Reactive triggers** — after each ingest: new-region (first-ever territory), recurs (known pattern again), drift — pushed live through `subscribe`, quarantine-grade events through `escalation`.

## 9.5 Calyx steers and controls (the closed loop)

The substrate doesn't just answer questions — it drives decisions, under a strict doctrine: **grounded + calibrated may control; provisional may only advise;** everything ledger-logged, reversible, and operator-overridable (and every override becomes an anchor that retrains the steering).

- **Model routing** — the `model` tool recommends models per task class from measured success/cost bits with confidence intervals, not vibes.
- **Tool steering** — agents spawn with recommended/discouraged tool sets backed by per-tool outcome bits and failure arrows; the 40-tool schema budget is curated by measured usage instead of hand tuning.
- **Risky-call gating** — destructive shell/delete/send calls get a pre-flight grounded consequence tree + out-of-distribution check: warn by default, deny per policy, honest *Insufficient* when evidence is thin.
- **Live agent intervention** — a running agent whose event stream drifts out of the trusted region is quarantined with pause/kill recommendations and per-slot evidence.
- **Steering the agent that uses Synapse** — Synapse measures its *own* MCP usage as a corpus; each completed call publishes its immutable source record, native constellation, outcome anchor, and grounding-ledger entry in one Calyx WAL/MVCC commit, then independently reads the source, constellation, and exact physical `(CxId, AnchorKind)` row back. The CxId is recomputed from the framed source identity before commit, and append-only source-key absence is the uniqueness Source of Truth; a redundant missing-key search through historical Base SSTs is not placed on the write path. The exact-key anchor proof avoids turning a one-row invariant into a database-wide range scan; validated SST key bounds also prune non-overlapping files from range and paged-range reads whenever lookup metadata is available. Tool responses carry in-band, evidence-tagged `steering` hints (next-best call, cheaper parameterization, misuse warnings), a `guide` action returns the kernel-backed optimal usage pattern, and defaults/tool-sets are annealed (shadow-tested, reversible).
- **Autonomy gating** — routine arming and autonomy-tier escalation require identity-lock + the readiness predicate; unready domains fail closed.
- **Hot-path safety** — reflex and capture ticks never call Calyx live; they consume only lowered, fingerprinted frozen artifacts, refreshed asynchronously.

## 10. Hardware posture

- **Measured CUDA admission, explicit CPU** — `auto`/`cuda` run vector and association math through CUDA only after an NVML-measured retained-footprint reservation and an exact per-dispatch device-buffer reservation are admitted in both process-local and host-wide ledgers. Contention, corrupt state, missing CUDA/NVML, or unprovable free memory fails with a structured code before output mutation. `cpu` deliberately selects the AVX-512-aware SIMD backend. `health` rereads and reports the physical ledger, current free memory, reservation identity, counters, and parity probe.
- **Lightweight by design** — encoders are weightless (no model downloads, no inference servers); the only heavy math is linear algebra the host already does; the ONNX/candle embedder runtimes that ship with Calyx stay dormant.

## 11. Trust properties, end to end

| Property | Mechanism |
|---|---|
| Nothing is claimed without grounding | anchors + provisional tagging + honesty gate |
| No answer is unexplainable | per-slot contributions, evidence paths, answer traces |
| No mutation is deniable | semantic Ledger entries + raw-commitment CF + checkpoint-cohort Merkle seals + independent `verify_chain` readback |
| No deletion is fake | redaction tombstones — erased content, intact chain |
| No silent failure | closed `CALYX_*`/structured error catalog, fail-closed everywhere |
| No frozen thing mutates | content-addressed lenses/records; drift ⇒ new identity |
| No unverified claim ships | manual FSV at the physical SoT for every issue (AGENTS.md D1) |

## 12. What this feels like in practice

- *"Show me everything like this episode"* → ranked, explained neighbors across 90 days, in milliseconds, on your own machine.
- *"What actually predicts when I confirm a routine?"* → "day-of-week: 0.42 bits; app sequence: 0.31 bits; time-of-day: 0.12 bits; title keywords: redundant with app (parked)."
- *"Is my panel even capable of predicting agent failures?"* → "No — 0.8 bits short; the deficit is concentrated in the tool-argument slot; propose a lens there."
- *"What happens if the agent runs this cleanup action?"* → a grounded consequence tree from history, or an honest *Insufficient*.
- *"When will my standup-notes routine fire next?"* → "Tomorrow 09:12 ± 14 min (confidence 0.83); it is not overdue today."
- *"Did anything tamper with the store? Erase last night's clipboard rows."* → chain verifies green; rows erased with tombstones; chain still green.
- *"Something feels off about this agent."* → it was quarantined 40 seconds ago: its event stream left the trusted region on the tool-call slot; escalation already raised.

- *(to the agent calling Synapse)* — your cost query just came back with a steering hint: "bound the window; the rollup answers this 100× cheaper" — because the last 40 unbounded scans measurably preceded retries.

One engine — absorbed into this repo as Synapse's own code (`calyx/`, fork-and-own; see `calyx/README.md`). Every record measured. Every association counted. Every claim grounded or labeled. Every answer explainable. Every byte verifiable. And the loop closed: Calyx steers Synapse, its agents, and the agents using it.
