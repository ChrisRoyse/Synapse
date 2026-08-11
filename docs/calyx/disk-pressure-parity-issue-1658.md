# Calyx Disk-Pressure Parity (#1658)

## Root Cause

`synapse-storage::pressure` already had the pressure levels, transition-code
history, probe readbacks, and per-CF write shedding that storage callers rely
on. The missing structural invariant was backend independence: the pressure
responder accepted the concrete database handle directly so Level2 and above
could run compaction across column families.

The Calyx backend therefore exposed only an in-process `PressureState` for
`pressure_level` and `pressure_permits_write`. The actual maintenance entry
points returned unsupported:

- `run_pressure_check_once`
- `run_pressure_check_with_free_bytes_sample`
- `spawn_pressure_task`

M3 then skipped all storage maintenance on Calyx, which meant pressure probes
and shedding could not become real until #1658 and #1659 both landed.

## Design

The storage crate now keeps a single Synapse pressure state machine and injects
backend-specific physical maintenance through `PressureCompaction`.

Legacy behavior remains the same:

- Level2 and above call compaction across all Synapse column families.
- The same `PressureReport` shape, transition codes, and `WriteShed` behavior
  remain visible to existing callers.

Calyx behavior is now explicit:

- `SynapseCalyxVault::compact_kv_once` bridges to Aster
  `compact_cf_once(ColumnFamily::Kv)`.
- Synapse Calyx storage maps all 17 Synapse CFs into namespaces inside Aster's
  physical `ColumnFamily::Kv`, so one successful KV compaction attempt covers
  the complete Synapse storage surface.
- Calyx pressure checks use the same thresholds and write-shed policy as
  the storage backend.
- Pressure errors fail closed through structured `StorageError` values; there
  is no fallback to pretending maintenance succeeded.
- `M3State::ensure_storage_maintenance` now starts pressure maintenance on
  Calyx and records only GC as unsupported until #1659.
- Health now evaluates pressure probe/task state even when GC remains
  unsupported, so Calyx GC status cannot hide a broken pressure task.

## Pressure Behavior

The parity surface exercised for #1658:

- Normal: all target CF writes are allowed.
- Level2: writes remain allowed, compaction is attempted, and GC is advised.
- Level3: rebuildable CFs such as `CF_TIMELINE` are shed while control-plane
  CFs such as `CF_AGENT_EVENTS` remain writable.
- Level4: only protected CFs such as `CF_SESSIONS` remain writable.
- Recovery to Normal reopens writes while preserving prior transition history.
- Structurally invalid CF names fail closed before writing physical Calyx rows.

## Research Inputs

- Storage write stalls slow or stop writes when flush or compaction cannot keep
  up, because otherwise space/read amplification can exhaust disk or degrade
  reads. Stateful storage should record stall conditions and support immediate
  non-blocking failures for callers that should not block indefinitely.
- CockroachDB admission control queues CPU and storage IO by priority to keep
  important work moving when individual stateful nodes hit hotspots. That
  supports Synapse's existing per-CF priority shedding rather than a single
  all-or-nothing gate:
  https://www.cockroachlabs.com/docs/stable/admission-control
- Kubernetes node-pressure eviction monitors disk signals against thresholds,
  reports pressure conditions, reclaims node-level resources before evicting
  lower-priority work, and uses priority when deciding what to shed:
  https://kubernetes.io/docs/concepts/scheduling-eviction/node-pressure-eviction/

## Boundary With #1659

#1658 makes Calyx pressure checks, pressure probes, transition codes, physical
compaction attempts, and per-CF shedding real. Public GC report parity,
`run_gc_once`, periodic GC reports, and GC maintenance surfaces remain #1659.
