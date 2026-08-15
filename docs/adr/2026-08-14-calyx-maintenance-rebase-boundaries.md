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

## Consequences

- A latest-only recovery can no longer permanently disable retention GC.
- Logically expired rows can again be physically reclaimed rather than visited
  on every ordered read.
- Resurrected ambient sessions cause one explicit immutable-generation rebuild,
  not an endless full-corpus retry loop.
- Committed private memory is measured independently from working set; both are
  recorded during manual verification.

## Research basis

- [Kubernetes API resource-version semantics](https://kubernetes.io/docs/reference/using-api/api-concepts/): when retained change history no longer covers a client's version, the client clears its cache, lists authoritative state again, and resumes from the returned version.
- [etcd maintenance](https://etcd.io/docs/v3.7/op-guide/maintenance/): compacted history is intentionally unavailable and physical storage reclamation is a separate maintenance concern.
- [RocksDB TTL behavior](https://github.com/facebook/rocksdb/wiki/RocksDB-FAQ): logical expiry does not guarantee prompt physical removal; expired keys are removed when compaction processes them.
- [Microsoft `PROCESS_MEMORY_COUNTERS_EX`](https://learn.microsoft.com/en-us/windows/win32/api/psapi/ns-psapi-process_memory_counters_ex): `PrivateUsage` is process commit charge, distinct from current working set.
