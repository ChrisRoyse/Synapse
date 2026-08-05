# Issue #2013: Process temporal backfill eligibility

Date: 2026-08-05

## Source of truth

The source is `CF_PROCESS_HISTORY`; the result is the native Calyx Base/Slot
constellation and the temporal-backfill report. Verification used a fresh Aster
vault, the real process constellation builder, the real paged backfill entry,
and an independent read-only `dump_cf --native-source --reveal-metadata` process.

## Root cause

Process event time is optional. The builder represents an absent timestamp with
`temporal_lane_state=inactive` and `temporal_inactive_reason=source_missing_created_at`.
The backfill loop sent every row to an Aster migration primitive that intentionally
accepts active temporal contracts only. The source contract and page eligibility
contract therefore disagreed, poisoning the complete page on the first valid
timestamp-absent row.

The initial diagnosis also found that `process_ts_ns` used a permissive numeric
helper, so a present string/fractional/negative timestamp was silently treated as
absent. The fix distinguishes missing/null from invalid values and reports explicit
`temporal_ineligible_rows`. Only the exact inactive shape (nonempty reason and no
event-time coordinates) is ineligible; every malformed or contradictory shape
still fails closed.

## Research

Exa MCP was live and returned a real search response. The built-in web lane was
also used. Applied primary/vendor guidance:

- [Apache Flink state schema evolution](https://nightlies.apache.org/flink/flink-docs-stable/docs/dev/datastream/fault-tolerance/serialization/schema_evolution/) requires explicit compatibility rules when persisted state evolves.
- [Databricks Auto Loader schema evolution](https://docs.databricks.com/aws/en/ingestion/cloud-object-storage/auto-loader/schema) distinguishes missing fields from corrupt records and retains explicit schema/readback state rather than coercing malformed input.

## Manual state verification

Scratch vault id: `01KZ9KVZ1EG93BCXWYAR0DAH28`.

Invalid edge:

```text
before CF_PROCESS_HISTORY rows=0
input {"pid":9,"ts_ns":"not-a-timestamp"}
error process timestamp must be an unsigned JSON integer or null
after CF_PROCESS_HISTORY rows=0
```

Happy and optional paths used four authoritative rows: three without timestamps
and one with `ts_ns=1785955000000000000`.

```text
before rows=0
after source put rows=4
backfill examined=4 inserted=4 changed=0 current=0 temporal_ineligible=3
latest_seq=18 more=false
```

Independent physical Base readback:

```text
p10 cx=75b19291525887f3a6da074add3552ef panel=1965005
  temporal_lane_state=inactive
  temporal_inactive_reason=source_missing_created_at
p40 cx=529d6a136f58e1508f8102ccd518c412 panel=1965005
  temporal_lane_state=active
  source_event_time_raw=1785955000000000000
  source_event_time_secs=1785955000
```

Every source row was separately reread byte-for-byte after the trigger. The invalid
row created neither a source row nor a constellation. The scratch vault was removed
after readback.

## Gates

`cargo check --workspace` passed. `pwsh -File scripts/lint.ps1` passed all seven
gates in both workspaces, including format, deny, clippy, and the Calyx public-API
ratchet at 363.

## Production deployment

Commit `5b941e2a` was deployed through the supported setup path. Installed daemon
PID `11788` owned `127.0.0.1:7700`; installed executable SHA-256 was
`F151F796C735BD95A0323A0C02E5725A106B1E614E7E604FEE0932C42E3AF34E`.
A fresh HTTP MCP `storage/temporal_backfill` call against
`CF_PROCESS_HISTORY` succeeded and returned the new structured
`temporal_ineligible_rows` field. Production's TTL-managed source had zero live
rows at that read (`examined=0`, `more=false`, `latest_seq=657409`), so the
nonempty behavioral proof remains the four-row real scratch vault above rather
than being inferred from an empty production pass.
