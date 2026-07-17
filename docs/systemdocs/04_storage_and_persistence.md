# 04. Storage and Persistence

**Source files covered:**

- `crates/synapse-storage/Cargo.toml`
- `crates/synapse-storage/src/lib.rs`
- `crates/synapse-storage/src/backend.rs`
- `crates/synapse-storage/src/cf.rs`
- `crates/synapse-storage/src/codecs.rs`
- `crates/synapse-storage/src/error.rs`
- `crates/synapse-storage/src/gc.rs`
- `crates/synapse-storage/src/pressure.rs`
- `crates/synapse-storage/src/timeline.rs`
- `crates/synapse-storage/src/episodes.rs`
- `crates/synapse-storage/src/agent_events.rs`
- `crates/synapse-storage/src/agent_transcripts.rs`
- `crates/synapse-storage/src/routines.rs`
- `crates/synapse-core/src/defaults.rs`
- `crates/synapse-core/src/retention.rs`
- `crates/synapse-mcp/src/m3.rs` (DB path + open call site)

---

## 1. Engine

The public storage surface is the `synapse_storage::Db` facade. The facade keeps
the existing `put_batch`, `get_cf`, scan, GC, pressure, and metrics API stable,
but the only concrete backend is now the Calyx vault.

`StorageBackendKind` accepts only `calyx` (empty config also resolves to the
default `calyx`). Any other `--storage-backend` / `SYNAPSE_STORAGE_BACKEND`
value fails startup with `StorageError::BackendInvalidConfig` /
`STORAGE_BACKEND_INVALID_CONFIG`. There is no compatibility fallback.

The opened handle is `Db`:

| Field | Type | Purpose |
|---|---|---|
| `path` | `PathBuf` | Calyx vault directory on disk |
| `schema_version` | `u32` | schema version this binary opened with |
| `backend` | `Box<dyn StorageBackend>` | internal object-safe backend, currently `CalyxBackend` |

The storage dependency surface is Calyx (`synapse-calyx`, `calyx-aster`) plus
`fs2` for disk free-space probing, `serde`/`serde_json` for JSON payloads,
`sha2` for readback hashes, and the Synapse core/telemetry crates.

## 2. Location and Open

The vault lives in the storage directory selected by `--db` / `SYNAPSE_DB`; the
default path is:

```text
%LOCALAPPDATA%\synapse\db
```

`synapse-mcp` acquires `SingleInstanceGuard` on the same directory before the
daemon opens storage. Duplicate daemons fail fast with the holder PID named in
the error.

Open sequence:

1. Parse storage backend config. Only `calyx` is accepted.
2. Open the Calyx vault at the selected path.
3. Verify or create the schema sentinel.
4. Create the Calyx-backed `PressureState`.
5. Return the `Db` facade and start the daemon-owned GC / pressure tasks.

## 3. Schema

| Constant | Location | Value |
|---|---|---|
| `SCHEMA_VERSION` | `crates/synapse-core/src/defaults.rs` | `1` |
| `PROFILE_SCHEMA_VERSION` | `crates/synapse-core/src/types/profile.rs` | `2` |

The storage sentinel key is `__schema_version`
(`SCHEMA_VERSION_KEY = b"__schema_version"` in `backend.rs`). It is stored in
the Calyx metadata collection as a 4-byte big-endian `u32`.

On open:

- Missing sentinel means a fresh vault; the backend writes the expected schema.
- Present sentinel is decoded with an exact 4-byte parser.
- Mismatch returns `StorageError::SchemaMismatch { expected, actual }`.
- Malformed bytes decode to `actual = 0` and fail closed as a mismatch.

## 4. Logical Column Families

`cf::ALL_COLUMN_FAMILIES` is a 17-entry array of logical column families. Calyx
stores each as a deterministic vault collection while preserving the existing
key bytes and JSON value bytes.

