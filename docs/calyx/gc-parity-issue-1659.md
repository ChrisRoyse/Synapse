# Calyx GC Parity (#1659)

## Root Cause

`synapse-storage::gc` was still legacy-backend-shaped. The report/task API was
backend-neutral, but the scheduler captured the concrete database handle directly and
the Calyx backend returned unsupported for:

- `run_gc_once`
- `run_gc_once_with_row_caps`
- `spawn_gc_task`

Calyx retention parity (#1657) had already made the logical data model strong:
every Synapse row stored in Aster `ColumnFamily::Kv` carries a retention
envelope with `expires_at_ms`, `written_at_ms`, and the payload. Calyx pressure
parity (#1658) had already added physical KV compaction. The missing piece was
using those two existing primitives to produce the same public `GcReport` and
`GcTaskReadback` surfaces that existing callers expect.

## Research Inputs

- LSM-style TTL has the important invariant that TTL metadata can mark data
  expired, but physical removal happens during compaction, and stale reads can
  see expired records until compaction has run.
- Cassandra tombstone documentation reinforces the same split: deletion and
  TTL write tombstones first, then compaction later removes tombstones when it
  is safe:
  https://cassandra.apache.org/doc/latest/cassandra/managing/operating/compaction/tombstones.html
- Tombstone-heavy ranges are operational maintenance concerns, not
  return-value-only concerns.
- Redis `EXPIRE` is a useful contrast for the public semantic contract: after
  expiration the key should not behave as live data. Calyx already honored this
  on reads; #1659 makes GC physically remove the expired rows too:
  https://redis.io/docs/latest/commands/expire/

## Design

The scheduler now accepts a `GcRunner` trait. Task spawning no longer needs to
know which backend owns the physical maintenance operation.

Calyx GC now:

- Builds byte budgets from `synapse_core::retention::DEFAULTS`.
- Builds deterministic row budgets for `run_gc_once_with_row_caps`.
- Scans the physical Aster `ColumnFamily::Kv` namespace for each logical
  Synapse CF.
- Decodes the existing Calyx retention envelope and fails closed on malformed
  rows with structured `StorageError::WriteFailed` logging.
- Treats expired rows as logically dead, writes physical tombstones for them,
  and counts them in `evicted_rows`.
- Applies oldest-first cap eviction using `(written_at_ms, user_key)` so
  same-timestamp batches have deterministic ordering.
- Preserves protected CFs such as `CF_KV` and `CF_ROUTINE_STATE`; over-cap
  protected CFs return `eviction_skipped_reason=protected_cf_policy_skipped`.
  `CF_KV` is protected because it now carries daemon control-plane state
  (workspace rows, cost price/index metadata, setup/session policy rows, and
  other extension prefixes) where generic storage GC cannot infer
  rebuildability.
- Plans every CF report before committing tombstones, so a malformed later CF
  cannot partially mutate an earlier CF during a default pass.
- Commits tombstones through the real Calyx write path and then calls
  `SynapseCalyxVault::purge_kv_tombstones`, which delegates to Aster
  tombstone-aware compaction for physical reclamation.

`M3State::ensure_storage_maintenance_tasks` no longer skips Calyx GC. Calyx now
starts the same pressure task and GC task surfaces as the previous backend.

## Failure Contract

There are no fallbacks:

- Unknown CF names fail before a vault operation.
- Invalid caps (`hard_cap < soft_cap` or zero caps) fail before mutation.
- Malformed Calyx value envelopes fail before mutation and leave physical rows
  unchanged.
- Tombstone commit or tombstone purge errors are returned as failed GC, not
  reported as a successful pass.

Direct `compact_cf` and `compact_cf_range` remain unsupported for Calyx because
Synapse CFs are logical namespaces inside one physical Aster KV CF. GC and
pressure maintenance use the backend-owned physical compaction bridge instead.
