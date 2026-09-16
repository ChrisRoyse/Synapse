# 04. Storage and Persistence

**Source files covered:**

- `crates/synapse-storage/Cargo.toml`
- `crates/synapse-storage/src/lib.rs`
- `crates/synapse-storage/src/cf.rs`
- `crates/synapse-storage/src/codecs.rs`
- `crates/synapse-storage/src/error.rs`
- `crates/synapse-storage/src/batch.rs`
- `crates/synapse-storage/src/compaction.rs`
- `crates/synapse-storage/src/gc.rs`
- `crates/synapse-storage/src/pressure.rs`
- `crates/synapse-storage/src/timeline.rs`
- `crates/synapse-storage/src/episodes.rs`
- `crates/synapse-storage/src/agent_events.rs`
- `crates/synapse-storage/src/agent_transcripts.rs`
- `crates/synapse-storage/src/routines.rs`
- `crates/synapse-core/src/defaults.rs`
- `crates/synapse-core/src/retention.rs`
- `crates/synapse-core/src/episodes.rs`
- `crates/synapse-core/src/types/timeline.rs`
- `crates/synapse-core/src/types/episode.rs`
- `crates/synapse-core/src/types/agent_event.rs`
- `crates/synapse-core/src/types/agent_transcript.rs`
- `crates/synapse-core/src/types/routine.rs`
- `crates/synapse-core/src/types/stored.rs`
- `crates/synapse-mcp/src/m3.rs` (DB path + open call site)

---

## 1. Connection / Engine Management

### 1.1 Engine

The storage engine is **RocksDB**, via the `rocksdb` crate (`crates/synapse-storage/Cargo.toml`) with features `["lz4", "zstd", "multi-threaded-cf"]`. The only other persistence-relevant dependencies are `fs2` (free-space probing for disk pressure), `serde`/`serde_json` (codecs), `synapse-core`, `synapse-telemetry`, `thiserror`, `tokio`, and `tracing`.

The opened handle is the `Db` struct in `crates/synapse-storage/src/lib.rs`:

| Field | Type | Purpose |
|---|---|---|
| `path` | `PathBuf` | DB directory on disk |
| `schema_version` | `u32` | schema version this binary opened with |
| `batcher` | `batch::Batcher` | background write-batching thread |
| `inner` | `Arc<DB>` | the RocksDB handle |
| `pressure` | `Arc<pressure::PressureState>` | disk-pressure state machine |

### 1.2 On-disk location

The DB lives in a single RocksDB directory. The default path is computed by `default_db_path()` in `crates/synapse-mcp/src/m3.rs`:

```
%LOCALAPPDATA%\synapse\db        (falls back to std::env::temp_dir()/synapse/db when LOCALAPPDATA is unset)
```

It is opened via `Db::open(&db_path, SCHEMA_VERSION)` (`crates/synapse-mcp/src/m3.rs`). A single-instance lock (`SingleInstanceGuard`) is acquired on the same directory before open.

### 1.3 Open sequence (`Db::open`, `crates/synapse-storage/src/lib.rs`)

1. Build base `Options` from `db_options()`.
2. `DB::list_cf(...)` enumerates the CFs physically present on disk (errors are ignored — a fresh DB).
3. Any on-disk CF that is neither `default` nor in `cf::ALL_COLUMN_FAMILIES` is treated as an **unknown CF**: it is opened with `Options::default()` and a `STORAGE_UNKNOWN_CF_OPENED` warning. Rows are preserved untouched — this is rollback safety so an older binary does not brick the DB after a CF was added.
4. Descriptors = every known CF (with `cf_options(name)`) chained with every unknown CF (default options).
5. `DB::open_cf_descriptors(...)` — failure maps to `StorageError::OpenFailed`.
6. `verify_schema_version(...)` runs (see §2).
7. Every known CF handle is re-checked for presence; a missing one is a fatal `OpenFailed`.
8. The `Batcher` background thread is spawned and `PressureState::default()` created.

### 1.4 DB-level options (`db_options()`)

| Option | Value |
|---|---|
| `create_if_missing` | `true` |
| `create_missing_column_families` | `true` |
| `max_background_jobs` | `2` |
| `compression_type` | `Lz4` |
| `max_open_files` | `256` |
| `keep_log_file_num` | `8` |
| `write_buffer_size` | `DEFAULT_WRITE_BUFFER_BYTES` = `64 MiB` |
| `max_write_buffer_number` | `3` |
| `target_file_size_base` | `64 MiB` |
| `level_zero_file_num_compaction_trigger` | `4` |
| block cache | LRU, `BLOCK_CACHE_BYTES` = `64 MiB` (via `apply_block_cache`) |

Module constants (`crates/synapse-storage/src/lib.rs`): `MIB = 1024*1024`, `DEFAULT_WRITE_BUFFER_BYTES = 64*MIB`, `MODEL_CACHE_WRITE_BUFFER_BYTES = 256*MIB`, `BLOCK_CACHE_BYTES = 64*MIB`, `TIMELINE_PERIODIC_COMPACTION_SECONDS = 86_400` (1 day).

