# Issue #1740 - Codex Schema Stale Diagnostics FSV

Date: 2026-07-18

## Root Cause

The stale Codex MCP schema state was real, but the runtime diagnosis was
ambiguous. `codex_client_surface` compared the configured Codex host snapshot
mostly by public tool names. That let a snapshot with the right 40 tool names
but stale schemas/descriptions appear partly healthy while setup handoffs carried
the actual schema-hash drift separately.

The doctor script also only handled the "no Synapse namespace" symptom. It did
not offer a shell-runnable stale-schema path that compared:

- current Codex process-start tool-surface hash
- `%APPDATA%\synapse\codex-tool-surface.json`
- a fresh production Codex `mcp__synapse.health` call
- live daemon PID/socket readback

During investigation, direct unauthenticated-by-session `/health` also proved an
important boundary: it reports the unscoped 241-tool admin surface. The Codex
schema snapshot is the 40-tool MCP session surface, so stale-schema comparison
must use a real fresh Codex MCP `health` call for the live hash, and direct
`/health` only for daemon reachability/PID.

## Research

- Exa and web research confirmed MCP clients discover tools with `tools/list`;
  tool metadata includes schemas, and servers advertise/list-change semantics:
  <https://modelcontextprotocol.io/specification/2025-06-18/server/tools>
- MCP client best practices recommend caching tool definitions but refreshing
  cached catalogs when `notifications/tools/list_changed` is received:
  <https://modelcontextprotocol.io/docs/develop/clients/client-best-practices>
- A current Codex stale-schema issue shows an already-running Codex session can
  keep old MCP tool schemas even after server restart and list-change
  notification; a fresh Codex process sees the new schema:
  <https://github.com/openai/codex/issues/19155>

Design conclusion: do not add compatibility fallbacks. Keep the server strict,
publish exact surface hashes in telemetry, and fail closed into a same-agent
restart handoff when the process-local Codex schema cache is physically stale.

## Changes

- `crates/synapse-mcp/src/server/health.rs`
  - Factored the existing full `tools/list` canonical fingerprint into
    `tool_surface_fingerprint_for_tools`.
- `crates/synapse-mcp/src/server/tool_profiles.rs`
  - Added live 40-tool surface hash/count to `codex_client_surface`.
  - Added direct host-snapshot-vs-live hash comparison and mismatch detail.
  - Exposed `current_process_start_tool_surface_sha256` from restart handoffs.
  - Added one-command doctor/readback hints to telemetry.
- `scripts/synapse-codex-doctor.ps1`
  - Added `-ObservedSynapseSchemaStale`.
  - Added process-start, host snapshot, fresh-Codex MCP health, and direct
    daemon health readbacks.
  - Writes `codex-restart-handoff-*.json/.md` plus `STATE\RECOVERY_NOTES.md`
    for physically proven schema drift.
  - Fails closed when stale schema is claimed but not proven.
  - Logs invalid `-ActiveIssue` input into a doctor report instead of throwing
    before report creation.
- Docs updated in `README.md` and `docs/systemdocs`.

## Structural Checks

These are build/lint checks only, not FSV:

- PowerShell parser:
  `PowerShell parser OK`
- `cargo fmt --all --check`: pass
- `cargo check -p synapse-mcp`: pass
- `cargo clippy --workspace --all-targets`: pass
- `cargo build --release -p synapse-mcp`: pass

## FSV Source Of Truth

- Process table:
  `Get-CimInstance Win32_Process -Filter "Name='synapse-mcp.exe'"`
- Socket table:
  `Get-NetTCPConnection -LocalPort 7700`
- Host Codex tool snapshot:
  `C:\Users\hotra\AppData\Roaming\synapse\codex-tool-surface.json`
- Current process-start env:
  `SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START`,
  `SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START`,
  `SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START`
- Restart handoffs:
  `C:\Users\hotra\AppData\Local\synapse\codex-restart-handoffs`
