# Issue #1668 panel lifecycle end-to-end FSV (2026-08-04)

## Root cause

The upstream `SwapController` queue had no Synapse caller or durable per-panel
owner. Writing only a new Slot CF row beside an old constellation could never
make it visible to panel-qualified search: record identity includes the panel
generation, so lifecycle backfill must re-read the authoritative source and
write a new CxId/Base generation. The manifest's single active-panel pointer is
a serving default and cannot safely own mutations for every `syn-*` panel.

## Research

The Exa lane was verified live with `scripts/check-research-lane.ps1` and used
alongside built-in web research. The implementation follows the durable,
observable, resumable, bounded-work model documented for online schema changes:

- CockroachDB online schema changes and jobs:
  https://www.cockroachlabs.com/docs/stable/online-schema-changes
- CockroachDB multi-version schema change design:
  https://github.com/cockroachdb/cockroach/blob/master/docs/RFCS/20151014_online_schema_change.md
- CockroachDB job status/readback:
  https://www.cockroachlabs.com/docs/stable/show-jobs

Applied here: one revision-guarded Registry CF row per logical panel; a separate
vault-global generation allocator; bounded claims; explicit restart recovery;
pressure checks before claim and each record; authoritative-source
remeasurement; physical readback before task completion; immutable old Base and
Slot rows.

## Source of truth

Disposable real vault:

`%TEMP%\synapse-panel-e2e-5c90a74ed6754d8ba6a4d7962d7d4b9d`

Physical sources inspected independently after closing the writer:

- Registry CF lifecycle row and queue state
- Base CF rows at allocated generation `1965009`
- Slot CF `slot_0` vector rows
- panel-scoped persisted search generation manifest and indexes

Driver:

`cargo run -p synapse-storage --example panel_lifecycle_end_to_end_fsv -- <absent-vault-dir>`

## Happy path

1. Persisted two real typed `EpisodeRecord` source rows and their built-in
   generation constellations.
2. One `add_panel_lens` call added deterministic `byte_features`, allocated
   generation `1965009`, slot `0`, and queued exactly two physical Base ids.
3. Two bounded one-row worker calls each re-read the authoritative episode JSON,
   re-measured the complete panel, wrote a new CxId/Base/Slot generation,
   hydrated the physical row independently, and only then completed its task.
4. A third source row ingested after the add was atomically measured into both
   the built-in and lifecycle generations.
5. Search rebuild accepted the durable lifecycle contract without changing the
   active-panel pointer. Query-by-example through only slot `0` returned two
   hits. Manifest SHA-256:
   `22f8d61625d5ea151ff50e9cece4898e01eea05066ee24eb870eff6dfd24d70d`.

## Physical readback

After closing `Db`, a separate read-only vault handle observed:

```text
Registry_rows=6
Base_target_panel_rows=3
Slot_0_rows=3
decoded_slot_rows=3
```

The three physical Slot values had SHA-256 values:

```text
406ea55980e21dac34cefd61be839611aeb8a29c6f2e9f8ea9dff47056947fe1
8e828c42b89549d3177b4df04b0ddf93bffb85eb4d1ea62d002486d3c5c1972d
c293d157e4c3562c4dadb0b7f5976a7f6892d40ef72dad2dbf5f2ecb26b0c238
```

## Boundary audit

The complete serialized lifecycle readback SHA-256 was
`199b774a0f4ad3bd4ece1dce514a1c6b111487c4954710266b1227168e509bd5`
before and after every rejected action:

1. Backfill limit `0`: refused with
   `SYNAPSE_CALYX_PANEL_LIFECYCLE_INVALID`; Registry unchanged.
2. Unknown panel `u32::MAX`: refused as exact-active-panel config error;
   Registry unchanged.
3. Operation id `BAD`: refused because the id is not exactly 64 lowercase hex
   characters; Registry unchanged.

## Gates

`pwsh -File scripts/lint.ps1` passed all seven gates in both workspaces.
The reached-public-API count improved from 373 to 370 and the ratchet was
tightened accordingly.

The freshly linked `target/debug/synapse-mcp.exe` was then launched as a real
isolated stdio child with a fresh vault and shell-job root. MCP `initialize`
negotiated protocol `2025-06-18`; `tools/list` advertised the storage schema;
and this real call succeeded:

```json
{"operation":"panel_lifecycle","panel_lifecycle":{"action":"read","panel_version":1964001}}
```

The response independently named `Calyx Registry CF panel=1964001 action=Read`
as its readback source and returned `registry_readback:null`, matching the fresh
vault's absent lifecycle row. Shutdown then flushed durable sequence 11,
recorded the matching lineage high-water mark, removed both PID sidecars, and
proved both daemon locks could be reacquired.
