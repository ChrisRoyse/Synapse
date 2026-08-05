# Issue #1681: persisted-search Anneal production FSV (2026-08-05)

## Scope and source of truth

The real installed daemon, production Calyx vault, and physical search artifacts were used.
No test harness or mock data participated.

- daemon: `http://127.0.0.1:7700/mcp`, installed PID 15684
- build: main commit `5f9218b1`
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

Physical tripwire readback after construction/search candidates:

```toml
[state.recall_at_k]
last_value = 0.8425925925925926
crossed = true

[state.search_p99]
last_value = 2.107572916666667
crossed = false
```

The 0.84259 recall remains below the configured 0.90 bound for alpha 1.1/1.3,
M=64/128, construction breadth 128/256, and search breadth 64/128. This newly
observed production defect is tracked as #2010. No threshold was weakened and no
failing candidate was promoted.

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

## Research and gates

`scripts/check-research-lane.ps1` reported Exa MCP live and executed a real Exa
query. Built-in web research used Kubernetes resource-request/admission guidance,
Linux kernel CPU-controller documentation, Elastic atomic alias guidance, and
Faiss evaluation guidance. The applicable design is admission before bounded
work, explicit accounting, immutable candidates, atomic pointer publication, and
ground-truth recall measurement.

`pwsh -File scripts/lint.ps1` passed all seven gates in both workspaces at
`5f9218b1` (API ratchet: 363).

## Verdict

The replay-budget defect is fixed and manually proven at 48 and 72 real queries.
Bad candidates fail closed and leave the physical live artifacts unchanged.
Issue #1681 is not accepted yet: a genuine non-regressing promotion plus
byte-identical explicit rollback is still required, and #2010 currently blocks
that proof on the only declared production DiskANN panel.
