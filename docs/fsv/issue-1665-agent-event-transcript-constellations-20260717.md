# Manual FSV Closeout: Issue #1665 Agent Event And Transcript Constellations

Date: 2026-07-17

Issue: https://github.com/ChrisRoyse/Synapse/issues/1665

## Result

Accepted. Agent journal events and agent transcript lines are now measured on
ingest into native Calyx constellations:

- `syn-agent-event-v1`, panel version `1665001`
- `syn-agent-transcript-v1`, panel version `1665002`

Token usage, cache usage, reasoning tokens, costs, byte counts, timestamps, and
line numbers are stored as exact scalar values. Spawn/session/model/tool/status
identity is stored as metadata. Raw prompt/tool content is not promoted to
metadata.

No automated tests, FSV harnesses, benchmarks, mocks, CI, or GitHub Actions were
created or run. The commands listed here are structural checks or manual
Source-of-Truth readbacks only.

## Root Cause

The first manual FSV run for this issue found a real idempotency bug before the
change could be accepted:

```text
spawn_id=agent-spawn-019f6f5a-168a-7133-a214-1203b7902573
stdout_line_count=328
marker=ISSUE1665_SYNTHETIC_SUM=4 DONE=true
native_source CF_AGENT_TRANSCRIPTS line=108 match_count=2
```

The duplicated native-source match had the same raw transcript source key but a
different content-addressed constellation. The structural cause was that
`parse_line` used `unix_time_ns_now()` for transcript row identity. A retry or
concurrent finalization of the same `(spawn_id,line_no)` rewrote the row with a
new timestamp, producing a second native constellation for the same source row.

The fix removes ingest wall-clock time from transcript identity. Transcript
rows now derive `ts_ns` from explicit source timestamps, RFC3339 timestamps,
UUIDv7 time anchors, or the stable spawn manifest epoch plus line offset.
Ambient session transcripts use the same deterministic derivation. Cursor
advancement also remains after raw row and constellation writes, so projection
failure leaves the lines eligible for re-ingest instead of silently advancing.

## Research Used

Research was done after identifying the problem, using Exa MCP and native web
research against primary sources:

- OpenTelemetry GenAI semantic convention registry:
  https://opentelemetry.io/docs/specs/semconv/registry/attributes/gen-ai/
- OpenTelemetry GenAI semantic conventions repository:
  https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/registry/attributes/gen-ai.md
- Elastic Filebeat delivery/registry model:
  https://www.elastic.co/docs/reference/beats/filebeat/how-filebeat-works

Takeaways applied here:

- GenAI usage counts, cache-read/cache-creation counts, and model identifiers
  are durable telemetry dimensions and should be represented as structured
  numeric/scalar fields.
- Cache token fields must not be hidden in unstructured text; downstream cost
  and reliability analysis needs exact counts.
- Cursor/offset advancement must happen only after the downstream projection is
  acknowledged. If projection fails, re-ingest may duplicate attempted delivery,
  so content identity and source keys must be deterministic.

## Source Of Truth

Runtime SoTs:

```text
daemon_pid=74140
bind=127.0.0.1:7700
exe=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
binary_sha256=2DFB163A1A4FA3AA1B54BDD0A552384DCF51788BEFA07128AEFAC6E7ED8D78CF
tool_count=40
tool_surface_sha256=7baef0742b0aacbb2a838a301a3ef25af3175468b15d831f6aa90b88dfd7b776
storage_backend=calyx
db=C:\Users\hotra\AppData\Local\synapse\db-daemon
```

Happy-path data SoTs:

```text
spawn_id=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
session_id=ef62c9ff-c676-4e54-aad0-da1e28f9d9a4
stdout=C:\Users\hotra\AppData\Local\Synapse\agent-spawns\agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b\stdout.jsonl
completion=C:\Users\hotra\AppData\Local\Synapse\agent-spawns\agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b\completion-status.json
final_message=C:\Users\hotra\AppData\Local\Synapse\agent-spawns\agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b\final-message.txt
raw_events=CF_AGENT_EVENTS
raw_transcripts=CF_AGENT_TRANSCRIPTS
native_calyx_base=Base rows addressed through source metadata
native_source_readback=target\debug\examples\dump_cf.exe --native-source --reveal-metadata
```

## MCP Preconditions

