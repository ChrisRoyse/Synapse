# Issue #1680: durable routine recurrence trigger FSV (2026-08-04)

## Diagnosis and design

Routine mining projected evidence into Calyx Recurrence, but discarded the
append readback. It therefore could not distinguish a newly inserted occurrence
from an identical replay. It also projected before lifecycle reconciliation,
so publishing immediately could announce an identity that the same mine then
quarantined.

The fix treats Calyx `Reactive` as the durable outbox. Only an `Inserted`
recurrence with frequency at least two becomes a candidate. After the complete
routine replacement and lifecycle reconciliation, only a still-`confirmed`
identity is committed as a typed, stable-keyed `Reactive` row. The exact row is
then independently point-read before it is returned for EventBus publication.
A dropped live subscription delivery is a structured error that points back to
the durable row.

Post-diagnosis research used both lanes. `scripts/check-research-lane.ps1`
reported `exa_mcp live` for exa-search-server 3.4.0 and completed a real
`web_search_exa` call. Built-in research used the primary AWS and Microsoft
transactional-outbox guidance. Both require durable event intent beside the
state transition, stable event identity, and replay-safe/idempotent delivery:

- https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/transactional-outbox.html
- https://learn.microsoft.com/en-us/azure/architecture/databases/guide/transactional-out-box-cosmos

## Source of truth

- Business fact: native Calyx Recurrence series for routine
  `rt1-9cbf202d68dda96f`, content address
  `33ec7b0e02ddccb42ba1cd019ec1d940`.
- Admission state: exact `CF_ROUTINE_STATE` row, independently read by
  `routine operation=inspect`, lifecycle `confirmed`, canonical identity lock
  matched.
- Notification intent: native Calyx `Reactive` CF.
- Trigger: real MCP stdio initialize, schema-bearing `tools/list`, bounded
  `CF_TIMELINE` diagnostic writes, episode segmentation, and routine mining.
- Independent read: repo example `reactive_drift_readback_fsv` reopened the
  physical vault read-only and scanned `Reactive` at a new snapshot.
- Disposable vault:
  `C:\Users\hotra\AppData\Local\Temp\synapse-routine-fsv-1677-1785849340208`.

## Happy path

Before the new occurrence, the confirmed noon routine had four occurrences and
the native recurrence readback predicted from support 4. The trigger wrote two
typed timeline rows on the fifth day (focus at noon and session end 120 seconds
later). Physical write readbacks were `CF_TIMELINE 8 -> 9` and `9 -> 10`.
Segmentation independently read those rows and persisted one two-minute episode:

```text
days_processed=1 timeline_rows=2 episodes_written=1 invalid_rows=0
constellations_inserted=1 constellation_failures=0
```

The real mine read five active days and returned exactly one admitted finding:

```text
routine_id=rt1-9cbf202d68dda96f
subject_cx_id=33ec7b0e02ddccb42ba1cd019ec1d940
occurrence_id=4
frequency=5
observed_seq=142
recurrence_findings=1
notifications_dropped=0
```

An independently reopened, read-only vault then produced the physical proof:

```text
REACTIVE_READBACK snapshot=147 rows=1
key_hex=5252454355523100000000000000008ed8c952a598c172f80000000000000004
value_json={"frequency":5,"observed_seq":142,"occurrence_id":4,
"subject_cx_id":"33ec7b0e02ddccb42ba1cd019ec1d940",
"subject_id":"rt1-9cbf202d68dda96f","subject_kind":"routine"}
```

All six returned identity/value fields match the stored bytes.

## Boundary audit

Each case started with one physical `Reactive` row and ended with a separate
read-only scan still reporting exactly one.

1. Identical replay: the complete non-dry mine returned
   `recurrence_findings=[]`; snapshot 151 had `rows=1`.
2. Inverted bounds: `start >= end` failed with `TOOL_PARAMS_INVALID`; snapshot
   152 had `rows=1`.
3. Dry run: computed the same five-day routines but reported zero routine/state
   writes and `recurrence_findings=[]`; snapshot 153 had `rows=1`.

An additional permission boundary was exercised before the happy path: without
the explicit startup `WRITE_STORAGE` grant, both diagnostic write and mining
failed with `SAFETY_PERMISSION_DENIED` and no timeline trigger was written.

## Verification gates

- `cargo check --workspace`: passed.
- `cargo build -p synapse-mcp --bin synapse-mcp`: passed.
- `pwsh -File scripts/lint.ps1`: all seven gates passed in both workspaces.

This completes the durable confirmed-routine recurrence producer. Issue #1680
remains open for new-region production and unattended durable relay/replay into
subscriptions.
