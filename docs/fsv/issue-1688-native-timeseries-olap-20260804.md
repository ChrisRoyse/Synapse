# Issue #1688 Full State Verification - 2026-08-04

## Diagnosis and research

The cost and telemetry implementations stored JSON projections in `CF_KV` / `CF_TELEMETRY`
while describing them as TimeSeries. Fleet cost reads were therefore corpus/index scans, telemetry
had no native window rollups, and the existing Aster OLAP column artifacts were not callable.

The Exa MCP lane was checked first with `scripts/check-research-lane.ps1`; its live query succeeded.
Independent primary-source research used Apache Flink's keyed-state/checkpoint documentation for
atomic, replayable materialization semantics and Prometheus instrumentation guidance for bounded,
low-cardinality telemetry. The resulting design uses immutable native TimeSeries generations,
durable source/outbox rows, explicit sealed watermarks, fail-closed conflicts, and bounded rollup
reads. No fallback scan exists for ordinary fleet queries.

## Sources of truth

- Cost input: physical `CF_AGENT_TRANSCRIPTS` rows.
- Cost aggregate: native Aster `TimeSeries` rows in the collection named by
  `CF_KV agent-cost/rollup/v2/__meta`; owner and mark rows bind points to source spawns.
- Telemetry input: sealed physical `CF_AGENT_EVENTS` rows.
- Telemetry aggregate: native Aster `TimeSeries` collection `syn-telemetry-v3` and its
  `telemetry/native/v3/*` owner/progress rows.
- OLAP: physical `derived/olap/<panel-slot>/slot-column.cxa1` bytes and its SHA-256 manifest.
- Process: Windows process table and TCP listener table.

## Live production verification

The setup path installed the repository release build and replaced PID 21100 with PID 18376 on
`127.0.0.1:7700`. Installed image SHA-256 was
`A01B6E0422C06323574F9E8E3803D1256613FA018FDE3F6AD6CE66991F01A779`.

Before cost materialization, the live vault held 50,973 transcript rows. A real MCP
`cost rollup_backfill reset=true` examined 56 spawns, scanned those 50,973 source rows, and wrote
105 native points. A real fleet `cost summarize {}` then returned:

```
query_strategy=native_timeseries_rollup_point_reads
scanned_rows=0 scanned_event_rows=0 scanned_index_rows=0
spawns_total=21 total_tokens=597933 source_reported_micro_usd=926041
rollup_windows_read=7304
```

An immediate `reset=false` replay wrote zero cells. Independent native-table scans before and after
were byte-identical:

```
TimeSeries rows=782 bytes=40813 sha256=38771e13221e619df5a028afe7152981a48e6f9851a57b576bca376565307663
Collections rows=3 bytes=249 sha256=eb236ead8707471cfde019c54c7ecf07e136d2035755756362421665fca3d238
```

Telemetry initially exposed a real collision: two durable agent events legitimately shared one
timestamp, while the attempted point-per-event mapping required `(series,timestamp)` uniqueness.
The final implementation instead seals and counts all events per hour, publishing one immutable
point per metric/hour. After deployment, repeated maintenance cycles logged
`TELEMETRY_HEALTH_TREND total_events=2 error_events=0`. Independent reads 20 seconds apart were
identical, proving replay stability:

```
TimeSeries rows=1264 bytes=65326 sha256=9c2bd82c3e7d1e4064d0cd801270451c0c1316ca7a16609532600fc8da4a1d68
Collections rows=4 bytes=308 sha256=3d439c30bc3fa36ecc768e6b3757982f1452c055479eb199d790cc14f29d36b7
CF_KV prefix telemetry/native/v3/: 3 rows
```

A real `telemetry status` call succeeded and independently increased the native table only through
its expected gauge samples. The public provenance contract now names native TimeSeries rather than
the obsolete empty `CF_TELEMETRY` family.

The real OLAP MCP operation over panel 1963001, slot 7 returned 1,375 rows with sum
`598.4590483009815`, average `0.43524294421889564`, minimum `0.43515148758888245`, maximum
`0.4353214502334595`, and artifact SHA-256
`17906318eac1e7b91bfd25958dd59e59e04e645f8b5d1f5a7ef7e150fd9f734b`. A separate PowerShell
binary parser read `slot-column.cxa1` and independently reproduced the same row count and values;
the file hash exactly matched the MCP response and manifest.

## 200,000-row differential

The deterministic seeder wrote 1,000 valid Claude spawn streams with 200 rows each into a real,
temporary Calyx vault. A separate typed raw-row scan read all physical rows and computed:

```
physical_rows=200000 result_rows=1000
input_tokens=100000 output_tokens=50000 total_tokens=150000 source_micro_usd=123000
```

The installed release daemon served that vault on `127.0.0.1:7788`. A real MCP initialize,
tools/list (40 schemas), lease acquisition, break-glass profile transition, `tools/call`, and lease
release produced:

```
backfill_ms=100257 rows_scanned=200000 spawns=1000 cells_written=9000
summary_ms=72 strategy=native_timeseries_rollup_point_reads rollup_reads=22
raw_rows=0 index_rows=0 spawns_total=1000
input=100000 output=50000 total=150000 source_micro_usd=123000
```

Thus every differential field equaled the independent raw-row recomputation. A `reset=false`
replay reported `spawns_changed=0 cells_written=0`. Physical native rows before and after were
identical:

```
TimeSeries rows=9027 bytes=307593 sha256=e4bc8d08775b580677d0c348cee30f5ffda11e107f92548acc47f11e9efb770c
Collections rows=1 bytes=95 sha256=40eb572e0db2d841f4b4cf5b98c6c94aafdebeb77b944bc90a5ce7d9250b1e3e
```

## Boundary and edge-case audit

1. Unsupported backfill field. Before: 200,000 source rows, no aggregate. Trigger:
   `rollup_backfill {reset:true,max_spawns:1000}`. After: unchanged source state and structured
   `TOOL_PARAMS_INVALID`, accepted field `reset` only.
2. Unauthorized maintenance. Before: normal-agent session and no aggregate. Trigger:
   `rollup_backfill {reset:true}`. After: unchanged source state and structured
   `TOOL_PROFILE_POLICY_DENIED`.
3. Invalid lease TTL. Before: lease absent. Trigger: acquire with 600,000 ms. After: lease still
   absent and structured `LEASE_TTL_OUT_OF_RANGE` naming the 100..300,000 ms range.
4. OLAP invalid column. Before/after artifact hash unchanged; operation failed
   `CALYX_OLAP_INVALID_PLAN` because column 1 is outside dimension 1.
5. OLAP row cap. Before/after artifact hash unchanged; max_rows 1,000 failed
   `CALYX_OLAP_SCAN_LIMIT` because the artifact contains 1,375 rows.
6. OLAP invalid slot. Before/after artifact hash unchanged; slot 99,999 failed
   `CALYX_OLAP_INVALID_SLOT`.
7. Vault replacement. Deleting the temporary vault while retaining its sibling lineage journal
   failed closed with `SYNAPSE_CALYX_VAULT_RESET_UNACKNOWLEDGED`. Only the exact newly printed
   vault ID was acknowledged for that isolated replacement; the production lineage was untouched.

The temporary daemon was stopped by exact PID after evidence capture. Temporary FSV vaults and
shell-job roots were removed only after resolving and proving their paths were below `%TEMP%`.
