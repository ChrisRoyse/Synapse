# Synapse on Calyx — Everything the Integrated System Can Do

**Status:** Target state, with a per-claim state ledger measured against the live daemon · claims dated 2026-07-15, ledger dated 2026-08-02
**Companion:** `docs/calyx/INTEGRATION_PLAN.md` (how) · this document (what it makes possible)

> ## Read §0 before any claim below
>
> Every section from §1 on describes the **target state**. §0 says, per claim, what is actually true on the deployed daemon today, and names the readback that establishes it.
>
> This split exists because of a specific failure this document has already had twice (#1683). Both times the prose was written accurately against the code, and both times it became misleading, because it described a **mechanism** rather than a **guarantee**. "KSG mutual information" is a true description of a function; it is not a statement about which of this system's lenses can be measured — and 7 of 8 could not be. The sharper case is #1953: all three assay surfaces answered, persisted to the Assay CF, and returned `sufficient=true` — over a panel whose slot 86 **is** its anchor. *The tool answers* and *the capability works* are different claims, and this document asserts the second.
>
> So no claim below is finalised until §0 carries a state marker sourced from a readback rather than from the tool list.

Synapse is a Windows-native perception/action/autonomy daemon that *captures* everything — operator activity timeline, derived episodes, mined routines, agent journals and transcripts, emitted actions, reflex audits, observations, process history — and stores it in a Calyx **association-native database** where the relationships between everything it captures are first-class, measured, grounded, and queryable. No learned embedders anywhere: every measurement is a deterministic encoder, every insight is information-theoretic (bits), every claim is anchored to a real outcome or explicitly tagged provisional. CUDA math is fail-closed when selected; CPU execution is an explicit operator choice, never an automatic degradation.

---

## 0. Capability state ledger

Measured 2026-08-02 against the live daemon — **pid 19256, binary sha256 `8E97B173646BF5BD601A109058E8CB6E2C644EEEA5A6477FC24293DA5538BFF7`** — over MCP-on-HTTP, one escalated session, one pass. Every row is a readback, not a reading of the tool list.

**Marker vocabulary.** `live` = exercised end to end and returned a usable result. `live, fails closed` = the surface works and correctly *refuses* on this corpus; the refusal is the capability behaving, and the remediation is a corpus problem, not a code problem. `partial` = the surface exists and something real is missing behind it. `not reachable` = no agent-surface entry point. `not built` = the engine crate is a dependency of zero Synapse crates.

| § | Claim | State | The readback |
|---|---|---|---|
| 1 | one vault, all classes | **live** | `calyx_panel_base_cf_rows = 102,590` across `calyx_panel_coverage_panels = 14`; `calyx_vault_latest_seq = 270,577` |
| 1 | tamper-evident history | **live** | `audit verify_chain` → `intact=true`, `entry_count=131,144`, `raw_commitment_count=140,758`, `raw_commitment_seal_count=17,577`, `raw_commitment_pending_count=4` |
| 2 | records measured into constellations | **live** | 14 panels, `calyx_panel_coverage_min_fraction = 1.0` against a `0.95` floor, `calyx_panel_census_decode_failures = 0` |
| 2 | **"adding a lens is one call"** | **not reachable** | the facade is 40 tools and contains no `add_lens`, `park_lens` or `retire_lens`. Panels are compile-time constants in `synapse_storage::constellations`. Adding a lens is a code change and a deploy (#1668) |
| 2 | capability gate admits/parks/retires by measured signal | **partial** | `storage intelligence ensemble_card` returns the verdicts (`declared_slots=12`, `deficit_bits=0.274`), but nothing consumes them: `calyx_inert_tuning_knob_count = 7` of 11, with `bit_floor_bits` and `correlation_ceiling` both `inert_hardcoded_elsewhere`. **Nothing is parked automatically** (#1668, #1883) |
| 3 | bits per lens about an outcome | **live** | `intelligence bits` on `syn-mcp-usage-v1 @ 1776006`: 12 slots, 8 measured, top lens 0.97017 bits |
| 3 | **every bits result names its instrument** | **live** | per-slot `estimator` / `estimator_selection`; the three unmeasured slots report `degenerate_column` / `insufficient_samples` rather than a zero (#1672, #1915) |
| 3 | **a lens that is the label is marked** | **live** | `anchor_source_declared=true`; slots 86/87/93 carry `anchor_source_carrier=true` with their shared fields; `total_bits 5.2625` vs `total_bits_carrier_free 3.3580` — **36% of this panel's reported bits are circular** (#1958, #1959) |
| 3 | redundancy + effective rank | **live** | `intelligence redundancy` → `effective_rank = 6.233` over 11 lenses, with named `low_signal_lenses` |
| 3 | panel sufficiency | **live, fails closed** | `intelligence sufficiency` refuses with `SYNAPSE_CALYX_ASSAY_ANCHOR_SOURCE_LEAKAGE` naming slots 86/87/93 and the remediation `excluded_slots=[86,87,93]`. A verdict over this panel *would be* circular, and refusing is the capability working (#1953, #1958) |
| 3 | synergy (interaction bits) | **live** | `intelligence synergy` → 28 pairs, `pairs_with_anchor_source_carrier=13`, each marked; `max_gain_bits 0.16396`, `max_gain_bits_carrier_free 0.16396` |
| 4 | periodicity (Lomb–Scargle + FAP) | **live** | `intelligence periodicity` on `syn-timeline-v1 @ 1900001`: `dominant_period_seconds = 94,000`, `dominant_false_alarm_probability = 0.1188`, `n_samples = 142` — a real period with an honest, **not** significant FAP |
| 4 | overdue hazard | **live** | `intelligence hazard`: `n_occurrences=921`, `mean_gap_seconds=683.0`, `overdue=false`, `overdue_threshold_seconds=3977.7`, `temporal_xterm_cf_rows_after=6` |
| 4 | directional causality (transfer entropy) | **live, fails closed** | `intelligence causality` refuses with `SYNAPSE_CALYX_TEMPORAL_GROUP_KEY_REQUIRED` — it will not build two directed streams without a `group_key` naming the partition. Correct, and it means the doc's "Slack precedes IDE" example needs a caller that supplies one |
| 4 | next-occurrence prediction feeding `assist` | **not built** | `calyx-oracle` is a dependency of **zero** Synapse crates (#1678) |
| 5 | grounding kernel | **partial** | `intelligence kernel` requires `content_slot`; the kernel path exists (`calyx-lodestar` is a `synapse-calyx` dependency) but is not exercised on any panel by the daemon |
| 5 | `kernel_answer` grounded Q&A | **partial** | the sub-operation exists; its parameters are `query_cx_id` + `max_hops`, i.e. **walk from a known record**, not the natural-language question the §5 prose promises (#1675) |
| 5 | grounding-gap report | **live** | `hygiene grounding_gap` on 1900001: `base_cf_rows=102,590`, `grounded_fraction=0.0`, `distinct_anchor_kinds=0`, 921 records present and **921 ungrounded on every slot**. The report works; it is reporting that the timeline panel carries no anchors at all |
| 6 | fused find-similar (RRF/BM25/temporal) | **live** | search generation `built` for the active panel `1900001`, `rows_covered=902`, `dense_slot_count=5`, `sparse_slot_count=3`, 3 generations maintained, 0 failed |
| 7 | blind-spot detection | **live** | `hygiene blind_spot` returns alerts with `calibration_p_value=0.0326` against `alpha=0.05` over `calibration_sample_count=921` — a calibrated verdict, not a threshold guess |
| 7 | drift detection (kernel MMD) | **live** | `hygiene drift` → 5 lenses measured with per-slot `mmd2`/`p_value`/`bandwidth`, `reference_n=645` vs `recent_n=276`, `drift_rows_persisted=5`, `drifted_lenses=0` |
| 7 | guard: conformal calibration, OOD verdicts | **live, fails closed** | `hygiene guard_calibrate` (dry run) refuses with `SYNAPSE_CALYX_GUARD_GOOD_CORPUS_INSUFFICIENT: slot 84 has 0 adjudicated good exemplar(s)`. The calibrator is reachable and refuses to fabricate a corpus — **the Guard CF is still empty and no guarded search is running** (#1677) |
| 7 | identity-locked routines | **not reachable** | no facade entry point; depends on a calibrated guard that does not exist |
| 8 | consequence what-if, abduction, completion, readiness | **not built** | `calyx-oracle` has no Synapse dependent (#1678) |
| 9 | anneal self-optimization | **not built** | `calyx-anneal` is a dependency of **zero** Synapse crates (#1681) |
| 9 | reactive triggers into `subscribe` | **partial** | the Reactive CF now has a real writer — `hygiene drift` persisted 5 rows and reports `reactive_cf_rows_after` — but **no `subscribe` consumer reads it** (#1680) |
| 9.5 | Synapse measures its own MCP usage | **live** | `mcp-usage/v1/call/` rows counted off disk with the daemon stopped: 900 written by one daemon generation, 0 in the pre-burst snapshot (#1936) |
| 10 | measured CUDA admission, explicit CPU | **live, hardware-limited** | `calyx_math_backend = cpu`, `cpu_simd_path = avx2`, fallback `SYNAPSE_CALYX_MATH_AUTO_CPU_NO_CUDA_DEVICE` ← `CUDA_DEVICE_ABSENT` (NVML absent). The admission path is exercised; **this host has no NVIDIA GPU**, so the CUDA arm cannot be verified here (#1906) |
| 11 | no silent failure | **live** | every refusal above arrived as a structured `{code, message, remediation}`, including four this probe provoked by sending the wrong parameter shape |

### What the ledger says as a whole

Three groups, and the middle one is the interesting one.

**Measurement is real.** §1, §3, §4 and §6 are live end to end and read back from physical CF rows. The bits layer is not only working but has grown the discipline to mark its own circular results.

**Several refusals are the capability, not its absence.** `sufficiency`, `causality` and `guard_calibrate` each fail closed with a named remediation. Recording these as "broken" would be as wrong as recording them as "delivered": the code is doing exactly what §11 promises, and what is missing is *corpus* — anchors on the timeline panel, a `group_key` partition, adjudicated good exemplars. That distinction is the whole reason this ledger has a `live, fails closed` marker.

**The closed loop is not closed.** §8 and §9's oracle and anneal claims have no Synapse dependent at all, §9's reactive triggers reach the store but not `subscribe`, and §2's "adding a lens is one call" is not true of the agent surface. Those four read today as shipped capabilities and are not.

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
  - **[§0: not reachable / partial]** The 40-tool facade has no `add_lens`, `park_lens` or `retire_lens`; panels are compile-time constants, so adding a lens is a code change and a deploy (#1668). The gate *computes* its verdicts (`ensemble_card`), but both of its knobs report `inert_hardcoded_elsewhere` in `health`, so nothing is parked automatically (#1883).

## 3. The system knows what actually matters (bits, not vibes)

Because real outcomes are attached as **anchors** — routine confirmations and disables, approval grants/rejections, agent end states, tool-call failures, verification results, episode interruptions, escalations — Synapse can answer, with confidence intervals:

- **Which captured signals carry real information**: mutual information in bits between any lens/field and any outcome ("does time-of-day actually predict which routines you confirm?", "which factors predict agent run failure?").
- **Which signals are redundant** — pairwise redundancy and effective rank say how many *truly independent* measurements exist (live: `effective_rank = 6.233` over 11 lenses); redundant lenses get parked automatically. **[§0: the parking is not automatic — the correlation ceiling is `inert_hardcoded_elsewhere`, #1668/#1883]**
- **Which lenses are the outcome rather than evidence about it** — a lens whose declared source fields intersect the anchor's determining fields is marked on every per-lens row, and the panel total is reported twice: with and without the carriers. On the live `syn-mcp-usage-v1` panel that gap is 5.2625 vs 3.3580 bits (#1958, #1959).
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

> **[§0: partial]** The kernel path exists (`calyx-lodestar` is a `synapse-calyx` dependency) and `intelligence kernel` / `kernel_answer` are on the facade, but neither is exercised on any panel by the daemon, and `kernel_answer` takes a `query_cx_id` + `max_hops` — a walk from a **known record**, not the natural-language question the bullets below describe (#1675). The grounding-gap report *is* live, and what it currently reports is that the active timeline panel has `distinct_anchor_kinds = 0`: 921 records, none grounded.

- **The ~1% that explains everything** — per-domain grounding kernels distill episodes/timeline/agent history to a minimal generating core, verified by a recall gate (~0.95) — simultaneously an index, a summary, and an answer path.
- **`kernel_answer`** — grounded question-answering over your own history: "what explains my Friday-afternoon context switching?", "which agent runs explain this week's token spend?" Every answer carries its evidence path (hop-scored graph walk) and grounding tags; nothing is asserted without a traceable basis.
- **Grounding-gap reports** — the system names the regions of its own corpus where it *lacks* outcomes to learn from.

## 6. Find-anything, explainably (fused structured search — no embeddings)

- **Query by example**: "find episodes like this one" — fused across *all* slots at once (Reciprocal Rank Fusion): similar numbers AND similar title keywords AND similar time-of-day, each slot's contribution reported (explainable ranking).
- **Query by fields**: partial field maps ("app≈chrome, duration long, evening") measured into query vectors.
- **BM25 lexical search** over title/text sparse slots; **HNSW** neighbors over record vectors; **temporal boosts** that nudge but never dominate.
- **Deduplication and near-duplicate detection** by meaning-of-structure, not string equality.

## 7. An immune system (the fail-closed guard)

> **[§0: mixed]** Blind-spot detection and MMD drift are **live** and calibrated (`p=0.0326` against `alpha=0.05` over 921 samples; 5 drift rows persisted). The **guard itself is not running**: `hygiene guard_calibrate` is reachable and refuses with `SYNAPSE_CALYX_GUARD_GOOD_CORPUS_INSUFFICIENT` because no slot has two adjudicated good exemplars, so the Guard CF is empty and no verdict, quarantine or identity-lock below is in force (#1677). The refusal is the design working — it will not fabricate a calibration corpus — but nothing is being guarded today.

- **Out-of-distribution detection, per slot, never averaged** — conformally calibrated thresholds from real anchored bad cases (failed runs, rejected approvals). Verdicts: accept / new-region / quarantine / refuse — always a structured answer, never a silent wrong one.
- **Agent supervision** — an agent whose event stream drifts outside the trusted region is quarantined and escalated *while it runs*.
- **Reality checking** — observations that don't fit any known region of operator behavior are flagged before autonomy acts on them.
- **Identity-locked routines** — an operator-confirmed routine's canonical form cannot silently drift; an impostor pattern is refused and surfaced.
- **Blind-spot detection** — records where one lens is confident while its neighbors disagree (mislabeled, anomalous, drifting) surface in `hygiene`.

## 8. Foresight before action (the oracle)

> **[§0: not built]** Every bullet in this section is target state. `calyx-oracle` is a dependency of **zero** Synapse crates, so no consequence tree, abduction, completion, next-occurrence prediction or readiness predicate exists on any surface (#1678). Nothing here is degraded or thin — it is absent.

- **Consequence what-if** — before a risky or novel action, a butterfly tree of grounded consequences from what historically followed similar actions (bounded depth, attenuated confidence, honesty-gated).
- **Root-cause abduction** — reverse walks from an outcome to its likely causes with grounded `n/(n+1)` confidence: "why did this evening's session go sideways?"
- **Honesty gate everywhere** — if the panel's bits cannot carry the outcome's entropy, the oracle answers *Insufficient*, with the per-sensor deficit — never a confident guess.
- **Field completion** — missing episode/agent fields imputed from trusted-region attractors, explicitly tagged inferred/provisional.
- **Readiness predicate** — a falsifiable, multi-tier "is this domain ready for autonomy" gate surfaced in `health`.

## 9. A system that improves itself, reversibly

> **[§0: not built / partial]** `calyx-anneal` is a dependency of **zero** Synapse crates, so no shadow-test, tripwire, promotion or rollback exists (#1681). Reactive triggers are **partial**: `hygiene drift` really does persist rows to the Reactive CF (5 on the live daemon, `reactive_cf_rows_after` reported), but no `subscribe` consumer reads them, so nothing is pushed anywhere (#1680).

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

> **[§0]** Of the eight vignettes below, the ones reachable on the deployed daemon today are the tamper/erase one and, in the shape actually shipped, the bits one — which now also reports whether the lens it just ranked first *is* the outcome. The consequence tree, the next-occurrence forecast and the live quarantine describe §8 and §7 capability that is not built or not calibrated. They are kept here as the target, not as a description of current behaviour.

- *"Show me everything like this episode"* → ranked, explained neighbors across 90 days, in milliseconds, on your own machine.
- *"What actually predicts when I confirm a routine?"* → "day-of-week: 0.42 bits; app sequence: 0.31 bits; time-of-day: 0.12 bits; title keywords: redundant with app (parked)."
- *"Is my panel even capable of predicting agent failures?"* → "No — 0.8 bits short; the deficit is concentrated in the tool-argument slot; propose a lens there."
- *"What happens if the agent runs this cleanup action?"* → a grounded consequence tree from history, or an honest *Insufficient*.
- *"When will my standup-notes routine fire next?"* → "Tomorrow 09:12 ± 14 min (confidence 0.83); it is not overdue today."
- *"Did anything tamper with the store? Erase last night's clipboard rows."* → chain verifies green; rows erased with tombstones; chain still green.
- *"Something feels off about this agent."* → it was quarantined 40 seconds ago: its event stream left the trusted region on the tool-call slot; escalation already raised.

- *(to the agent calling Synapse)* — your cost query just came back with a steering hint: "bound the window; the rollup answers this 100× cheaper" — because the last 40 unbounded scans measurably preceded retries.

One engine — absorbed into this repo as Synapse's own code (`calyx/`, fork-and-own; see `calyx/README.md`). Every record measured. Every association counted. Every claim grounded or labeled. Every answer explainable. Every byte verifiable.

**And the loop is not yet closed.** Measurement, association, temporal structure and search are live and read back from physical rows; the guard refuses honestly rather than running; the oracle and the self-optimizer are absent. That last sentence used to read "the loop closed: Calyx steers Synapse, its agents, and the agents using it", which was the target written in the present tense — the exact habit §0 exists to break. The state ledger is the contract: a claim is finalised when §0 carries its readback, and this document is re-measured whenever the daemon binary changes.
