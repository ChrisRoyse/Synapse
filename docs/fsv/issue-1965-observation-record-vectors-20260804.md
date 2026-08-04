# Issue #1965: observation aggregate-vector retirement

Date: 2026-08-04

## Diagnosis and decision

The historical `syn-observation-v1` generation carried two aggregate
`syn_record_vector` lanes:

- slot 70, `syn.observation.hud_scalars.v1`, mixed HUD readings of unknown
  units and scales;
- slot 74, `syn.observation.record_vector.v1`, mixed a Unix-millisecond clock
  with counts, sizes, flags and DPI values.

Slot 74 had already measured nearest-neighbor cosine exactly 1.0. Slot 70 had
the same raw-magnitude construction and no frozen field scales. The live
authoritative `CF_OBSERVATIONS` population is currently empty after TTL, so a
replacement HUD squash cannot be graded on this machine. Publishing an
unmeasured replacement would violate the capability gate. Both lanes were
therefore retired, not renamed or silently transformed.

The panel advanced from `1776004` to `1965006`; slots 70 and 74 remain reserved,
the old generation remains readable, and the magnitude grandfather entry was
removed. `CF_OBSERVATIONS` gained a re-measure path that applies the same
deterministic sampling predicate as the write path before building any
constellation. An exact row outside the sampled population fails closed.

## Research

After diagnosis, `scripts/check-research-lane.ps1` proved Exa MCP live through
initialize, tools/list, and a real `web_search_exa` call. Built-in web research
used primary sources:

- scikit-learn documents `VarianceThreshold` as removing low-variance features:
  <https://scikit-learn.org/stable/api/sklearn.feature_selection.html>
- Microsoft migration guidance treats source/target inconsistencies as a
  reportable data-consistency condition rather than silent success:
  <https://learn.microsoft.com/en-us/exchange/mailbox-migration/track-prevent-data-loss-dcs>
- `rand_chacha` documents deterministic, portable generation, supporting the
  general requirement that a sampled population be reproducible:
  <https://docs.rs/rand_chacha/latest/rand_chacha/>

## Candidate failure and repair

The first release candidate failed before handoff. Setup retained 30 evidence
files; stderr SHA-256 was
`14FFAE497B216DD2ADE55AA1BFA528A684A1DA96E7E34A93EB647D6C8027DC0E`.
The startup gate reported orphaned provenance declarations for slots 70 and 74.
Root cause: the active slot catalog and builder removed the slots, but
`synapse-calyx::lens_provenance` still declared them. Commit `43f617c3`
removed those declarations and advanced remaining observation provenance to
generation `1965006`.

The corrected isolated candidate passed health and tool-surface verification.
The setup handoff installed:

- PID `17560`
- executable `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`
- SHA-256
  `A322548EA2B55569FAD3DDFB1646A0A55363A97C2864FA801A555945AB14E15A`

Setup's final status was the intentional
`SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` refusal because this Codex process
started with the prior storage schema. The daemon handoff itself completed and
the handoff file records the changed `health` and `storage` schemas.

## Full State Verification

### Source of truth

The sources of truth are:

1. authoritative live `CF_OBSERVATIONS` rows and panel-coverage census;
2. Calyx Base rows decoded from a separately verified durable backup;
3. the backup's per-file SHA-256 manifest and hash-chain verification.

### Trigger and independent reads

The real backfill trigger returned:

```text
source_cf=CF_OBSERVATIONS
examined=0 inserted=0 changed=0 current=0
more=false latest_seq=468132
```

An independent panel-coverage read returned:

```text
panel=syn-observation-v1 version=1965006
source_cf=CF_OBSERVATIONS source_is_full_cf=false
active_version_records=0 backfill_owed=false coverage_below_floor=false
superseded_records=2
```

The zero result is expected evidence, not fabricated history: the TTL-managed
source has no extant rows. Superseded generations remain `closed=false` because
closure is inferred from non-overlapping old/latest and active/earliest
timestamps; an empty active generation has no timestamp from which to infer
closure and is correctly non-reclaimable.

### Durable backup and physical Base proof

Backup:
`%LOCALAPPDATA%\synapse\fsv\issue-1965-observation-vectors-20260804T0240Z`

- vault files: 1,878
- vault bytes: 2,164,152,976
- backup manifest SHA-256:
  `70E748E75F08B6666D8CC6715658F60AFD2320E7040B42BC941D53F82D1FE6C5`
- constellations: 235,170
- anchors: 50,621
- ledger entries: 297,172
- ledger tip:
  `915531f5daaeb9bbe1834884cad6d37770b2798d50e3c6d5ecd9c4a09965f602`
- WAL bytes: 16,574,977
- restore verification: success, chain intact

The read-only `retired_record_vectors_fsv` Base decoder reported:

```text
observation panel=1965006 base_rows=0
rows_declaring_retired_slots_70_74=0
verdict=PASS
```

It simultaneously rechecked action/reflex/process and found no declarations of
their retired slots.

### Boundary and edge audit

1. Empty happy path: `max_rows=1000` completed with zero examined/inserted rows,
   `more=false`; independent coverage remained 0 active rows and no debt.
2. Underflow/overflow: `max_rows=0` and `max_rows=1001` each returned structured
   `TOOL_PARAMS_INVALID`, the accepted `1..=1000` range, and stated that no
   storage operation was attempted.
3. Invalid exact key: the absent 12-byte key
   `000000000000000000000000` returned `STORAGE_READ_FAILED` with the exact
   source CF and key rather than a zero-row success.

### Verification gates

- `cargo check -p synapse-calyx -p synapse-storage -p synapse-mcp`: pass
- `pwsh -File scripts/lint.ps1`: all seven gates passed in both workspaces

## Verdict

PASS. The active observation panel cannot write either magnitude-weighted
aggregate vector, its sampled backfill population is identical to its write
population, the empty live source produces no fabricated records or debt, and
physical Base bytes contain no active declaration of retired slots 70 or 74.