### 1.5 Per-CF tuning (`cf_options(name)`)

Base per-CF options: `write_buffer_size = 64 MiB`, `max_write_buffer_number = 3`, `target_file_size_base = 64 MiB`, `level0 compaction trigger = 4`, `compression = Lz4`. Then overridden by name:

| CF group | Compression | Prefix extractor | Periodic compaction | Other |
|---|---|---|---|---|
| `CF_EVENTS`, `CF_ACTION_LOG`, `CF_REFLEX_AUDIT` | Lz4 | fixed 8-byte | — | — |
| `CF_MODEL_CACHE` | **None** | — | — | `write_buffer_size = 256 MiB` |
| `CF_OBSERVATIONS`, `CF_SESSIONS` | Zstd | — | — | — |
| `CF_TIMELINE`, `CF_EPISODES`, `CF_AGENT_EVENTS` | Zstd | fixed 8-byte | 86 400 s | long-retention TTL profile |
| `CF_AGENT_TRANSCRIPTS` | Zstd | — (variable-length keys) | 86 400 s | long-retention TTL profile |
| all others | Lz4 (base) | — | — | — |

After the match, every CF gets `compaction::install_ttl_filter(...)` (see §5.1) and `apply_block_cache(...)`.

The fixed 8-byte prefix extractor matches the 8-byte big-endian `ts_ns` prefix of timeline-style keys, accelerating time-range prefix scans. Periodic compaction forces cold SST files through the TTL compaction filter at least daily, so long-TTL rows still expire even without write churn.

---

## 2. Schema Versioning

| Constant | Location | Value |
|---|---|---|
| `SCHEMA_VERSION` | `crates/synapse-core/src/defaults.rs` | `1` |
| `PROFILE_SCHEMA_VERSION` | `crates/synapse-core/src/types/profile.rs` | `2` (profile docs, separate axis) |

The storage sentinel key is `__schema_version` (`SCHEMA_VERSION_KEY = b"__schema_version"` in `lib.rs`), stored in the RocksDB **default** CF as a 4-byte big-endian `u32`.

`verify_schema_version()` (`crates/synapse-storage/src/lib.rs`):

- If the key is **absent** (fresh DB), it writes `schema_version.to_be_bytes()`.
- If present, it decodes via `decode_schema_version()` (`u32::from_be_bytes` over exactly 4 bytes; otherwise `None`).
- Match → `Ok`. Mismatch → `StorageError::SchemaMismatch { expected, actual }` (actual defaults to 0 if undecodable).

`defaults.rs` notes pre-v1 migrations may bump this freely. Individual stored records additionally carry their own per-record envelope versions (see §6), independent of the DB-wide `SCHEMA_VERSION`.

---

## 3. Column Families (logical tables)

`cf::ALL_COLUMN_FAMILIES` (`crates/synapse-storage/src/cf.rs`) is a 17-entry array. RocksDB's implicit `default` CF additionally holds the `__schema_version` sentinel. All Synapse value payloads are JSON (see §4).

| CF name | Stores | Key format | Value | Notes |
|---|---|---|---|---|
| `CF_EVENTS` | Replay event log (`StoredEvent`) | not determined from source (no dedicated key codec in this crate) | JSON | TTL 24 h |
| `CF_OBSERVATIONS` | Observation snapshots (`StoredObservation`) | not determined from source | JSON | TTL 6 h |
| `CF_PROFILES` | Cached profile loads (on-disk TOML is source of truth) | not determined from source | JSON | no TTL |
| `CF_MODEL_CACHE` | Downloaded ONNX model cache | not determined from source | binary blobs (compression None) | LRU-only |
| `CF_SESSIONS` | MCP session continuity (`StoredSession`) | not determined from source | JSON | TTL 30 d |
| `CF_REFLEX_AUDIT` | Per-reflex audit trail (`StoredReflexAudit`) | not determined from source | JSON | TTL 7 d |
| `CF_OCR_CACHE` | OCR memoization for stable regions | not determined from source | JSON | TTL 1 h |
| `CF_TELEMETRY` | Local metric ring buffer | not determined from source | JSON | TTL 6 h |
| `CF_ACTION_LOG` | Emitted action log | not determined from source | JSON | TTL 24 h |
| `CF_PROCESS_HISTORY` | Process start/exit history | not determined from source | JSON | TTL 6 h |
| `CF_KV` | Generic bounded key-value extension | not determined from source | JSON | no TTL |
| `CF_TIMELINE` | Operator activity timeline (`TimelineRecord`) | `ts_ns (8 BE) ‖ seq (4 BE)` (12 B) | JSON | §6.1; TTL 90 d |
| `CF_EPISODES` | Derived episodes (`EpisodeRecord`) | `start_ts_ns (8 BE) ‖ ordinal (4 BE)` (12 B) | JSON | §6.2; TTL 90 d; rebuildable |
| `CF_ROUTINES` | Derived mined routines (`RoutineRecord`) | `rt1-` + 16 hex, UTF-8 (20 B) | JSON | §6.5; no TTL; replaced wholesale |
| `CF_ROUTINE_STATE` | Operator routine lifecycle (`RoutineStateRecord`) | same routine id keyspace (20 B) | JSON | §6.5; no TTL; survives re-mine |
| `CF_AGENT_EVENTS` | Agent lifecycle/telemetry journal (`AgentEventRecord`) | `ts_ns (8 BE) ‖ seq (4 BE)` (12 B) | JSON | §6.3; TTL 30 d; append-only |
| `CF_AGENT_TRANSCRIPTS` | Normalized agent transcripts (`AgentTranscriptRecord`) | `spawn_id ‖ 0x00 ‖ line_no (8 BE)` | JSON | §6.4; TTL 30 d; idempotent re-ingest |

