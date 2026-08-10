# FSV — #2201, #2204, #2206: durable reflex lifecycle and nonblocking K2

Date: 2026-08-10  
Host: configured Windows production host  
Branch: `main` only; no worktree or secondary branch  
Vault: `C:\Users\hotra\AppData\Local\synapse\db-daemon`  
Vault id: `01KYJPGWATPD4XNMZY3ERGTKQW`

## Accepted behavior

This batch establishes one durable desired-state authority for executable reflexes and
makes the real operator-panic K2 finalizer asynchronous. Registration, cancellation,
operator disable, restart recovery, source audits, order projections, aggregates,
constellations, anchors, provenance, and desired state no longer cross independently
visible commit boundaries.

Scheduler-owned terminal transitions remain a distinct boundary tracked by #2205. That
issue is not represented as accepted by this record.

## Diagnosis and independent research

The source defects were diagnosed before researching solutions:

- `ReflexRuntime::spawn_with_config` initialized an empty definition set even though
  lifecycle audits survived restart; those summary audits could not reconstruct action
  semantics.
- cancellation and disable mutated the scheduler before a fallible audit write, allowing
  false-negative responses after physical mutation.
- K2 called the blocking `fire_release_all_blocking_with_timeout` bridge from a Tokio
  task. Its sleep/poll loop starved the action emitter it was waiting for, timed out at
  five seconds, and left operator panic pending.

`scripts/check-research-lane.ps1` performed a real MCP `initialize`, `tools/list`, and
`tools/call`; `exa-search-server` v3.4.0 returned `exa_mcp=live`. Exa results were
independently checked through the built-in web lane against primary sources:

- Kubernetes controllers reconcile current state toward durable desired state:
  <https://kubernetes.io/docs/concepts/architecture/controller/>.
- Microsoft Event Sourcing treats the event store as the system of record and rebuilds
  idempotent projections from it:
  <https://learn.microsoft.com/en-us/azure/architecture/patterns/event-sourcing>.
- Akka persists events before changing actor state and reconstructs state by replay:
  <https://doc.akka.io/libraries/akka-core/current/typed/persistence.html>.
- Tokio tasks must not block executor threads; asynchronous timeout is the bounded wait
  primitive:
  <https://docs.rs/tokio/latest/tokio/task/> and
  <https://docs.rs/tokio/latest/tokio/time/fn.timeout.html>.

The FSV issue comments retain the exact diagnosis and research chronology:

- #2201: <https://github.com/ChrisRoyse/Synapse/issues/2201#issuecomment-5239588081>
- #2204: <https://github.com/ChrisRoyse/Synapse/issues/2204#issuecomment-5239588220>
- #2206 and interruption evidence:
  <https://github.com/ChrisRoyse/Synapse/issues/2206>

## Source of truth

Acceptance used independent reads of all relevant state authorities:

1. executable scheduler state through `routine operation=reflex_list` and exact
   `reflex_history` reads;
2. `CF_KV` rows under `reflex/desired/v1/` for complete private executable definitions;
3. exact `CF_REFLEX_AUDIT` and `CF_REFLEX_AUDIT_ORDER` rows;
4. Calyx constellation/anchor reads and the append-only provenance chain;
5. `synapse-action` intent/final WAL plus physical Win32 key state;
6. operator-panic browser and extension durable-owner ledgers;
7. OS process table, installed executable hash, daemon lifecycle record, and vault
   identity across process replacement.

Return values were not used as acceptance evidence.

## Build deployed for reality verification

Two source commits were built and installed through `scripts/synapse-setup.ps1`:

- `0acbb7a0a4374cdf4eaeaa4d1e0391fd4b1743df` — durable desired state and atomic
  lifecycle publication;
- `1cbb490023cfebafb895ff94cd44be9dc6ab2432` — asynchronous K2 final sweep.

The production health readback after supported restart proved:

- build commit and checkout commit both `1cbb490023cfebafb895ff94cd44be9dc6ab2432`;
- `refs/heads/main`, clean source, `build_matches_checkout=true`;
- build input count `1451`, manifest SHA-256
  `e9b72f26c1bd7f6fbe45c634b87e91b549b9dd180d25c1aee95c206f61a3326a`;
- installed executable SHA-256
  `DBE6DE6A69B04DE3AF502909CBBE9AC53CD0ED8699EE0EA08521BFF0A58A2D57`;
- same vault id, no reset, CPU path `avx2`, fixed math probes bit-for-bit equal to
  portable math, and Windows execution-speed throttling disabled.

The host has an Intel i7-1355U (10 cores / 12 logical processors), 32 GiB RAM, and no
NVIDIA/CUDA device. The daemon therefore selected AVX2 CPU execution rather than
pretending CUDA was available.

`cargo check --workspace` passed. The canonical
`pwsh -File scripts/lint.ps1` gate passed both workspaces after one ordinary clippy
finding was corrected at source.

## Legacy orphan reconciliation (#2204)

Before migration, production contained two contradictory active lifecycle rows with no
executable definitions:

- `019feb4a-f6ed-7ce3-93e8-d3486c089ff9`;
- `019feb4b-a287-75b2-9279-31be70e29690`.

The migration did not invent definitions from summaries. One Calyx transaction changed
audit/order counts from `11/11` to `13/13` and published explicit `Disabled` terminal
facts for both IDs with `REFLEX_DURABLE_DEFINITION_MISSING`. Their original timestamps
were preserved. Exact audit and grounding reads found both new rows.

## Happy path: durable registration, restart, and cancellation

An inert production reflex was registered with priority `941`:

`019febbf-4a2b-7d82-ba50-4ca42cb753a8`

Separate runtime, history, physical CF, and grounding reads found its active state. A
supported clean process replacement then recovered the same executable definition and
original registration metadata from `CF_KV`, rather than the public audit summary. A
real cancel published terminal runtime state plus the guarded desired-state revision and
grounded cancellation audit. Another restart recovered it terminal, not executable.

## Happy path: real operator hotkey and K2 (#2201, #2206)

### Before

Two new inert reflexes were registered through the supported public facade:

| ID | Priority | Exclusive | Registration audit | Registered at |
| --- | ---: | --- | --- | --- |
| `019febf5-d677-7dc1-8be9-373d2d804db3` | 944 | false | `019febf5-d679-7483-9cd6-fb14166d79ee` | `1786370184823900200` ns |
| `019febf5-d708-78c0-be1f-587e7b00e8a6` | 945 | true | `019febf5-d709-7150-9d1f-bfa1499acb01` | `1786370184968435300` ns |

The runtime list contained exactly these two active definitions. Operator panic was
`epoch=0`, `pending=false`; browser and extension action gates were enabled and every
durable owner count was zero.

### Trigger

Win32 `keybd_event` generated the real `Ctrl+Alt+Shift+P` operator hotkey at Unix ms
`1786370220897`. `GetAsyncKeyState` proved Control, Alt, Shift, and P were all up both
before and after the trigger. Verification then waited eight seconds without polling the
transition.

### Independent after-state

- runtime active count: `0`;
- both records: `Disabled`, with exact original timestamps, priorities, and exclusivity;
- alpha terminal audit: `019febf6-641d-7af3-933b-8d93f5723ba8`, timestamp
  `1786370221085563900` ns;
- beta terminal audit: `019febf6-641d-7af3-933b-8dae32a102f4`, timestamp
  `1786370221085568900` ns;
- both error codes: `REFLEX_DISABLED_BY_OPERATOR`, reason `operator_hotkey`;
- panic state: epoch `1`, pending `false`, accounting incident `false`, outstanding
  generations/finalizations/publications all `0`;
- gates recovered enabled in order; browser disable sequence `1`, extension sequence
  `4`; every durable-owner count remained zero.

The raw action WAL independently recorded:

- `release_all.result=ok`, no error or detail;
- emitter readback with every held-input count `0` and `terminal=true`;
- `reflex_active_count_after=0`, disable result `ok`, no reflex readback error;
- `final_safety_sweep.terminal=true`;
- the final command row key `18ca76406c6a5e6800000013`, value length `24983`,
  SHA-256 `a82569d1bd5229e0015e57a3522451c8a841f1b1858d61f33247a5362a8d1d1a`.

Exact Calyx anchors existed for the final action and both disabled lifecycle facts:

- action Cx: `6ef3c6a83512818a4351a9e6dcd2f335`;
- alpha Cx: `bb917abecc6ab752b5ea0332fec9ebec`;
- beta Cx: `a4d35cc0903c8ba21eac1c297140c0c2`.

## Process-boundary and physical disk verification

A supported stop completed graceful vault close and removed the PID sidecar. With the
daemon offline, a separate physical dump found:

- `CF_KV=16190`, desired-state prefix rows `5`;
- exactly one desired-state row for each hotkey reflex ID;
- `CF_REFLEX_AUDIT=31`;
- exactly one matching disabled audit key for each ID.

A supported start opened the same vault and reported the prior shutdown clean/graceful.
Both records recovered `Disabled` with their exact original metadata and two-event
histories. The scheduler active count stayed zero. Reflex health was `ok`, retained late
and degraded tick counts were zero, and no action owner appeared.

The four synthetic reflexes used across the batch were then explicitly cancelled. A
separate runtime read returned an empty list, and each exact history gained one durable
`cancelled` row after its prior active/disabled rows.

## Boundary and edge-case audit

Each edge printed state before and after. Session bookkeeping legitimately advanced
unrelated `CF_KV`; the lifecycle authorities under test did not change.

1. **Priority above maximum.** Before: runtime `[]`, audit/order `31/31`. Trigger:
   valid inert definition with priority `1001`. Result:
   `REFLEX_PRIORITY_INVALID`, `durable_registration_committed=false`,
   `scheduler_activated=false`. After: runtime `[]`, audit/order `31/31`.
2. **Blank cancellation identity.** Before: runtime `[]`, audit/order `31/31`.
   Trigger: ID containing three spaces. Result: `TOOL_PARAMS_INVALID`, with remediation
   to provide an existing ID and inspect runtime plus `CF_REFLEX_AUDIT`. After: runtime
   `[]`, audit/order `31/31`.
3. **History maximum.** Before: runtime `[]`, audit/order `31/31`. Trigger:
   `limit=1001`. Result: `TOOL_PARAMS_INVALID`, exact maximum `1000`. After: runtime
   `[]`, audit/order `31/31`.
4. **Duplicate executable definition.** Before: runtime `[]`, audit/order `31/31`.
   First registration created `019fec03-de82-7340-be89-26c4ae86e35f`, priority `946`,
   and moved audit/order to `32/32`. The identical active registration failed closed and
   counts stayed `32/32`. Cleanup cancellation moved both to `33/33`; history contained
   exactly the active audit `019fec03-de84-7d03-be98-a2b0c42b014a` and cancelled audit
   `019fec03-e2e4-76c3-8ee9-3ff82022fc80`.

## Final integrity evidence

The full public `audit operation=verify_chain` independently rehashed the physical
provenance chain and raw commitment cohorts:

- verdict `intact`;
- `342868` ledger entries, head height `342868`;
- `965038` raw commitments, `965035` sealed, three current checkpoint-tail rows
  pending sealing;
- `39540` Merkle cohort seals;
- raw commitments intact;
- tip hash `1fed1881ef1ee468aa4ac78471ad582e7910ebc83763f3efa7551e78a9ebb86b`;
- vault generation `1`, reset count `0`, lineage-seeded historical coverage reported
  truthfully as not covering pre-ledger vault history.

The final daemon readback was healthy on PID `552`, exact build `1cbb490023cf`, the same
vault, clean previous shutdown, reflex health `ok`, active count `0`, and audit timestamp
failure count `0`.

