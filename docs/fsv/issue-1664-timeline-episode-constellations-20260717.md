# Issue #1664 Timeline And Episode Constellations FSV

Date: 2026-07-17
Agent: Codex
Issue: https://github.com/ChrisRoyse/Synapse/issues/1664

## Result

Accepted for the Codex-owned #1664 scope. Timeline and episode ingest now
measures versioned native Calyx constellations from the authoritative raw rows,
stores provenance back to those rows, and converges idempotently when episode
segmentation is re-run.

No automated tests, FSV harnesses, benchmarks, or CI were created or run.
Compile/lint commands below are structural checks only and are not FSV.

## Root Cause

The first episode panel implementation used Calyx slot ids `101..115`. Native
Calyx durable column-family tags are one byte; raw slots are encoded as
`64 + slot_id`, so durable slot ids must be `0..47`. Slot ids above that range
either alias static tags or produce unknown tags on reopen. In the live DB this
created bad episode Base rows whose slot metadata referenced `101..115`; one
write path reached tag `130` and caused reopen/read failures.

The durable encoder also accepted out-of-range slot ids instead of failing at
the write boundary, so future panels could repeat the same class of corruption.

## Research Used

Exa and native web research were used after isolating the concrete failure:

- RocksDB WAL format: durable log records should have bounded, explicit record
  and column-family identifiers before append.
  https://github.com/facebook/rocksdb/wiki/Write-Ahead-Log-File-Format
- Azure Event Sourcing pattern: raw event rows stay authoritative; derived
  projections must be rebuildable from them.
  https://learn.microsoft.com/en-us/azure/architecture/patterns/event-sourcing
- W3C PROV-DM: derived artifacts retain explicit source/provenance references.
  https://www.w3.org/TR/prov-dm/
- RFC 8785 JSON Canonicalization: deterministic bytes are required before
  deriving content ids or hashes.
  https://datatracker.ietf.org/doc/html/rfc8785

## Implementation

Storage/Calyx:

- `cf_tag` now returns `Result<u8>` and rejects durable slot ids above `47`.
- Episode panel slots are renumbered to valid ids `8..22`; timeline uses `1..7`.
- Added `syn-timeline-v1` panel version `1664001`.
- Added `syn-episode-v1` panel version `1664002`.
- Constellation ids are derived from canonical raw row bytes plus panel version.
- Base metadata records `synapse_source_cf`, `synapse_source_key_hex`,
  `synapse_raw_sha256`, `synapse_raw_len_bytes`, and panel name.
- Measurement success/failure metrics and structured logs include panel/source
  identity and disposition.
- Measurement failures now fail the calling operation after logging the exact
  panel/source/error context; raw rows are not silently treated as fully
  accepted when their derived constellation write fails.

MCP/ingest:

- Timeline writers measure the timeline constellation after the raw
  `CF_TIMELINE` row is written.
- Episode segmentation measures each new raw `CF_EPISODES` row after the day
  replacement write.
- User-facing timeline audit and episode segment calls return errors when
  measurement fails instead of hiding the missing derived state behind a
  successful tool result.
- Public `episode` facade now exposes `operation=segment` for strict MCP FSV.
- `dump_cf` can read native Calyx Base/Slot/Scalars by source row and contains a
  one-time repair mode for the bad `101..115` episode slot rows.

## Sources Of Truth

- Daemon/process SoT: Windows process table and TCP listener `127.0.0.1:7700`.
- MCP SoT: `mcp__synapse.health`, strict client tools/list surface.
- Storage SoT: `C:\Users\hotra\AppData\Local\synapse\db-daemon`.
- Raw timeline rows: `CF_TIMELINE`.
- Raw episode rows: `CF_EPISODES`.
- Derived constellation rows: native Calyx Base, Slot, SlotRaw, Scalars, and
  Anchor column families read by `dump_cf --native-source`.

## MCP Preconditions

Configured daemon readback:

```text
pid=62860
path=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
bind=127.0.0.1:7700
db=C:\Users\hotra\AppData\Local\synapse\db-daemon
allowed_permissions=READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE
```

After the final source edits and release reinstall:

```text
installed_binary_sha256=50C347929F171D48BB9509779EA545B2991C9F300D7EF29665793EF1C7627FFC
previous_binary_sha256=8FD70AF5DEA7DEB36501FD4C6318519C60301C19D9ABAAEBE92AE1DBF8B92393
pid=87184
path=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
listener=127.0.0.1:7700 owner_pid=87184
db=C:\Users\hotra\AppData\Local\synapse\db-daemon
allowed_permissions=READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE
```