- Recovery notes:
  `C:\code\Synapse\STATE\RECOVERY_NOTES.md`
- Doctor reports:
  `C:\Users\hotra\AppData\Local\synapse\codex-no-facade-doctor` and
  `C:\Users\hotra\AppData\Local\synapse\fsv\issue-1740\*`
- Real client trigger:
  deferred Synapse tool discovery, then real `mcp__synapse.health`,
  `mcp__synapse.profile`, and `mcp__synapse.telemetry`.

## MCP Preconditions

Before:

```text
synapse-mcp.exe process table: empty
127.0.0.1:7700 listener: empty
host snapshot: tool_count=40 hash=b83c1d7204a53267e0b2a03d1330db875af55e8e0e88bc8a8e0c03060207f207
process start hash: d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb
```

Started repo-built release daemon:

```text
pid=43756
exe=C:\code\Synapse\target\release\synapse-mcp.exe
cmd="...\synapse-mcp.exe" --mode http --bind 127.0.0.1:7700 --db C:\Users\hotra\AppData\Local\synapse\fsv\issue-1740\db-final --storage-backend calyx
socket=Listen 127.0.0.1:7700 owner=43756
direct /health ok=true pid=43756 tool_count=241 hash=6dc58d5587e48a84bc08655a0a926b2b6ec3e580b37b5bcac895c030c8bb73b6
```

Real wired MCP client:

```text
tool_search loaded Synapse health/profile/telemetry without schema errors
mcp__synapse.health ok=true pid=43756 tool_count=40
mcp__synapse.health tool_surface_sha256=98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e
```

Runtime telemetry after the Rust fix:

```text
codex_client_surface.status=restart_required_for_live_codex_pid
diagnostic_code=SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE
live_tool_count=40
live_tool_surface_sha256=98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e
host_snapshot.tool_surface_sha256=b83c1d7204a53267e0b2a03d1330db875af55e8e0e88bc8a8e0c03060207f207
host_snapshot_matches_live_tool_surface=false
host_snapshot_live_mismatch_detail=host snapshot ... does not match live daemon ... for 40 tools
latest_restart_handoff.current_process_start_tool_surface_sha256=d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb
live_stale_codex_process.pid=58888
stale_schema_doctor_command_hint=pwsh -NoProfile -File .\scripts\synapse-codex-doctor.ps1 -ProjectDir C:\code\Synapse -ObservedSynapseSchemaStale -ActiveIssue <issue>
```

## Happy Path

Trigger:

```powershell
pwsh -NoProfile -File .\scripts\synapse-codex-doctor.ps1 `
  -ProjectDir C:\code\Synapse `
  -ObservedSynapseSchemaStale `
  -ActiveIssue 1740 `
  -FreshProbeTimeoutSec 240
```

Before:

```json
{"phase":"before","latest_handoff":"C:\\Users\\hotra\\AppData\\Local\\synapse\\codex-restart-handoffs\\codex-restart-handoff-58888-20260718T140519080Z.json","latest_write_utc":"2026-07-18T14:05:19.2356928Z","recovery_write_utc":"2026-07-18T14:05:19.2407741Z"}
```

After report:

```json
{"status":"handoff_written","reason_code":"SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE","observed_synapse_schema_stale":true,"fresh_probe_pid_matches_direct_health":true,"process_start_hash":"d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb","host_snapshot_hash":"b83c1d7204a53267e0b2a03d1330db875af55e8e0e88bc8a8e0c03060207f207","live_daemon_hash":"98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e","schema_stale_proven":true}
```

Separate SoT read after:

```json
{"path":"C:\\Users\\hotra\\AppData\\Local\\synapse\\codex-restart-handoffs\\codex-restart-handoff-58888-20260718T151816169Z.json","length":22927,"reason_code":"SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE","reason":"current_process_start_hash_mismatch","phase":"doctor_observed_schema_stale","required_restart":true,"stale_pid":58888,"daemon_pid":43756,"daemon_hash":"98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e","process_start_hash":"d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb","host_snapshot_hash":"b83c1d7204a53267e0b2a03d1330db875af55e8e0e88bc8a8e0c03060207f207","schema_stale_proven":true,"fresh_probe_pid_match":true}
```

