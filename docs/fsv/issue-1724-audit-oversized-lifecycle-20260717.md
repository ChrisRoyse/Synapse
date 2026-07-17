# Issue #1724 FSV - audit lifecycle oversized rows - 2026-07-17

## Scope

Issue #1724: `audit operation=lifecycle_events` failed closed when a matching daemon lifecycle JSONL row exceeded the caller's `max_line_bytes`.

Root invariant: audit reads must preserve availability of bounded evidence. A matching oversized row must not block the whole query when the row is valid JSON and can be represented as sanitized metadata. Corrupt or unreadable JSONL still fails closed.

## Root Cause

`read_lifecycle_tail()` treated a matching row larger than `max_line_bytes` as `STORAGE_READ_FAILED`. It parsed just enough of the oversized row to know it matched the filters, then returned an error instead of returning the same metadata-only summary already used for normal rows.

That made the audit Source of Truth unavailable exactly when manual FSV needed it. The physical row remained valid JSONL; only the reader policy was wrong.

## Research Used

Research was done with Exa MCP and native web research after isolating the bug.

- JSON Lines: https://jsonlines.org/
- OWASP Logging Cheat Sheet: https://cheatsheetseries.owasp.org/cheatsheets/Logging_Cheat_Sheet.html
- NIST SP 800-92 overview: https://csrc.nist.gov/pubs/sp/800/92/final

Applied conclusions:

- JSONL is record-oriented: each line is an independent JSON value and works well for log files.
- Log viewers should expose enough structured metadata for analysis but exclude, mask, sanitize, hash, or encrypt sensitive values.
- Log management has to preserve confidentiality, integrity, and availability. A single large but valid record should not make unrelated bounded audit reads unavailable.

## Fix

- Matching oversized lifecycle rows are parsed as JSON and summarized through the same sanitized `AuditLifecycleRowSummary` path as normal rows.
- Returned oversized rows carry `oversized=true`, `raw_len_bytes`, `raw_sha256`, line number, tool/status/error metadata, session-id hash, and timing fields.
- The response now includes `oversized_lines_returned` alongside `oversized_lines_seen` and `oversized_lines_skipped`.
- Nonmatching oversized rows are still skipped by filter and counted.
- Corrupt oversized rows still fail closed with `STORAGE_READ_FAILED`; the fix does not hide invalid bytes.

No raw oversized row payload is returned.

## Sources Of Truth

- Runtime process/socket: Windows process table and `Get-NetTCPConnection` for `127.0.0.1:7700`.
- Real MCP trigger: `mcp__synapse.audit operation=lifecycle_events`.
- Audit ledger bytes: `C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-tool-events.jsonl`.
- Setup/install evidence: `scripts\synapse-setup.ps1`, `C:\Users\hotra\AppData\Local\synapse\logs\setup-build.log`, setup diagnostics, installed binary hash.

## Before State

Original real MCP trigger from #1724:

```json
{"operation":"lifecycle_events","lifecycle_events":{"tool":"browser_tabs","limit":5,"max_line_bytes":2000}}
```

Before the fix, that failed through `mcp__synapse.audit` with:

```text
code=STORAGE_READ_FAILED
reason=oversized_row
source_id=C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-tool-events.jsonl
line_no=356
line_bytes=5325
max_line_bytes=2000
row_tool=browser_tabs
row_status=error
row_event_kind=tool_call
```

Separate physical SoT read of the offending row after the fix still found the same ledger row:

```text
path=C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-tool-events.jsonl
line_no=356
len=5325
sha256=237174e9de5a39547f34aa1fe52408f39efd0c2c23033a138f291c2a8fef6c22
event_kind=tool_call
tool=browser_tabs
status=error
error_code=A11Y_CDP_EXTENSION_UNAVAILABLE
```

## Installation Readback

First setup attempt hit a transient release compiler/linker access violation after producing an exclusive-open artifact:

```text
SYNAPSE_RELEASE_BUILD_COMPILER_FAILED
rustc exit code: 0xc0000005 STATUS_ACCESS_VIOLATION
artifact_exists=true
artifact_sha256=4233B90A8FD434822D5C3FCA35536729EC9F9E2D594961D807D5CDF675B8970F
artifact_exclusive_open=ok
```

The standard setup path was retried. The retry built, candidate-validated, and installed the daemon:

```text
installed daemon pid=29796
binary=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
binary_sha256=5BA2E630307C895EF9BB473048D644714139576F0D0C15297AE485D30CC6A747
bind=127.0.0.1:7700
tool_count=40
tool_surface_sha256=d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb
chrome_bridge.status=ok
```

Setup then failed closed with the expected current-process schema-stale handoff because this already-running Codex process started before the audit output schema changed:

```text
SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE
input_schema_changed=episode
output_schema_changed=audit,episode
daemon_pid=29796
```