Health through the wired MCP client:

```text
ok=true
pid=87184
tool_count=40
tool_surface_sha256=7baef0742b0aacbb2a838a301a3ef25af3175468b15d831f6aa90b88dfd7b776
storage_backend=calyx
storage_db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
```

The already-running parent Codex process still reported
`SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` for the new episode schema. The
episode FSV triggers below were therefore run from fresh `codex exec` clients
that successfully called the real public `mcp__synapse.episode
operation=segment` tool.

## Bad Slot Repair Readback

Setup stopped the verified daemon under the maintenance lock, then the repo-built
`dump_cf` repair path inspected the real DB and tombstoned visible bad Base and
anchor references.

```text
repair before_seq=3239
candidate_count=69
target_count=828
duplicate_target_count=0
candidate slot_ids=101..115 source_cf=CF_EPISODES
repair_commit commit_seq=3240 after_seq=3240
repair_readback remaining_bad_episode_slot_constellations=0
```

Separate source readback for a formerly bad source key after repair:

```text
native_source source_cf=CF_EPISODES source_key_hex=18c3015f42d67ecc00000044
snapshot=3240
match_count=0
```

## Timeline FSV

Source of Truth: `CF_TIMELINE` raw row plus native Calyx rows for source key
`18c300c6fe96f324ffff0000`.

Before:

```text
CF_TIMELINE row_count=246985
```

Trigger:

```text
real MCP tool=mcp__synapse.privacy operation=purge
synthetic text=issue1664-fsv-zero-match-1784270785
dry_run=false
matched_rows=0
deleted_rows=0
audit_key_hex=18c300c6fe96f324ffff0000
```

After:

```text
CF_TIMELINE row_count=246986
```

Separate native readback:

```text
native_source source_cf=CF_TIMELINE source_key_hex=18c300c6fe96f324ffff0000
match_count=1
cx_id=cfa492a6a547626290d198776fb1a78e
panel_version=1664001
synapse_panel_name=syn-timeline-v1
slot_count=7
scalar_count=3
raw_len_bytes=466
record_version=1
ts_unix_ms=1784270732043
```

## Episode Happy Path FSV

Source of Truth: `CF_EPISODES` raw rows plus native Calyx rows for source key
`18c2fc77f81c48bc0000000a`.

Before:

```text
CF_EPISODES row_count=3707
```

Trigger:

```text
real MCP tool=mcp__synapse.episode operation=segment
start_ts_ns=1784270732043000000
end_ts_ns=1784270732044000000
include_agent_activity=true
dry_run=false
range_start_ns=1784264400000000000
range_end_ns=1784350800000000000
days_processed=1
scanned_rows=370
invalid_rows=0
ignored_agent_rows=0
payload_anomalies=0
episodes_written=85
episodes_deleted=69
constellations_inserted=85
constellations_deduped=0
constellation_failures=0
stopped_because=range_complete
```

After all accepted segment passes:

```text
CF_EPISODES row_count=3723
```

Public episode readback:

```text
episode_id=ep1-9efaf9ff52a35e54
key_hex=18c2fc77f81c48bc0000000a
actor=human
app=Code.exe
document=03_embedder_panel.md - leapablememory - Visual Studio Code
start_ts_ns=1784265994586048700
end_ts_ns=1784266003714191900
duration_ms=9128
row_count=4
timeline_refs=6
refs_invalid_rows=0
```

Separate native readback:

```text
native_source source_cf=CF_EPISODES source_key_hex=18c2fc77f81c48bc0000000a
match_count=1
cx_id=6cf4fb0d57c88e41ced129e932a6eef3
base_key_hex=6cf4fb0d57c88e41ced129e932a6eef3
value_sha256=127a316bb33f652c29d801a9d473cfedcf3fba92228c7fdad1451e30411f3d38
panel_version=1664002
synapse_panel_name=syn-episode-v1
synapse_source_cf=CF_EPISODES
synapse_source_key_hex=18c2fc77f81c48bc0000000a
slot_count=15
scalar_count=11
scalar_cf_rows_for_cx=11
slot_ids_present=8,9,10,11,12,13,14,15,16,17,18,19,20,21,22
base_scalar row_count=4
base_scalar duration_ms=9128
base_scalar keystroke_count=1
base_scalar click_count=2
base_scalar start_unix_ms=1784265994586
base_scalar end_unix_ms=1784266003714
```

