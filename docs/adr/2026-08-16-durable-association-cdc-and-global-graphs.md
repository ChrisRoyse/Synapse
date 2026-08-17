# ADR: Durable association CDC and globally valid graph state

- **Status:** accepted
- **Date:** 2026-08-16
- **Issue:** #2255
- **Supersedes:** process-local association cursors and batch-local graph publication

## Context

The association maintainer consumed Aster's process-local changed-key journal
with a process-local cursor. Checkpoint/recovery can legitimately discard that
journal, so a cursor below its recovered floor was permanently unenumerable.
Restarting could also initialize the cursor at the current sequence and silently
bless records that had never been woven.

Two derived products compounded the loss. A bounded delta weave overwrote the
panel agreement edge with that batch's mean, and its k-nearest-neighbour edges
compared only records inside the batch. Neither value described the complete
panel. Recomputing the global agreement mean after every recovery chunk would
be correct but population-proportional.

PostgreSQL replication slots separate a consumer's durable acknowledgement
(`confirmed_flush_lsn`) from the retained-log floor (`restart_lsn`). Debezium
incremental snapshots publish bounded chunks while ordered live changes keep
flowing. Those are the relevant recovery contracts, not a best-effort in-memory
journal.

Primary references:

- https://www.postgresql.org/docs/19/view-pg-replication-slots.html
- https://www.postgresql.org/docs/current/runtime-config-replication.html
- https://debezium.io/documentation/reference/3.0/configuration/signalling.html
- https://docs.rs/tokio/latest/tokio/time/struct.Interval.html#method.reset_after
- https://docs.rs/tokio/latest/tokio/time/enum.MissedTickBehavior.html

## Decision

1. Aster atomically adds one panel/sequence/identity CDC row to every Base or
   quantized-slot commit. The KV prefix is reserved; normal and erasure callers
   cannot forge or delete it.
2. The association cursor is a schema/source/panel-bound KV row. A cursor is
   advanced in process only after the row is durably committed and separately
   reread byte-for-byte.
3. Cursor absence never means current. The maintainer scans authoritative Base
   rows at one renewable MVCC snapshot and publishes them as bounded CDC chunks
   while ordinary writes continue. It starts at the source snapshot and drains
   through the final chunk, so concurrent changes remain ordered.
4. Bootstrap does not consult the persisted search membership or Aster's
   disposable changed-key journal. Those products may be stale for the exact
   reason bootstrap is required.
5. A crash before bootstrap completion publishes no cursor. The next attempt's
   newer source snapshot makes older partial snapshot events irrelevant.
6. CDC retention is acknowledgement-driven and multi-consumer. Bootstrap rows
   occupy a separate ordered snapshot prefix and may retire at the association
   cursor; real Base/slot mutations retire only through the minimum of the
   association cursor and persisted search-generation base. A durable mutation
   floor records the coverage origin and every later retirement, so a consumer
   below it fails closed and rebases. Tombstones and floor updates share one
   bounded commit and receive independent latest-state readback.
7. Each changed or removed identity reconciles its complete physical XTerm
   prefix. Missing current keys are tombstoned before the cursor advances.
8. Agreement edges persist exact `sum_agreement` and `n`. Existing vaults run
   one full physical XTerm fold to establish the v2 marker. Thereafter old
   contributions are subtracted and new contributions added in the same atomic
   XTerm/Graph commit; every changed aggregate row is independently reread.
9. A bounded batch is never published as the global between-record graph. The
   Graph CF instead carries a byte-verified pointer to the complete persisted
   search generation plus its exact bounded live delta. An absent, stale,
   corrupt, or over-bound source refuses the weave.
10. Legacy batch-local graph rows remain historical bytes but are not an
    authoritative graph source. Consumers must follow the versioned complete
    graph reference.
11. Persisted search reconciliation consumes the same commit-atomic mutation
    lane rather than Aster's disposable MVCC journal. Snapshot signals are
    excluded before identity coalescing. The first CDC publication seals its
    pre-commit sequence as the coverage floor, preventing an upgraded vault
    from interpreting absent pre-install history as an empty delta.
12. A successful full derived-state tick that physically reports association
    backlog schedules an association-only continuation after a five-second
    foreground-admission window. The continuation runs no already-completed
    search, coverage, graph, kernel, causal-map, or relay phase. A failure or a
    settled frontier always restores the normal five-minute cadence. Tokio's
    `reset_after` supplies the required completion-relative deadline and
    explicitly ignores missed-tick policy, so cadence debt cannot create a
    burst.
13. Post-ingest drift is measured only when the association cursor reaches its
    target. A partial bootstrap is a knowingly incomplete population; treating
    it as a drift corpus is both scientifically invalid and a redundant corpus
    pass. The final continuation performs the one settled-frontier assay.
14. Snapshot publication, Loom interval work, and drift sampling own separate
    record-limit constants. They currently share a measured value of 2,000 but
    cannot silently retune one another.

## Consequences

- Checkpoint, compaction, and process recovery cannot create an unliftable
  association-history gap or silently skip an unprocessed panel.
- Base moves/deletes and slot-only corrections remove stale derived state.
- Routine agreement work is delta-proportional without sacrificing a global
  mean; the one-time migration remains independently reconstructible from
  XTerms.
- The durable CDC log is bounded by the slowest durable consumer rather than
  either growing forever or being pruned ahead of search or association state.
- Between-record traversal must resolve the complete graph reference on demand;
  materializing a partial neighbour batch is explicitly invalid.
- Large first-install/rebase snapshots converge at the measured Loom rate
  rather than adding five minutes of scheduler latency per chunk, while every
  chunk still leaves a real idle/admission boundary for foreground work.
