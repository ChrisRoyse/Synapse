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

A subsequent live read exposed a third invariant mismatch. Search maintenance
used the query-time changed-key count and coverage ratio as its only refresh
triggers, but panel-membership consumers read an immutable generation and
require its base sequence to cover the exact panel's derived-content watermark.
One panel-content commit could therefore make membership unusable while the
query delta was still small enough for search maintenance to report
`none_needed`. Lens coverage and Loom then failed every scheduled tick with
`SYNAPSE_CALYX_STALE_DERIVED`. A first attempted repair made any newer content
watermark force a rebuild. Physical readback disproved that policy: the real
MCP rebuild's own durable audit ingest advanced the watermark four sequences
after publication, so every successful rebuild immediately invalidated itself.
Those independent whole-corpus phases also ran without explicit
allocator-release boundaries, so dead pages from one phase remained committed
while the next phase allocated its own corpus.

Manual boundary verification then showed that an invalid drift request
(`max_records=0`) returned the correct named error but only after waiting behind
the exclusive maintenance owner for roughly ninety seconds. Bounds validation
lived at the start of Calyx execution, which was still too late: the facade had
already entered maintenance admission. Invalid input could therefore consume
queue time and delay its actionable error even though it required no vault
state.

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
9. Search-generation status reads the manifest, vault sequence, exact-panel
   content watermark, and changed-key delta from one panel-pinned snapshot.
   Panel membership uses the hash-verified immutable sidecar as its baseline,
   then merges only the exact panel's changed Base identities at that same
   snapshot: live additions are inserted, tombstoned or moved identities are
   removed. It shares search's changed-key definition and 8,192-key hard bound.
   An over-bound or unprovable history interval fails closed and requires an
   authoritative rebuild; ordinary post-generation ingest does not.
10. Each independent whole-corpus maintenance phase owns and releases its
    allocations before the next phase begins. The daemon invokes its installed
    allocator reclaimer after graph lanes, before panel coverage, after panel
    coverage, and after the final scheduled kernels, and reads process-private
    bytes before and after every release. An unavailable reclaimer or unreadable
    process-memory Source of Truth fails the tick with a named diagnostic; this
    is lifecycle ownership, not a memory limit or degraded execution path.
11. Drift's complete typed parameter validator is one authoritative Calyx
    function. The MCP facade builds and validates that typed request before
    maintenance admission; Calyx validates it again before its first physical
    read so non-MCP callers retain the same invariant. Invalid input never opens
    storage, acquires the exclusive lane, allocates a corpus, or changes a
    bound, and preserves its exact named error and remediation.

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
- Panel membership stays current under bounded routine ingest without a global
  Base scan or a rebuild for every event, while over-bound/unprovable deltas
  still refuse instead of serving an incomplete membership set.
- Search status and maintenance decisions cannot combine a manifest from one
  instant with a panel watermark or delta from another.
- Derived-state peak private memory is the largest live phase rather than the
  accumulated committed pages of unrelated completed phases.
- Structurally invalid drift requests fail at the public admission boundary
  instead of waiting behind unrelated whole-vault maintenance.

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
- [Materialize snapshotting](https://materialize.com/docs/concepts/snapshotting/): a materialized snapshot is committed atomically and queries wait for a complete serving version instead of observing a partial generation.
- [Debezium incremental snapshot design](https://github.com/debezium/debezium-design-documents/blob/main/DDD-3.md): explicit low/high watermarks and bounded chunks make incremental reconstruction consistent and resumable.
- [Materialize self-correcting materialized views](https://materialize.com/blog/self-correcting-materialized-views/): authoritative readback and serialized hydration avoid retaining duplicate snapshot state during repair.
- [RocksDB snapshots](https://github.com/facebook/rocksdb/wiki/Snapshot) and [memory usage](https://github.com/facebook/rocksdb/wiki/Memory-usage-in-RocksDB): snapshots provide a consistent point-in-time view, while iterator and cache lifetime directly controls retained resources.
- [Materialize isolation levels](https://materialize.com/docs/reference/isolation-level/): readers are served the freshest consistent snapshot and fail or wait when no qualifying consistent view exists.
- [Tower HTTP request validation](https://docs.rs/tower-http/latest/tower_http/validate_request/): validation middleware rejects an invalid request before allowing it through to the wrapped service.
- [Model Context Protocol tool errors](https://modelcontextprotocol.io/specification/2025-11-25/server/tools): servers must validate tool inputs, and out-of-range values are tool execution errors with actionable feedback.
