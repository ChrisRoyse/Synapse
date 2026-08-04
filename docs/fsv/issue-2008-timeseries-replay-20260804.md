# Issue #2008 - TimeSeries replay consistency FSV (2026-08-04)

## Scope and source of truth

Defect: replaying `TimeSeriesLayer::ts_write` overwrote the raw point while
folding the same value into minute/hour/day rollups again.

Physical source of truth: an isolated durable Aster vault under `%TEMP%`, its
`TimeSeries` point/rollup rows, and its `Ledger` rows. The manual instrument was
`calyx-aster/examples/timeseries_replay_fsv.rs`; it used real vault writes and
separate range/rollup/ledger reads. The vault was removed only after all
readbacks were printed.

Research lanes: Exa MCP was live (`exa-search-server` v3.4.0, real search call
succeeded). Built-in web research used Apache Flink's primary checkpoint and
stateful-stream documentation. Its exactly-once contract requires replayed
records to affect managed state exactly once, matching the insert-once policy
implemented here:

- https://nightlies.apache.org/flink/flink-docs-master/docs/ops/state/checkpoints/
- https://nightlies.apache.org/flink/flink-docs-release-2.3/docs/concepts/stateful-stream-processing/

## Trigger and physical readback

Command:

```powershell
cd calyx
cargo run -p calyx-aster --example timeseries_replay_fsv
```

The isolated vault path was
`%TEMP%\synapse-timeseries-replay-fsv-01KZ799BFPJZBA97Y710SAS3RS`.

Happy path before: no points and no minute/hour/day rollups. After writing
`series=7, ts=3661000000000, value=4`, the independent reads returned one raw
point and `count=1, sum=4, min=4, max=4` in all three rollups at sequence 2.

Identical replay before/after:

```text
points=[(3661000000000, 4.0)]
minute/hour/day=count=1 sum=4 min=4 max=4
replay sequence=2; unchanged=true
```

The replay did not advance the vault sequence or ledger and did not mutate any
aggregate.

## Edge audit

Conflicting same-key value `9.0`:

```text
code=CALYX_TIMESERIES_POINT_CONFLICT
existing_bits=4010000000000000 requested_bits=4022000000000000
remediation=use a new timestamp for a distinct measurement or replay the byte-identical value
after: point=4; minute/hour/day=count=1 sum=4; unchanged=true
```

Invalid non-finite value at a new timestamp:

```text
code=CALYX_INVALID_ARGUMENT
message=time-series value must be finite (NaN/inf rejected to protect rollups)
after: original point and all rollups unchanged=true
```

Window boundary: writing `6` at `ts=7200000000000` produced a distinct
minute/hour cell with `count=1,sum=6`; the first hour stayed `count=1,sum=4`.
Both timestamps are in the same day, whose rollup correctly became
`count=2,sum=10,min=4,max=6`.

Final physical evidence:

```text
latest_seq=3
ledger_rows=2
ledger_last_key=0000000000000001
```

There are exactly two ingest ledger entries for the two distinct points. The
identical replay, conflict, and invalid input wrote none.

## Gates

`pwsh -File scripts/lint.ps1` passed all seven gates in both workspaces. The
manual instrument made two existing TimeSeries read APIs reachable, lowering
the unreached-public-API ratchet from 370 to 368; the baseline was tightened.
