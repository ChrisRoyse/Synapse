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