The real wired `mcp__synapse.health` call returned:

```text
ok=true
pid=74140
bind_addr=127.0.0.1:7700
storage.status=ok
storage.storage_backend=calyx
tool_count=40
tool_names includes agent
tool_surface_sha256=7baef0742b0aacbb2a838a301a3ef25af3175468b15d831f6aa90b88dfd7b776
```

The trigger used the real `mcp__synapse.agent` tool through the strict Codex MCP
client. Storage verification used separate read-only physical reads of the
Calyx-backed database and native-source index.

## Before State

Before the fixed happy-path trigger:

```text
CF_AGENT_EVENTS row_count=61840
CF_AGENT_TRANSCRIPTS row_count=481175
latest_spawn_before=agent-spawn-019f6f5a-168a-7133-a214-1203b7902573
```

## Happy Path Trigger

Real MCP trigger:

```text
tool=mcp__synapse.agent
operation=spawn
kind=codex
prompt marker=ISSUE1665_FIXED_SUM=8 DONE=true
working_dir=C:\code\Synapse
mcp_url=http://127.0.0.1:7700/mcp
wait_timeout_ms=300000
hold_open_ms=0
```

MCP return identified the physical artifacts:

```text
spawn_id=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
session_id=ef62c9ff-c676-4e54-aad0-da1e28f9d9a4
stdout_path=C:\Users\hotra\AppData\Local\Synapse\agent-spawns\agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b\stdout.jsonl
```

## After State

File SoT:

```text
STDOUT_EXISTS=True
COMPLETION_EXISTS=True
FINAL_EXISTS=True
STDOUT_LINE_COUNT=336
completion.status=ok
completion.exit_code=0
completion.stdout_line_count=336
completion.final_message_present=true
completion.stderr_bytes=0
final_message contains ISSUE1665_FIXED_SUM=8 DONE=true
```

Relevant raw transcript lines:

```text
line=332 method=item/completed role=assistant marker=ISSUE1665_FIXED_SUM=8 DONE=true
line=333 method=thread/tokenUsage/updated
line=333 inputTokens=152084
line=333 cachedInputTokens=123648
line=333 outputTokens=1903
line=333 reasoningOutputTokens=1444
line=333 totalTokens=153987
```

Storage counts after ingest:

```text
CF_AGENT_EVENTS row_count=61847
CF_AGENT_TRANSCRIPTS row_count=481511
delta_events=+7
delta_transcripts=+336
```

Final `agent.query` readback converged to the durable completion event:

```text
found=true
spawn_id=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
session_id=ef62c9ff-c676-4e54-aad0-da1e28f9d9a4
reason_code=spawn_completed
events_matched=6
transcript_rows_scanned=336
recent_events includes exited reason_code=spawn_completed
turn.source_line_no=333
turn.input_tokens=152084
turn.cache_read_input_tokens=123648
turn.output_tokens=1903
turn.reasoning_output_tokens=1444
turn.total_tokens=277635
```

`turn.total_tokens` is the query surface's context estimate. The native scalar
readback below preserves the raw `totalTokens=153987` field from the JSONL
source as `usage_total_tokens`.

## Native Transcript Readback

Line 332 source key:

```text
source_cf=CF_AGENT_TRANSCRIPTS
source_key_hex=6167656e742d737061776e2d30313966366636642d636138632d376230332d613439322d30663861643730666562356200000000000000014c
native_source_result match_count=1
panel_version=1665002
synapse_panel_name=syn-agent-transcript-v1
agent_transcript_spawn_id=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
agent_transcript_line_no=332
agent_transcript_role=assistant
agent_transcript_event_kind=codex_app_server/item/completed/agentMessage
content_bytes=364
raw_line_bytes=716
slot_count=13
scalar_count=11
slots present=35..47
```

Line 333 source key:

```text
source_cf=CF_AGENT_TRANSCRIPTS
source_key_hex=6167656e742d737061776e2d30313966366636642d636138632d376230332d613439322d30663861643730666562356200000000000000014d
native_source_result match_count=1
panel_version=1665002
synapse_panel_name=syn-agent-transcript-v1
agent_transcript_spawn_id=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
agent_transcript_line_no=333
agent_transcript_role=result
agent_transcript_event_kind=codex_app_server/thread/tokenUsage/updated
usage_input_tokens=152084
usage_cache_read_input_tokens=123648
usage_output_tokens=1903
usage_reasoning_output_tokens=1444
usage_total_tokens=153987
model=gpt-5.5
slot_count=13
scalar_count=22
slots present=35..47
```

