# Issue #1965 MCP-usage record-vector Full State Verification

Date: 2026-08-04. Commit: `22038923`. Host: configured production Windows host.

## Root cause and research

The active MCP-usage panel had no rebuild source even though its authoritative rows
are the `mcp-usage/v1/` subset of shared `CF_KV`. Its slot 93 used
`syn_record_vector` over raw magnitudes; the live search census reported every
nearest-neighbour cosine as exactly `1.0`. A whole-`CF_KV` backfill would also
reinterpret unrelated outcome rows, and hashing the JSON value would not reproduce
the ingestion identity, which frames CF, key, and value.

The Exa lane was verified live with `scripts/check-research-lane.ps1` before use.
Research also used the Microsoft Data API Builder `$after` documentation and AWS
DynamoDB pagination documentation. Both support the implemented contract: a bounded
candidate page, opaque exclusive continuation token, and continuation based on the
token rather than whether the filtered result is empty.

## Source of truth

- Authoritative inputs: physical Calyx KV rows under `mcp-usage/v1/`.
- Result: Calyx Base rows at panel `1965007`, physical slot CF 115 rows, Anchors CF
  rows, Ledger chain, panel-coverage census, installed daemon image, and backup bytes.
- Pre-change verified backup:
  `issue-1965-observation-vectors-20260804T0240Z`.
- Post-change verified backup:
  `issue-1965-mcp-usage-vectors-20260804T0320Z`.

## Execute and inspect

The real-corpus scale probe read 10,264 authoritative rows: 10,264 decoded, zero
failures. The production builder measured slot 115 for every row. A deterministic
1,027-row discrimination sample produced 38 distinct nearest-neighbour cosine grades
in `[0.994863, 1.000000]`, replacing the old one-grade result.

Before migration, `storage panel_coverage` physically counted three active rows,
10,264 grounded rows on superseded generations, `backfill_owed=true`, and 10,264
stranded anchors. Eleven opaque-cursor pages examined 10,278 source rows. They
inserted exactly 10,264 missing active rows, recognized 14 concurrently written rows
as current, and carried exactly 10,264 grounded anchors. The final page returned
`more=false`.

Independent post-trigger coverage reported:

```text
panel=syn-mcp-usage-v1 version=1965007
active_version_records=10279 grounded_records=10279 grounded_fraction=1.0
anchors_stranded_on_superseded=0 backfill_owed=false
accounting_holds=true decode_failures=0 unknown_panel_versions=[]
```

An independent read-only slot census at snapshot 489312 then read 10,280 active
Base rows. Every declared slot had 10,280 physical rows, zero missing CF rows, and
zero undecodable rows. Slot 115 was dense dimension 128 with 3,224 distinct stored
vectors. Slot 93 was absent from the active contract.

Re-running the first 1,000-row page was idempotent: `inserted=0`,
`already_current=1000`, `anchors_carried_forward=0`, with the same opaque cursor.

## Boundary audit

Panel state immediately before the three rejects was Base 245,478, active/grounded
10,282, no owed backfill and no stranded MCP anchors.

1. `max_rows=0` failed `TOOL_PARAMS_INVALID`, named the accepted `1..=1000` range,
   and stated that no storage operation was attempted.
2. `max_rows=1001` failed identically at the hard upper boundary.
3. Exact key `00` failed `STORAGE_BACKEND_INVALID_CONFIG` because it is outside the
   declared `mcp-usage/v1/` prefix, with a prefix-specific remediation.

After these real tool calls, coverage remained `backfill_owed=false`, stranded zero,
accounting true, decode failures zero, and unknown panels empty. Base and active
counts rose by four only because each MCP call is itself atomically recorded as a
new grounded usage row; no rejected migration inserted an old-generation target.

## Installed and durable evidence

The installed daemon is PID 21112 at `%USERPROFILE%\.cargo\bin\synapse-mcp.exe`,
SHA-256 `EB07574D2667873EE136B28DDF6FB746ED6E982EAE93795A105D2C7F37EFA152`,
185,552,489 bytes. Setup's final nonzero verdict was the intentional
`SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` gate after successful handoff.

The post-change backup manifest SHA-256 is
`f35edfdb0a7708648ef9dd26b5cad3e49a9db301501a712b87df766a60a24eb6`, total
2,219,267,676 bytes. A separate `restore_verify` reopened its bytes and reported
245,493 constellations, 60,919 anchors, 317,871 ledger entries, intact chain tip
`621ac1f049caeb1bea285fe4571751544e97b766fc2a6b06ab4e3fde41f3e6f2`, and no
failure reasons. The temporary maintenance profile and foreground lease were restored.