`Db` exposes read/scan helpers over CFs: `scan_cf`, `scan_cf_prefix`, `scan_cf_prefix_from` (stops when key no longer matches prefix), `scan_cf_from` (paged window, returns `(rows, more)`), `scan_cf_tail` (last N rows, re-reversed to ascending), `compact_cf`, `compact_cf_range`. Size/metric helpers: `cf_sizes` (exact byte scan), `cf_row_counts` (exact), `cf_live_data_size_estimates` / `cf_estimated_row_counts` (RocksDB property-backed, cheaper; missing properties returned as 0 plus a list of CF names).

---

## 4. Codecs (`crates/synapse-storage/src/codecs.rs`)

All persisted values are **JSON** via `serde_json`. Binary persisted codecs are explicitly prohibited (ADR-0001 / RUSTSEC-2025-0141) so state-readback bytes stay inspectable.

| Function | Signature | Behavior | Error |
|---|---|---|---|
| `encode_json<T: Serialize>` | `(&T) -> StorageResult<Vec<u8>>` | `serde_json::to_vec` | `StorageError::EncodeJson { type_name, source }` |
| `decode_json<T: DeserializeOwned>` | `(&[u8]) -> StorageResult<T>` | `serde_json::from_slice` | `StorageError::DecodeJson { type_name, source }` |

Both are re-exported from the crate root (`pub use codecs::{decode_json, encode_json}`). Note that **keys** are not JSON — they are raw byte schemes produced by the per-CF key codecs (§6). The schema-version sentinel value is raw big-endian bytes, not JSON.

---

## 5. Compaction, GC, and Disk Pressure

### 5.1 TTL compaction filter (`crates/synapse-storage/src/compaction.rs`)

`install_ttl_filter(options, cf_name)` attaches a RocksDB compaction filter named `synapse_ttl_<cf_name>` when the CF has a finite TTL (resolved via `ttl_ns_for_cf`, which looks the CF up in `retention::DEFAULTS`). On each row examined during compaction it calls `ttl_decision(ttl_ns, now_ns, value)`:

- Extract the top-level JSON field `"ts_ns"` from the value **bytes** (`extract_ts_ns` — a byte-window search for `"ts_ns"`, skip whitespace, require `:`, then parse the digit run as `u64`).
- If `now_ns - ts_ns > ttl_ns` → `CompactionDecision::Remove`; otherwise `CompactionDecision::Keep`. A value lacking a parseable `ts_ns` is always kept.

This is why every long-retention record type keeps `ts_ns` as a required top-level field. `current_time_ns()` is `SystemTime::now()` since `UNIX_EPOCH` (a test-only fixed clock exists). TTL unit conversion (`ttl_to_ns`): `Hours(h) = h*3600*1e9`, `Days(d) = d*24*3600*1e9`; `None` and `LruOnly` yield no TTL filter.

Constants: `NANOS_PER_SECOND = 1_000_000_000`, `SECONDS_PER_HOUR = 3600`, `HOURS_PER_DAY = 24`, `TS_NS_FIELD = b"\"ts_ns\""`.

### 5.2 Garbage collection (`crates/synapse-storage/src/gc.rs`)

Periodic GC enforces per-CF **size budgets** (soft/hard caps from `retention::DEFAULTS`). `GcConfig::from_retention_defaults()` builds one `GcBudget` per CF with `soft_cap = soft_cap_mb * MIB`, `hard_cap = hard_cap_mb * MIB`, `unit = Bytes` (`MIB = 1024*1024`). Constant `GC_INTERVAL = 5 minutes`.

Per-CF pass (`run_cf`):

