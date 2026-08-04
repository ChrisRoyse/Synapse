# Issue #1680: scheduled Reactive drift FSV (2026-08-04)

## Scope and source of truth

This verifies the production derived-state maintenance path that runs after new
constellations are woven, persists significant MMD findings, and publishes the
exact committed finding to subscribers. The sources of truth are the disposable
Calyx vault's `Base`, `XTerm`, `Graph`, and `Reactive` column families and the
independently drained real `EventBus` subscription. Return values alone were not
used as acceptance evidence.

The instrument is `reactive_scheduler_fsv`; it calls the same bounded maintenance
function as the daemon task. Every run used a new vault under `%TEMP%` and real
timeline rows plus real constellations.

## Diagnosis and research

The first known-positive run failed before drift evaluation because a valid
title-less timeline record stored empty sparse vectors. Loom correctly refused
cosine agreement with `CALYX_LOOM_ZERO_NORM_VECTOR`. Issue #2002 records this
cross-layer invariant defect. Empty optional text now becomes typed
`Absent::NotApplicable`; encoder defects still fail the entire write.

The next run wove successfully but reported no drift. `load_drift_corpus` claimed
that `Base` scan order was oldest-first, while `Base` is keyed by content-addressed
`CxId`. That mixed both distributions in hash order. Issue #2003 records the
defect. Selection now retains the newest bounded `(created_at, cx_id)` set and
hydrates only that set, giving deterministic chronological order and
`O(max_records)` memory/slot-read cost.

Exa MCP v3.4.0 was live and returned 8,203 characters for the diagnosed
zero-norm/missing-value/chronological-drift query. The built-in research lane
read the scikit-learn cosine definition and vector-space guide: cosine divides
by both L2 norms, so absence has no direction and must not be assigned an
invented agreement. Sources:

- https://scikit-learn.org/stable/modules/generated/sklearn.metrics.pairwise.cosine_similarity.html
- https://scikit-learn.org/stable/modules/metrics.html#cosine-similarity

## Happy path

Vault:
`C:\Users\hotra\AppData\Local\Temp\synapse-reactive-scheduler-positive-1785859474393`

Before trigger: `Reactive=0`, subscriber count `1`, timeline fixture rows `0`.
The trigger inserted 50 baseline title-less human/focus rows, paused across a
creation-time boundary, inserted 50 shifted agent/purge rows, flushed, and ran
one production maintenance pass.

Independent after-state:

- weave readback: `records_woven=100`, `XTerm=450`, `Graph=6405`;
- detector: `records_scanned=100`, `drifted_lenses=3`;
- physical `Reactive` count: `3`;
- delivery: `matched=3`, `queued=3`, `dropped=0`;
- drained events: exactly 3, slots `1`, `6`, `104`, each `p_value=0.01`,
  `reference_n=70`, `recent_n=30`, `significant=true`;
- slot 104 persisted/published `mmd2=0.4014747863243866` and
  `bandwidth=0.8835546873074208`.

This also proves #2002's title-less boundary: the 50 empty-title records were
physically woven rather than stopping the panel at a zero-norm agreement.

## Boundary audit

### No new ingest

Before the second tick `Reactive=3`. After the exact maintenance trigger,
`Reactive=3`, all panel weave counts were zero, and the drained subscription had
zero events. The watermark prevents duplicate production.

### Below calibration floor

Vault:
`C:\Users\hotra\AppData\Local\Temp\synapse-reactive-scheduler-insufficient-1785859489578`

Before: `Reactive=0`. Twenty records (10 per distribution) were inserted and
all 20 were physically woven. After: `drifted_lenses=0`, `Reactive=0`, delivered
events `0`. A second idle tick retained zero rows and zero events.

### Delivery boundary absent

Vault:
`C:\Users\hotra\AppData\Local\Temp\synapse-reactive-scheduler-no-sink-1785859504356`

Before: `Reactive=0`; no delivery sink was registered. The known-positive
trigger physically committed `Reactive=3`, then failed loudly with
`STORAGE_DERIVED_STATE_REACTIVE_DRIFT_FAILED`, naming the missing daemon sink and
the required replay repair. After: `Reactive=3`, delivered events `0`. Thus a
delivery fault cannot erase the durable evidence or masquerade as success.

## Gates

`cargo check -p synapse-calyx -p synapse-storage -p synapse-mcp` passed.
`pwsh -File scripts/lint.ps1 -Fix` passed all seven gates in both workspaces,
including format, cargo-deny, root Clippy, Calyx Clippy, and the public-API
ratchet (`370`, baseline `370`).

The scratch vaults intentionally have no grounded anchors, so the independent
scheduled kernel phase reports `SYNAPSE_CALYX_KERNEL_NO_DOMAIN`. That expected
kernel refusal is outside this Reactive producer and did not prevent its
physical state verification.
