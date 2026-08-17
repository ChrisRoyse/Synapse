# Exhaustive typed causal-evidence maps

- **Status:** accepted
- **Date:** 2026-08-15
- **Issues:** #2249, #2245
- **Supersedes:** the implicit assumption that one transfer-entropy pair is the system's causal model

## Context

Calyx already carries Gaussian PC-stable skeleton discovery, partial-correlation networks, transfer entropy, linear Granger causality, convergent cross mapping, signed cross-correlation, temporal cross-K, and multivariate Hawkes branching. Synapse exposed only a single transfer-entropy comparison. When the caller omitted stream names, that path chose the two most frequent values; its shared loader stopped at a 20,000-row panel-membership prefix, ignored the public time-window fields, and did not disclose incompleteness. A successful observational estimate was reported as `grounded=true`, which confused estimator stability with identification of a structural effect.

Primary sources constrain the interpretation:

- PC structure recovery relies on Gaussian/Markov/faithfulness/causal-sufficiency/sparsity assumptions, and the Calyx implementation returns a skeleton rather than oriented effects.
- Transfer entropy and Granger are equivalent for Gaussian variables, so their agreement is not independent confirmation.
- Hawkes branching matrices describe event triggering under the point-process model; stationarity interpretation requires a subcritical spectral radius.
- Exhaustive p-value families require explicit multiplicity control.
- Observational effect claims still require consistency, exchangeability, positivity, and a declared estimand/interference model.

## Decision

1. `storage operation=intelligence sub-operation=causal_map` is the production agent surface. No new top-level MCP tool is added.
2. The request is defined by a panel, metadata `group_key`, and inclusive/exclusive source-event-time scope. Omitting both stream values means every `C(n,2)` pair; supplying both means exactly one pair; supplying one fails closed.
3. The temporal loader must read the complete requested scope. Encountering record `max_records + 1` is `SYNAPSE_CALYX_TEMPORAL_SCOPE_EXCEEDS_MAX_RECORDS`, never a partial estimate. Missing group values and excessive stream cardinality also fail rather than sample.
4. The artifact keeps estimator lanes separate, including their assumptions, samples, and typed failures. There is no combined causal score and no estimator fallback.
5. Benjamini-Hochberg q-values are persisted for each applicable p-value family. Transfer entropy and CCM retain their native confidence/convergence evidence rather than receiving invented p-values.
6. Every artifact is `observational_predictive` and `structural_effect_identified=false` unless a future version accepts and verifies a persisted intervention/identification contract. Estimator agreement alone cannot change that class.
7. The complete artifact is content-addressed under the native Calyx Graph CF (`GCMP1...`). Runtime-only estimator timestamps are excluded from the semantic bytes; source-data time remains explicit in `earliest_event_ns` / `latest_event_ns`, and upstream projection drift fails closed instead of reintroducing nondeterminism. The write is flushed and separately read byte-for-byte before the response exposes its key, SHA-256, byte count, Graph row count, and readback verdict.
8. A second Graph row (`GCMI1...`) is the materialized-view pointer for a normalized panel/group/pair/window-shape/bin/lag/FDR scope. The immutable artifact and pointer publish in one Aster batch guarded by the pointer's physical revision. An older or concurrent computation cannot regress the serving frontier.
9. `causal_map_read` is the read-only serving path. It follows the pointer, derives and hashes the artifact key, validates the complete typed artifact contract, then independently reloads and fingerprints every source event in the artifact's closed window. Missing, corrupt, cross-scope, incomplete, or source-stale state is an error; reads never recompute or fall back.
10. The derived-state owner refreshes declared native temporal populations hourly over a six-hour closed window. It does not sample high-cardinality populations: empty, one-stream, and scopes exceeding a declared measured work/storage budget are named non-publications, while storage/schema/invariant failures fail the maintenance tick. Health exposes the exact pointer/artifact/source identities only after the independent read succeeds.
11. BH adjustments are family-local and explicitly disclose their validity boundary: FDR control assumes independent or positive-regression-dependent p-values within the named family. No arbitrary-dependence or cross-family error-rate claim is made.
12. Exact panel membership is independent of retrieval admission. Queryable panels use their normal search generation. For a finite-only panel, the mutating producer first ensures a hash-sealed membership-only generation whose manifest has zero retrieval slots. It is built when absent; otherwise it is reopened and validated before reconciliation. A valid generation whose bounded delta cannot be reconstructed because it predates the recovered change-history floor, or whose measured delta exceeds the hard reconciliation bound, is rebuilt from authoritative Base rows and reconciled again. The read-only serving path never builds or repairs it. A present corrupt, wrong-panel, future, or otherwise invalid generation fails closed and remains preserved. This removes the accidental requirement that a temporal population possess meaningful ANN geometry before exact analytics can read it.
13. Foreground MCP whole-corpus calls and autonomous maintenance share the same one-permit semaphore but not the same wait contract. Autonomous GC, pressure, and derived-state passes wait fairly until admitted. A foreground tool waits at most one second for admission and then returns `STORAGE_MAINTENANCE_BUSY`, naming the active operation, its observed ownership duration, the admission budget, and proving its closure was not dispatched. The active owner is tracked under a generation-guarded RAII record and cleared before its owned permit drops. This keeps the single-working-set memory invariant while preventing an MCP transport timeout from erasing a queued causal request before it starts.
14. Integer-valued occurrence streams select the discrete plug-in transfer-entropy estimator under its declared auto rule. Strict CUDA executes that estimator natively: exact dense state codes feed batch-private integer histograms; small alphabets use dynamic shared memory and larger valid alphabets use explicitly VRAM-budgeted global rows; entropy and Miller-Madow terms use a fixed block reduction. Symbol interning and seeded selection construction remain deterministic host control work, but no entropy estimate runs on CPU and no failure substitutes continuous KSG. GPU allocation, launch, index, alphabet, or numerical failures remain typed terminal lane evidence.
15. `agent operation=recommend_tools` consumes the independently read rolling
    `syn-mcp-usage-v1:mcp_usage_tool` generation. It joins client-qualified
    `mcp__synapse__<route>` names to canonical daemon routes, overlays the task
    class's real success/failure counts, and serves relevant pair lanes plus
    PC/partial/Hawkes context, relevant family-local BH decisions, source
    fingerprint, and Graph pointer/artifact hashes. It does not use
    observational arrows to rewrite empirical success posteriors. A never-built
    generation is named provisional; stale, corrupt, cross-bound, or incomplete
    state fails the recommendation rather than falling back.
