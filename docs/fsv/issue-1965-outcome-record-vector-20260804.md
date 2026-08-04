# Issue #1965 Outcome Record-vector Full State Verification

Date: 2026-08-04. Implementation commit: `aac3f32f`.

## Diagnosis and source of truth

The outcome panel's slot 81 used the magnitude-weighted `syn_record_vector` and
had no rebuild path because the catalog described it only as a subset of shared
`CF_KV`. An independent Base metadata census proved all 18 historical records,
with zero missing source identity, came from `CF_KV` prefix
`escalation/v1/audit/`. A separate authoritative KV scan found exactly 18 rows
under that prefix out of 64,831 KV rows. This exact agreement defines the
population without admitting unrelated escalation or KV data.

Exa MCP was verified live and used. Built-in research used the Microsoft Data
API Builder `$after` and AWS DynamoDB pagination primary documentation: bounded
candidate pages retain an opaque exclusive cursor and continuation does not
depend on a non-empty filtered result.

## Trigger and physical readback

The production builder measured all 18 authoritative rows into candidate slot
116, producing seven distinct vectors. The deployed active generation is
`1965008`; retired slot 81 remains reserved historically. Installed daemon PID
1616 runs `%USERPROFILE%\.cargo\bin\synapse-mcp.exe`, SHA-256
`095D873D3D2288DC58079BB9848DC7B3CA721A9B6008153D12CE1C0DA148183D`,
185,554,025 bytes.

Before migration: active 0, superseded grounded 18, stranded 18,
`backfill_owed=true`. One bounded prefix page examined 18, inserted 18, carried
18 anchors, returned `more=false`, and advanced durable sequence to 489905.

Independent panel coverage then read active 18, grounded 18, fraction 1.0,
stranded 0, `backfill_owed=false`, Base accounting true, zero decode failures,
and no unknown panel versions. An independent read-only physical census at
snapshot 489912 read all 245,538 Base rows and found 18 outcome rows. Every one
had all seven declared physical slot rows, with zero missing/undecodable rows.
Slot 116 was dense dimension 128 with seven distinct values; slot 81 was absent
from the active contract.

An idempotent rerun examined 18, reported 18 already current, inserted zero and
carried zero anchors.

## Boundary audit

Real calls with `max_rows=0` and `max_rows=1001` failed
`TOOL_PARAMS_INVALID`, named the accepted `1..=1000` range and stated no storage
operation ran. Exact key `00` failed `STORAGE_BACKEND_INVALID_CONFIG`, named the
`escalation/v1/audit/` boundary and remediation. No rejected call altered the
18-row target population.

## Durable evidence

Final retained backup:
`%LOCALAPPDATA%\synapse\fsv\issue-1965-all-record-vectors-20260804T0355Z`.
Manifest SHA-256:
`2596c14a9ef46af864d5725992b38c499a4db05202763bdc38841fee456b56ca`,
2,253,217,233 bytes. A separate `restore_verify` reopened the backup and read
245,550 constellations, 60,951 anchors, 318,020 ledger entries, intact tip
`8ecd51422e57bdd6e434f85133ea00022bd0372f2bd0b9a1a77a6772c42ac2bc`,
36,085,972 WAL bytes and no failure reasons. Temporary maintenance state was
restored and its foreground lease released.

After this final backup passed, the two superseded #1965 backups were scope-
checked under the FSV root and removed, reclaiming 4,384,361,033 bytes.
