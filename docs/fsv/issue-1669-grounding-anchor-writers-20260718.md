# Issue #1669 Manual FSV - grounded outcome anchor writers

Date: 2026-07-18

Issue: https://github.com/ChrisRoyse/Synapse/issues/1669

No automated tests, FSV harnesses, benchmarks, mocks, CI, or GitHub Actions were
created or run. Structural commands are listed separately and are not FSV.

## Research

The implementation followed the event-sourcing and telemetry guidance reviewed
after the local root-cause analysis:

- Exa MCP research: Azure Event Sourcing Pattern, AWS Event Sourcing Pattern,
  OpenTelemetry error recording guidance.
- Native web research:
  - https://learn.microsoft.com/en-us/azure/architecture/patterns/event-sourcing
  - https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/event-sourcing-pattern.html
  - https://opentelemetry.io/docs/specs/semconv/general/recording-errors/

Applied practice: outcome anchors are append-only evidence derived from real
events, projections are rebuilt from the physical event/constellation rows,
duplicate submissions are idempotent only when the requested anchor exactly
matches the existing physical row, conflicts fail closed with typed errors, and
logs use stable low-cardinality codes plus concrete Source-of-Truth identifiers.

## Root Cause

Outcome paths built Calyx constellations but did not persist grounded anchor rows
for the final decisions/results. That left the agent/controller layer with a
provisional relationship but no durable label at the physical Anchors CF.

Manual FSV found additional concrete faults while fixing the root issue:

- Initial approval anchoring failed with `CALYX_LEDGER_SECRET_IN_PAYLOAD`
  because the ledger payload carried raw source key/value details. The fix uses
  a secret-safe `synapse.grounding_anchor.v2` ledger payload with source/key/value
  hashes and counts only.
- Transcript end-state anchoring originally wrote one row at a time, creating a
  partial-state risk. The fix writes all transcript source anchors in a single
  multi-constellation batch and then performs exact physical readback counts.
- Session teardown can emit a second terminal event for an already completed
  spawn. Before the final fix this retried transcript anchoring with a different
  `observed_at_ms` and failed closed as a conflicting anchor. The fix canonicalizes
  each spawn's transcript anchor to the earliest terminal event timestamp and
  rejects conflicting terminal outcomes.
- `scripts/synapse-setup.ps1` could adopt an already-running daemon that lacked
  `WRITE_STORAGE`. The setup supervisor now expects and verifies the allowed
  permission set before adoption; the default configured daemon grant includes
  `READ_EVENTS READ_REFLEX READ_PROFILE READ_STORAGE WRITE_STORAGE`.

## Implementation

- Added Calyx ledger-stamped grounding anchor writers with exact Anchors CF
  readback in `synapse-storage`, `synapse-reflex`, and `synapse-calyx`.
- Added atomic multi-source anchor batching in `calyx-aster` so all current
  transcript rows are anchored together or the operation errors.
- Added `storage operation=anchors` implementation for physical anchor readback
  by exact `(cf_name, key_hex)`.
- Grounded approval decisions, routine transitions/evidence episodes, episode
  segmentation outcomes, verification outcomes, escalation audit events, agent
  tool-call success/failure, and agent terminal event/transcript outcomes.
- Added exact `key_hex` fields to agent query responses so physical source rows
  can be re-read without guessing.
- Made terminal-agent transcript anchoring source-complete aware, idempotent, and
  canonical under duplicate terminal events.

## Source Of Truth

Primary SoT:

- Calyx daemon DB: `C:\Users\hotra\AppData\Local\synapse\db-daemon`.
- Grounding rows: Calyx Anchors CF keyed by source constellation Cx id and anchor
  kind.
- Source rows:
  - `CF_KV` approval audit rows.
  - `CF_ROUTINE_STATE` routine state rows.
  - `CF_EPISODES` episode/evidence rows.
  - `CF_AGENT_EVENTS` agent event rows.
  - `CF_AGENT_TRANSCRIPTS` transcript rows.

Configured daemon precondition after final install:

