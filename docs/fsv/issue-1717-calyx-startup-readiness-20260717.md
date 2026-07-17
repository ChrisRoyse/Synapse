# Manual FSV Closeout: Issue #1717 Calyx Startup Readiness

Date: 2026-07-17

Issue: https://github.com/ChrisRoyse/Synapse/issues/1717

## Result

Accepted. Synapse now keeps Calyx startup/request readiness separate from
scan-bound maintenance. The daemon still fails closed when storage, vault open,
socket bind, or maintenance task startup fails, but strict MCP clients no longer
inherit large-CF GC/retention scans during startup or HTTP session creation.

No automated tests, FSV harnesses, benchmarks, mocks, CI, or GitHub Actions were
created or run. The commands listed here are structural checks or manual
Source-of-Truth readbacks only.

## Root Cause

The first failure exposed by #1716 was startup GC: the HTTP server did not
become reachable until after eager storage maintenance had run against a copied
Calyx vault with large column families. Moving the first periodic maintenance
tick off startup exposed the deeper request-path root cause:

- the strict `codex-mcp-client` initialized successfully;
- the MCP HTTP session store wrote one session row to `CF_KV`;
- `CalyxBackend::put_batch_pressure_bypass` performed synchronous retention
  preflight/enforcement around the foreground write;
- the copied `CF_KV` had `531526` rows and about `107 MB` live data;
- the single session write triggered a hard-cap retention sweep and two
  chunked tombstone commits totaling `524367` rows;
- the strict client timed out after 30 seconds before session persistence
  completed.

Failure evidence root:

```text
C:\Users\hotra\AppData\Local\synapse\fsv\issue-1717\startup-readiness-delayed-20260716T235305334Z
pid=87592
bind=127.0.0.1:7784
client_error=timed out handshaking with MCP server after 29.9999999s
```

Daemon log evidence from that failed run:

```text
23:57:01 Service initialized as server client_info.name="codex-mcp-client"
23:57:05 STORAGE_CF_HARD_CAP_REACHED cf="CF_KV" before_live_bytes=107133561 soft_cap_bytes=10485760 hard_cap_bytes=52428800
23:57:36 STORAGE_CALYX_WRITE_BATCH_CHUNK_COMMITTED cf="CF_KV" chunk_rows=406220 chunk_payload_bytes=66060149
23:57:45 STORAGE_CALYX_WRITE_BATCH_CHUNK_COMMITTED cf="CF_KV" chunk_rows=118147 chunk_payload_bytes=19524218
23:57:45 STORAGE_CALYX_WRITE_BATCH_CHUNKED cf="CF_KV" total_rows=524367 total_payload_bytes=85584363
23:57:48 MCP_HTTP_SESSION_STORE_WRITE session_id="972a4e29-bbe3-46bd-8619-d3a61a2337f3"
```

The fix is to make foreground Calyx writes bounded and direct: validate the CF,
encode rows, plan bounded WAL chunks, commit, and fail closed on real write
errors. Retention scans and cap eviction now remain GC work instead of running
inside latency-sensitive MCP request/session writes. Startup still validates
storage and the Calyx vault before serving, starts maintenance after HTTP
readiness prerequisites, and delays the first periodic GC/pressure tick.

Health also no longer asks Calyx for scan-bound CF size estimates on the hot
health path. It reports that the size readback was intentionally skipped and
exposes whether GC or pressure maintenance is actively running.

## Research Used

Research was done after identifying the root cause, using Exa MCP and native web
research against primary sources:

- Bigtable garbage collection docs:
  https://docs.cloud.google.com/bigtable/docs/garbage-collection
- RocksDB write stalls docs:
  https://github.com/facebook/rocksdb/wiki/Write-Stalls
- Kubernetes liveness/readiness/startup probe docs:
  https://kubernetes.io/docs/concepts/workloads/pods/probes/

Takeaways applied here:

- GC/retention is background maintenance; clients should not depend on
  immediate collection on the foreground request path.
- Latency-sensitive writes must not inherit compaction/maintenance stalls.
- Readiness should be cheap, explicit, and separate from long-running startup
  or maintenance work.

## Source Of Truth

Normal configured daemon precondition:

```text
pid=64508
bind=127.0.0.1:7700
exe=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
storage_backend=rocksdb
wired_client_health=ok
tool_count=40
```

Issue-specific FSV root:

```text
C:\Users\hotra\AppData\Local\synapse\fsv\issue-1717\startup-readiness-final-20260717T000357102Z
```

Issue-specific daemon:

