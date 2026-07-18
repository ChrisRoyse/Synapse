# Issue #1736 - ambient `file-history-delta` transcript rows

Date: 2026-07-18

## Root Cause

The ambient Claude session parser in
`crates/synapse-mcp/src/server/ambient_agents.rs` accepted a closed inline set
of Claude session metadata `type` values. Current Claude session files under
`%USERPROFILE%\.claude\projects` contain `file-history-delta` records with the
same metadata-only shape as `file-history-snapshot`, but that discriminator was
not in the parser's accepted metadata set.

That made the parser classify a current, non-conversational Claude metadata
record as `UNKNOWN_RECORD_TYPE`, write the row as invalid, and emit
`AMBIENT_LINE_INVALID`. The storage layer was doing the right fail-loud thing;
the parser contract was stale.

## Research

Exa MCP and native web research were used before implementation.

- JSON Lines is line-oriented: each line is one valid JSON value, blank lines
  are not valid values, and line numbering starts at value 1:
  https://jsonlines.org/
- Claude Code session files are append-only JSONL under `.claude/projects`;
  each line has a `type` discriminator, and system/meta event `type` values
  evolve with Claude Code versions:
  https://claude-dev.tools/docs/jsonl-format
- Confluent's schema-evolution guidance frames this as a compatibility problem:
  consumers must keep reading older data while producers evolve, and new
  fields/event shapes should be added compatibly:
  https://docs.confluent.io/platform/current/schema-registry/fundamentals/schema-evolution.html
- OpenTelemetry GenAI conventions use structured message/tool/usage attributes
  and warn that raw prompt, output, and tool content can contain sensitive data:
  https://opentelemetry.io/docs/specs/semconv/registry/attributes/gen-ai/

Applied conclusion: accept explicitly observed metadata-only Claude session
record types as bounded `system` transcript rows, keep truly unknown types
fail-loud as invalid rows, and add source path plus raw byte/hash fields to the
invalid-line log without dumping raw content.

## Change

- Added `CLAUDE_SESSION_METADATA_TYPES` and included `file-history-delta`.
- Kept unknown `type` values invalid with `UNKNOWN_RECORD_TYPE`.
- Strengthened `AMBIENT_LINE_INVALID` logging with:
  - `source_path`
  - `raw_line_bytes`
  - `raw_line_sha256`
- Updated the `ClaudeSessionJsonl` source comment to name
  `file-history-delta`.

## Structural Checks

These are build/lint/format checks only, not FSV:

- `cargo fmt --all` - passed.
- `cargo check -p synapse-core -p synapse-mcp` - passed.
- `cargo clippy --workspace --all-targets` - passed.
- `git diff --check` - passed.

## Manual FSV

### Source of Truth

- Runtime SoT: live `synapse-mcp.exe` daemon process and listener.
- Input SoT: synthetic Claude session JSONL file under
  `%USERPROFILE%\.claude\projects\C--code-Synapse-fsv-1736`.
- Output SoT: `CF_AGENT_TRANSCRIPTS` rows in
  `%LOCALAPPDATA%\synapse\db-daemon`, keyed by
  `spawn_id || 0x00 || line_no_be_u64`.
- Runtime read surface: real `mcp__synapse.agent` MCP facade,
  `operation=query`.
- Error observability SoT: daemon JSON log
  `%LOCALAPPDATA%\synapse\logs\synapse.log.2026-07-18`.

### Runtime Precondition

`mcp__synapse.health {"detail":"compact"}` readback:

- `ok=true`
- daemon `pid=30032`
- HTTP bind `127.0.0.1:7700`
- `tool_count=40`
- `tool_surface_sha256=68343c88e00c9a7a7feb26a1b46a365b093dbff7d79c8f021f5eefd18f22ee2c`
- `storage.status=ok`
- `calyx_vault.status=ok`

Separate process/socket readback during setup showed:

- process path `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`
- listener `127.0.0.1:7700`, owner PID `30032`
- installed binary SHA-256
  `E69BB9090FDC241B5C7576B0BCAFF628F1A13103C37EFEFA73A46B41C5FD2655`

### Before State

Synthetic spawn:

`agent-spawn-ambient-claude-17360000-0000-4000-8000-202607180436`

Before the trigger:

- `Test-Path` for the synthetic source file returned `False`.
- `CF_AGENT_TRANSCRIPTS` exact-prefix/plaintext scan found no matching
  synthetic spawn rows.
- `CF_KV` cursor scan found no
  `ambient-agents/cursor/<synthetic-spawn>` row.
- daemon log search found no `AMBIENT_LINE_INVALID` rows for the synthetic
  spawn.

### Trigger

Created this synthetic source file:

`C:\Users\hotra\.claude\projects\C--code-Synapse-fsv-1736\17360000-0000-4000-8000-202607180436.jsonl`

The file had 5 lines and 604 bytes. Synthetic line inputs and expected
outcomes:

| Line | Input class | Raw bytes | Raw SHA-256 | Expected outcome |
| --- | --- | ---: | --- | --- |
| 1 | valid `file-history-delta` metadata | 312 | `ba5084b0be6770bb1ba62a19cbe1b4d1089f2aea63ca1a448fb7ad681cfd74d0` | parsed system row, `event_kind=file-history-delta` |
| 2 | empty line | 0 | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` | invalid row, `LINE_NOT_JSON` |
| 3 | object missing `type` | 118 | `b24dea6a9e80d75beca693365d8c2e9fb649670e89a9464c54b2872c06cd99f5` | invalid row, `MISSING_TYPE` |
| 4 | unknown discriminator | 106 | `d5b4fc95cbdd83118ec8cb050f61fefdc24338c81b57be8e5d276a46b1097687` | invalid row, `UNKNOWN_RECORD_TYPE` |
| 5 | malformed JSON object | 58 | `10edaafa253b18699adc32b6c0531ea15cf77fdae3040edf4fa78c25278eb96d` | invalid row, `LINE_NOT_JSON` |

The real ambient daemon tailer, not a helper write, ingested the file.

### After State - MCP Readback

Real MCP trigger/read:

`mcp__synapse.agent {"operation":"query","query":{"session_id":"agent-spawn-ambient-claude-17360000-0000-4000-8000-202607180436","max_events":5,"lookback_ms":604800000,"deep":false}}`

Readback:

- `found=true`
- `spawn_id=agent-spawn-ambient-claude-17360000-0000-4000-8000-202607180436`
- `agent_kind=claude`
- `recent_events`: `spawn_requested`, `spawn_ready`, `state_changed`
- `scan.events_matched=3`
- `scan.transcript_rows_scanned=5`
- `readback_source_of_truth="CF_AGENT_EVENTS/CF_AGENT_TRANSCRIPTS scan found=true events=3 transcripts=5"`

### After State - Physical Calyx Rows

Read command shape:

`cargo run -q -p synapse-storage --example dump_cf -- --native-source --reveal-metadata %LOCALAPPDATA%\synapse\db-daemon CF_AGENT_TRANSCRIPTS <source_key_hex>`

This is a read-only storage inspection utility. It did not trigger behavior and
did not replace the runtime ingest path.

Exact row evidence:

| Line | Key SHA-256 | Calyx match count | Status metadata | Event/parse metadata | Role metadata | Raw line bytes |
| --- | --- | ---: | --- | --- | --- | ---: |
| 1 | `d5fc7fafed75a7788652ae0eebe3b940b0e172d49958d84be599f2fb47b4e518` | 1 | `agent_transcript_status=parsed` | `agent_transcript_event_kind=file-history-delta` | `agent_transcript_role=system` | 312 |
| 2 | `1118fccfd14662caa376abaf79b1fc4728e1ba8f31964c01992fccc1b4872e76` | 1 | `agent_transcript_status=invalid` | `agent_transcript_parse_error_excerpt=LINE_NOT_JSON: EOF while parsing a value at line 1 column 0` | absent | 0 |
| 3 | `352288cd5afb795748a2011b3f8eb3be738d3c0b0645345bc83220e863b90072` | 1 | `agent_transcript_status=invalid` | `agent_transcript_parse_error_excerpt=MISSING_TYPE: line has no string type field` | absent | 118 |
| 4 | `087c5dfb3e4c16e755fb050c3327881a0e1590b0ef62efe2a2d11a1753d4ef4c` | 1 | `agent_transcript_status=invalid` | `agent_transcript_parse_error_excerpt=UNKNOWN_RECORD_TYPE: definitely-unknown-issue-1736` | absent | 106 |
| 5 | `4647091f921f84b6d192ef619843f147914199982f5dc54f9e9b73804075a19f` | 1 | `agent_transcript_status=invalid` | `agent_transcript_parse_error_excerpt=LINE_NOT_JSON: EOF while parsing an object at line 1 column 58` | absent | 58 |

Each row also had:

- `agent_transcript_source=claude_session_jsonl`
- `agent_transcript_spawn_id=<synthetic spawn>`
- `agent_transcript_conversation_id=17360000-0000-4000-8000-202607180436`
- `synapse_panel_name=syn-agent-transcript-v1`
- `synapse_source_cf=CF_AGENT_TRANSCRIPTS`
- `synapse_source_key_hex=<exact transcript key>`

### Error Log Readback

`Select-String` over the daemon JSON log returned `AMBIENT_LINE_INVALID` rows
for lines 2-5 only. Each row included the synthetic spawn id, source path, line
number, `raw_line_bytes`, `raw_line_sha256`, and structured `detail`.

Line 1 did not emit `AMBIENT_LINE_INVALID`; its physical row is parsed as
`event_kind=file-history-delta` and `role=system`.

### Edge Case Audit

1. Empty input line:
   - Before: no synthetic row/log existed.
   - Trigger: line 2 was a blank JSONL line.
   - After: DB row line 2 exists with `status=invalid`,
     `LINE_NOT_JSON`, `raw_line_bytes=0`; log carries same line/source/hash.

2. Structurally invalid object:
   - Before: no synthetic row/log existed.
   - Trigger: line 3 was a JSON object with no string `type`.
   - After: DB row line 3 exists with `status=invalid`, `MISSING_TYPE`,
     `raw_line_bytes=118`; log carries same line/source/hash.

3. Unknown event discriminator:
   - Before: no synthetic row/log existed.
   - Trigger: line 4 used `type=definitely-unknown-issue-1736`.
   - After: DB row line 4 exists with `status=invalid`,
     `UNKNOWN_RECORD_TYPE`; log carries same line/source/hash. This proves the
     fix did not add a broad fallback that accepts unknown record classes.

4. Malformed JSON:
   - Before: no synthetic row/log existed.
   - Trigger: line 5 was a truncated JSON object.
   - After: DB row line 5 exists with `status=invalid`, `LINE_NOT_JSON`,
     `raw_line_bytes=58`; log carries same line/source/hash.

### Cleanup

After all readbacks, the synthetic input directory was removed:

`C:\Users\hotra\.claude\projects\C--code-Synapse-fsv-1736`

Cleanup readback:

- `exists_after=False`

The durable Calyx rows and daemon log evidence remain for audit.

## Follow-Up Filed

During FSV the synthetic ambient session also produced a critical
`silent_timeout_unprobeable` escalation after registration. That is a separate
ambient escalation semantics problem, tracked as #1738.