1. `collect_keys` — full forward scan of all keys (oldest-first; keys sort chronologically by scheme).
2. `before_value` = measured value: for `Bytes` budgets it is the RocksDB `rocksdb.estimate-live-data-size` property; for `Rows` budgets it is the exact key count.
3. `hard_cap_reached = before_value >= hard_cap` → logs `STORAGE_CF_HARD_CAP_REACHED` warning. (GC does **not** itself force the CF under the hard cap; it logs and continues with soft-cap eviction.)
4. If `before_value > soft_cap` and keys is non-empty → `evict_oldest`.
5. Re-measure `after_value` / estimated key counts; on eviction, increment the `cache_evictions_total` metric (labels `cf`, `reason="soft_cap"`) and log.

`evict_oldest`: computes `remove_count`, then `delete_range_cf(start, end)` over the oldest keys, `flush_cf`, then `compact_range_cf(.., None, None)` (full-CF compaction to reclaim tombstone space). `end` is `keys[remove_count]`, or for the tail case `key_after(last)` (last key with a trailing `0x00`).

`remove_count` (`remove_count`):

| `unit` | Eviction count |
|---|---|
| `Rows` (operator one-shot row cap) | exactly `before_value - soft_cap` (lands precisely at soft cap), capped at `key_count` |
| `Bytes` (periodic / pressure GC) | `key_count.div_ceil(4)` — a **25%-of-store batch per pass**, capped at `key_count` |

`spawn(db, config)` launches a Tokio task on the current runtime (fails with `WriteFailed` if no runtime); the task ticks at `config.interval` and calls `run_once`. `GcTask` aborts the task and signals shutdown on drop.

`Db` API: `run_gc_once()`, `run_gc_once_with_row_caps(cf, soft, hard)` (test-only `Rows` unit), `spawn_gc_task()`.

Reports: `GcReport { cf_reports: Vec<GcCfReport> }` with `total_evicted_rows()` and `cf(name)`. `GcCfReport` fields: `cf_name`, `before_value`, `after_value`, `before_estimated_num_keys`, `after_estimated_num_keys`, `evicted_rows`, `hard_cap_reached`, `hard_cap_code: Option<&str>`.

### 5.3 Disk pressure (`crates/synapse-storage/src/pressure.rs`)

A separate periodic task polls **free bytes on the DB volume** (via `fs2::available_space`) and drives a 5-level pressure state machine. `POLL_INTERVAL = 30 s`.

Thresholds (`PressureThresholds`, defaults; `GB = 1e9`, `MB = 1e6`):

| Level | Enum | Triggered when free bytes < | Default |
|---|---|---|---|
| Normal | `Normal` (0) | — (≥ Level1) | — |
| 1 | `Level1` | `LEVEL_1_FREE_BYTES` | 2 GB |
| 2 | `Level2` | `LEVEL_2_FREE_BYTES` | 1 GB |
| 3 | `Level3` | `LEVEL_3_FREE_BYTES` | 500 MB |
| 4 | `Level4` | `LEVEL_4_FREE_BYTES` | 200 MB |

`level_for(free_bytes)` picks the most severe threshold crossed. On `apply_free_bytes`:

- `transition_to(level)` swaps the atomic level; on an actual change it emits the level's code (`DiskPressureLevel::code()`) once and records it in `emitted_codes`.
- `gc_advised = transitioned && level >= Level1` (logs a warning advising the next GC tick).
- `compacted_cfs`: if `transitioned && level >= Level2`, runs `compact_all(db)` — `compact_range_cf(.., None, None)` over every CF in `ALL_COLUMN_FAMILIES`.

**Write shedding** by level (`permits_write_at(level, cf_name)`), consulted by `Db::put_batch`:

| Level | Writes permitted |
|---|---|
| Normal / Level1 / Level2 | all CFs |
| Level3 | all **except** the rebuildable/cache set: `CF_OBSERVATIONS`, `CF_OCR_CACHE`, `CF_TELEMETRY`, `CF_MODEL_CACHE`, `CF_PROCESS_HISTORY`, `CF_TIMELINE`, `CF_EPISODES`, `CF_ROUTINES`, `CF_AGENT_TRANSCRIPTS` |
| Level4 | **only** `CF_REFLEX_AUDIT` and `CF_SESSIONS` |

`CF_AGENT_EVENTS` stays writable even at Level3 (control-plane audit journal). When a write is shed, `put_batch` increments `storage_writes_shed_total` (label `cf`), logs `STORAGE_WRITE_FAILED`, and returns `StorageError::WriteShed`. Deletes are always allowed under pressure.

`PressureState`: `level: AtomicU8`, `emitted_codes: Mutex<Vec<&str>>`. API on `Db`: `pressure_level()`, `pressure_permits_write(cf)`, `pressure_transition_codes()`, `run_pressure_check_once()`, `run_pressure_check_with_free_bytes_sample(free_bytes)` (test), `spawn_pressure_task()`. `PressureReport` fields: `free_bytes`, `previous_level`, `current_level`, `emitted_code`, `compacted_cfs`, `gc_advised`. Disk probing is abstracted behind the `DiskProbe` trait (`Fs2DiskProbe` in production, `SequenceDiskProbe` for tests). `PressureTask` aborts on drop.

