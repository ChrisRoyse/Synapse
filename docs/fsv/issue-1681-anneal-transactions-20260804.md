# Issue #1681 Native Anneal Transaction Verification - 2026-08-04

## Scope and source of truth

This verification covers the implemented native transaction substrate and its load-bearing RRF
configuration. The source of truth is the physical Calyx vault, specifically content-addressed
tuning artifacts in `Kv`, the live pointer and change rows in `AnnealRollback`, append-only
`Ledger` rows, and the manifest durable sequence. Return values were retained only as trigger
evidence; acceptance used a separate read-only vault open after the writer closed.

The Exa MCP lane was checked and executed successfully. The lock defect discovered during this
verification was independently researched and is documented in the #2009 FSV record.

## Measured workload

The smallest deterministic workload exercising every native tripwire used real production RRF
fusion, labeled guard scores, and real 4,096-byte file write/sync/read cycles:

```
recall_at_k=1.0
guard_far=0.0
guard_frr=0.0
search_p99_ms=0.0048
ingest_p95_ms=2.6089
canary_sha256=3166ab8180cc4a9e8d8b9ba11bcd42ede3d6d5579a6f4f31610fe0ea3f2d6ddb
```

The baseline artifact was 505 bytes at
`8ed7f65833059660a0f15ebcf2afea1701bbd42bd9d63db3a1b5842230872a78`. Changing production
`fusion_k` from 60 to 5 produced artifact
`4fd88d3c406ea5e70c1e44dc2e2f8fae169b2a94000e9d4ea5b1b4afaf703908` and promoted native
change `1785894508014826775`. A subsequent forced-bad candidate crossed the recall tripwire and
left that live hash intact. Explicit rollback restored the exact baseline hash and 505-byte value.

## Independent physical readback

After orderly close, a separately opened read-only vault selected only `Kv`, `AnnealRollback`,
and `Ledger`. It read:

```
latest_seq=17
artifact_rows=4
rollback_rows=4
ledger_rows=5
live row value=ARL1 + 8ed7f65833059660a0f15ebcf2afea1701bbd42bd9d63db3a1b5842230872a78
```

The artifact bytes addressed by that hash were re-hashed and decoded independently. They contained
`fusion_k=60`, `index_m_max=32`, `index_ef_construction=64`, `index_ef_search=64`,
`index_beamwidth=32`, and `index_alpha=1.2`. The canary was separately read at 4,096 bytes with the
expected SHA-256, deleted, then independently confirmed absent while the evidence vault remained.

## Boundary and edge-case audit

1. Empty replay. Before: live baseline, one rollback row. Trigger: proposal with zero held-out
   queries. After: `insufficient_replay`, two rollback rows, live baseline unchanged.
2. Unchanged candidate. Before/after: live baseline unchanged. Trigger failed closed with
   `SYNAPSE_CALYX_ANNEAL_CANDIDATE_UNCHANGED` and remediation to change a load-bearing field.
3. Unknown rollback. Before/after: live baseline unchanged. Trigger with `u64::MAX` failed closed
   with `SYNAPSE_CALYX_ANNEAL_CHANGE_UNKNOWN` and remediation to read status first.
4. Bad candidate. Before: promoted hash. Trigger: measured recall `0.0`. After: native
   `TripwireCrossed(RecallAtK)` and promoted hash unchanged.

The public `hygiene anneal_status` MCP operation was also invoked against the installed daemon and
reported the same 505-byte baseline hash, five tripwires, and persisted index configuration. The
authoritative `scripts/lint.ps1` passed all seven gates in both workspaces; the newly reached API
lowered the fail-closed unreached-public-API baseline from 365 to 364.

This is not evidence that every field named by #1681 is complete. Per-slot fusion weights and
measured quantization promotion remain unimplemented and the issue must stay open until those are
load-bearing and verified rather than represented as inert configuration.
