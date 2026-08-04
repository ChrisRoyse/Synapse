# Issue #1676: fused find and temporal rerank FSV (2026-08-04)

## Source of truth

The manual instrument `find_temporal_fsv` created four native timeline
constellations in a fresh Aster vault. The authoritative state was read through
the physical Base and per-slot column families, the Registry temporal-policy
row, and the immutable persisted-search manifest. Expected ordering was fixed
before ingest: row 1 shares the query's app, actor, kind, hour, and title tokens;
rows 2 and 3 are unrelated distractors.

Command:

```powershell
cargo run -p synapse-storage --example find_temporal_fsv -- $freshDirectory
```

Verified vault:
`%TEMP%\synapse-find-fsv-8225b4b709414e10bb035c830b028db4`

## Evidence

```text
SOURCE OF TRUTH BEFORE: base_rows=0
SOURCE OF TRUTH AFTER: base_rows=4
manifest_sha256=fd318df9ad5f4663ce23cf257ac09cdf104455e862118889ac558312dba88bcb

PLAIN rank=1 cx=16760000000000000000000000000001 score=0.145161286 rrf_delta=4.326e-9
PLAIN rank=2 cx=16760000000000000000000000000002 score=0.094990082 rrf_delta=2.720e-9
PLAIN rank=3 cx=16760000000000000000000000000003 score=0.093998015 rrf_delta=9.461e-10

BOOST rank=1 cx=...01 base=0.145161286 score=0.159594059 e2=0.989 e3=1.000 e4=1.000
BOOST rank=2 cx=...03 base=0.093998015 score=0.100499541 e2=1.000 e3=0.500 e4=0.333
BOOST rank=3 cx=...02 base=0.094990082 score=0.097923480 e2=0.001 e3=0.500 e4=0.667
ALL CLAIMS OK
```

For every hit, the executable independently summed
`weight / (rrf_k + lane_rank)` and compared it with the returned pre-boost
score. It separately read the hit's Base row and every consulted slot row. The
temporal pass recomputed
`base * (1 + clamp(0.5*E2 + 0.35*E4 + 0.15*E3) * alpha)` and enforced the AP-60
maximum `base * (1 + alpha)`. Temporal scoring changed distractor order, proving
that it was applied rather than merely requested.

## Boundary audit

```text
missing_registration: Registry 1 -> 1, absent=true
invalid_cx_format: Base 4 -> 4, Registry 1 -> 1, refused=true
missing_example: Base 4 -> 4, Registry 1 -> 1, refused=true
```

All invalid operations left authoritative state unchanged.

## Live daemon MCP verification

Deployed commit `cdb9f4d5` through `scripts/synapse-setup.ps1 -SourceDir
C:\code\synapse`. The installed daemon was PID 18980 bound to
`127.0.0.1:7700`; setup recorded release artifact SHA-256
`110CE02CD87B39E1586F2A34645B0DBEEEE4AFD9B6815EFD5B735D7C90C4A4DD`.

The first real `find` call correctly refused superseded panel 1921001 because
its code contract is no longer reconstructable. Current panel 1965002 then
correctly refused a missing generation. Using the public audited surfaces, the
session acquired the input lease, entered `break_glass`, and called
`storage operation=search_rebuild`. Physical result:

```text
panel=1965002 base_seq=497330 slots=15 raw_sidecars=5
manifest_sha256=e5376a9a8447de72d15f72cb10405b3d57cdfd5a6d624b26911fd4ec34216707
diskann_build_backend=cpu-vamana
```

A real `find` tools/call with `by_text`, panel 1965002, `rrf`, and an explicit
temporal request returned three grounded hits with `temporal_applied=true`.
Consulted slots 107 and 109 were both physical `sparse_bm25` lanes. The first
hit carried contributions `1/61` and `1/79`, event-time evidence, and Ledger
provenance seq 279492/hash
`462d5ec77b5c2789be4ffe805fce245d2c1292ea859995a4cb21a992d4133e83`.

Independent reads after the call:

- `Get-FileHash` over the physical 5,456-byte manifest returned exactly
  `E5376A9A...34216707`.
- `audit operation=reproduce` re-read constellation
  `d41522402f6640927699573bfdaafb26` and proved `entry_present=true`,
  `entry_self_verifies=true`, `subject_matches=true`, `reproduced=true`, and
  `drift=none`; its physical entry hash matched the find result.
- The session restored `normal_agent` and released the foreground lease; the
  lease readback reported `held=false` and `outcome=released`.
