# ADR: Timeline Data Model — CF_TIMELINE Schema, Retention, and GC Policy

Status: Accepted
Date: 2026-06-11
Issue: #835
Parent: #829 (epic), #828 (program)

## Context

The ambient memory & proactive assist program (#828) needs a long-retention
record of the operator's own activity: foreground app changes, window titles,
document paths, URLs, idle/active transitions, interaction cadence, and
enrichment events (clipboard, file activity, browser navigation). Episode
segmentation (#846), routine mining (#848), and timeline search (#841) all
read this store.

The existing storage layer cannot host this data as-is:

- Every audit column family is short-retention (`CF_EVENTS` 24h,
  `CF_OBSERVATIONS` 6h). Routine mining needs weeks-to-months of history.
- The agent-audit pipeline hash-redacts window titles, paths, and clipboard
  content. The timeline needs that content in plaintext to be searchable and
  minable.
- TTL eviction is implemented as a RocksDB compaction filter keyed on a
  top-level `ts_ns` JSON field. Compaction filters only run when a file is
  compacted. Synapse's short-TTL CFs churn enough that files compact
  naturally; a 90-day, low-write CF accumulates cold SST files that may
  never be selected for compaction, so expired rows would linger
  indefinitely. RocksDB's documented remedy is
  `periodic_compaction_seconds`, which forces files older than the bound
  through the compaction (and therefore the filter).

Binding operator decisions recorded in #828:

- Local-only IS the privacy model. Nothing leaves the machine; no consent
  tiers; the timeline stores plaintext. The hash-redaction discipline for
  agent-audit CFs is unchanged.
- Single-user learning only; the store serves exactly one operator.
- Manual Full-State-Verification per AGENTS.md D1 gates every task; no
  silent success anywhere, including pressure shedding.

Prior art reviewed: ActivityWatch's bucket/event model (one event type per
watcher: `currentwindow` carries `app` + `title`; `afkstatus` carries the
active/inactive state; heartbeat coalescing collapses identical consecutive
states). Its event-type separation maps onto a single CF with a `kind`
discriminant here because all kinds share one time axis and one retention
policy, and cross-kind chronological scans are the dominant read pattern
(episode segmentation walks all kinds in time order).

## Decision

### 1. One new column family, additive, schema version unchanged

`CF_TIMELINE` joins `cf::ALL_COLUMN_FAMILIES` (12th CF) in the existing
database. Adding a CF is additive: `create_missing_column_families(true)`
creates it on first open of an existing database, and the schema-version
sentinel stays at `SCHEMA_VERSION = 1`. The sentinel exists to reject
incompatible layout changes, not additive ones. A regression test opens a
database created with the 11-CF layout and verifies the 12-CF open succeeds
and the new handle exists.

### 2. Key scheme: `ts_ns (8B BE) || seq (4B BE)`

Identical to `CF_OBSERVATIONS` keys. Big-endian nanosecond timestamp prefix
gives chronological iteration, time-range scans via
`scan_cf_prefix_from`, and oldest-first eviction in the existing GC engine
with zero changes. The 4-byte big-endian sequence disambiguates same-tick
writes. An 8-byte fixed-prefix extractor is installed like the other
time-ordered CFs. Key encode/decode lives in `synapse_storage::timeline`
so every future producer (#837 recorder, #839/#840 enrichment) and consumer
(#841 search) shares one implementation; decode errors are structured
(`TIMELINE_KEY_INVALID` detail in `StorageError::ReadFailed`), never
silently skipped.

### 3. Value: JSON `TimelineRecord` envelope with mandatory `ts_ns`

Storage payloads stay JSON (ADR-0001 / RUSTSEC-2025-0141: persisted bytes
must remain inspectable). The envelope is defined in
`synapse_core::types::timeline`:

- `record_version: u32` — envelope schema version, starts at 1.
- `ts_ns: u64` — REQUIRED, top level. This is the TTL compaction-filter
  contract; a row without it is immortal until the soft cap evicts it.
  The typed envelope makes omission unrepresentable.
- `kind: TimelineKind` — `focus_change`, `title_change`, `idle_start`,
  `idle_end`, `session_start`, `session_end`, `interaction_summary`,
  `clipboard`, `file_activity`, `browser_nav`, `demo_marker`, and `purge`
  (added with #843: the counts-only audit record a `timeline_purge` leaves
  behind; blanket purges never delete `purge` rows unless the kind is named
  explicitly, so a purge cannot consume its own audit trail). Extensible
  enum; unknown kinds are a decode error, not a silent skip.
- `actor: TimelineActor` — `human` or `agent { session_id }`. Recorded so
  mining can separate human activity from agent-driven activity (#837
  acceptance) and so cadence stats exclude synthetic input.
- `app: Option<String>` — process executable name where applicable.
- `payload: serde_json::Value` — kind-specific body (plaintext titles,
  paths, URLs, snippets, counts). Kind-specific typed payloads land with
  their producers (#837–#840); the envelope fixes only what retention,
  ordering, and attribution need.

Deliberately not representable at this layer: raw keystroke content.
`interaction_summary` carries counts and cadence buckets only (#838). This
is a schema-level decision, not a recorder courtesy.

### 4. Retention: TTL Days(90) + periodic compaction + size caps

`synapse_core::retention::DEFAULTS` gains
`CF_TIMELINE { ttl: Days(90), soft_cap_mb: 4096, hard_cap_mb: 8192 }`:

- The existing TTL compaction filter applies automatically (it reads
  `DEFAULTS`).
- `periodic_compaction_seconds = 86_400` is set on CF_TIMELINE options so
  every SST file passes the TTL filter at least daily — the long-retention
  correctness fix described in Context. Other CFs keep their behavior
  (short TTLs + write churn + GC/pressure compaction already cover them).
- The existing GC engine enforces the byte caps (evict-oldest beyond soft
  cap, `STORAGE_CF_HARD_CAP_REACHED` at hard cap). Sizing: event-driven
  recording is a few hundred bytes per row; a heavy interactive day is
  tens of thousands of rows (~10–20 MB), so 4 GiB soft cap comfortably
  holds 90 days with enrichments while bounding worst-case growth.
- Retention is config-overridable later via the recorder's config task; the
  defaults table remains the single source of truth for the filter and GC.

### 5. Disk pressure: shed at Level3, observable, never silent

`permits_write_at` policy for `CF_TIMELINE`: accepted at Normal–Level2,
shed at Level3 and Level4. The timeline is valuable but non-essential
relative to coordination/audit state (`CF_REFLEX_AUDIT`, `CF_SESSIONS`
remain the only Level4 survivors). Because pressure shedding silently
returning `Ok` contradicts the no-silent-success doctrine for a store whose
consumers mine continuity, every shed batch now increments a
`storage_writes_shed_total{cf}` metric alongside the existing structured
warning — for all CFs, not just the timeline. `timeline_stats` (#842) must
surface recorder gaps from this signal.

### 6. Purge and exclusion (forward constraint for #843)

Purge is `delete_batch` over a scanned time/key range followed by
`compact_range_cf` (tombstone reclamation), audit-logged with counts, not
content. Exclusion lists are enforced at the recorder (#837): excluded
processes never produce rows, so storage needs no notion of exclusion.

Implemented with #843: `timeline_purge` shares `timeline_search`'s filter
machinery (what you can find is exactly what you can delete), requires an
explicit filter or `all = true`, supports `dry_run`, compacts exactly the
purged key range (`Db::compact_cf_range`), and writes its `purge` audit row
with the pressure bypass plus an exact-key readback so the audit obligation
can never silently shed. Pause/exclusion state is one durable `CF_KV` row
(`timeline/control/v1`) hydrated at recorder startup; the recorder's single
write choke point enforces both gates, with suppression counters instead of
silent drops, and pause supports a Recall-style auto-resume deadline.

## Consequences

- `cf.rs`, `retention.rs`, and `m3/storage.rs` CF arrays go from 11 to 12;
  all storage tools (`storage_inspect`, `storage_put_probe_rows`,
  `storage_gc_once`) cover CF_TIMELINE with no further changes.
- Existing databases gain the CF on next open; no migration, no version
  bump. Rolling back the binary leaves an unused CF behind (harmless).
- The TTL filter's JSON `ts_ns` scan is reused unchanged; the typed
  envelope guarantees the field exists in every row.
- Long-retention plaintext content now lives in the daemon's RocksDB.
  This is the operator-decided posture; the purge/pause/exclusion controls
  (#843) are the user-freedom counterweight, not consent machinery.
- `periodic_compaction_seconds` requires `max_open_files` headroom; the DB
  already caps at 256 open files, which RocksDB handles by reopening files
  during periodic compaction (only FIFO-with-TTL requires `-1`).

## Sources

- RocksDB wiki, Compaction Filter:
  https://github.com/facebook/rocksdb/wiki/Compaction-Filter
- RocksDB wiki, RocksDB Tuning Guide (Periodic and TTL Compaction):
  https://github.com/facebook/rocksdb/wiki/RocksDB-Tuning-Guide
- RocksDB wiki, FIFO compaction style (considered, rejected: whole-file
  drops conflict with leveled reads and the shared GC/caps engine):
  https://github.com/facebook/rocksdb/wiki/FIFO-compaction-style
- RocksDB wiki, Delete A Range Of Keys (purge mechanics):
  https://github.com/facebook/rocksdb/wiki/Delete-A-Range-Of-Keys
- ActivityWatch docs, Buckets and Events (record-kind prior art):
  https://docs.activitywatch.net/en/latest/buckets-and-events.html
