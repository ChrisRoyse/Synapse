# FSV — #1874: `verify_chain` tamper REJECTION exercised against reality

Date: 2026-07-28
Host: CABTOP (Windows 11 Pro 10.0.26200)
Binary: `target\release\examples\provenance_tamper_fsv.exe`, built from the same
tree as the deployed daemon (commit `f801b386` + the FSV instrument).
Research lane: not required — this issue is a verification gap, not a defect
needing a best-practice fix. No Exa call was made.

## The gap being closed

`audit operation=verify_chain` had only ever been observed reporting `intact`.
Nothing had ever observed it *reject* anything, so the tamper-evidence property
was advertised but unproven — and `verdict=intact` looks identical whether the
verifier is computing hard or trivially returning success.

## Why an isolated vault, and why the tamper is written under the API

The only vault on this host is the live production one. Corrupting it to prove a
verifier works is not an acceptable trade, so every case below runs against a
disposable vault under the session scratchpad. Everything in that vault is real:
a genuine Aster vault opened by the real `SynapseCalyxVault`, real WAL/MVCC
commits, the real checkpoint sealer, the real `verify_ledger_chain`.

Two attempted tampers were **refused by the storage layer itself**, which is a
result worth recording on its own:

```
CALYX_ASTER_RAW_COMMITMENT_WRITE_FORBIDDEN
  write_cf_batch: raw commitment mutation is reserved: row_index=0 key_len=8 value_len=56
  remediation=... Aster derives the sequence-bound commitment row atomically and
              callers cannot supply or overwrite it

CALYX_ASTER_LEDGER_RAW_WRITE_FORBIDDEN
  write_cf_batch: raw Ledger mutation is reserved: row_index=0 key_len=8 value_len=201
  remediation=write Ledger entries through append_ledger_entry, ...
```

So the API-level tamper vector is closed. Real corruption and a real attacker do
not use the API, so the tampers below are written directly into the physical SST
bytes with **both CRC layers recomputed** (per-record CRC and the SST body CRC).
The storage layer therefore accepts every tampered file as structurally intact —
anything reported afterwards is the provenance layer's own detection, not a CRC
failure. This is exactly the failure mode #1874 called out as the one a naive
recompute-and-compare is fooled by.

## Case 1 — build, seal, verify intact (the control)

Six real single-row batches, then a forced checkpoint so the cohort seals.

```
SEED_BATCH batch=0 committed_seq=1 key=fsv-1874/row-0000 value_len=27
... (batches 1..5, committed_seq=2..6)
SEED_AFTER latest_seq=7
SEED_VERIFY {"intact":true,"verdict":"intact","head_height":1,
  "raw_commitments_intact":true,"raw_commitment_seal_count":1,
  "raw_commitment_count":6,"raw_commitment_sealed_count":6,
  "raw_commitment_pending_count":0,"raw_commitment_coverage_from_seq":1,
  "raw_commitment_sealed_through_seq":6, ...}
```

Physical SoT read back independently (`list-commitments`), six 56-byte rows,
each with the `CYXRAW01` magic, `row_count=2`, and a distinct batch hash.

## Case 2 — mutate a SEALED raw-commitment value

One bit flipped in the batch hash of the commitment at seq 3, mid-cohort. Every
structural field is byte-identical; only the committed hash differs.

```
old ...957ebe98…584f41ed7
new ...957ebe98…584f41ed6
SST_PATCH_WRITTEN records=6 crc_reparse_ok=true   (both SSTs holding the row)
```

Independent re-open + verify:

```
{"intact":false,"verdict":"corrupt",
 "corrupt_reason":"raw commitment Ledger seal 0 does not match physical
                   commitment rows 1..=6 count=6",
 "raw_commitments_intact":false,"raw_commitment_seal_count":0,
 "raw_commitment_sealed_count":0}
```

**Rejected.** The cohort is named exactly; the seal count drops to zero.

## Case 3 — mutate a Ledger entry inside the verified range

A fresh vault seeded to three Ledger entries (`head_height=3`). The actor field
of entry seq 1 was changed (`calyx-aster` → `calyx-astes`) — a change that does
not touch the raw-commitment seal payload, so any detection must come from the
Ledger hash chain itself.

```
SST_PATCH_WRITTEN records=1 crc_reparse_ok=true   (both SSTs holding the row)
```

Independent re-open:

```
SYNAPSE_CALYX_LEDGER_CORRUPT
  open durable Calyx Aster vault: ledger entry seq 1 hash mismatch
  remediation=ledger CF integrity violation — run verify_chain to identify range
```

**Rejected, and stronger than the issue asked for**: the mutation is caught at
vault *open*, naming the exact height (seq 1). The daemon refuses to serve a
vault whose Ledger has been altered at all — verification is not something a
query has to remember to ask for.

## Case 4 — delete a raw-commitment row inside a sealed cohort

Fresh vault, cohort of 6 sealed. The row at seq 3 (mid-cohort) was removed from
the SSTs, with the record region and index rebuilt and both CRC layers recomputed.

Physical readback after the drop shows seqs 1, 2, 4, 5, 6 — seq 3 gone.

```
{"intact":false,"verdict":"corrupt",
 "corrupt_reason":"raw commitment Ledger seal 0 claims 6 rows but only 5 remain
                   in the physical commitment CF",
 "raw_commitments_intact":false,"raw_commitment_count":5,
 "raw_commitment_sealed_count":0}
```

**Rejected.** The missing row does not silently shrink the cohort into a valid
smaller root.

## Case 5 — roll the tail back so a seal references absent sequences

Fresh vault, cohort of 6 sealed. The highest sealed row (seq 6) was removed —
the shape a truncation or rollback produces.

Physical readback shows seqs 1..5.

```
{"intact":false,"verdict":"corrupt",
 "corrupt_reason":"raw commitment Ledger seal 0 claims 6 rows but only 5 remain
                   in the physical commitment CF",
 "raw_commitment_sealed_count":0}
```

**Rejected, and fail-closed rather than `pending`**: `sealed_count` is 0 and the
verdict is `corrupt`. The surviving rows are not quietly reclassified as an
unsealed tail.

## Result

| # | Case | Expected | Observed |
| - | ---- | -------- | -------- |
| 1 | seal then verify | `intact` | `intact`, 1 seal / 6 sealed / 0 pending |
| 2 | mutate sealed commitment | fail-closed, cohort named | `corrupt`, seal↔rows 1..=6 mismatch |
| 3 | mutate Ledger entry | chain break at exact height | `CALYX_LEDGER_CORRUPT` at open, `seq 1` |
| 4 | delete row in cohort | detected, not shrunk | `corrupt`, "claims 6 … only 5 remain" |
| 5 | roll back the tail | fail-closed, not `pending` | `corrupt`, sealed_count=0 |

The negative direction of `verify_chain` is now exercised. The verifier is not
returning a constant, and it detects each of the four tamper shapes named in the
issue — including the two (4 and 5) where the data and the commitment move
together.

## Instrument

`crates/synapse-storage/examples/provenance_tamper_fsv.rs`, kept in the tree so
the rejection path can be re-exercised on demand rather than re-derived. It
operates only on a vault directory named on the command line and never discovers
the production vault.
