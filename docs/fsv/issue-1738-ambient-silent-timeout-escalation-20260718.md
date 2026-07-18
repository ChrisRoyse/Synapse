# Issue #1738 - ambient silent-timeout escalation suppression

Date: 2026-07-18

## Root Cause

`AgentStateTracker::sweep` treated observed ambient Claude transcript agents the
same as launch-owned live agents after the generic `stuck_after_ms` threshold.
Ambient transcript agents have no process handle, so their silence is not an
actionable proof of a live stuck process. They should remain visible until the
unprobeable-dead threshold reaps them.

The escalation layer already knew `silent_timeout_unprobeable` was not Tier 1
eligible for ambient sessions, but it still wrote an acknowledged critical
escalation row. That created operator-facing noise for historical or scanner
observed transcript files.

## Research

Exa MCP and native web research were used before implementation.

- Google SRE alerting guidance: pages should be actionable and alert fatigue
  from noisy alerts harms incident response:
  https://sre.google/sre-book/monitoring-distributed-systems/
- Kubernetes TTL-after-finished keeps completed state observable, then removes
  it by an explicit TTL controller:
  https://kubernetes.io/docs/concepts/workloads/controllers/ttlafterfinished/
- Elastic Filebeat keeps and later cleans file state with explicit inactive and
  cleanup thresholds rather than silently dropping active state:
  https://www.elastic.co/docs/reference/beats/filebeat/filebeat-input-log

Applied conclusion: an observed transcript with no process handle should use a
visibility-then-reap lifecycle, not an operator interrupt. Parser/storage
failures remain loud, with physical rows and structured logs. The escalation
layer also suppresses this policy class before creating a row as defense in
depth.

## Change

- Added an ambient/no-process classifier for `agent-spawn-ambient-*` entries.
- Excluded ambient no-process entries from the generic working/spawning silence
  stuck transition.
- Let ambient no-process entries be reaped by
  `SYNAPSE_AGENT_UNPROBEABLE_DEAD_AFTER_MS` with
  `reason_code=unprobeable_silent_ended`.
- Treated `unprobeable_silent_ended` as a normal terminal reason.
- Added an escalation defense-in-depth gate that logs
  `ESCALATION_SUPPRESSED` and returns before item creation when an
  `operator_interrupt_suppressed_reason` applies.

## Structural Checks

These are build/lint/format checks only, not FSV:

- `cargo fmt --all --check` - passed.
- `cargo check -p synapse-mcp` - passed.
- `cargo clippy --workspace --all-targets` - passed.
- `git diff --check` - passed.

## Manual FSV

### Source of Truth

- Runtime SoT: live `synapse-mcp.exe` daemon process and `127.0.0.1:7700`
  listener.
- Client/tool SoT: real `mcp__synapse.health`, `mcp__synapse.agent`, and
  `mcp__synapse.escalation` tools loaded through the wired Codex MCP client.
- Input SoT: synthetic Claude JSONL files under
  `C:\Users\hotra\AppData\Local\synapse\fsv\issue-1738-20260717T234823\claude-projects\C--code-Synapse-fsv-1738`.
- Output SoT: `CF_AGENT_EVENTS`, `CF_AGENT_TRANSCRIPTS`, and `CF_KV`
  escalation rows in `C:\Users\hotra\AppData\Local\synapse\db-daemon`.
- Storage readback SoT: read-only
  `cargo run -q -p synapse-storage --example dump_cf -- --native-source --reveal-metadata`.
- Log SoT:
  `C:\Users\hotra\AppData\Local\synapse\logs\synapse.log.2026-07-18`.

### Runtime Precondition

Before accepting behavior, the repo-built daemon was installed and started with
accelerated liveness thresholds for this manual run:

- Installed binary SHA-256:
  `44E8A957E7CA355D00BD61852BC79EF9F99EFEBF763C704B78FA23C3A9A5B023`.
- FSV daemon PID: `69356`; parent supervisor PID: `42884`.
- Bind: `127.0.0.1:7700`.
- Real wired `mcp__synapse.health {"detail":"compact"}`:
  `ok=true`, `pid=69356`, `tool_count=40`,
  `tool_surface_sha256=68343c88e00c9a7a7feb26a1b46a365b093dbff7d79c8f021f5eefd18f22ee2c`,
  `storage.status=ok`, `calyx_vault.status=ok`.