## Idempotency FSV

Initial idempotency proof before the final release reinstall:

```text
before CF_EPISODES row_count=3723
trigger real MCP episode segment same range
episodes_written=85
episodes_deleted=85
constellations_inserted=0
constellations_deduped=85
constellation_failures=0
after CF_EPISODES row_count=3723
```

Final installed-daemon idempotency proof on PID `87184` used a paused recorder
so fresh FSV activity could not keep changing the current snapped day.

Pause readback:

```text
real MCP tool=mcp__synapse.privacy operation=pause
paused=true
persisted=true
boundary_row_written=true
changed_at_ns=1784276282939337200
```

Before paused idempotency:

```text
CF_EPISODES row_count=4201
native_source match_count=1
cx_id=6cf4fb0d57c88e41ced129e932a6eef3
value_sha256=127a316bb33f652c29d801a9d473cfedcf3fba92228c7fdad1451e30411f3d38
panel_version=1664002
slot_count=15
scalar_count=11
```

The final daemon then processed newly accumulated current-day rows once and
immediately repeated the same range:

```text
real MCP tool=mcp__synapse.episode operation=segment
same range as happy path, recorder paused
PAUSED_FIRST written=143 deleted=104 inserted=40 deduped=103 failures=0 scanned_rows=564 stopped=range_complete
PAUSED_SECOND written=143 deleted=143 inserted=0 deduped=143 failures=0 scanned_rows=564 stopped=range_complete
```

After:

```text
CF_EPISODES row_count=4240
native_source match_count=1
cx_id=6cf4fb0d57c88e41ced129e932a6eef3
value_sha256=127a316bb33f652c29d801a9d473cfedcf3fba92228c7fdad1451e30411f3d38
panel_version=1664002
slot_count=15
scalar_count=11
```

Resume readback:

```text
real MCP tool=mcp__synapse.privacy operation=resume
paused=false
persisted=true
boundary_row_written=true
changed_at_ns=1784276603529374100
suppressed_paused_total=15180
```

## Edge Case FSV

Dry-run edge:

```text
before CF_EPISODES row_count=3723
before native_source match_count=1 value_sha256=127a316bb33f652c29d801a9d473cfedcf3fba92228c7fdad1451e30411f3d38
trigger dry_run=true scanned_rows=370 episodes_written=85 episodes_deleted=85 constellation_failures=0
after CF_EPISODES row_count=3723
after native_source match_count=1 value_sha256=127a316bb33f652c29d801a9d473cfedcf3fba92228c7fdad1451e30411f3d38
```

Invalid range edge:

```text
before CF_EPISODES row_count=3723
trigger start_ts_ns=end_ts_ns=1784270732043678500
expected MCP error=-32099
code=TOOL_PARAMS_INVALID
message=episode_segment start_ts_ns 1784270732043678500 must be < end_ts_ns 1784270732043678500
after CF_EPISODES row_count=3723
after native_source match_count=1 value_sha256=127a316bb33f652c29d801a9d473cfedcf3fba92228c7fdad1451e30411f3d38
```

Empty future range edge:

```text
before CF_EPISODES row_count=3723
trigger start_ts_ns=4102444800000000000 end_ts_ns=4102444860000000000
scanned_rows=0
episodes_written=0
episodes_deleted=0
constellations_inserted=0
constellations_deduped=0
constellation_failures=0
stopped_because=range_complete
after CF_EPISODES row_count=3723
after native_source match_count=1 value_sha256=127a316bb33f652c29d801a9d473cfedcf3fba92228c7fdad1451e30411f3d38
```

## Structural Checks

All commands completed successfully. They are structural only and are not FSV:

```text
cargo fmt --all --check
cargo check -p synapse-storage --examples
cargo check -p synapse-mcp
cargo clippy --workspace --all-targets
cargo fmt --manifest-path calyx\Cargo.toml --all --check
cargo clippy --manifest-path calyx\Cargo.toml --workspace --all-targets
```

## Follow-Up Filed

While looking for a closed-day idempotency target, a strict MCP
`episode segment` call against a 2026-07-16 historical day exceeded the
client's 300s `tools/call` timeout while the daemon remained healthy and
`CF_EPISODES` later showed changed state. This is a performance/partial-verdict
risk separate from #1664's durability/provenance fix.

Follow-up issue filed: https://github.com/ChrisRoyse/Synapse/issues/1719
