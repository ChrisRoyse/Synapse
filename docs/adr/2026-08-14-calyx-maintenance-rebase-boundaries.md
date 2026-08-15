# ADR: Calyx maintenance rebases when incremental proof is impossible

## Status

Accepted — 2026-08-14 (#2245).

## Context

Two unattended derived-state owners could enter permanent retry loops:

- retention GC cached a derived-source reachability baseline, then asked Aster
  for a Base delta older than the process's recovered changed-key-history floor;
- agent-cost rollups published immutable TimeSeries contributions for an
  incomplete ambient session that later resumed and acquired more transcript
  rows.

The retention owner also existed twice per vault: periodic maintenance retained
one complete reachability census while every explicit MCP GC call constructed a
second runner and rebuilt the same corpus-sized census. The duplicate ownership,
not an absent memory limit, raised process-private commit above 1 GiB.

A later physical tick exposed two adjacent contract failures. The live
`syn-agent-event-v1` generation was an explicit Loom/Lodestar consumer but had
no search membership generation. An initial change added its exact storage
schema to the existing `syn_active_panel_contract`; the real search trigger then
correctly refused it with `CALYX_PANEL_NO_GRADED_DENSE_LENS`. #1965 had retired
the panel's only graded record vector after measuring one tied cosine value over
the real corpus. Its remaining lanes have finite directions and cannot rank a
neighbourhood. The actual defect was therefore conflating a reconstructable
storage schema with advertised query capability, plus scheduling a finite-only
panel for consumers that require query membership. Separately, a progressing
panel-membership walk outlived its fixed 30-second reader lease and failed after
the watchdog correctly expired the pin. The lease duration had become an
accidental operation timeout even though the scan was still making bounded
forward progress.

Neither condition can be repaired incrementally. The first lacks the historical
facts needed to prove a delta. The second would require changing already
published immutable points. Retrying the same operation every 15 seconds did no
useful work, retained expired physical rows, repeatedly scanned the transcript
corpus, and drove committed private memory above the lightweight daemon budget.

## Decision

Incremental maintenance state is an optimization over authoritative rows, never
the authority.

1. Aster exposes its exact changed-key-history floor. A consumer whose cached
   baseline predates that floor performs one complete read from a newly pinned
   snapshot and replaces its cache only after that read succeeds.
2. Agent-cost TimeSeries points remain immutable. If an authoritative spawn
   contribution differs from its published marker, the maintainer rotates the
   entire rebuildable generation: it deletes the old rollup namespaces, proves
   each namespace empty with a separate read, and builds a new generation from
   retained `CF_AGENT_TRANSCRIPTS` rows.
3. Sequence inversion, corrupt rows, expired leases, failed purge readback, and
   failed replacement builds remain hard errors with named diagnostics. No
   partial cache, stale rollup, mutable-point rewrite, or global fallback is
   served.
4. Each opened vault owns exactly one shared GC runner. Scheduled and explicit
   MCP passes use that authority's serialized census and pressure state. Exact
   source ranges use a validated 64-bit representation; construction reserves
   bounded blocks and discards excess capacity after publication.
5. A full census rebase destroys and releases its superseded baseline before
   allocating the replacement. A failed replacement leaves the cache absent,
   so the next pass must rebuild authoritative state instead of serving stale
   reachability.
6. Storage schema and query capability are distinct authorities. The
   reconstructable contract mirrors ingest slot-for-slot and supports lifecycle,
   grading, and measurement. The queryable contract is an explicit subset and
   additionally enforces a genuinely graded dense lens. Agent-event keeps its
   exact reconstructable contract but is intentionally absent from query
   admission. Lens coverage follows the declared-queryable set; unattended Loom
   and Lodestar share one target table containing only query-admissible panels.
   Startup validates that table and refuses duplicate or non-queryable targets.
7. A progressing panel walk renews the same registered reader id and pinned
   sequence at a bounded row cadence. Renewal validates presence, liveness, and
   sequence identity atomically under the lease-registry lock. Missing, expired,
   or mismatched leases fail closed; renewal never rebases a read and never
   resurrects an expired pin.
8. Exact GC source keys are compacted physically in place after lexical
   deduplication. Dropping duplicate range entries without moving the byte arena
   is not reclamation: unreachable duplicate key bytes remain resident. The
   in-place destination is required never to advance beyond unread source bytes,
   avoiding a second corpus-sized allocation while preserving exact lookup.

## Consequences

- A latest-only recovery can no longer permanently disable retention GC.
- Logically expired rows can again be physically reclaimed rather than visited
  on every ordered read.
- Resurrected ambient sessions cause one explicit immutable-generation rebuild,
  not an endless full-corpus retry loop.
- Committed private memory is measured independently from working set; both are
  recorded during manual verification.
- Periodic and operator GC can no longer retain two copies of the exact Base
  reachability index, and rebase peak memory no longer includes old + new
  baselines simultaneously.
- Scheduled search, lens coverage, Loom, and Lodestar no longer request an
  impossible agent-event membership generation. Agent-event's real schema stays
  available to non-neighbourhood lifecycle and grading consumers, while a
  direct neighbourhood request fails closed instead of inventing queryability.
- Large progressing panel scans remain one coherent MVCC instant without using
  an unbounded lease; stalled/abandoned readers still expire.
- Duplicate references no longer leave their raw key bytes resident in the
  long-lived GC cache after their index entries are removed.

## Research basis

- [Kubernetes API resource-version semantics](https://kubernetes.io/docs/reference/using-api/api-concepts/): when retained change history no longer covers a client's version, the client clears its cache, lists authoritative state again, and resumes from the returned version.
- [etcd maintenance](https://etcd.io/docs/v3.7/op-guide/maintenance/): compacted history is intentionally unavailable and physical storage reclamation is a separate maintenance concern.
- [RocksDB TTL behavior](https://github.com/facebook/rocksdb/wiki/RocksDB-FAQ): logical expiry does not guarantee prompt physical removal; expired keys are removed when compaction processes them.
- [Microsoft `PROCESS_MEMORY_COUNTERS_EX`](https://learn.microsoft.com/en-us/windows/win32/api/psapi/ns-psapi-process_memory_counters_ex): `PrivateUsage` is process commit charge, distinct from current working set.
- [Rust `Arc`](https://doc.rust-lang.org/std/sync/struct.Arc.html) and [`Mutex`](https://doc.rust-lang.org/std/sync/struct.Mutex.html): shared ownership points clones at one allocation, while synchronized interior mutability gives one-at-a-time access to the protected state.
- [Rust `Vec`](https://doc.rust-lang.org/std/vec/struct.Vec.html): vectors do not shrink automatically; capacity is observable, and `shrink_to_fit` explicitly requests release of unused backing capacity.
- [Confluent Schema Registry concepts](https://docs.confluent.io/platform/current/schema-registry/fundamentals/index.html): one versioned registry is the serving authority for schemas and compatibility metadata, preventing producers and consumers from maintaining divergent declarations.
- [Kubernetes API discovery](https://kubernetes.io/docs/concepts/overview/kubernetes-api/): supported resources/versions/operations are advertised by Discovery separately from the OpenAPI resource schemas; a schema's existence is not itself an operation capability.
- [PostgreSQL operator classes](https://www.postgresql.org/docs/current/indexes-opclass.html): an index is valid for the operators and semantics declared by its operator class, not merely because a column's data type can be stored.
- [etcd lease API](https://etcd.io/docs/v3.7/learning/api/): a live lease is extended through explicit keep-alives; expiry remains the fail-closed liveness boundary when keep-alives stop.
- [Kubernetes Leases](https://kubernetes.io/docs/concepts/architecture/leases/): active holders update `renewTime`, while the absence of renewal is what permits expiry and reclamation.