16. Artifact v3 removes the unrelated 16-stream ceiling. Admission derives and
    persists checked `C(n,2)` pair rows, aligned stream cells, pair/lag evidence
    points, Granger and signed-correlation hypothesis capacities, and a
    conservative PC-stable conditional-test upper bound over both frozen
    endpoint neighborhoods. Every output vector is fallibly pre-reserved. A
    bounded JSON writer refuses before 32 MiB, leaving headroom inside Aster's
    64-MiB WAL record boundary; an independent reader re-derives every resource
    field and rejects over-budget or mismatched bytes. Resource refusal happens
    before estimator publication and never licenses sampling, pair omission, or
    conditioning-set omission.

## Consequences

- Large or heterogeneous calls must narrow their physical source-event scope instead of receiving a biased prefix.
- A lane can be unresolved because its assumptions or quorum are not met while the map remains a complete account of what was and was not measurable. The persisted typed error is evidence, not a silent substitute.
- Consumers can use predictive arrows for explanation and hypothesis generation, but structural control requires a separately identified causal contract.
- Repeating the same request over byte-identical source rows reuses the same Graph key and value digest; invocation time belongs in runtime provenance, not the content-addressed artifact.
- Existing periodicity, drift, hazard, and targeted causality calls inherit the source-event window and complete-scope refusal, removing the same silent-truncation class from the shared loader.
- Consumers use `causal_map_read` for a current materialized generation instead of recomputing estimators on each query. A stale generation remains physically auditable but cannot be served as current.
- Autonomous publication supplies system-wide causal evidence without turning observational arrows into search weights, guard authorization, or intervention effects. Those policy decisions require their own identified contract.
- Finite-only event panels no longer fail merely because no search manifest exists, and they do not acquire fake indexes as the price of becoming analyzable. The one-time membership build scans the physical Base population under one pinned reader, atomically publishes only the sealed identity filter, and subsequent calls use bounded panel reconciliation.
- A busy autonomous pass is now an explicit, retryable concurrency state rather than a five-minute silent wait. Tokio's fair semaphore continues to order admitted owners, and timing out the acquisition safely removes only the foreground waiter's queue position; no blocking closure, estimator, or storage mutation has started.
- Strict-CUDA causal maps no longer strand transfer entropy on the exact integer data for which discrete TE is required. Integer atomics make histogram counts scheduling-independent; fixed reduction order bounds the floating-point surface; device-room accounting prevents an oversized alphabet/bootstrap batch from becoming an implicit CPU route or an uncontrolled allocation.
- Tool steering now receives the complete typed causal context already mined by
  Calyx instead of the obsolete `provisional_no_transfer_entropy_assay`
  placeholder. The response still labels these arrows observational/predictive;
  a tool appearing before a failed tool is not thereby declared a structural
  cause of failure.
- Stream names are no longer treated as a proxy for cost. A 19-stream action
  universe is admissible when its exact 171 pairs and conditioning work fit;
  a smaller but extremely long/high-lag universe can still refuse on the actual
  cell, evidence-point, PC-test, allocation, or serialized-byte dimension. The
  persisted accounting makes that boundary auditable by consumers and health.

## References

- Kalisch and Bühlmann, “Estimating High-Dimensional Directed Acyclic Graphs with the PC-Algorithm,” JMLR 8 (2007).
- Barnett, Barrett, and Seth, “Granger Causality and Transfer Entropy Are Equivalent for Gaussian Variables,” Physical Review Letters 103 (2009).
- Xu, Farajtabar, and Zha, “Learning Granger Causality for Hawkes Processes,” ICML/PMLR 48 (2016).
- Embrechts and Kirchner, “Hawkes Graphs,” Theory of Probability and Its Applications 62 (2018; preprint 2017).
- Benjamini and Hochberg, “Controlling the False Discovery Rate,” JRSS B 57 (1995).
- Hernán and Robins, *Causal Inference: What If* (living edition).
- Tokio `Semaphore` and `time::timeout` API documentation (fair queueing, owned-permit lifetime, and acquire cancellation semantics).
- NVIDIA, *CUDA Programming Guide*, histogram/shared-memory/atomics guidance: <https://docs.nvidia.com/cuda/cuda-programming-guide/02-basics/writing-cuda-kernels.html>.
- NVIDIA, *CUDA C++ Best Practices Guide*, coalescing and shared-memory guidance: <https://docs.nvidia.com/cuda/cuda-c-best-practices-guide/>.
- Colombo and Maathuis, “Order-Independent Constraint-Based Causal Structure
  Learning,” JMLR 15 (2014): <https://www.jmlr.org/papers/volume15/colombo14a/colombo14a.pdf>.
- Rust standard library `Vec::try_reserve_exact` (fallible preallocation):
  <https://doc.rust-lang.org/std/vec/struct.Vec.html>.
- `serde_json::to_writer` (serialization into an explicit bounded writer):
  <https://docs.rs/serde_json/latest/serde_json/fn.to_writer.html>.