```text
pid=62536
bind=127.0.0.1:7784
exe=C:\code\Synapse\target\debug\synapse-mcp.exe
dev_binary_sha256=65780D255A7C1FFC7EA1A2080975BA2BED263DF5F191FCC61020086180CB52D7
db=C:\Users\hotra\AppData\Local\synapse\fsv\issue-1717\startup-readiness-final-20260717T000357102Z\target-calyx-copy
storage_backend=calyx
```

Physical SoTs:

- process table and socket table for daemon PID/bind ownership;
- daemon log:
  `...\startup-readiness-final-20260717T000357102Z\daemon-7784.stderr.log`;
- strict-client transcripts:
  `strict-client-health-post-start.jsonl` and
  `strict-client-health-repeat.jsonl`;
- Calyx CF readback dumps:
  `before-cf-dump-summary.json` and `after-happy-cf-dump-summary.json`;
- edge readbacks under
  `edge-empty-20260717T001155310Z` and
  `edge-port-occupied-20260717T001420773Z`.

## MCP Preconditions

The real configured MCP client was checked before accepting the issue-specific
FSV. The wired `mcp__synapse.health` call returned:

```text
ok=true
pid=64508
bind=127.0.0.1:7700
tool_count=40
storage.status=ok
storage.storage_backend=rocksdb
```

The issue-specific trigger used fresh strict `codex exec` clients against
`http://127.0.0.1:7784/mcp`, authenticated with `SYNAPSE_BEARER_TOKEN`, loaded
the strict schema-validated tool surface, and called the real `health` tool.

## Before State

The target was a physical copy of the #1661 migrated Calyx vault:

```text
source=C:\Users\hotra\AppData\Local\synapse\fsv\issue-1661\happy-final\target-calyx
target=C:\Users\hotra\AppData\Local\synapse\fsv\issue-1717\startup-readiness-final-20260717T000357102Z\target-calyx-copy
copied_file_count=317
copied_total_bytes=2806986955
machine_salt_sha256=44E338D664E841C5385B7635606BAB9E95AA1BE7CDC8E002F9B73446C6285E06
before_listener=127.0.0.1:7700 pid=64508
```

Initial physical Calyx CF readback:

```text
CF_AGENT_TRANSCRIPTS rows=466546 sha256=9ACA160A6611881A7CE675E208F4D0CCC868C8F88F19AC223A7CE523494BCAFA bytes=171210059
CF_KV rows=531526 sha256=93CDF1253E2E5D983001B4854DACCD380B09C40F910E9D41FB801A9689E7CE1B bytes=201565230
CF_EVENTS rows=94 sha256=B36BCCD553F00CEF5CDB56BBEB4ABCFD2B30F802672E6E59D0DBB471D59B5BAB bytes=34331
```

Daemon startup log order:

```text
00:05:53 MCP_DAEMON_STORAGE_AND_CALYX_OPENED
00:05:58 MCP_DAEMON_STORAGE_MAINTENANCE_STARTED_AFTER_HTTP_READY_PREREQS
00:05:58 MCP_HTTP_STARTED bind=127.0.0.1:7784
```

There were no `STORAGE_CALYX_GC`, `STORAGE_CALYX_RETENTION`,
`STORAGE_CF_HARD_CAP_REACHED`, or `STORAGE_CALYX_WRITE_BATCH_CHUNK*` records
before HTTP readiness.

## Happy Path: Large Calyx Vault Strict Client

Trigger: strict `codex-mcp-client` MCP session plus `tools/call health` against
the issue-specific Calyx daemon.

Strict-client result:

```text
exit_code=0
elapsed_ms=34678.5
mcp_call_succeeded=true
pid=62536
ok=true
tool_count=40
tool_names_len=40
storage_status=ok
storage_backend=calyx
storage_gc_task_running=true
storage_gc_tick_active=false
storage_pressure_task_running=true
storage_pressure_probe_active=false
storage_pressure_probe_observed=true
storage_cf_sizes_skipped_reason="calyx backend health skips scan-bound CF size estimates; use storage summary/inspect for explicit storage readback"
```

Separate daemon log readback:

```text
00:06:43 Service initialized as server client_info.name="codex-mcp-client"
00:06:44 MCP_HTTP_SESSION_STORE_WRITE session_id="f7344276-2679-4cf8-a47f-bd371f9a9202"
00:07:33 MCP_HTTP_SESSION_STORE_DELETE session_id="f7344276-2679-4cf8-a47f-bd371f9a9202"
retention_events=0
chunk_events=0
gc_events=0
```

