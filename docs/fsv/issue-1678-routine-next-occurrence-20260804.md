# Issue #1678: routine next-occurrence Oracle FSV (2026-08-04)

## Diagnosis

Routine mining already projected every supporting occurrence into Calyx native
Recurrence rows, but no Synapse crate depended on `calyx-oracle`. The data needed
for prediction existed physically while the Oracle implementation was
unreachable. `routine_inspect` therefore exposed only the mined schedule label,
not the Oracle cadence, uncertainty interval, confidence ceiling, or an honest
insufficient result.

The root workspace now pins the local `calyx-oracle` crate. For a currently
mined, explicitly confirmed identity, `routine_inspect` reads the native
recurrence series and calls Calyx's median-cadence/MAD predictor. The response
carries the recurrence content address and durable vault sequence. Candidate,
disabled, quarantined, archived, or unmined identities do not predict.

## Research

Post-diagnosis research used both required lanes:

- Exa MCP: `scripts/check-research-lane.ps1 -Probe 'robust recurring event next
  occurrence forecast median cadence median absolute deviation prediction
  interval best practices'` initialized exa-search-server 3.4.0 and a real
  `web_search_exa` call returned 8,115 characters (`verdict=live`).
- Built-in web: NIST documents median absolute deviation as an alternative
  robust scale statistic, while *Forecasting: Principles and Practice* explains
  why forecast uncertainty must be expressed with prediction intervals.

Sources:

- https://www.itl.nist.gov/div898/handbook/eda/section3/eda35h.htm
- https://otexts.com/fpp3/prediction-intervals.html
- https://otexts.com/fpp3/tscv.html

The existing Oracle formulation is consistent with that guidance, so this
change wires it rather than replacing its mathematics.

## Source of truth

- Input: native Calyx Recurrence CF series addressed by
  `33ec7b0e02ddccb42ba1cd019ec1d940`.
- Identity/lifecycle: exact `CF_ROUTINES` and `CF_ROUTINE_STATE` rows.
- Trigger: real stdio MCP initialize, schema-validating `tools/list`, and
  `tools/call routine_inspect` against the repo-built binary.
- Vault: `C:\Users\hotra\AppData\Local\Temp\synapse-routine-fsv-1677-1785849340208`.
- Independent read: a second daemon process reopened the same vault and decoded
  the same rows. Final shutdown recorded durable high-water mark 98.

## Happy path and arithmetic proof

Six typed timeline rows had already produced two three-day routines. Re-mining
reprojected recurrence evidence idempotently. The noon routine was explicitly
reconfirmed, binding its identity digest, before prediction.

Physical routine evidence gives the final occurrence:

```text
last day start = 1785646800 seconds
minute of day = 720 = 43200 seconds
last occurrence = 1785690000
median cadence = 86400 seconds
expected next = 1785776400
```

Independent `routine_inspect` readback returned:

```text
status=predicted
predicted_at_secs=1785776400
cadence_secs=86400
cadence_mad_secs=0
support=3 active_support=3 rolled_support=0
periodic_confidence=1
confidence=0.2399999946
confidence_ceiling=0.3006418347
interval=[1785710736, 1785842064]
recurrence_cx_id=33ec7b0e02ddccb42ba1cd019ec1d940
```

Observed equals expected exactly. Confidence remained below the mined routine's
confidence ceiling. The interval contained the point prediction.

## Boundary and edge cases

Each lifecycle mutation was followed by a separate physical state read.

1. Candidate routine: the second mined routine remained `candidate`; its
   inspection omitted `next_occurrence` even though recurrence data existed.
2. Disabled confirmed routine: before disable a prediction was present; after
   the durable transition to `disabled`, inspection omitted it. Enabling moved
   it to `candidate`, which also omitted it. Explicit reconfirmation restored
   the same recurrence prediction.
3. Invalid identity: `routine_inspect` with `bad-id` failed with
   `TOOL_PARAMS_INVALID` and the exact `rt1-` plus 16 lowercase-hex contract;
   the valid routine row was unchanged.

The wired insufficient branch returns `status=insufficient`, support counts,
`CALYX_ORACLE_INSUFFICIENT`, and remediation for a confirmed series with fewer
than three active occurrences (including rollup-only cadence loss). The current
miner admits a routine only at the same three-occurrence floor, so manufacturing
such a row through the public routine workflow would require corrupting the
source of truth and was correctly not done for FSV.

## Supporting gates

- `cargo check -p synapse-mcp`: passed.
- `cargo build -p synapse-mcp --bin synapse-mcp`: passed.