```text
Process PID: 72876
Executable: C:\Users\hotra\.cargo\bin\synapse-mcp.exe
Bind/socket: 127.0.0.1:7700 LISTEN owned by PID 72876
Command line: --mode http --bind 127.0.0.1:7700 --db C:\Users\hotra\AppData\Local\synapse\db-daemon --allowed-permissions READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE
Installed binary SHA256: EAAA7AAC7B6F492666C3E86FC473ACF2DF9C75D349198DCAFBD49AEDFBD39CFF
MCP health: ok=true, pid=72876, storage=calyx, tool_count=40
Tool surface SHA256: b83c1d7204a53267e0b2a03d1330db875af55e8e0e88bc8a8e0c03060207f207
```

The running Codex parent process still has stale callable schemas for several
new/expanded facades. Setup correctly reported
`SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` and wrote the restart handoff under
`%LOCALAPPDATA%\synapse\codex-restart-handoffs`. The repo-built daemon itself
advertises the correct 40-tool surface through `health.tool_names`.

## Manual FSV

### Approval decision rejected

Trigger: real `approval` MCP call rejecting approval
`apr1-019f7469b7be77b189bbf7d665d1daec`.

Before:

```text
Approval audit row existed in CF_KV with no physical Anchors CF row for
label:synapse:approval_decision on the audit source key.
```

After physical readback:

```text
Evidence file: %LOCALAPPDATA%\synapse\fsv-1669-approval-clean.json
CF: CF_KV
Source key prefix: 617070726f76616c2f76312f61756469742f617072312d3031396637343639623762653737623138396262663764363635643164616563
Anchor kind: label:synapse:approval_decision
Anchor source: operator
Confidence: 1.0
Value: rejected
Anchor count: 1
```

### Routine confirmed

Trigger: real `routine` MCP call confirming routine `rt1-4b350766b401cda3`.

Before:

```text
Routine state row status: candidate
Transition count: 1
```

After physical readback:

```text
Evidence file: %LOCALAPPDATA%\synapse\fsv-1669-routine-confirm.json
CF: CF_ROUTINE_STATE
Source key hex: 7274312d34623335303736366234303163646133
Routine state row status: confirmed
Transition count: 2
Anchor kind: label:synapse:routine_transition
Anchor source: operator
Confidence: 1.0
Value: confirmed
Anchor count: 1
```

### Agent failed

Trigger: real `agent` MCP spawn that intentionally failed task-start readiness.

Before:

```text
Spawn: agent-spawn-019f7497-888f-7b20-9aa9-ae0862c9d2c5
Terminal event anchor: absent
```

After physical readback:

```text
Evidence file: %LOCALAPPDATA%\synapse\fsv-1669-agent-failure-readback.json
Reducer state: dead
Reason code: task_start_readiness_readback_failed
Terminal event source key: 18c358fbb90c7f1c00000010
Anchor kind: label:synapse:agent_end_state
Anchor source: synapse-agent-event
Value: failed
Anchor count: 1
```

### Agent completed, duplicate terminal retry

Trigger: real `agent` MCP spawn with prompt marker
`FSV_1669_CANONICAL_RETRY_OK`.

Before:

```text
Spawn: agent-spawn-019f754b-9858-7091-b0b7-d40626a43f40
Session: bc53006c-0354-4dd9-9c82-e273887d2a1f
Terminal event anchor: absent at first post-dead read
Transcript anchor: absent at first post-dead read
```

Execution readback:

```text
completion-status.json status: ok
exit_code: 0
wrapper_process_id: 55776
agent_process_id: 31932
final_message_present: true
stdout_line_count: 298
final-message.txt marker: FSV_1669_CANONICAL_RETRY_OK
Windows process table after completion: PIDs 55776/31932 absent
```

Agent query after completion:

```text
Reducer state: dead
Reason code: spawn_completed
Events scanned: 11936
Transcript rows scanned: 298
Terminal event key 1: 18c3639d9d8b0f4000000007
Terminal event key 2: 18c3640ccdd084cc00000009
Latest transcript source key: 6167656e742d737061776e2d30313966373534622d393835382d373039312d623062372d643430363236613433663430000000000000000127
```

Physical log/readback after background grounding completed:

```text
2026-07-18T13:02:10Z CALYX_GROUNDING_ANCHOR_PUT
  CF_AGENT_EVENTS source_key_hex=18c3639d9d8b0f4000000007
  cx_id=c53f0bb37899f78b0572c30229cb498d
  anchor kind/source=label:synapse:agent_end_state/synapse-agent-event
  ledger_seq=20496

2026-07-18T13:04:39Z AGENT_END_STATE_TRANSCRIPT_ROWS_ANCHORED
  spawn_id=agent-spawn-019f754b-9858-7091-b0b7-d40626a43f40
  outcome=completed
  transcript_rows=298
  written_anchor_count=298
  existing_anchor_count=0
  readback_exact_match_count=298
  ledger_seq=Some(20497)

2026-07-18T13:04:43Z AGENT_END_STATE_TRANSCRIPT_ROWS_ANCHORED
  duplicate retry wrote 0, found existing 298, readback_exact_match_count=298
  ledger_seq=None

2026-07-18T13:04:49Z AGENT_END_STATE_EVENT_ROWS_ANCHORED
  duplicate terminal event set accepted with event_rows=2
```

Physical Anchors CF reads after the trigger:

```text
CF_AGENT_EVENTS key 18c3639d9d8b0f4000000007
  Anchor count: 1
  Anchor kind: label:synapse:agent_end_state
  Anchor source: synapse-agent-event
  Confidence: 1.0
  Value: completed
  Observed at ms: 1784379405973
  Cx id: c53f0bb37899f78b0572c30229cb498d

CF_AGENT_TRANSCRIPTS key 6167656e742d737061776e2d30313966373534622d393835382d373039312d623062372d643430363236613433663430000000000000000127
  Anchor count: 1
  Anchor kind: label:synapse:agent_end_state
  Anchor source: synapse-agent-event
  Confidence: 1.0
  Value: completed
  Observed at ms: 1784379405973
  Cx id: e7e90b5eb3ea13b6afd04c96f8d2c20b
```

The only `CALYX_GROUNDING_ANCHOR_BATCH_FAILED` entry for the day remained the
pre-fix failure at `2026-07-18T12:36:20Z` for the earlier batch run. No new batch
failure was logged for `agent-spawn-019f754b-9858-7091-b0b7-d40626a43f40`.

## Edge Cases

Control row before each edge:

```text
CF_AGENT_EVENTS key 18c3639d9d8b0f4000000007
Anchor count: 1
Value: completed
```

Edge 1 - structurally invalid key:

```text
Trigger: storage anchors CF_AGENT_EVENTS key_hex=zz
Expected: fail closed before DB lookup
Actual: TOOL_PARAMS_INVALID, "key_hex invalid: invalid hex digit at byte offset 0"
After: control row unchanged, anchor count 1, value completed
```

Edge 2 - unknown column family:

```text
Trigger: storage anchors CF_NOT_REAL key_hex=00
Expected: fail closed before DB lookup
Actual: TOOL_PARAMS_INVALID, "cf_name is not known: \"CF_NOT_REAL\""
After: transcript control row unchanged, anchor count 1, value completed
```

Edge 3 - absent source row:

```text
Trigger: storage anchors CF_AGENT_EVENTS key_hex=00
Expected: fail closed with source-row absence
Actual: STORAGE_READ_FAILED, "source row not found: cf_name=CF_AGENT_EVENTS key_hex=00"
After: event/transcript control rows unchanged, anchor count 1, value completed
```

Edge 4 - oversized query boundary:

```text
Trigger: agent query max_events=300
Expected: fail closed because accepted range is 1..=200
Actual: TOOL_PARAMS_INVALID, AGENT_QUERY_MAX_EVENTS_INVALID
After: agent reducer still state=dead, reason_code=spawn_completed, transcript_rows_scanned=298
```

## Structural Checks

These commands are compile/lint/format checks only. They are not FSV and do not
replace the manual SoT readbacks above.

```text
cargo check - PASS
cargo fmt --all --check - PASS
cargo fmt --manifest-path calyx\Cargo.toml --all --check - PASS
cargo clippy --workspace --all-targets - PASS
cargo clippy --manifest-path calyx\Cargo.toml --workspace --all-targets - PASS
git diff --check - PASS
tracked-source scan for prohibited test/FSV-driver surfaces - PASS, no matches
```

The only warnings were CUDA build-script environment notes and Git line-ending
normalization notices. They did not indicate failing code or behavioral
verification.

## Verdict

#1669 is manually FSV-accepted for grounded outcome anchor writers. The accepted
state is physical Anchors CF rows with exact readback for approval/routine/agent
outcome paths, atomic transcript batch anchoring with exact readback counts, and
fail-closed edge handling with stable error codes and unchanged control state.
