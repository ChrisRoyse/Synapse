# FSV: planted cross-lens blind spot (#1674)

Date: 2026-08-04

## Source of truth

The fixture writes 200 real native Calyx constellations, each carrying two separate dense slot vectors. Lens A and B agree for every record except index 100. At that record B is the exact opposite of A's expected nearest neighbor. The Source of Truth is the physical `Base`, `slot_01`, and `slot_02` CF bytes read through a fresh read-only handle after a durable checkpoint.

The lens angles use deterministic irregular spacing. This matters: their nearest-neighbor confidence has multiple real values, so neither direction is rejected as a definitional one-hot/cyclic lane (#1961).

## Research

Exa MCP and the built-in web lane were used after diagnosis. Primary literature on conformal outlier detection supports calibrating a nonconformity score into a distribution-free p-value and interpreting an alpha threshold as a false-positive bound under the exchangeability contract:

- <https://arxiv.org/abs/2104.08279>
- <https://proceedings.mlr.press/v60/ishimtsev17a.html>

## Happy path and physical recomputation

Before: `C:\Users\hotra\AppData\Local\Temp\synapse-fsv-1674-acceptance-20260804` did not exist.

```text
records                                         200
evaluated directions                            2
nondiscriminative directions                    0
alerts                                          2 (both directions of the planted record)
healthy-record false positives                  0 (alpha bound 10)
planted CxId                                    16740064000000000000000000000000
reported A->B delta / p-value                   1.999608 / 0.005
fresh read-only snapshot                        200
physical Base rows                              200
physical nearest neighbor                       index 99
recomputed A similarity                         0.999608
recomputed B similarity                         -1.0
recomputed delta                                1.999608
reported-vs-physical match                      true
```

Thus the alert names the planted record and slot pair, the healthy corpus remains inside the bound, and the alert's evidence is reproduced from independently read stored bytes rather than trusted from the detector response.

## Boundary audit

| Case | Before | Trigger | After |
| --- | --- | --- | --- |
| Missing argument | 12 matching scratch paths | invoke with no path | exit 1 with usage; still 12 paths |
| Reused vault | 20 files, 363,783 bytes, manifest SHA-256 `4802DD5586A7E46E36C143D782E24233C458B9103A7790671831C40A48075FF3` | invoke using existing vault | exit 1 `SYNAPSE_FSV_SCRATCH_CREATE_FAILED`; count, bytes, hash unchanged |
| Existing file | `Cargo.toml` 9,512 bytes, SHA-256 `6F3CB94A5272D9EAACFA47CE098A26386D79235EC436C6916B94D722695BD75A` | invoke using file path | exit 1; length and hash unchanged |

## Gates

- `cargo check -p synapse-calyx --example blind_spot_planted_anomaly_fsv`: passed.
- release fixture execution: passed.
- `pwsh -File scripts/lint.ps1`: all seven gates passed in both workspaces.