Recovery notes readback:

```text
Reason: SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE (current_process_start_hash_mismatch)
Phase: doctor_observed_schema_stale
JSON: C:\Users\hotra\AppData\Local\synapse\codex-restart-handoffs\codex-restart-handoff-58888-20260718T151816169Z.json
Stale Codex PID: 58888
Daemon bind: 127.0.0.1:7700
Daemon tool surface: 98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e
Current process start tool surface: d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb
Host snapshot tool surface: b83c1d7204a53267e0b2a03d1330db875af55e8e0e88bc8a8e0c03060207f207
Active issue: #1740
```

## Automatic Stale Detection

Trigger: same doctor command without `-ObservedSynapseSchemaStale`.

Before:

```json
{"phase":"before_auto_stale_detection","latest_handoff":"C:\\Users\\hotra\\AppData\\Local\\synapse\\codex-restart-handoffs\\codex-restart-handoff-58888-20260718T151816169Z.json","env_process_start_hash":"d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb","report_count_before":0}
```

After:

```json
{"phase":"after_auto_stale_detection","exit_code":0,"new_handoff_written":true,"latest_handoff":"C:\\Users\\hotra\\AppData\\Local\\synapse\\codex-restart-handoffs\\codex-restart-handoff-58888-20260718T152226521Z.json","report_status":"handoff_written","reason_code":"SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE","observed_schema_stale":false,"schema_stale_proven":true,"live_hash":"98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e","process_start_hash":"d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb","host_snapshot_hash":"b83c1d7204a53267e0b2a03d1330db875af55e8e0e88bc8a8e0c03060207f207","handoff_reason":"current_process_start_hash_mismatch","handoff_daemon_pid":43756,"handoff_daemon_hash":"98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e"}
```

Final telemetry after automatic detection:

```text
latest_restart_handoff.path=C:\Users\hotra\AppData\Local\synapse\codex-restart-handoffs\codex-restart-handoff-58888-20260718T152226521Z.json
latest_restart_handoff.daemon_pid_matches_live_daemon=true
latest_restart_handoff.active_issue_ref=#1740
latest_restart_handoff.current_process_start_tool_surface_sha256=d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb
latest_restart_handoff.daemon_tool_surface_sha256=98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e
codex_client_surface.status=restart_required_for_live_codex_pid
```

## Edge Case 1 - Invalid Issue Reference

Before:

```json
{"phase":"before_edge_invalid_issue","latest_handoff":"C:\\Users\\hotra\\AppData\\Local\\synapse\\codex-restart-handoffs\\codex-restart-handoff-58888-20260718T151816169Z.json","report_count_before":0}
```

Trigger: `-ActiveIssue not-an-issue`

After:

```json
{"phase":"after_edge_invalid_issue","exit_code":1,"handoff_unchanged":true,"report_status":"failed","reason_code":"SYNAPSE_ACTIVE_ISSUE_INVALID","message":"SYNAPSE_ACTIVE_ISSUE_INVALID value=not-an-issue remediation=pass an issue number like 1715, #1715, or https://github.com/ChrisRoyse/Synapse/issues/1715"}
```

## Edge Case 2 - Missing Snapshot

Before:

```json
{"phase":"before_edge_missing_snapshot","missing_snapshot_exists":false,"latest_handoff":"C:\\Users\\hotra\\AppData\\Local\\synapse\\codex-restart-handoffs\\codex-restart-handoff-58888-20260718T151816169Z.json","report_count_before":0}
```

Trigger: `-ToolSurfaceSnapshotPath <missing file>`

After:

```json
{"phase":"after_edge_missing_snapshot","exit_code":1,"handoff_unchanged":true,"report_status":"failed","reason_code":"SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_MISSING","snapshot_exists":false,"snapshot_reason":"missing","message":"Codex tool-surface snapshot missing: C:\\Users\\hotra\\AppData\\Local\\synapse\\fsv\\issue-1740\\edge-missing-snapshot\\missing-codex-tool-surface.json"}
```

## Edge Case 3 - Invalid Snapshot JSON

Before:

```json
{"phase":"before_edge_invalid_snapshot","bad_snapshot":"C:\\Users\\hotra\\AppData\\Local\\synapse\\fsv\\issue-1740\\edge-invalid-snapshot\\bad-codex-tool-surface.json","bad_snapshot_length":39,"latest_handoff":"C:\\Users\\hotra\\AppData\\Local\\synapse\\codex-restart-handoffs\\codex-restart-handoff-58888-20260718T151816169Z.json","report_count_before":0}
```

Trigger: malformed JSON snapshot.

After:

```json
{"phase":"after_edge_invalid_snapshot","exit_code":1,"handoff_unchanged":true,"report_status":"failed","reason_code":"SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_INVALID","snapshot_exists":true,"snapshot_readable":false,"snapshot_valid":false,"snapshot_reason":"unreadable_or_invalid_json","message":"Codex tool-surface snapshot is unreadable or lacks tool_count/tool_surface_sha256: C:\\Users\\hotra\\AppData\\Local\\synapse\\fsv\\issue-1740\\edge-invalid-snapshot\\bad-codex-tool-surface.json"}
```

## Edge Case 4 - False Positive Stale Claim

Setup: temp snapshot used real 40 tool names and the real live session hash
`98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e`; the
doctor child env set the process-start hash/count/snapshot to the same values.

Before:

```json
{"phase":"before_edge_false_positive","matching_snapshot":"C:\\Users\\hotra\\AppData\\Local\\synapse\\fsv\\issue-1740\\edge-false-positive\\matching-codex-tool-surface.json","matching_hash":"98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e","env_hash_before":"d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb","latest_handoff":"C:\\Users\\hotra\\AppData\\Local\\synapse\\codex-restart-handoffs\\codex-restart-handoff-58888-20260718T151816169Z.json","report_count_before":0}
```

Trigger: `-ObservedSynapseSchemaStale` with matching process/start/host/live
hashes.

After:

```json
{"phase":"after_edge_false_positive","exit_code":1,"handoff_unchanged":true,"report_status":"failed","reason_code":"SYNAPSE_CODEX_SCHEMA_STALE_NOT_REPRODUCED","schema_stale_proven":false,"process_matches_live":true,"host_matches_live":true,"message":"ObservedSynapseSchemaStale was supplied, but process-start, host snapshot, and live daemon tool-surface hashes did not prove schema drift"}
```

## Edge Case 5 - Missing Daemon

Before:

```json
{"phase":"before_edge_missing_daemon","listener_count":0,"latest_handoff":"C:\\Users\\hotra\\AppData\\Local\\synapse\\codex-restart-handoffs\\codex-restart-handoff-58888-20260718T152226521Z.json","report_count_before":0}
```

Trigger: doctor with no daemon/listener.

After:

```json
{"phase":"after_edge_missing_daemon","exit_code":1,"listener_count":0,"handoff_unchanged":true,"report_status":"failed","reason_code":"SYNAPSE_DAEMON_BIND_ABSENT","tcp_listener_count":0,"message":"no listener found at 127.0.0.1:7700"}
```

## Final Host Hygiene

The FSV daemon PID `43756` was stopped after manual verification.

```text
synapse-mcp.exe process table: empty
127.0.0.1:7700 Listen rows: empty
latest handoff: C:\Users\hotra\AppData\Local\synapse\codex-restart-handoffs\codex-restart-handoff-58888-20260718T152226521Z.json
latest handoff reason_code: SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE
latest handoff daemon hash: 98f027b17f5ee8fc030365cc2427cfb579440552043d148d2ce078496c17286e
```