---

## 6. Store Data Models

All record types are JSON-serialized with `#[serde(deny_unknown_fields)]` (except `RoutineStateRecord` content additions which use `#[serde(default)]`). `Option`/empty-collection fields use `skip_serializing_if`. Each record carries a `record_version` envelope field.

### 6.1 Timeline — `CF_TIMELINE`

Key codec: `crates/synapse-storage/src/timeline.rs`. `TIMELINE_KEY_LEN = 12`. Key = `ts_ns (8 BE) ‖ seq (4 BE)`; `seq` (u32) breaks same-nanosecond ties. `timeline_key(ts_ns, seq)`, `timeline_scan_start(ts_ns)` (= `timeline_key(ts_ns, 0)`), `decode_timeline_key` (rejects non-12-byte keys with `TIMELINE_KEY_INVALID`).

Record: `TimelineRecord` (`crates/synapse-core/src/types/timeline.rs`), `TIMELINE_RECORD_VERSION = 1`:

| Field | Type |
|---|---|
| `record_version` | `u32` |
| `ts_ns` | `u64` (required; TTL anchor) |
| `kind` | `TimelineKind` |
| `actor` | `TimelineActor` |
| `app` | `Option<String>` |
| `payload` | `serde_json::Value` |

`TimelineKind` (snake_case): `FocusChange`, `TitleChange`, `IdleStart`, `IdleEnd`, `SessionStart`, `SessionEnd`, `InteractionSummary`, `Clipboard`, `FileActivity`, `BrowserNav`, `DemoMarker`, `Purge`. Raw keystroke content is deliberately unrepresentable (interaction rows carry counts only). `TimelineActor` (internally tagged `actor`): `Human` | `Agent { session_id: SessionId }`.

### 6.2 Episodes — `CF_EPISODES`

Key codec: `crates/synapse-storage/src/episodes.rs`. `EPISODE_KEY_LEN = 12`. Key = `start_ts_ns (8 BE) ‖ ordinal (4 BE)`; ordinal = episode index within its segmentation day. `episode_key`, `episode_scan_start`, `decode_episode_key` (`EPISODE_KEY_INVALID`).

Record: `EpisodeRecord` (`crates/synapse-core/src/types/episode.rs`), `EPISODE_RECORD_VERSION = 1`:

| Field | Type | Notes |
|---|---|---|
| `record_version` | `u32` | |
| `ts_ns` | `u64` | TTL anchor; always == `start_ts_ns` |
| `episode_id` | `String` | `ep1-` + 16 hex (SHA-256 prefix) |
| `start_ts_ns` | `u64` | |
| `end_ts_ns` | `u64` | |
| `actor` | `TimelineActor` | |
| `app` | `Option<String>` | |
| `document` | `Option<String>` | URL host (browser) or normalized title |
| `url` | `Option<String>` | representative full URL |
| `title_first` / `title_last` | `Option<String>` | |
| `distinct_title_count` | `u32` | |
| `row_count` | `u64` | evidence rows fed |
| `keystroke_count` / `click_count` | `u64` | aggregated from `interaction_summary` |
| `interruption_count` | `u32` | absorbed flicker spans |
| `interrupted_ms` | `u64` | |
| `started_because` / `ended_because` | `EpisodeBoundary` | |

`EpisodeBoundary` (snake_case): `AppSwitch`, `DocumentSwitch`, `IdleGap`, `SilentGap`, `SessionBoundary`, `DayBoundary`, `RangeEdge`. `duration_ms() = (end_ts_ns - start_ts_ns)/1e6`.

**Segmentation engine** (`crates/synapse-core/src/episodes.rs`): `segment_range(rows, range_start_ns, range_end_ns, end_is_day_boundary, config)` is a pure deterministic function — same rows + config produce byte-identical episodes including ids. `episode_id(start_ts_ns, actor, app, document)` = `"ep1-"` + first 8 bytes (16 hex) of SHA-256 over `start_ts_ns(BE) ‖ 0 ‖ actor_token ‖ 0 ‖ app ‖ 0 ‖ document`.

`SegmentationConfig` defaults: `min_focus_ns = 5_000_000_000` (5 s), `silent_gap_ns = 600_000_000_000` (10 min), `include_agent_activity = false`, `browser_apps = [chrome.exe, msedge.exe, firefox.exe, brave.exe, opera.exe, vivaldi.exe, arc.exe]`. Output `Segmentation { episodes, considered_rows, ignored_agent_rows, payload_anomalies }`. Boundary heuristics: app/document switch, idle start/end, session boundaries, silent gaps (recorder death), and sub-`min_focus_ns` alt-tab flicker absorption (`absorb_interruption` merges `[X, b, X']` trios). Errors (`SegmentationError`): `InvalidRange`, `RowOutOfRange`, `RowsNotChronological`, `InvalidConfig`.

### 6.3 Agent events — `CF_AGENT_EVENTS`