Verdict: PASS. The same strict client that previously timed out loaded the tool
surface and called health. The session write did not trigger foreground Calyx
retention or GC.

## Edge Case 1: Repeated Strict Client On Oversized CF_KV

Boundary condition: repeat the strict client handshake/tool call against the
same oversized `CF_KV` after one prior session had already created and deleted
session state.

Before:

```text
CF_KV initial rows=531526
first strict client had completed
daemon still pid=62536 bind=127.0.0.1:7784
```

Trigger: second strict `codex-mcp-client` MCP session plus `tools/call health`.

Result:

```text
exit_code=0
elapsed_ms=45011.5
mcp_call_succeeded=true
pid=62536
ok=true
tool_count=40
tool_names_len=40
storage_status=ok
storage_backend=calyx
storage_gc_task_running=true
storage_gc_tick_active=false
storage_pressure_probe_observed=true
```

Separate log readback:

```text
00:08:10 Service initialized as server client_info.name="codex-mcp-client"
00:08:11 MCP_HTTP_SESSION_STORE_WRITE session_id="eeac7d50-f87b-4a93-80b9-adbcbd0c5edd"
00:09:05 MCP_HTTP_SESSION_STORE_DELETE session_id="eeac7d50-f87b-4a93-80b9-adbcbd0c5edd"
retention_events=0
chunk_events=0
gc_events=0
session_store_events=4
```

After physical CF readback:

```text
CF_KV rows=531524 sha256=AB83DAD52B22603B2A999A3DAD6C0D5EBFBF7CE7878BD67B755198DD35961A97 bytes=201564494
CF_AGENT_TRANSCRIPTS rows=466546 sha256=9ACA160A6611881A7CE675E208F4D0CCC868C8F88F19AC223A7CE523494BCAFA bytes=171210059
```

The `CF_KV` row count changed by `-2`, not by the earlier failing run's
`-524367`. The daemon log showed two normal MCP session creates followed by two
session deletes, and an unrelated escalation retention prune removed two stale
terminal rows. No mass Calyx retention sweep ran on the session write path.

Verdict: PASS.

## Edge Case 2: Empty/New Calyx Vault

FSV root:

```text
C:\Users\hotra\AppData\Local\synapse\fsv\issue-1717\edge-empty-20260717T001155310Z
```

Before:

```text
target=...\edge-empty-20260717T001155310Z\empty-calyx
port=127.0.0.1:7785 free
```

Daemon:

```text
pid=75036
bind=127.0.0.1:7785
storage_backend=calyx
00:11:57 MCP_DAEMON_STORAGE_AND_CALYX_OPENED
00:11:57 MCP_DAEMON_STORAGE_MAINTENANCE_STARTED_AFTER_HTTP_READY_PREREQS
00:11:57 MCP_HTTP_STARTED
```

Trigger: strict `codex-mcp-client` MCP session plus `tools/call health`.

Result:

```text
exit_code=0
elapsed_ms=17831.1
mcp_call_succeeded=true
pid=75036
ok=true
tool_count=40
tool_names_len=40
storage_status=ok
storage_backend=calyx
storage_gc_task_running=true
storage_gc_tick_active=false
storage_pressure_probe_observed=true
```

After exact PID cleanup and separate CF readback:

```text
after_process_exists=false
after_socket_exists=false
CF_KV rows=0 sha256=6EF8BBAB8C728BAFA3143F5FDD735F5C837BC40FF9DC577FEAA3C9274CED1A3D
CF_SESSIONS rows=0 sha256=482E9E5359F3BDE94693BC586A702F02436E8467CB63B9289A87A6CFF06FA6D4
```

Verdict: PASS. Empty storage starts cleanly, strict MCP works, and session
cleanup leaves no persistent rows.

## Edge Case 3: Invalid Bearer Token

Trigger: direct `GET /health` with an invalid bearer token against the empty
Calyx daemon.

Before:

```text
session_or_store_events=2
```

After:

```text
status_code=401
body=HTTP_TOKEN_INVALID
elapsed_ms=41.6
session_or_store_events=2
session_or_store_event_delta=0
```

Verdict: PASS. Invalid auth fails closed and does not create MCP session/store
state.

## Edge Case 4: Occupied Port Fails Closed

FSV root:

```text
C:\Users\hotra\AppData\Local\synapse\fsv\issue-1717\edge-port-occupied-20260717T001420773Z
```

Before:

```text
127.0.0.1:7700 listener pid=64508
owner_path=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
```

Trigger: start a second repo debug daemon on occupied `127.0.0.1:7700`.

After:

```text
candidate_pid=32820
candidate_exit_code=1
after_listener=127.0.0.1:7700 pid=64508
normal_daemon_unchanged=true
stderr_contains_bind_error=true
```

Error:

```text
synapse-mcp error: bind HTTP MCP transport to 127.0.0.1:7700: Only one usage of each socket address (protocol/network address/port) is normally permitted. (os error 10048)
```

Verdict: PASS. Bind conflicts fail closed, with a concrete socket error, and
the existing daemon remains the physical listener.

## Final-Binary Post-Build Verification

After the final source/doc cleanup, `cargo build -p synapse-mcp` produced a new
debug binary hash:

```text
binary=C:\code\Synapse\target\debug\synapse-mcp.exe
binary_sha256=63FFA78020A33B8815410691559115F85DD68164BC2D55B09C2EAC4FA1D1F408
```

The final binary was re-run against the same large Calyx copy to ensure the
accepted behavior still held for the executable present at commit time.

FSV root:

```text
C:\Users\hotra\AppData\Local\synapse\fsv\issue-1717\startup-readiness-postbuild-20260717T002919105Z
```

Before:

```text
pid=86204
bind=127.0.0.1:7786
db=C:\Users\hotra\AppData\Local\synapse\fsv\issue-1717\startup-readiness-final-20260717T000357102Z\target-calyx-copy
shell_job_root=C:\Users\hotra\AppData\Local\synapse\fsv\issue-1717\startup-readiness-postbuild-20260717T002919105Z\shell-jobs
CF_KV rows=531524
```

Startup log order:

```text
00:29:49 MCP_DAEMON_STORAGE_AND_CALYX_OPENED
00:29:52 MCP_DAEMON_STORAGE_MAINTENANCE_STARTED_AFTER_HTTP_READY_PREREQS
00:29:52 MCP_HTTP_STARTED bind=127.0.0.1:7786
```

Strict-client trigger:

```text
codex-mcp-client through codex exec
server=http://127.0.0.1:7786/mcp
tool=health
detail=compact
```

Strict-client result:

```text
exit_code=0
elapsed_ms=37318.3
mcp_call_succeeded=true
pid=86204
ok=true
tool_count=40
tool_names_len=40
storage_status=ok
storage_backend=calyx
storage_gc_task_running=true
storage_gc_tick_active=false
storage_pressure_task_running=true
storage_pressure_probe_active=false
storage_pressure_probe_observed=true
storage_cf_sizes_skipped_reason="calyx backend health skips scan-bound CF size estimates; use storage summary/inspect for explicit storage readback"
```

Separate log and CF readbacks:

```text
retention_events=0
chunk_events=0
gc_events=0
session_store_writes=2
session_store_deletes=2
ESCALATION_ITEM_RETENTION_PRUNED deleted_rows=11
CF_KV rows=531513
after_dump_sha256=28FC3913BCA178437BF0E6209C5FC00333A0E8C7B95DEB10B1957EF84361F9A6
```

The row count changed by `-11` because the daemon's escalation retention worker
pruned eleven terminal rows (`ESCALATION_ITEM_RETENTION_PRUNED`). The two MCP
HTTP sessions were written and deleted normally. There were still no Calyx
foreground retention, hard-cap, chunk, or GC events on the handshake path.

Verdict: PASS for the final debug binary.

## Host Hygiene

The issue-specific daemon PID `62536` was stopped only after verifying its
process path, command line, and socket ownership. Final readback:

```text
after_process_exists=false
after_socket_exists=false
stopped_at_utc=2026-07-17T00:09:50.7200203Z
```

The empty-vault daemon PID `75036` was also stopped after the same exact-PID
verification. The configured daemon on `127.0.0.1:7700` remained running and
healthy.

The post-build daemon PID `86204` was stopped after verifying its process path,
command line, and socket ownership:

```text
after_process_exists=false
after_socket_exists=false
stopped_at_utc=2026-07-17T00:33:17.0150655Z
```

## Structural Checks

Structural checks are compile/lint/format checks only. They are not FSV.

```text
cargo fmt --all --check
git diff --check
cargo check -p synapse-storage
cargo check -p synapse-mcp
cargo build -p synapse-mcp
cargo clippy -p synapse-storage --all-targets
cargo clippy -p synapse-mcp --all-targets
cargo clippy --workspace --all-targets
```

All passed. The final `cargo build -p synapse-mcp` produced debug binary SHA256
`63FFA78020A33B8815410691559115F85DD68164BC2D55B09C2EAC4FA1D1F408`, which was
used by the post-build strict-client FSV above.