| CF name | Stores | Key format | Value | Notes |
|---|---|---|---|---|
| `CF_EVENTS` | Replay event log (`StoredEvent`) | caller-defined bytes | JSON | TTL 24 h |
| `CF_OBSERVATIONS` | Observation snapshots (`StoredObservation`) | caller-defined bytes | JSON | TTL 6 h |
| `CF_PROFILES` | Cached profile loads | caller-defined bytes | JSON | no TTL |
| `CF_MODEL_CACHE` | Downloaded model cache | caller-defined bytes | binary blobs | LRU-only |
| `CF_SESSIONS` | MCP session continuity (`StoredSession`) | caller-defined bytes | JSON | TTL 30 d |
| `CF_REFLEX_AUDIT` | Per-reflex audit trail (`StoredReflexAudit`) | caller-defined bytes | JSON | TTL 7 d |
| `CF_OCR_CACHE` | OCR memoization | caller-defined bytes | JSON | TTL 1 h |
| `CF_TELEMETRY` | Local metric ring buffer | caller-defined bytes | JSON | TTL 6 h |
| `CF_ACTION_LOG` | Emitted action log | caller-defined bytes | JSON | TTL 24 h |
| `CF_PROCESS_HISTORY` | Process start/exit history | caller-defined bytes | JSON | TTL 6 h |
| `CF_KV` | Control-plane and extension key-value rows | caller-defined bytes | JSON | no TTL; protected from generic cap eviction |
| `CF_TIMELINE` | Operator activity timeline | `ts_ns (8 BE) || seq (4 BE)` | JSON | TTL 90 d |
| `CF_EPISODES` | Derived episodes | `start_ts_ns (8 BE) || ordinal (4 BE)` | JSON | TTL 90 d; rebuildable |
| `CF_ROUTINES` | Derived mined routines | `rt1-` + 16 hex | JSON | no TTL |
| `CF_ROUTINE_STATE` | Operator routine lifecycle | routine id keyspace | JSON | no TTL |
| `CF_AGENT_EVENTS` | Agent lifecycle journal | `ts_ns (8 BE) || seq (4 BE)` | JSON | TTL 30 d |
| `CF_AGENT_TRANSCRIPTS` | Normalized transcripts | `spawn_id || 0x00 || line_no (8 BE)` | JSON | TTL 30 d |

`Db` exposes `scan_cf`, `scan_cf_prefix`, `scan_cf_prefix_from`,
`scan_cf_from`, `scan_cf_tail`, `compact_cf`, and `compact_cf_range` over these
logical CFs. Size/count helpers use exact Calyx scans and return missing
estimate lists only when a backend-level estimate is unavailable.

## 5. Codecs

All persisted values are JSON via `serde_json` unless a specific cache payload
is explicitly binary. The shared helpers are:

| Function | Behavior | Error |
|---|---|---|
| `encode_json<T: Serialize>` | `serde_json::to_vec` | `StorageError::EncodeJson` |
| `decode_json<T: DeserializeOwned>` | `serde_json::from_slice` | `StorageError::DecodeJson` |

Keys are raw bytes produced by per-CF codecs. Readback tooling emits key/value
hashes and lengths rather than raw key or value material unless a caller is
inside the daemon and already has storage authority.

## 6. Retention and GC

Calyx stores logical TTL metadata in the vault row envelope. Periodic storage GC
is backend-neutral through the `GcRunner` trait; `CalyxBackend` supplies the
runner and returns the same `GcReport` / `GcCfReport` shape used by storage and
health surfaces.

`GcConfig::from_retention_defaults()` currently carries the 5-minute interval.
`GcTaskReadback` records the last tick start/completion timestamps, duration,
last error, and any CFs whose policy was skipped by the runner. Startup delays
the first tick by one interval so daemon readiness is not blocked by an
immediate scan.

`CF_KV` and `CF_ROUTINE_STATE` are intentionally protected from generic
oldest-first cap eviction. They contain control-plane state where the storage
layer cannot know which rows are safely rebuildable; callers that need bounded
growth for a prefix must own an explicit prefix-level cleanup path with its own
readback evidence. Health exposes protected skips through
`storage_gc_last_unsupported_policy_skips`.

One-shot maintenance GC remains exposed by `Db::run_gc_once()` and
`Db::run_gc_once_with_row_caps(...)` for the maintenance-gated storage facade.
These calls must be verified by reading vault row counts before and after the
real MCP trigger.

## 7. Disk Pressure

`pressure.rs` polls free bytes on the vault volume with `fs2::available_space`
and drives a 5-level pressure state machine.

| Level | Triggered when free bytes < | Write policy |
|---|---|---|
| Normal | n/a | all writes |
| Level1 | 2 GB | all writes, GC advised |
| Level2 | 1 GB | all writes, GC advised |
| Level3 | 500 MB | sheds rebuildable/cache CF writes |
| Level4 | 200 MB | only `CF_REFLEX_AUDIT` and `CF_SESSIONS` |

When a write is shed, `Db::put_batch` increments
`storage_writes_shed_total`, logs `STORAGE_WRITE_FAILED`, and returns
`StorageError::WriteShed`. Deletes and explicit maintenance operations still
fail or succeed based on the concrete storage result; there is no silent
drop/pretend-success path.

## 8. Manual Readback Helpers

The read-only helpers are supporting inspection surfaces, not FSV automation:

- `scan_cf_read_only`
- `scan_cf_read_only_with_expired`
- `dump_cf_read_only`
- `dump_cf_read_only_with_expired`
- `inspect_calyx_vault_read_only`

They open the Calyx vault separately from the daemon and return physical row
counts, hashes, sizes, expiry histograms, and schema metadata. For Synapse MCP
behavior, D1 still requires the real MCP tool call as trigger and a separate
manual vault readback afterward.

## 9. Error Contract

Storage errors are structured and carry machine-readable codes. Configuration
errors fail before serving. Open/schema failures stop daemon startup. Read,
write, pressure, GC, and codec failures are returned to the caller and logged
with context; storage code must not fall back to synthetic data or hide a failed
write behind a successful response.