Key codec: `crates/synapse-storage/src/agent_events.rs`. `AGENT_EVENT_KEY_LEN = 12`. Key = `ts_ns (8 BE) ‖ seq (4 BE)`; `seq` is a process-wide monotonic counter (ordering authority within a tick). `agent_event_key`, `agent_event_scan_start`, `decode_agent_event_key` (`AGENT_EVENT_KEY_INVALID`). Append-only; `Db::put_batch` returns after a synced WAL write; terminal-state writers call `Db::flush()`.

Record: `AgentEventRecord` (`crates/synapse-core/src/types/agent_event.rs`), `AGENT_EVENT_RECORD_VERSION = 1`. Bounds: `AGENT_EVENT_MAX_ID_CHARS = 512`, `AGENT_EVENT_MAX_REASON_CHARS = 128`.

| Field | Type |
|---|---|
| `record_version` | `u32` |
| `ts_ns` | `u64` (required; TTL anchor; must be > 0) |
| `kind` | `AgentEventKind` |
| `session_id` | `Option<String>` |
| `spawn_id` | `Option<String>` |
| `reason_code` | `Option<String>` |
| `end_state` | `Option<AgentEndState>` |
| `state_from` / `state_to` | `Option<String>` |
| `attributes` | `GenAiAttributes` |
| `payload` | `serde_json::Value` |

`AgentEventKind` (snake_case): `SpawnRequested`, `SpawnReady`, `StateChanged`, `ToolCallStarted`, `ToolCallFinished`, `TurnStarted`, `TurnFinished`, `MessageSent`, `MessageReceived`, `LeaseAcquired`, `LeaseReleased`, `Interrupted`, `Killed`, `Exited`. `AgentEndState`: `Success`, `Error`, `Indeterminate`. `GenAiOperationName`: `Chat`, `CreateAgent`, `Embeddings`, `ExecuteTool`, `GenerateContent`, `InvokeAgent`, `InvokeWorkflow`, `Retrieval`.

