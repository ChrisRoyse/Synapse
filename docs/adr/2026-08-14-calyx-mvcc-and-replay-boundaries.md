# ADR: Public Calyx MVCC and replay boundaries

## Status

Accepted — 2026-08-14 (#2245, #2118).

## Context

Calyx already gave Synapse coherent MVCC snapshots internally, but the public
MCP surface exposed only latest-state storage reads. A numeric one-shot `as_of`
call would be dishonest: snapshot GC may have reclaimed the requested version
before the call starts, and an unbounded reader can retain obsolete versions
and memory indefinitely.

The absorbed Ledger also contained a dormant generic answer-replay reader with
no producer. More importantly, it interpreted Registry `measure_frozen` output
(one query embedding) as a vector of per-candidate scores. Adding a writer would
have connected incompatible meanings and could have reported a false
reproduction.

## Decision

Public historical storage reads use an explicit three-operation lease contract
inside the existing `storage` MCP facade:

1. `snapshot_open` pins the current committed Aster sequence for `100..=60000`
   ms. At most 64 public leases may be live in one daemon.
2. `snapshot_read` reads one exact logical Synapse CF/key through that retained
   snapshot. It returns presence, retention metadata, length, and SHA-256; raw
   payload bytes remain behind their typed owner tools.
3. `snapshot_release` removes the exact lease once. Unknown, expired, reused,
   malformed, or over-capacity requests fail closed with a named remediation.

The process-local public lease table and Aster's reader registry / snapshot-GC
floor are one lifecycle. Admission is atomic under the public lease-table lock,
and expired entries are pruned without allowing the table to grow unbounded.
`snapshot_gc_status` independently reads the Aster watchdog's live reader
count, oldest pinned sequence, and monotonic expired-reader count beside the GC
floor and current sequence. Those fields come from the physical registry, not
from an open/read/release response, so a leaked or failed-to-release reader is
directly observable without a version-table scan.

Generic answer replay is removed. `EntryKind::Measure` keeps its historical wire
code reserved so old bytes can never be reinterpreted. Answer traces retain any
producer-owned fusion JSON verbatim as provenance but do not assign replay
semantics to it. The working public `audit reproduce` path remains: it
re-derives a record's physical Ledger provenance binding.

A future answer-replay design is a new contract. It must persist the exact query
input entity, frozen panel and lens identities, search/index generation and
base sequence, candidate universe, tuning/seed, and generated output before it
may claim deterministic re-execution.

## Consequences

- Historical reads are coherent and bounded instead of best-effort.
- The live lease floor makes memory retention observable and independently
  verifiable.
- MCP remains a stable 40-tool surface; capability grows as strict operations
  within `storage`.
- No API claims answer reproducibility from incomplete or semantically
  incompatible evidence.

## Research basis

- [TiDB GC overview](https://docs.pingcap.com/tidb/stable/garbage-collection-overview/): MVCC GC advances a safe point while preserving snapshots after it.
- [TiDB timeout guidance](https://docs.pingcap.com/tidb/stable/dev-guide-timeouts-in-tidb/): active transactions block GC only for bounded durations; excessive version retention costs resources.
- [W3C PROV-DM](https://www.w3.org/TR/prov-dm/): reproducible derivation requires explicit entities, activities, usage, and generation.
- [Microsoft event sourcing](https://learn.microsoft.com/en-us/azure/architecture/patterns/event-sourcing): immutable events are the system of record; snapshots are an optimization, not a substitute.
- [MCP tool schema guidance](https://modelcontextprotocol.io/seps/2106-json-schema-2020-12): tool arguments retain an object-root JSON Schema contract.
- [RocksDB snapshots](https://github.com/facebook/rocksdb/wiki/Snapshot): the
  database registers every live snapshot, preserves versions visible to it,
  and requires the caller to release the snapshot's resources.
- [PostgreSQL activity statistics](https://www.postgresql.org/docs/current/monitoring-stats.html#MONITORING-PG-STAT-ACTIVITY-VIEW):
  the active backend's cleanup-pinning `xmin` horizon is an explicit monitoring
  field rather than an inference from transaction return values.
