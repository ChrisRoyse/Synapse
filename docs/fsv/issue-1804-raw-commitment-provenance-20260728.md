# FSV — issue #1804: raw namespaced CF rows are per-batch commitment-sealed

Host: `CABTOP`. Date: 2026-07-28. Implementation shipped in `393c18d5` /
`c9d3bb6a`; this record is the acceptance verification that was outstanding.

All triggers ran through the real wired MCP HTTP transport
(`initialize` → `notifications/initialized` → `tools/call` against
`http://127.0.0.1:7700/mcp`) — a production client shape, not a harness. Every
number was corroborated against a second, independent surface: either the
daemon's own `CALYX_ASTER_RAW_COMMITMENT_COHORT_SEALED` log events or the
physical CF directories.

## 1. Physical Source of Truth exists

```
cf\raw_commitment : 9 files, 1,399,888 bytes
cf\ledger         : 9 files, 3,933,539 bytes
```

## 2. `audit operation=verify_chain` over the wired client

```json
{"verdict":"intact","intact":true,"raw_commitments_intact":true,
 "entry_count":9306,"head_height":9306,"verified_from_seq":0,"verified_to_seq":9306,
 "tip_hash":"8461be557bfee8fdf1f2de2f03a4a1577246d4427955fd705cdbeed726197380",
 "raw_commitment_count":14252,"raw_commitment_sealed_count":14234,
 "raw_commitment_pending_count":18,"raw_commitment_seal_count":1835,
 "raw_commitment_sealed_through_seq":23514,"raw_commitment_coverage_from_seq":1}
```

`coverage_from_seq = 1` — the commitment coverage starts at the first sequence,
so there is no unclaimed prefix.

## 3. Happy path — a known trigger moves the chain

Three real `shell operation=run` actions, each of which writes `CF_ACTION_LOG`
rows:

| | before | after | delta |
| --- | --- | --- | --- |
| `raw_commitment_count` | 14265 | 14289 | **+24** |
| `raw_commitment_pending_count` | 3 | 27 | +24 |
| `raw_commitment_sealed_through_seq` | 23565 | 23565 | unchanged |
| `verdict` | intact | intact | — |

Exactly the designed shape: a commitment is written atomically with **every**
batch, and sealing is deferred to the checkpoint boundary.

## 4. Edge case — the checkpoint seal, cross-checked to the unit

After the next checkpoint, the daemon logged independently:

```
CALYX_ASTER_RAW_COMMITMENT_COHORT_SEALED
  first_commit_seq=23567 last_commit_seq=23639 commitment_count=37 ledger_seq=9349
```

and the MCP readback moved:

```
sealed_through_seq  23565 -> 23639     == last_commit_seq exactly
sealed_count        14262 -> 14299     == +37, == commitment_count exactly
```

Two independent surfaces agreeing to the unit, on both the sequence boundary and
the count.

## 5. Edge case — durability across a daemon restart

The daemon (pid 17588) was killed and the supervisor started a fresh process
(pid 12116), which re-verified the chain from disk:

```
verdict=intact  raw_commitments_intact=True  coverage_from_seq=1
entry_count         9364 -> 9381    (monotonic)
sealed_through_seq 23671 -> 23679   (monotonic)
```

## 6. Edge case — burst, and the volume property the design exists for

This is the property that made Option A and plain Option B unacceptable in the
issue's own analysis, measured directly.

Twenty real actions in a burst, before any checkpoint:

```
commitments added : 150
seals added       : 0
verdict           : intact
```

Every one of the 150 rows is individually committed and tamper-evident, yet the
append-only Ledger did not grow **at all**. After the next checkpoint:

```
CALYX_ASTER_RAW_COMMITMENT_COHORT_SEALED
  first_commit_seq=23747 last_commit_seq=24043 commitment_count=162 ledger_seq=9535

sealed_count 14355 -> 14517   (+162, == commitment_count exactly)
seal_count    1843 -> 1844    (+1)
```

**162 raw commitments sealed by exactly one Ledger entry.** Option A would have
written 162. That is the whole point of B′, and it is now a measurement rather
than a projection.

## 7. Internal invariants held at every sample

`sealed_count + pending_count == raw_commitment_count` was true at all six
readings, and `first_pending_seq > sealed_through_seq` held after each — no gap
and no overlap between the sealed and pending partitions.

The verifier is demonstrably computing rather than returning a constant:
`tip_hash` advanced across readings
(`8461be55…` → `50155723…` → `3be17a92…`) as entries were appended.

## 8. Honestly not exercised

- **`CALYX_ASTER_RAW_COMMITMENT_WRITE_FORBIDDEN`.** The guard rejects rows or
  revision guards targeting `ColumnFamily::RawCommitment`. It is a
  *library-boundary* defence against in-process Synapse callers. The public MCP
  surface exposes no arbitrary-CF write at all — `StorageOperation` is
  `Inspect | Summary | GcOnce | Anchors | TemporalPanels | TemporalRerank |
  TemporalBackfill | SearchRebuild | FindSimilar | RetireOrphanSlotCfs | Backup |
  RestoreVerify | Intelligence` — so there is no client-reachable path to
  attempt the forgery, and none was manufactured. The absence of the attack
  surface at the client boundary is the stronger property; the guard remains
  unverified against reality.
- **Tamper detection.** Proving the verifier *rejects* a mutated commitment
  would require corrupting the live production vault. Not done, and not
  simulated. It needs an isolated vault fixture to be verified honestly.
