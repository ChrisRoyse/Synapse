# FSV — issue #1876: raw-commitment seal mismatch names the offending sequence

Date: 2026-07-29 (UTC) · Host: configured Windows 11 host · Daemon: live (`synapse-mcp` pid 18800)

Driver: `scripts/diagnostics/issue-1876-seal-mismatch-localization-fsv.ps1`, over the
existing instrument `crates/synapse-storage/examples/provenance_tamper_fsv.rs`.
Everything exercised is real: genuine Aster vaults opened by the real
`SynapseCalyxVault`, real WAL/MVCC commits, the real checkpoint sealer, the real
`verify_ledger_chain` verifier, and physical SST byte patching that rewrites both
CRC layers so the storage layer accepts the file as intact.

## What the issue asked for, and what turned out to be true

The issue asked the verifier to **bisect the cohort against the sealed Merkle
root** to name the offending sequence. That is not achievable, and the reason
matters:

> The verifier holds the physical cohort and one 32-byte root. Testing whether
> leaf *i* is the culprit requires the sealed value of leaf *i* — or of a subtree
> containing it — and the seal stores neither.

This is the standard Merkle result: RFC 6962 localizes an individual leaf only
via an **inclusion proof** (the sibling hashes along its path). A bare root
authenticates the set as a whole and says nothing about any member. So a bisect
against the root would have to guess, and a guess in a
`CALYX_ASTER_CORRUPT_SHARD` verdict is exactly what sends an operator toward a
destructive repair — the #1875 lesson the issue itself cites.

The delivered fix therefore does two things:

1. **Localizes everything that is determinable from an existing (v1) seal**, and
   says plainly when something is not, rather than guessing.
2. **Makes content divergence exactly localizable from now on**, by storing a
   bounded per-row digest ladder beside the root — the two-tier
   per-row-digest + aggregate pattern that is standard practice for
   corruption localization in stored data.

## The fix

`calyx/crates/calyx-aster/src/vault/raw_commitment.rs`:

- **v2 seal payload** (`CYXSEAL2`). Identical header fields to v1, then
  `min(cohort_len, 256)` bucket digests. A cohort of ≤ 256 rows — every real
  cohort on this vault, which averages 6–8 — gets one digest per row, so
  localization is exact. Larger cohorts bucket, keeping the payload bounded at
  8 KiB while still narrowing to `ceil(n/256)` rows. A bucket digest is
  `merkle_root` of the bucket's leaves, so a single-leaf bucket digests to that
  leaf's own hash and needs no special case.
- **`decode_seal` accepts both versions.** `seal_matches` compares only the four
  v1 fields via `RawCommitmentSeal::core()`, so the ladder can never change a
  verdict and a cohort sealed by a pre-#1876 build verifies byte-identically.
  **Detection behaviour is unchanged, as the issue required.**
- **`describe_mismatch`** decomposes a divergence field by field
  (`first_seq` / `last_seq` / `commitment_count` / `merkle_root`), names any row
  sitting inside the cohort that the seal never covered, and — on a content
  divergence — walks the ladder to name the offending sequence(s).
- **`describe_truncated_cohort`** (found by FSV, see CASE 5) handles the
  *separate* code path taken when rows are missing. It pins the drop position
  against the ladder, brackets the gap by the surviving sequences on either
  side, and hands over the missing rows' sealed leaf digests.

Both run on the failure path only, so an intact vault pays nothing.

## Manual FSV — all cases passed

| Case | Trigger | Verdict |
|---|---|---|
| 0 — baseline | 12-row cohort, untouched | `intact=true`, one seal, 12 commitments |
| 1 — **one row tampered mid-cohort** (seq 7) | 1 bit flipped in `batch_hash`, both CRC layers rebuilt | `offending_sequences=[7] localization=exact` |
| 2 — **two rows tampered** (seq 3, 10) | | `offending_sequences=[3,10] localization=exact` |
| 3 — **boundary: first row** (seq 1) | | `offending_sequences=[1] localization=exact` |
| 4 — **boundary: last row** (seq 12) | | `offending_sequences=[12] localization=exact` |
| 5 — **row dropped** (seq 6) | row removed from the SST entirely | `dropped_row_count=1 dropped_at_sealed_index=5 between_physical_seq=5 and_physical_seq=7 missing_sealed_leaf_digests=[c5963a0a…] localization=exact` |
| 6 — **regression: real v1 seals still verify** | the LIVE production vault, copied read-only | `intact=true`, **3,029 seals over 23,185 commitments**, all written by pre-#1876 builds |
| 7 — **honesty: tampered v1 cohort** | one row tampered in the real v1 vault | `offending_sequences=unavailable` + the reason + `physical_leaf_digests=[…]`; **no fabricated sequence** |

Case 1's actual message:

```
raw commitment Ledger seal 0 does not match physical commitment rows 1..=12 count=12:
merkle_root sealed=74894baa… physical=d26eaadb…
offending_sequences=[7] localization=exact
```

Case 7's actual message, on a real production cohort of 6 rows:

```
raw commitment Ledger seal 7351 does not match physical commitment rows 18947..=18952 count=6:
merkle_root sealed=227ec350… physical=9f686a7a…
offending_sequences=unavailable reason=this cohort was sealed by a pre-#1876 build whose
Ledger seal carries only the Merkle root, and a root alone cannot identify which leaf moved
(RFC 6962 localizes a leaf only via an inclusion proof, which this seal does not store);
cohorts sealed from this build carry a bounded per-row digest ladder and name the exact
sequence. physical_leaf_digests=[18947:e5ba5332…,18948:fa91675f…,…]
```

Before this change both of those read only:
`raw commitment Ledger seal N does not match physical commitment rows A..=B count=C`.

Case 6 is the important regression: the production vault's **3,029 existing v1
seals over 23,185 commitments still verify `intact`**, proving the payload change
is backward compatible on real durable bytes rather than on a synthetic.

Two incidental findings, both correct fail-closed behaviour and neither a defect:
the lineage journal (#1875) refused a disposable vault re-seeded over a stale
sibling journal (`SYNAPSE_CALYX_VAULT_RESET_UNACKNOWLEDGED`), and refused a vault
copied to a new path carrying its old journal
(`SYNAPSE_CALYX_VAULT_LINEAGE_PATH_MISMATCH`).

## Research lane

Exa remains `registered_but_unusable` (`EXA_API_CREDITS_EXHAUSTED_402`, #1864) — not
re-diagnosed, per AGENTS.md. Built-in web lane, primary sources:

- RFC 6962 §2.1.1 (Certificate Transparency) — Merkle audit path / inclusion
  proof; RFC 9162 renames it "Merkle inclusion proof". An inclusion proof is what
  binds an individual leaf to a root.
- Two-tier per-row-digest + aggregate checksum practice for localizing a corrupt
  row, versus an aggregate hash which detects but cannot localize.
