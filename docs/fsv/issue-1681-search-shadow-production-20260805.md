# Issue #1681: persisted-search Anneal production FSV (2026-08-05)

## Scope and source of truth

The real installed daemon, production Calyx vault, and physical search artifacts were used.
No test harness or mock data participated.

- daemon: `http://127.0.0.1:7700/mcp`, final acceptance PID 12092
- build: main through `bcd9bd2d`
- vault: `%LOCALAPPDATA%\synapse\db-daemon`
- live index pointer: `idx/search/panel_<version>/manifest.json`
- optimizer state: Calyx KV tuning/rollback rows, read independently through
  `hygiene anneal_status`
- tripwire state: `.anneal/tripwire.toml`
- transaction evidence: structured `SYNAPSE_CALYX_ANNEAL_SHADOW_COMPLETE` log rows

## Root defects and fixes

The first real 48-query replay always reverted as `BudgetExhausted`. The resource
handle contained `ceil(1000ms / 100ms) = 10` cooperative ticks while shadow
execution consumed one tick per query. Query 11 therefore failed regardless of
host utilization. Commit `5f9218b1` makes CPU/VRAM admission unchanged but sizes
the cooperative quota to the already bounded replay cardinality. Structured
exhaustion now records `evaluated_queries` and `replay_queries`.

Earlier production runs also found and fixed: sequential MVCC cuts, missing flat
dense identity readback, stale tuning-to-generation bindings, and restart
reconciliation that could republish an older healthy generation. Those fixes are
commits `493047c6`, `8a2c05c3`, `bf5e876f`, and `248a311b`.

The large-panel replay then exposed two search defects. Row-count-only placement
sent cheap 1/2/8/16-dimensional structured lanes with extensive ties through
DiskANN, producing repeatable 0.666666 recall. Scalar-work placement moved those
lanes to exact search. That exact path then exceeded the 200 ms latency tripwire
because it fully sorted 50,973 scores and allocated display strings in the
tie-break comparator. Commit `bcd9bd2d` uses linear-time partial selection,
bytewise deterministic ID ties, and sorts only the retained top-k. Candidate and
incumbent timing is warmed and paired in alternating order by `0591e6dc`.

## Execute and inspect

Before the trigger, timeline panel 1963001 was physically read at `ef_search=64`;
its tuning artifact was
`f27f4058998e32f90ba18253d0de8e51c506575309dd1c449c480e4a4b5a3634`.

After deploying `5f9218b1`, the identical real 48-query trigger completed every
query and reached the real metric gate. It reverted as
`MetricRegression(SearchP99)`, not budget exhaustion. Independent status readback
kept the live tuning hash and `ef_search=64` unchanged.

Declared panel 1965002 then exercised 50,973 production records, five DiskANN
lanes, and 72 exact-reference queries. The deliberately weaker `ef_search=63`
candidate reverted as `TripwireCrossed(RecallAtK)`. The live artifact remained
byte-identical. Candidate and incumbent generations were separately
content-addressed and bound to the same MVCC base sequence.

After the root fixes, the same 72-query production trigger promoted
`ef_search=63` as change `1785935442568826775` on Base sequence 613418. Slots
35/36/37/42/43/44/45/46 all measured recall mean/min 1.0. Their maximum per-query
p99 was 3.8264 ms, down from 274.8573 ms before top-k selection. The 96-dimensional
DiskANN lane (slot 110) measured recall mean 0.9166667, minimum 0.6666667, and p99
maximum 16.2679 ms. No threshold was weakened.

Independent readback after promotion found tuning SHA-256
`40877fa31b8235fe9dbc468744dbc8993bf203314e06ada7ab7e938f5ad9157b`,
`ef_search=63`, and live manifest SHA-256
`424a9b142eb7fa731bac7d5abd71e10c1b6183ffa0f0bc05b6732f5a813b7ab9`.
Every declared index and filter SHA-256 matched its physical file; the two
DiskANN sidecars also existed and were independently hashed.

Explicit rollback restored the exact prior tuning SHA-256
`f27f4058998e32f90ba18253d0de8e51c506575309dd1c449c480e4a4b5a3634`
and exact incumbent manifest SHA-256
`ce13ebb64f819ed99ccaf000acff0aa7f8689e4e72f3427f57dc7bc334e4446d`.
All declared sidecar hashes still matched after rollback.

## Boundary audit

1. Unauthorized proposal from a normal profile returned
   `TOOL_PROFILE_POLICY_DENIED`; the manifest SHA-256 was unchanged.
2. `m_max=0` returned `SYNAPSE_CALYX_CONFIG_INVALID`; physical tuning and
   manifest state were unchanged.
3. Historical panel 1921001, which has index bytes but no declared panel
   contract, returned `SYNAPSE_CALYX_ANNEAL_PANEL_UNKNOWN`; no candidate was
   published.
4. A 48-query replay (greater than the former ten-tick boundary) completed and
   reached `SearchP99`; a 72-query replay completed and reached `RecallAtK`.
5. Unknown rollback change `u64::MAX` failed closed; the physical live manifest
   SHA-256 was identical before and after. Its facade error-classification defect
   is tracked as #2012; mutation safety was not affected.

## Research and gates

`scripts/check-research-lane.ps1` reported Exa MCP live and executed a real Exa
query. Built-in web research used Kubernetes resource-request/admission guidance,
Linux kernel CPU-controller documentation, Elastic atomic alias guidance, and
Faiss evaluation/tied-vector guidance, and the official Rust slice documentation
for linear-time `select_nth_unstable_by`. The applicable design is admission
before bounded work, explicit accounting, immutable candidates, atomic pointer
publication, ground-truth recall measurement, and top-k selection rather than a
full corpus sort.

`pwsh -File scripts/lint.ps1` passed all seven gates in both workspaces after the
final search changes (API ratchet: 363).

## Verdict

The replay-budget, measurement-order, tied-lane placement, and exact top-k defects
are fixed. A genuine non-regressing production candidate promoted, every live
artifact was independently verified, and explicit rollback restored the exact
prior tuning and manifest bytes. Issues #1681 and #2010 are accepted and closed.
