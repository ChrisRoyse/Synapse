# ADR: Finalized event-time windows and source-guarded causal-map publication

Date: 2026-08-16
Status: Accepted
Issues: #2245, #2250

## Context

The causal-map producer originally loaded one MVCC source snapshot, ran every
typed estimator, published the immutable Graph artifact and serving pointer,
and only then re-read the source. A real scheduled pass observed 1,095 source
records, published, then found 1,096 records in the same closed window and
returned `SYNAPSE_CALYX_CAUSAL_MAP_SOURCE_STALE`. The reader failed closed, but
the stale candidate had already replaced the previous pointer.

A panel-wide generation guard is not sufficient. An active panel keeps
receiving events beyond the bounded analysis window, so guarding the entire
panel would prevent a long analysis from ever committing even when its exact
historical source was unchanged.

Event-time systems use watermarks to declare progress through event time while
retaining an explicit late-data policy. Flink documents both this event-time
model and allowed lateness. PostgreSQL's repeatable-read documentation likewise
establishes that a computation must remain tied to a consistent snapshot, while
write conflicts must be resolved before publication.

Primary references:

- https://nightlies.apache.org/flink/flink-docs-stable/docs/concepts/time/
- https://nightlies.apache.org/flink/flink-docs-stable/docs/dev/datastream/operators/windows/
- https://www.postgresql.org/docs/current/transaction-iso.html

## Decision

1. Autonomous causal maps use a six-hour event-time window ending two complete
   one-minute bins behind wall-clock time. This is an explicit finalization lag,
   not an ingestion-time substitution.
2. Aster returns every event-index member together with the exact Base-CF bytes
   read in the same MVCC snapshot. Those exact bytes define both the estimator
   input and a domain-separated, ordered SHA-256 source revision.
3. Artifact and pointer publication compares that bounded source revision again
   while holding Aster's process and cross-process durable commit lock. Pointer
   revision comparison, source comparison, and the Graph batch are one atomic
   boundary.
4. Any index insertion, removal, timestamp change, or Base-value change inside
   the bounded window refuses publication with
   `CALYX_EVENT_TIME_SOURCE_REVISION_CONFLICT`. New records outside the exact
   window do not conflict.
5. Scheduled maintenance may recompute the identical finalized contract once
   after the typed source-stale conflict. A second conflict fails loudly. No
   estimator, source, group, window, or threshold is substituted.
6. The independent post-commit Graph read and semantic source fingerprint check
   remain mandatory. The atomic guard prevents stale publication; readback
   proves the committed bytes and serving identity.
7. Health exposes the exact last `since`, `until`, and finalization lag, and the
   per-target action records the recomputation count.

## Consequences

- A stale computation can no longer replace a valid serving pointer.
- Active out-of-window ingestion does not starve bounded intelligence.
- Truly late in-window data is visible as an exact conflict and recomputation,
  never silently omitted.
- Event-time range reads now return the exact Base bytes they validated, avoiding
  a second read that could define a different statistical population.
- The durable lock performs one bounded range re-read before publication. The
  existing `max_records` cap bounds that work and fails closed rather than
  hashing a prefix.
