# ADR: Preserve exact requests and add bounded causal measurement lanes

Date: 2026-08-17  
Status: superseded by `2026-08-17-action-request-semantic-atoms.md` after
production measurement exposed the remaining semantic deficit

## Context

Action panel `2_185_002` correctly made the pre-execution request observable in
slot 118 without reading status, error, response, after-state, or any other
post-treatment field. The first physical assay exposed a different failure:
the exact request vector carried 217 distinct values over 457 paired records.
Repeated request/outcome groups correctly selected the discrete estimator, but
225 occupied joint cells required 1,125 samples for the estimator's declared
five-samples-per-cell bias bound. Slot 118 was therefore unmeasured rather than
misreported.

The exact lane cannot be coarsened in place. It is the audit-grade identity and
similarity carrier, and its frozen contract has already produced physical
vectors. Conversely, treating an estimator refusal as zero, weakening the bias
bound, adding outcome fields, or adding deterministic unique noise would make a
number appear without adding trustworthy causal information.

Point-in-time feature construction is binding. [BigQuery feature serving](https://docs.cloud.google.com/bigquery/docs/feature-serving)
and [Azure ML point-in-time joins](https://learn.microsoft.com/en-us/azure/machine-learning/offline-retrieval-point-in-time-join-concepts?view=azureml-api-2)
both require training features to be values available at prediction time.
Categorical compression is also a distinct operation from deleting the exact
value; supervised compression literature such as [ICML 2019, *Learning to
Screen*](https://arxiv.org/abs/1904.13389) motivates bounded categorical views
while the exact source remains available for audit.

## Decision

Issue immutable action panel `2_185_003` and retain slot 118 unchanged. Add:

- slot 119 `syn.action.request_size_class.v1`: one of eight frozen,
  domain-defined payload byte regimes from empty through the authenticated
  1 MiB request ceiling;
- slot 120 `syn.action.request_shape_class.v1`: one of thirty deterministic
  feature-hash buckets over root kind, binned node/depth counts, binned
  container/scalar counts, and an explicit structural-overflow bit.

Both lanes are computed from the same authenticated pre-treatment request
source as slot 118. The shape signature contains no scalar value, digest,
timestamp, source key, status, error, response, or outcome. Historical rows
without a safe request remain `Absent` on all three request lanes. A malformed
digest/length envelope still fails measurement before any class is emitted.

The finite supports are estimator contracts. With a binary outcome, size has
at most 16 occupied joint cells and shape at most 60. This preserves the
Miller-Madow sparsity guard rather than weakening it. Feature-hash collisions
are deterministic and disclosed; exact identity stays in slot 118.

All nine active action slots remain typed and separate. The per-record complete
association yield rises from `7 + C(7,2) + 1 = 29` to
`9 + C(9,2) + 1 = 46`: 17 additional base measurements/cross-terms without
claiming information beyond the panel/outcome DPI ceiling.

## Consequences

- `2_185_002` remains immutable readable history and is listed as superseded.
- Guard, kernel, held-out validation, sufficiency, readiness, search, and causal
  maps must be rebuilt from physical `2_185_003` rows. No derived artifact or
  calibration is inherited.
- Slots 119/120 participate automatically in bits, redundancy, synergy, the
  exhaustive typed association map, Ward calibration, and kernel composition.
- The provenance table declares every pre-treatment source field and declares
  `outcome`/`status` only as reward-determining fields, so a future label carrier
  fails the structural leakage gate.
- Production acceptance requires strict-client MCP backfill plus separate
  Base/slot/anchor/Assay/Guard/Kernel/Anneal and lowered-artifact readbacks. A
  compile or returned tool payload is not acceptance.