The important regression check is `match_count=1` for each source key. The
pre-fix run produced `match_count=2` for a single transcript source key.

## Native Agent Event Readback

Known event from the daemon log:

```text
event=state_spawning_to_working
ts_ns=1784280958618302500
seq=9
source_key_hex=18c30a140deff82400000009
```

Native-source readback:

```text
source_cf=CF_AGENT_EVENTS
native_source_result match_count=1
panel_version=1665001
synapse_panel_name=syn-agent-event-v1
agent_event_kind=state_changed
agent_event_reason_code=spawn_ready
agent_event_session_id=ef62c9ff-c676-4e54-aad0-da1e28f9d9a4
agent_event_spawn_id=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
agent_event_state_from=spawning
agent_event_state_to=working
slot_count=12
scalar_count=3
slots present=23..34
```

Second known event:

```text
event=state_working_to_dead
ts_ns=1784280977006121200
seq=10
source_key_hex=18c30a1855efd0f00000000a
native_source_result match_count=1
panel_version=1665001
synapse_panel_name=syn-agent-event-v1
agent_event_reason_code=process_gone_without_exit_event
agent_event_state_from=working
agent_event_state_to=dead
slot_count=12
scalar_count=3
slots present=23..34
```

## Edge Audit

### Edge 1: Unknown Stats Field

Before:

```text
CF_AGENT_EVENTS row_count=61850
CF_AGENT_TRANSCRIPTS row_count=481511
latest_spawn=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
```

Trigger:

```json
{"operation":"stats","stats":{"limit":5}}
```

MCP result:

```text
TOOL_PARAMS_INVALID
unknown field `limit`
accepted fields: since_ns, until_ns, spawn_id, session_id, group_by
```

After:

```text
CF_AGENT_EVENTS row_count=61850
CF_AGENT_TRANSCRIPTS row_count=481511
latest_spawn=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
```

### Edge 2: Empty Spawn Prompt

Before:

```text
CF_AGENT_EVENTS row_count=61850
CF_AGENT_TRANSCRIPTS row_count=481511
latest_spawn=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
```

Trigger:

```json
{"operation":"spawn","spawn":{"kind":"codex","prompt":"","working_dir":"C:\\code\\Synapse","mcp_url":"http://127.0.0.1:7700/mcp","wait_timeout_ms":300000,"hold_open_ms":0,"require_approval_gate":false}}
```

MCP result:

```text
TOOL_PARAMS_INVALID
act_spawn_agent direct spawn prompt must not be empty
```

After:

```text
CF_AGENT_EVENTS row_count=61850
CF_AGENT_TRANSCRIPTS row_count=481511
latest_spawn=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
```

### Edge 3: Invalid Spawn Kind

Before:

```text
CF_AGENT_EVENTS row_count=61850
CF_AGENT_TRANSCRIPTS row_count=481511
latest_spawn=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
```

Trigger:

```json
{"operation":"spawn","spawn":{"kind":"not_a_real_agent_kind","prompt":"ISSUE1665_EDGE_INVALID_KIND_SHOULD_NOT_SPAWN","working_dir":"C:\\code\\Synapse","mcp_url":"http://127.0.0.1:7700/mcp","wait_timeout_ms":300000,"hold_open_ms":0,"require_approval_gate":false}}
```

MCP result:

```text
TOOL_PARAMS_INVALID
unknown variant `not_a_real_agent_kind`, expected one of `codex`, `claude`, `local_model`
```

After:

```text
CF_AGENT_EVENTS row_count=61850
CF_AGENT_TRANSCRIPTS row_count=481511
latest_spawn=agent-spawn-019f6f6d-ca8c-7b03-a492-0f8ad70feb5b
marker search in latest three spawn stdout files: false,false,false
```

## Structural Checks

Supporting structural checks run locally after implementation:

```text
cargo fmt --all
cargo check --workspace
git diff --check
```

Final `cargo fmt --all --check` and `cargo clippy --workspace --all-targets`
were run before commit. These are compile/lint checks only, not FSV.