`GenAiAttributes` serializes field names as **verbatim OpenTelemetry GenAI attribute names** (e.g. `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.agent.id/.name`, `gen_ai.conversation.id`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`/`output_tokens`/`cache_read.input_tokens`/`cache_creation.input_tokens`, `gen_ai.tool.name`/`call.id`/`call.arguments`/`call.result`, `error.type`). Tool-call arguments/results are opt-in (default `None`). `validate()` rejects stale version, `ts_ns == 0`, anonymous events (neither `session_id` nor `spawn_id`), and over-long/non-visible-ASCII ids — error string `AGENT_EVENT_INVALID`.

### 6.4 Agent transcripts — `CF_AGENT_TRANSCRIPTS`

Key codec: `crates/synapse-storage/src/agent_transcripts.rs`. Key = `spawn_id bytes ‖ 0x00 ‖ line_no (8 BE)` (`KEY_SEPARATOR = 0x00`). Spawn ids are `agent-spawn-` + ASCII alphanumerics/dashes, so `0x00` can never appear inside an id. `agent_transcript_key`, `agent_transcript_spawn_prefix(spawn_id)` (scans one spawn), `decode_agent_transcript_key` (`AGENT_TRANSCRIPT_KEY_INVALID` on missing separator / non-UTF-8 id / non-8-byte suffix). One row per source JSONL line; re-ingesting a line lands on the same key (idempotent).

Record: `AgentTranscriptRecord` (`crates/synapse-core/src/types/agent_transcript.rs`), `AGENT_TRANSCRIPT_RECORD_VERSION = 1`. Bounds: `AGENT_TRANSCRIPT_MAX_SUMMARY_CHARS = 2048`, `AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS = 8192`, `AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS = 8192`.

| Field | Type |
|---|---|
| `record_version` | `u32` |
| `ts_ns` | `u64` (ingestion time; TTL anchor; > 0) |
| `spawn_id` | `String` (must start `agent-spawn-`) |
| `line_no` | `u64` (1-based) |
| `source` | `TranscriptSource` |
| `status` | `TranscriptParseStatus` |
| `role` | `Option<TranscriptRole>` |
| `event_kind` | `Option<String>` |
| `turn_index` | `Option<u64>` |
| `conversation_id` | `Option<String>` |
| `model` | `Option<String>` |
| `content_summary` | `Option<String>` (capped) |
| `content_bytes` | `Option<u64>` |
| `content_sha256` | `Option<String>` |
| `content_truncated` | `bool` |
| `tool_calls` | `Vec<TranscriptToolCall>` |
| `usage` | `Option<TranscriptUsage>` |
| `source_error` | `Option<String>` |
| `parse_error` | `Option<String>` |
| `raw_line_bytes` | `u64` |
| `raw_line_sha256` | `String` (64 lowercase hex) |

`TranscriptSource` (snake_case): `ClaudeStreamJson`, `ClaudeSessionJsonl`, `CodexExecJson`, `LocalModelJson`. `TranscriptParseStatus`: `Parsed`, `Invalid`. `TranscriptRole`: `System`, `Assistant`, `Tool`, `Result`.

`TranscriptToolCall`: `tool_name`, `tool_call_id?`, `arguments?` (capped), `arguments_bytes?`, `arguments_truncated`, `result_summary?` (capped), `result_bytes?`, `result_truncated`, `status?`, `exit_code?: i64`.

`TranscriptUsage`: `input_tokens?`, `output_tokens?`, `cache_read_input_tokens?`, `cache_creation_input_tokens?`, `cache_creation_5m_input_tokens?` (Anthropic 5-min TTL, billed 1.25x), `cache_creation_1h_input_tokens?` (1-hour TTL, billed 2x), `reasoning_output_tokens?`, `total_cost_micro_usd?` (micro-USD, integer-exact), `model_usage: Vec<TranscriptModelUsage>`. `TranscriptModelUsage`: `model`, `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`, `cost_micro_usd?`.

`validate()` enforces version, `ts_ns > 0`, spawn-id format, 1-based `line_no`, the parse-status/`parse_error` invariant (invalid rows must carry `parse_error`; parsed rows must not), and 64-lowercase-hex `raw_line_sha256` — error string `AGENT_TRANSCRIPT_INVALID`.

### 6.5 Routines — `CF_ROUTINES` and `CF_ROUTINE_STATE`

Key codec: `crates/synapse-storage/src/routines.rs`. Both CFs share the routine-id keyspace. `ROUTINE_KEY_LEN = 20`, `ROUTINE_ID_PREFIX = "rt1-"`; valid id = `rt1-` + 16 lowercase hex chars. `routine_key` / `routine_state_key` (encode, `WriteFailed` on invalid), `decode_routine_key` / `decode_routine_state_key` (`ReadFailed`). `validate_routine_id` rejects wrong length/prefix/uppercase/non-hex — error string `ROUTINE_KEY_INVALID`.

`RoutineRecord` (`crates/synapse-core/src/types/routine.rs`), `ROUTINE_RECORD_VERSION = 1` — derived, replaced wholesale on each mining run:

| Field | Type |
|---|---|
| `record_version` | `u32` |
| `ts_ns` | `u64` (mining instant; provenance, no TTL) |
| `routine_id` | `String` (`rt1-` + 16 hex) |
| `granularity` | `RoutineGranularity` (`App` \| `AppDocument`) |
| `steps` | `Vec<RoutineStep>` |
| `dow_class` | `RoutineDowClass` |
| `mean_minute_of_day` | `u32` (0..1440) |
| `tolerance_minutes` | `u32` |
| `schedule_label` | `String` |
| `support_days` | `u32` |
| `occurrence_count` | `u32` |
| `opportunity_days` | `u32` |
| `confidence` | `f64` (Wilson 95% lower bound) |
| `window_start_ns` / `window_end_ns` | `u64` |
| `active_days_in_window` | `u32` |
| `first_seen_day_start_ns` / `last_seen_day_start_ns` | `u64` |
| `evidence` | `Vec<RoutineEvidence>` |

`RoutineStep`: `app`, `document?`. `RoutineEvidence`: `day_start_ns`, `minute_of_day`, `episode_ids: Vec<String>`. `RoutineDowClass`: `Daily`, `Weekdays`, `Weekend`, `Days { days: Vec<u8> }` (0=Mon..6=Sun).

`RoutineStateRecord`, `ROUTINE_STATE_RECORD_VERSION = 2` — operator-owned, survives `CF_ROUTINES` replace-all. Caps: `ROUTINE_STATE_MAX_FEEDBACK_EVENTS = 200`, `ROUTINE_STATE_MAX_TRANSITIONS = 64`, `ROUTINE_STATE_MAX_CONFIDENCE_POINTS = 180` (all newest-last, oldest dropped with a `*_truncated` counter). v1 rows deserialize cleanly as "no feedback yet" because the #856 fields are `#[serde(default)]`.

| Field | Type |
|---|---|
| `record_version` | `u32` |
| `routine_id` | `String` |
| `lifecycle` | `RoutineLifecycle` |
| `label` | `Option<String>` |
| `created_ts_ns` / `updated_ts_ns` | `u64` |
| `last_mined_ts_ns` | `Option<u64>` |
| `present_in_last_mine` | `bool` |
| `transitions` | `Vec<RoutineTransition>` |
| `transitions_truncated` | `u64` |
| `confidence_history` | `Vec<RoutineConfidencePoint>` |
| `confidence_history_truncated` | `u64` |
| `feedback_events` | `Vec<RoutineFeedbackEvent>` (default) |
| `feedback_events_truncated` | `u64` (default) |
| `accept_count` / `decline_count` / `ignore_count` / `abandon_count` | `u32` (default) |
| `consecutive_declines` | `u32` (default) |
| `cooldown_level` | `u32` (default) |
| `cooldown_until_ts_ns` | `Option<u64>` (default) |

`RoutineLifecycle`: `Candidate`, `Confirmed`, `Disabled`, `Archived`. `RoutineStateAction`: `Discovered`, `Confirm`, `Disable`, `Enable`, `Archive`, `Rename`. `RoutineTransition`: `ts_ns`, `action`, `from?`, `to`, `by`, `label_before?`, `label_after?`, `note?`. `RoutineConfidencePoint`: `ts_ns`, `confidence`, `support_days`, `opportunity_days`. `RoutineFeedbackOutcome`: `Accepted`, `Declined`, `IgnoredTimeout`, `Abandoned`. `RoutineFeedbackEvent`: `ts_ns`, `outcome`, `by`, `note?`.

### 6.6 Generic stored records (`crates/synapse-core/src/types/stored.rs`)

These carry their own `schema_version: u32` field (set by the producer) and are the JSON value payloads for the non-key-coded CFs in §3: `StoredEvent` (`CF_EVENTS`), `StoredObservation` (`CF_OBSERVATIONS`), `StoredReflexAudit` (`CF_REFLEX_AUDIT`), `StoredSession` (`CF_SESSIONS`). Supporting types: `StoredRedaction`, `StoredBackendPolicy`, `StoredAppContext`, `StoredAuditContext`, `StoredReflexStep`, `StoredProfileHistoryEntry`. Key schemes for these CFs are not defined in `synapse-storage` (no dedicated key codec); producers compose keys at the call site (not determined from this crate's source).

---

## 7. Batch Writes (`crates/synapse-storage/src/batch.rs`)

Writes are aggregated through a single background `Batcher` thread (spawned in `Db::open`). Public surface on `Db`:

| Method | Path | Pressure gate | Behavior |
|---|---|---|---|
| `put_batch(cf, kvs)` | batcher | yes (returns `WriteShed` per §5.3) | writes the submitted batch with synced WAL before returning; empty input is a no-op |
| `put_batch_pressure_bypass(cf, kvs)` | direct | **no** | synchronous `WriteBatch` + `flush_cf`; reserved for maintenance rewrites |
| `mutate_batch_pressure_bypass(cf, deletes, puts)` | direct | no | atomic deletes-then-puts + `flush_cf`; for coordination state with no release gap |
| `delete_batch(cf, keys)` | direct | n/a (always allowed) | `WriteBatch` deletes + `flush_cf` |
| `flush()` | batcher | - | synchronous WAL flush |

All paths first resolve the CF handle and return `StorageError::WriteFailed` if it is missing. Pressure-gated `put_batch` returns `StorageError::WriteShed` when policy rejects rows, so callers can surface or retry recording gaps.

Batcher mechanics: the worker accepts `Write`/`Flush`/`Shutdown` commands over a `sync_channel(1)`. A `Write` command builds one `WriteBatch`, calls `write_opt` with `WriteOptions::set_sync(true)`, calls `flush_wal(true)`, and only then replies to the caller. `Flush` and `Shutdown` call `flush_wal(true)`. On `Drop`, the batcher sends `Shutdown`, waits up to `2 * FLUSH_INTERVAL`, and joins the worker.

---

## 8. Error Types (`crates/synapse-storage/src/error.rs`)

`type StorageResult<T> = Result<T, StorageError>`. `StorageError` carries a stable code via `StorageError::code()`.

| Variant | Fields | `code()` (`error_codes`) |
|---|---|---|
| `OpenFailed` | `path: PathBuf`, `detail: String` | `STORAGE_OPEN_FAILED` |
| `EncodeJson` | `type_name: &'static str`, `source: serde_json::Error` | `STORAGE_WRITE_FAILED` |
| `DecodeJson` | `type_name: &'static str`, `source: serde_json::Error` | `STORAGE_READ_FAILED` |
| `WriteFailed` | `cf_name: String`, `detail: String` | `STORAGE_WRITE_FAILED` |
| `WriteShed` | `cf_name: String`, `pressure_level: String`, `rows: usize` | `STORAGE_WRITE_FAILED` |
| `ReadFailed` | `cf_name: String`, `detail: String` | `STORAGE_READ_FAILED` |
| `SchemaMismatch` | `expected: u32`, `actual: u32` | `STORAGE_SCHEMA_MISMATCH` |

Related codes emitted via tracing (not `StorageError` variants): `STORAGE_UNKNOWN_CF_OPENED` (warn on rollback-unknown CF), `STORAGE_CF_HARD_CAP_REACHED` (GC), `STORAGE_DISK_PRESSURE_LEVEL_1..4` (pressure transitions). All `STORAGE_*` codes are defined in `crates/synapse-core/src/error_codes.rs`.

---

See [01_system_overview.md](01_system_overview.md) for where storage sits in the daemon. The activity-timeline, episode, routine, and agent-event/transcript pipelines that produce these CFs are documented in their respective subsystem docs.
