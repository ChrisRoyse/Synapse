# Manual FSV Closeout: Issue #1716 Calyx GC WAL Chunking

Date: 2026-07-16

Issue: https://github.com/ChrisRoyse/Synapse/issues/1716

## Result

Accepted. Synapse now pre-plans Calyx CF write batches against the physical
Aster WAL record ceiling before writing anything, splits oversized GC batches
into bounded chunks, and fails closed with explicit row and payload diagnostics
when one row cannot fit.

No automated tests, FSV harnesses, benchmarks, or CI were created or run. The
commands listed here are structural checks or manual Source-of-Truth readbacks
only.

## Root Cause

The failing path was not Calyx row migration itself. The migrated vault had a
large number of expired/tombstoned rows. Startup GC accumulated all tombstone
purges into one `write_cf_batch`, and Aster encodes a write batch as one WAL
record. The Aster WAL hard cap is `67,108,864` payload bytes. The failing GC
batch was `86,568,178` estimated payload bytes, so the WAL writer correctly
failed closed instead of writing an oversized record.

The fix is to make Synapse respect the Calyx WAL boundary at the caller:

- expose `calyx_aster::wal::MAX_RECORD_BYTES` for embedding runtimes;
- estimate Calyx write-batch payload bytes before writing;
- plan every chunk before the first write, so a single-row oversize fails before
  partial mutation;
- commit each chunk below `MAX_RECORD_BYTES - 1 MiB` headroom;
- log chunk commit/summary records with row counts, payload bytes, and the WAL
  ceiling.

## Research Used

The design was checked with Exa MCP and native web research after identifying
the root cause:

- Prometheus WAL format, including page/sub-record behavior and segment
  boundaries:
  https://github.com/prometheus/prometheus/blob/main/tsdb/docs/format/wal.md
- Prometheus WAL writer code, which splits large records before writing:
  https://github.com/prometheus/prometheus/blob/0279e14d/tsdb/wlog/wlog.go
- RocksDB WAL file format, including block fragmentation with FIRST/MIDDLE/LAST
  record fragments:
  https://github.com/facebook/rocksdb/wiki/Write-Ahead-Log-File-Format

The operational lesson is that WAL writers either fragment large logical records
or callers must preflight/chunk before the fail-closed WAL boundary. Calyx Aster
currently has a fail-closed per-record cap, so Synapse now preflights and chunks
its logical write batches.

## Source Of Truth

- FSV evidence root:
  `C:\Users\hotra\AppData\Local\synapse\fsv\issue-1716\gc-chunk-final-20260716T224144721Z`
- Physical Calyx target copy:
  `C:\Users\hotra\AppData\Local\synapse\fsv\issue-1716\gc-chunk-final-20260716T224144721Z\target-calyx-copy`
- WAL SoT:
  `...\target-calyx-copy\wal`
- Daemon log SoT:
  `...\daemon-7783.stderr.log`
- Strict-client trigger transcripts:
  `...\strict-client-happy-gc-valid.jsonl`
  and `...\strict-client-edge-cases.jsonl`
- Separate physical CF readbacks:
  `...\after-final-*.dump.txt`
  and `...\after-final-cf-dump-summary.json`
- Separate WAL parse readback:
  `...\after-final-wal-summary.json`

## MCP Preconditions

Issue-specific daemon:

```text
pid=10216
bind=127.0.0.1:7783
exe=C:\code\Synapse\target\debug\synapse-mcp.exe
db=C:\Users\hotra\AppData\Local\synapse\fsv\issue-1716\gc-chunk-final-20260716T224144721Z\target-calyx-copy
storage_backend=calyx
calyx_vault=false
dev_binary_sha256=F53E70D7188AB685A160186242BCF1430056218F72E60373F92B9FC3C32E13FF
```

A fresh strict `codex exec` MCP client connected to
`http://127.0.0.1:7783/mcp`, authenticated with `SYNAPSE_BEARER_TOKEN`, loaded
the public tool surface, and called `health`. The health call returned
`ok=true`, `pid=10216`, `tool_count=40`, and Calyx storage `status=ok`.