- Startup log:
  `AMBIENT_INGEST_PERIODIC_SCHEDULED`, `interval_secs=1`,
  `startup_delay_secs=0`, `max_idle_secs=3600`,
  `root_scope=explicit_env`, `db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon`.
- Startup log:
  `AGENT_LIVENESS_SWEEP_STARTED`, `sweep_interval_ms=500`,
  `stuck_after_ms=1000`.

After FSV, temporary user environment overrides were cleared and the normal
daemon was restored. Final host readback:

- Real wired `mcp__synapse.health {"detail":"compact"}`:
  `ok=true`, daemon PID `82164`, bind `127.0.0.1:7700`,
  `tool_count=40`, same tool-surface hash, `storage.status=ok`,
  `calyx_vault.status=ok`, `chrome_bridge.status=ok`.
- Process/socket SoT: `synapse-mcp.exe` PID `82164`, path
  `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, listener owner PID `82164`,
  scheduled task `SynapseMcpDaemon` state `Running`.
- Normal startup log root:
  `C:\Users\hotra\.claude\projects`, `root_scope=configured_daemon_db`,
  `interval_secs=5`, `startup_delay_secs=8`, `max_idle_secs=86400`.
- Temporary FSV user env keys
  `SYNAPSE_AMBIENT_CLAUDE_PROJECTS_DIR`,
  `SYNAPSE_AMBIENT_INGEST_INTERVAL_SECS`,
  `SYNAPSE_AMBIENT_INGEST_STARTUP_DELAY_SECS`,
  `SYNAPSE_AMBIENT_MAX_IDLE_SECS`,
  `SYNAPSE_AGENT_STUCK_AFTER_MS`,
  `SYNAPSE_AGENT_LIVENESS_SWEEP_MS`, and
  `SYNAPSE_AGENT_UNPROBEABLE_DEAD_AFTER_MS` read back as empty.

The current Codex process still reports the known
`SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` setup warning from #1398, but the
real wired client loaded the 40-tool public surface and executed the required
`agent` and `escalation` calls during this FSV.

### Before State

Synthetic ambient anchors:

- `agent-spawn-ambient-claude-17380000-0000-4000-8000-000000005100`
- `agent-spawn-ambient-claude-17380000-0000-4000-8000-000000005101`
- `agent-spawn-ambient-claude-17380000-0000-4000-8000-000000005102`

Before trigger readback:

- `mcp__synapse.agent operation=query` returned `found=false`,
  `events_matched=0`, `transcript_rows_scanned=0` for each anchor.
- `mcp__synapse.escalation operation=list` returned `returned=0`,
  `total_open=0` for each anchor.
- The synthetic input root contained no matching transcript files.
- The daemon log contained no matching synthetic anchor lines.

### Trigger

The real ambient daemon tailer ingested three synthetic Claude session JSONL
files from the configured ambient root. Inputs and expected outcomes:

| Session | Input class | File bytes | File SHA-256 | Expected outcome |
| --- | --- | ---: | --- | --- |
| `...5100` | assistant `end_turn` | 459 | `DCADE1D240C263E8EE6CA89A66DB24AA51F2F2342AF08A156C3CD9A718DD1B9E` | parsed row, working -> idle -> dead, no escalation |
| `...5101` | assistant `tool_use` with `Read` | 521 | `D84EEAC0AD607748EB94F9B7F39AA52AF237EEBCEEC736232934E82AAA853D18` | parsed row, stays working through 4x in-flight grace, then dead, no escalation |
| `...5102` | object missing `type` | 119 | `4E17DB13400367B42CABE127C3D4FF8FD4FF0840BACF724061831B5CBD3300E4` | invalid row and structured error log, dead, no escalation |

### Happy Path - Idle Ambient Session

Real MCP read:

`mcp__synapse.agent {"operation":"query","query":{"session_id":"agent-spawn-ambient-claude-17380000-0000-4000-8000-000000005100","max_events":10,"lookback_ms":604800000,"deep":false}}`

After readback:

- `found=true`
- `state=dead`
- `reason_code=unprobeable_silent_ended`
- `activity_summary="fsv idle end turn"`
- `agent_kind=claude`
- `transcript_rows_scanned=1`
- `events_matched=6`
- recent event progression:
  `spawn_requested(ambient_discovered)`,
  `spawn_ready(ambient_observed)`,
  `state_changed spawning->working(spawn_ready)`,
  `turn_finished(ambient_turn_finished)`,
  `state_changed working->idle(turn_finished)`,
  `state_changed idle->dead(unprobeable_silent_ended)`
- Real MCP escalation read for the anchor returned `returned=0`,
  `total_open=0`.

Physical transcript row read:

- Key:
  `6167656e742d737061776e2d616d6269656e742d636c617564652d31373338303030302d303030302d343030302d383030302d303030303030303035313030000000000000000001`
- `match_count=1`
- `agent_transcript_status=parsed`
- `agent_transcript_event_kind=assistant`
- `agent_transcript_role=assistant`
- `agent_transcript_spawn_id=agent-spawn-ambient-claude-17380000-0000-4000-8000-000000005100`
- `raw_line_bytes=457`
- usage metadata: input `11`, output `4`, total `15`

### Edge 1 - Empty / Absent Input Baseline

Before:

- Synthetic input files absent.
- `agent` queries for all three anchors returned `found=false`,
  `events_matched=0`, `transcript_rows_scanned=0`.
- Escalation lists for all three anchors returned no rows.
- Logs had no matching anchors.

After:

- Only the three explicitly created synthetic files produced rows.
- No unrelated anchor appeared in `CF_AGENT_EVENTS`, `CF_AGENT_TRANSCRIPTS`, or
  `CF_KV` escalation lists during the run.

### Edge 2 - In-Flight Tool Grace Boundary

Real MCP read during the grace window for
`agent-spawn-ambient-claude-17380000-0000-4000-8000-000000005101`:

- Read at elapsed time about `6316ms`, greater than the `4000ms`
  unprobeable-dead base threshold and less than the `16000ms` in-flight tool
  grace threshold.
- `state=working`
- `reason_code=tool_activity`
- `current_tool_call.in_flight=true`
- `tool_name=Read`
- `events_matched=4`
- `transcript_rows_scanned=1`
- Escalation read returned `returned=0`, `total_open=0`.

After the in-flight grace expired:

- `state=dead`
- `reason_code=unprobeable_silent_ended`
- `events_matched=5`
- `transcript_rows_scanned=1`
- Recent events included `tool_call_started` and
  `state_changed working->dead(unprobeable_silent_ended)`.
- Escalation read returned `returned=0`, `total_open=0`.

Physical transcript row read:

- Key:
  `6167656e742d737061776e2d616d6269656e742d636c617564652d31373338303030302d303030302d343030302d383030302d303030303030303035313031000000000000000001`
- `match_count=1`
- `agent_transcript_status=parsed`
- `agent_transcript_tool_names=Read`
- `raw_line_bytes=519`
- `tool_call_count=1`
- `tool_argument_bytes_total=44`
- usage metadata: input `10`, output `2`, total `12`

### Edge 3 - Structurally Invalid Transcript Line

Real MCP read for
`agent-spawn-ambient-claude-17380000-0000-4000-8000-000000005102`:

- `found=true`
- `state=dead`
- `reason_code=unprobeable_silent_ended`
- `events_matched=4`
- `transcript_rows_scanned=1`
- Recent event progression:
  `spawn_requested`, `spawn_ready`,
  `state_changed spawning->working`,
  `state_changed working->dead(unprobeable_silent_ended)`.
- Escalation read returned `returned=0`, `total_open=0`.

Physical transcript row read:

- Key:
  `6167656e742d737061776e2d616d6269656e742d636c617564652d31373338303030302d303030302d343030302d383030302d303030303030303035313032000000000000000001`
- `match_count=1`
- `agent_transcript_status=invalid`
- `agent_transcript_parse_error_excerpt=MISSING_TYPE: line has no string type field`
- `raw_line_bytes=117`

Log readback:

- `AMBIENT_LINE_INVALID` at `2026-07-18T05:12:42.699151Z`
- `line_no=1`
- `raw_line_bytes=117`
- `raw_line_sha256=08a0916493963765b74cdc3b654d81c03afcbe01f73fbab8cc06cdda8bb28ef8`
- `detail=MISSING_TYPE: line has no string type field`

This proves parser failure remained fail-loud while the lifecycle fix avoided
operator escalation noise.

### Escalation Negative Readback

Daemon log search over the synthetic anchors returned:

- `matching_log_lines=11`
- `silent_timeout_unprobeable_count=0`
- `escalation_opened_count=0`
- `escalation_suppressed_count=0`

No `ESCALATION_SUPPRESSED` log was expected on this real path because the state
tracker prevented the bad `silent_timeout_unprobeable` transition before the
escalation layer was invoked. The escalation pre-create suppression gate is
defense in depth for any future injected/synthetic transition in the same
policy class.