The audit input schema remained stable, and the real wired `mcp__synapse.audit` tool call below executed through the configured connector against daemon PID `29796`. A fresh Codex process should read the handoff under `%LOCALAPPDATA%\synapse\codex-restart-handoffs`.

## Manual FSV

### Happy Path - Original #1724 Trigger No Longer Fails

Before:

```text
process/socket: PID 29796 listening on 127.0.0.1:7700
mcp__synapse.health: ok=true, tool_count=40, tool_surface_sha256=d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb
ledger line 356: browser_tabs error row, len=5325, sha256=237174e9de5a39547f34aa1fe52408f39efd0c2c23033a138f291c2a8fef6c22
```

Trigger:

```json
{"operation":"lifecycle_events","lifecycle_events":{"tool":"browser_tabs","limit":5,"max_line_bytes":2000}}
```

After:

```text
returned_count=5
matched_lines_seen=90
oversized_lines_seen=42
oversized_lines_skipped=38
oversized_lines_returned=4
readback_source_of_truth=C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-tool-events.jsonl lines_read=732 returned_count=5
```

Verdict: the original bounded read returns sanitized lifecycle metadata instead of failing on line 356.

### Edge 1 - Matching Oversized Row Returned

Trigger:

```json
{"operation":"lifecycle_events","lifecycle_events":{"tool":"browser_tabs","status":"error","limit":20,"max_line_bytes":2000}}
```

After:

```text
returned_count=9
oversized_lines_returned=4
line 661 oversized=true raw_len_bytes=2086 raw_sha256=sha256:17394c3a52dd87da4ce1695f87eea442d3e72476ba2c0c599bdab533a79913d7 error_code=ACTION_TARGET_INVALID
line 403 oversized=true raw_len_bytes=2031 raw_sha256=sha256:0571f25942dd1f323c163e89f2a6a1f607f562499780f60aa3c7a57ab982edd4 error_code=ACTION_TARGET_INVALID
line 359 oversized=true raw_len_bytes=5056 raw_sha256=sha256:24c27aaf604f5b4643a624f32e3308936988a7e3ea5c6fd66c6806d796123faf error_code=A11Y_CDP_EXTENSION_UNAVAILABLE
line 356 oversized=true raw_len_bytes=5325 raw_sha256=sha256:237174e9de5a39547f34aa1fe52408f39efd0c2c23033a138f291c2a8fef6c22 error_code=A11Y_CDP_EXTENSION_UNAVAILABLE
```

Separate ledger read for line 356 matched the returned length/hash/tool/status/error. No raw row payload was returned by the MCP response.

### Edge 2 - Nonmatching Oversized Rows Skipped

Trigger:

```json
{"operation":"lifecycle_events","lifecycle_events":{"tool":"no_such_tool_for_1724","limit":5,"max_line_bytes":2000}}
```

After:

```text
returned_count=0
matched_lines_seen=0
oversized_lines_seen=42
oversized_lines_skipped=42
oversized_lines_returned=0
rows=[]
```

Verdict: oversized rows that do not match the requested filters do not fail the query and are explicitly counted as skipped.

### Edge 3 - Invalid Bounds Still Fail Closed

Trigger:

```json
{"operation":"lifecycle_events","lifecycle_events":{"tool":"browser_tabs","limit":5,"max_line_bytes":0}}
```

After:

```text
MCP error -32602
code=TOOL_PARAMS_INVALID
source_id=max_line_bytes
message=max_line_bytes must be 1..=524288
```

Verdict: the fix does not introduce a permissive fallback for invalid caller bounds.

### Edge 4 - Different Matching Tool Still Bounded

Trigger:

```json
{"operation":"lifecycle_events","lifecycle_events":{"tool":"browser_nav","limit":5,"max_line_bytes":2000}}
```

After:

```text
returned_count=5
matched_lines_seen=38
oversized_lines_seen=42
oversized_lines_skipped=41
oversized_lines_returned=1
rows include only sanitized browser_nav summaries with raw_len_bytes/raw_sha256/status/error metadata
```

Verdict: the oversized metadata path is generic and filter-respecting, not special-cased to `browser_tabs`.

## Structural Checks

These are structural gates only, not FSV:

```text
cargo fmt --all --check
cargo check
cargo clippy --workspace --all-targets
git diff --check
```

`cargo clippy --workspace --all-targets` exited 0 with pre-existing warning-only findings in `synapse-storage` and `dump_cf`; no clippy failure remained in this audit/browser diff.

## Follow-Up

The first setup attempt's release compiler access violation did not repeat and setup recovered with a retry. Tracked separately as #1726.

## Verdict

#1724 is fixed and manually FSV-verified against the installed repo-built daemon, real `mcp__synapse.audit` trigger, physical process/socket SoT, and separate JSONL byte-level ledger readback. No automated tests, FSV scripts, FSV harnesses, or GitHub Actions were added or used.