The normal configured daemon remained separate and healthy:

```text
pid=64508
bind=127.0.0.1:7700
tool_count=40
```

## Before State

The target copy was copied from the #1661 migration target with
`317` files and `2,806,986,955` bytes. The copied machine salt SHA256 was
`44E338D664E841C5385B7635606BAB9E95AA1BE7CDC8E002F9B73446C6285E06`.

Initial physical CF readback before the GC trigger:

```text
CF_AGENT_TRANSCRIPTS rows=466546 sha256=0BEC043170409BD77090154165165875B17AF9F37C214E79FF220867FFDA5B91
CF_MODEL_CACHE rows=0 sha256=F778C4BB17A6CF100A59B3CCAF71B10C62CECF028918D31EE601DA6A1E5ED7C0
CF_ROUTINE_STATE rows=179 sha256=818C4D6487625BCC296CEB65239C42AF39E4DF64B08AA629CE550388E9E9A5B6
CF_EVENTS rows=107 sha256=ACBE6368387F92A5761033EC75332B1A6FBF77599D839F094B03F6542F431395
```

Initial physical WAL parse:

```text
segments=15
records=38
max_payload_bytes=61253766
wal_max_record_bytes=67108864
oversized_records=0
```

## Startup GC Chunk Evidence

The daemon ran startup storage GC against the copied Calyx vault before HTTP
serving became available. This hit the original failure class and proved the
chunking fix:

```text
STORAGE_CALYX_WRITE_BATCH_CHUNK_COMMITTED chunk_index=1 chunk_count=2 chunk_rows=410164 chunk_payload_bytes=66060183 max_payload_bytes=66060288 wal_max_record_bytes=67108864
STORAGE_CALYX_WRITE_BATCH_CHUNK_COMMITTED chunk_index=2 chunk_count=2 chunk_rows=126877 chunk_payload_bytes=20507999 max_payload_bytes=66060288 wal_max_record_bytes=67108864
STORAGE_CALYX_WRITE_BATCH_CHUNKED chunk_count=2 total_rows=537041 total_payload_bytes=86568178 largest_row_payload_bytes=253 max_payload_bytes=66060288 wal_max_record_bytes=67108864
STORAGE_CALYX_GC_TOMBSTONES_PURGED tombstone_rows=537041
```

The formerly oversized `86,568,178` byte batch was split into two bounded
chunks, both below the `67,108,864` byte WAL ceiling.

This startup behavior also exposed a separate readiness problem: HTTP did not
start until after long startup GC. That is tracked as
https://github.com/ChrisRoyse/Synapse/issues/1717.

## Happy Path

Trigger: strict MCP `tools/call` through `codex exec`:

```text
storage operation=gc_once
cf_name=CF_AGENT_TRANSCRIPTS
soft_cap_rows=1
hard_cap_rows=1
profile=full_capability
session_id=62b0a47b-f24b-4526-9886-fa8f14d8fdc7
```

Tool result:

```text
before_rows=466546
after_rows=1
total_evicted_rows=466545
cache_evictions_total_delta=466545
```

Separate physical readback after all triggers:

```text
CF_AGENT_TRANSCRIPTS rows=1 sha256=9A1AD3B2D01732EDE80672934349A5DF49A31B167890FC46F9BF348515435249
first_row key_sha256=sha256:708df8960460a9582b271d628113ba92c5fc32e18d0ad16e02c052d73c086d0f value_sha256=sha256:9984b8fc657d90eeadf02dec4120553e8f140e9ba7805e7b866ef9923c04d1b7
```

Verdict: PASS. The strict MCP trigger evicted the expected rows and the
separate Calyx CF dump proved the remaining physical row.

## Edge Cases

### Structurally Invalid Audit-Retention Payload

Trigger:

```text
storage operation=gc_once
cf_name=CF_AGENT_TRANSCRIPTS
run_id=<provided>
```

Result:

```text
status=error
code=TOOL_PARAMS_INVALID
message=storage operation=gc_once failed for CF_AGENT_TRANSCRIPTS: storage_gc_once audit retention fields require cf_name="AUDIT_RETENTION"
```

Separate later readback showed `CF_AGENT_TRANSCRIPTS` remained at the expected
state until the valid happy-path trigger changed it. Verdict: PASS.

### Empty CF No-Op

Before strict-client summary:

```text
CF_MODEL_CACHE rows=0
```

Trigger:

```text
storage operation=gc_once
cf_name=CF_MODEL_CACHE
soft_cap_rows=1
hard_cap_rows=1
```

After strict-client summary and physical dump:

```text
tool before_rows=0 after_rows=0 evicted_rows=0
physical rows=0
physical sha256=F778C4BB17A6CF100A59B3CCAF71B10C62CECF028918D31EE601DA6A1E5ED7C0
```

Verdict: PASS.

### Protected CF Boundary

Before strict-client summary:

```text
CF_ROUTINE_STATE rows=179
```

Trigger:

```text
storage operation=gc_once
cf_name=CF_ROUTINE_STATE
soft_cap_rows=1
hard_cap_rows=1000000
```

After strict-client summary and physical dump:

```text
tool before_rows=179 after_rows=179 evicted_rows=0 eviction_skipped_reason=protected_cf_policy_skipped
physical rows=179
physical sha256=818C4D6487625BCC296CEB65239C42AF39E4DF64B08AA629CE550388E9E9A5B6
```

Verdict: PASS.

### Invalid Cap Relationship Fails Closed

Before strict-client summary:

```text
CF_EVENTS rows=101
```

Trigger:

```text
storage operation=gc_once
cf_name=CF_EVENTS
soft_cap_rows=10
hard_cap_rows=1
```

Result:

```text
status=error
code=TOOL_PARAMS_INVALID
message=storage operation=gc_once failed for CF_EVENTS: storage_gc_once hard_cap_rows must be >= soft_cap_rows
```

After strict-client summary and physical dump:

```text
strict-client rows=101
physical rows=101
physical sha256=DB0DB0F2306C3285B4DC1F85BEAFEF7393227514A629173EF73A175B13A8415A
```

Verdict: PASS. The invalid call failed closed and did not mutate `CF_EVENTS`.

### Invalid Lease TTL Fails Closed

The edge-case strict client also attempted `act operation=lease_acquire` with
`ttl_ms=300000`. The daemon rejected it with `TOOL_PARAMS_INVALID` and
`detail_code=LEASE_TTL_OUT_OF_RANGE`, then accepted the documented maximum
`ttl_ms=30000` for the actual storage triggers. Verdict: PASS.

## Final WAL Readback

The final WAL parse was performed after stopping only the isolated PID `10216`
and verifying the `127.0.0.1:7783` listener was gone.

```text
segments=18
records=158
wal_max_record_bytes=67108864
max_payload_bytes=66060209
oversized_records=0
total_payload_bytes=983156729
```

Largest physical records:

```text
00000000000000000015.wal seq=39 payload_bytes=66060209 over_limit=false
00000000000000000011.wal seq=35 payload_bytes=61253766 over_limit=false
00000000000000000010.wal seq=34 payload_bytes=60201303 over_limit=false
00000000000000000012.wal seq=36 payload_bytes=60128957 over_limit=false
```

Verdict: PASS. The physical WAL has no records above Calyx's
`67,108,864` byte ceiling.

## Structural Checks

Already run during implementation:

```text
cargo fmt --all
cargo check -p synapse-storage
cargo clippy -p synapse-storage --all-targets
cargo build -p synapse-mcp
```

Final pre-push checks were run again before commit.

## Host Hygiene

The isolated FSV daemon was stopped only after exact PID/path/command-line/socket
verification:

```text
pid=10216
process_exists=false
listener_127.0.0.1_7783=false
```

The normal configured daemon on `127.0.0.1:7700` remained live.
