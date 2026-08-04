# Issue #1965 - action, reflex, and process record-vector retirement

Date: 2026-08-04

## Source of truth

The source of truth is the live Calyx source-CF/Base coverage plus the physical
Base rows in the verified backup:

`%LOCALAPPDATA%\synapse\fsv\issue-1965-retired-record-vectors-20260804T0050Z`

The API reports migration progress. The independent
`retired_record_vectors_fsv` reader reopens the backup and decodes every Base row
to prove the new generations do not declare the retired slots.

## Diagnosis and research

Slots 50, 59, and 66 were already measured over the real corpus by #1964 and
all had nearest-neighbour cosine exactly 1.0. Reading their builders showed that
they only aggregate fields already represented by dedicated categorical, time,
hash, rank, or log-scaled slots on the same panel. Rescaling the aggregate would
therefore add redundancy rather than independent information. The correct
capability-gate decision is to park it.

Exa MCP was live and queried after diagnosis for constant/redundant feature
selection and versioned data migration. Built-in research read primary sources:

- scikit-learn's `VarianceThreshold` removes zero-variance features:
  <https://scikit-learn.org/stable/modules/generated/sklearn.feature_selection.VarianceThreshold.html>.
- Microsoft describes explicit incremental migrations that preserve existing
  data and record applied versions:
  <https://learn.microsoft.com/en-us/ef/core/managing-schemas/migrations/>.

Commit `26dac997` advances action/reflex/process to panel generations
1965003/1965004/1965005, reserves historical slot IDs 50/59/66, removes those
slots from new builders and provenance, and lists 1776001/1776002/1776003 as
superseded. The existing source-CF backfill paths perform the migration.

## Before, trigger, after

Before deployment, independent coverage read:

- action 1776001: 975 active records; `CF_ACTION_LOG` had 110 surviving rows
- reflex 1776002: 5 active records; `CF_REFLEX_AUDIT` had 5 rows
- process 1776003: 2 active records; `CF_PROCESS_HISTORY` had 0 rows

This is TTL reality. A migration may re-measure only extant source rows; it must
not invent expired history.

After deployment, before backfill, new generations had zero active records;
action owed 110 and reflex owed 5. Real `temporal_backfill` then reported:

- action: examined 110, inserted 110, more=false
- reflex: examined 5, inserted 5, more=false
- process: examined 0, inserted 0, more=false

Independent post-trigger coverage read action 110/110 and reflex 5/5 at coverage
1.0 with zero uncovered rows and no debt. Process remained correctly 0/0 with
coverage 1.0. The old TTL-expired generations remain explicitly superseded.

## Physical proof

The new backup contains 1774 declared vault files and 2165206270 vault bytes;
manifest SHA-256 is
`4e1e0dd2602389dba748746ad084658b6ac67c780c6581411026d287830a6164`.
Independent restore verification passed with:

- 235093 constellations
- 50577 anchors
- 296877 ledger entries
- intact tip `bee4d65efe598cb6c19d98c9b53279705df0286507ab65c985c1a6bb51c81ef3`
- 15643000 WAL bytes

The physical Base reader printed:

```text
action panel=1965003 base_rows=110 rows_declaring_retired_slot_50=0
reflex panel=1965004 base_rows=5 rows_declaring_retired_slot_59=0
process panel=1965005 base_rows=0 rows_declaring_retired_slot_66=0
verdict=PASS physical Base rows omit every retired record-vector slot
```

## Boundary audit

1. Idempotent replay: action backfill repeated over all 110 source rows and
   returned `already_current_rows=110`, `inserted_rows=0`, `more=false`.
2. Unsupported CF: `CF_KV` returned `STORAGE_BACKEND_INVALID_CONFIG`; `CURRENT`
   SHA-256 `1C6AEF4ECB79E2F3094D4D7DAE162068CD760AE418B6205FA590999DBF69E5F9`
   and manifest SHA-256
   `4A3E6DF858B93BEBE0DEE81C23FA4AD718F653E77974D4ECD69D39ACA2FABCA1`
   were identical before and after.
3. Row-count lower boundary exposed #1990: zero was initially accepted as a
   false-success mutation. Work stopped, #1990 was filed and fixed, and the
   corrected boundary evidence is recorded separately.

After the latest backup passed restore verification, 13.334 GiB of older FSV
copies were removed by exact scope-checked paths. Only this current artifact is
retained.
