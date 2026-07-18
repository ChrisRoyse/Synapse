# Issue #1737 FSV - Reflex Scheduler Tick-Late Classification

Date: 2026-07-18
Issue: https://github.com/ChrisRoyse/Synapse/issues/1737

## Root Cause

`record_tick_sample` wrote a durable `CF_REFLEX_AUDIT` row with
`REFLEX_TICK_LATE` for any single scheduler tick whose elapsed time exceeded
`late_after`. `last_tick_late_signal` only deduplicated continuous episodes and
reset after an on-time tick, so normal Windows/runtime jitter could repeatedly
create active scheduler audit rows during ordinary verification workload.

That made telemetry noise look like actionable reflex failure. The scheduler
already records jitter samples and the `REFLEX_TICK_JITTER_METRIC`, so isolated
non-degraded deadline misses belong in telemetry. Durable audit rows should be
reserved for actionable cases: dispatch blockage, degraded fallback execution,
severe non-degraded lateness, or sustained consecutive misses.

## Research Used

Exa MCP and native web research were both used after local RCA. Primary sources:

- Google SRE, Monitoring Distributed Systems:
  https://sre.google/sre-book/monitoring-distributed-systems/
- Google SRE Workbook, Alerting on SLOs:
  https://sre.google/workbook/alerting-on-slos/
- Microsoft Learn, About Timers:
  https://learn.microsoft.com/en-us/windows/win32/winmsg/about-timers
- OpenTelemetry Metrics Data Model:
  https://opentelemetry.io/docs/specs/otel/metrics/data-model/
- Microsoft Azure Well-Architected Observability:
  https://learn.microsoft.com/en-us/azure/well-architected/operational-excellence/observability

The fix follows those principles: keep high-frequency scheduler jitter as
bounded samples/metrics/logs, make durable audit rows classified and actionable,
and account for timer imprecision instead of treating every approximate timer
wakeup as an error.

## Implementation

- Added `SchedulerConfig::deadline_miss_audit_after` defaulting to `3`.
- Added `SchedulerConfig::severe_deadline_miss_after` defaulting to `8 ms`.
- Added validation for zero audit streak and severe threshold not exceeding
  `late_after`.
- Added `TickSample::deadline_miss_streak`.
- Added runtime/health readbacks for `deadline_miss_streak`,
  `deadline_miss_audit_after`, and `severe_deadline_miss_after_us`.
- Changed durable `REFLEX_TICK_LATE` emission to require one of:
  `dispatch_blocked`, `degraded_deadline_miss`, `severe_deadline_miss`, or
  `sustained_deadline_miss`.
- Added classified audit/event details:
  `classification`, `deadline_miss_streak`, `deadline_miss_audit_after`, and
  `severe_deadline_miss_after_us`.
- Logged isolated non-degraded deadline misses as
  `REFLEX_TICK_JITTER_SAMPLE` debug telemetry instead of durable audit rows.

## Structural Checks

Supporting checks only; these are not FSV:

- `cargo fmt --all --check` passed.
- `cargo check -p synapse-core -p synapse-reflex -p synapse-mcp` passed.
- `cargo clippy --workspace --all-targets` passed.
- `git diff --check` passed.

## Source of Truth

Behavior SoT:

- Daemon process and socket: `synapse-mcp.exe`, `127.0.0.1:7700`.
- MCP client parity: fresh `codex exec` strict client initialized the configured
  `mcp__synapse` server and loaded `tools/list`.
- Storage SoT: Calyx `CF_REFLEX_AUDIT`, read through real
  `mcp__synapse.storage operation=summary/inspect`.
- Reflex state SoT: real `mcp__synapse.routine reflex_list` and
  `reflex_history`.
- Health SoT: real `mcp__synapse.health detail=compact`.

Current chat Codex PID `58888` had stale callable schemas, so FSV triggers used
fresh strict Codex clients. Setup wrote handoffs under
`C:\Users\hotra\AppData\Local\synapse\codex-restart-handoffs` and failed closed
with `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE`, while the fresh clients
loaded the new 40-tool MCP surface successfully.

## Manual FSV Evidence

### Daemon Precondition

Before write-enabled FSV:

- Process PID `32660`, path `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`.
- Command line included:
  `--mode http --bind 127.0.0.1:7700 --db C:\Users\hotra\AppData\Local\synapse\db-daemon --allowed-permissions READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_REFLEX`.
- Socket readback showed PID `32660` owning `127.0.0.1:7700`.
- Fresh strict MCP client health read:
  `health_ok=true`, `tool_count=40`,
  `tool_surface_sha256=a7fd582574fda661a7a3393ef6724a0e1bb730198dbcc3e7083784476a9da538`.
- Profile read:
  `effective_grant_names=["READ_EVENTS","WRITE_REFLEX","READ_REFLEX","READ_PROFILE","READ_STORAGE"]`.

Baseline storage/readback:

- `CF_REFLEX_AUDIT=41`.
- `reflex_list include_expired=false` returned `[]`.
- Latest scheduler baseline row:
  `audit_id=019f736f-d821-7781-93b8-e041c146a11d`,
  `reflex_id=__scheduler__`, `error_code=REFLEX_TICK_LATE`,
  `ts_ns=1784348137505110500`, `elapsed_us=2224`,
  `classification=null`. This was an old row from before the fix.

### Happy Path: Register Synthetic Reflex

Trigger:

`mcp__synapse.routine reflex_register` with:

```json
{
  "kind": "on_event",
  "when": { "op": "none" },
  "then": { "action": "audit/readback" },
  "priority": 1000,
  "lifetime": { "kind": "until_cancelled" },
  "debounce_ms": 0
}
```

Before/after SoT:

- Before `CF_REFLEX_AUDIT=41`.
- Registered reflex id:
  `019f73e3-0e84-7aa3-bdd1-63033db7571a`.
- `reflex_list include_expired=false` returned that id.
- `reflex_history` for the id returned one row:
  `audit_id=019f73e3-0e84-7aa3-bdd1-6329337d9f4f`,
  `status=active`, `error_code=null`,
  `ts_ns=1784355688068506700`.
- After `CF_REFLEX_AUDIT=42`; expected delta `+1`, actual delta `+1`.
- Health after trigger:
  `reflex_status=ok`, `deadline_miss_audit_after=3`,
  `severe_deadline_miss_after_us=8000`, `late_tick_count=0`.

### Normal Verification Workload Window

After the register path and ordinary verification workload:

- `CF_REFLEX_AUDIT=42`.
- Latest row remained the synthetic active row.
- No `__scheduler__` rows appeared after baseline row
  `019f736f-d821-7781-93b8-e041c146a11d`.
- Health read:
  `reflex_status=ok`, `deadline_miss_audit_after=3`,
  `severe_deadline_miss_after_us=8000`,
  `late_tick_count=0`, `degraded_tick_count=0`.

This directly verifies that normal jitter during verification no longer writes
durable scheduler audit rows.

### Edge A: Missing Reflex History Spec

Trigger:

`mcp__synapse.routine {"operation":"reflex_history"}`

Before/after SoT:

- Before `CF_REFLEX_AUDIT=42`.
- Error: `TOOL_PARAMS_INVALID`.
- Message:
  `routine operation=reflex_history requires a matching reflex_history spec`.
- After `CF_REFLEX_AUDIT=42`; expected delta `0`, actual delta `0`.

### Edge B: Boundary Invalid History Limit

Trigger:

`mcp__synapse.routine {"operation":"reflex_history","reflex_history":{"limit":1001}}`

Before/after SoT:

- Before `CF_REFLEX_AUDIT=42`.
- Error: `TOOL_PARAMS_INVALID`.
- Message: `reflex_history limit must be <= 1000`.
- After `CF_REFLEX_AUDIT=42`; expected delta `0`, actual delta `0`.

### Edge C: Structurally Invalid Blank Cancel Id

Trigger:

`mcp__synapse.routine {"operation":"reflex_cancel","reflex_cancel":{"reflex_id":"   "}}`

Before/after SoT:

- Before `CF_REFLEX_AUDIT=42`.
- Error: `TOOL_PARAMS_INVALID`.
- Message: `reflex_cancel reflex_id must not be empty`.
- After `CF_REFLEX_AUDIT=42`; expected delta `0`, actual delta `0`.
- `reflex_list include_expired=false` still returned only
  `019f73e3-0e84-7aa3-bdd1-63033db7571a`.

### Cleanup Path: Cancel Synthetic Reflex

Trigger:

`mcp__synapse.routine reflex_cancel` for
`019f73e3-0e84-7aa3-bdd1-63033db7571a`.

Before/after SoT:

- Before `CF_REFLEX_AUDIT=42`.
- `reflex_list include_expired=false` after cancel returned `[]`.
- `reflex_history` for the id returned exactly two rows:
  - `audit_id=019f73ed-70e8-7472-a3dd-dc16a5f7744b`,
    `status=cancelled`, `ts_ns=1784356368616676100`.
  - `audit_id=019f73e3-0e84-7aa3-bdd1-6329337d9f4f`,
    `status=active`, `ts_ns=1784355688068506700`.
- After `CF_REFLEX_AUDIT=43`; expected delta `+1`, actual delta `+1`.
- Health read:
  `reflex_status=ok`, `deadline_miss_audit_after=3`,
  `severe_deadline_miss_after_us=8000`,
  `late_tick_count=0`, `degraded_tick_count=0`.

### Final Scheduler Read Before Permission Restore

- `CF_REFLEX_AUDIT=43`.
- Active reflex list: `[]`.
- Latest five audit rows were the synthetic cancelled row, synthetic active row,
  then old pre-fix scheduler rows.
- No `__scheduler__` rows appeared after baseline row
  `019f736f-d821-7781-93b8-e041c146a11d`.
- Health read:
  `reflex_status=ok`, `deadline_miss_audit_after=3`,
  `severe_deadline_miss_after_us=8000`,
  `late_tick_count=0`, `degraded_tick_count=0`.

### Daemon Restore / Host Hygiene

Setup command:

```powershell
pwsh -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1 -SourceDir 'C:\code\Synapse' -ForceRestart -ActiveIssue '#1737' -InstallHealthTimeoutSeconds 900
```

Setup installed the repo-built daemon and intentionally exited nonzero only at
the stale-current-Codex guard:

- Installed binary hash:
  `BE145A3A481188BCCEE94F8A338AF9BA6B6C205E64F7C28A6877B4A846C70000`.
- New daemon PID `79364`.
- New command line has no `--allowed-permissions` argument:
  `--mode http --bind 127.0.0.1:7700 --db C:\Users\hotra\AppData\Local\synapse\db-daemon --profile-dir C:\Users\hotra\.cargo\bin\profiles --log-level info`.
- Fresh strict MCP client read:
  `effective_grant_names=["READ_EVENTS","READ_REFLEX","READ_PROFILE","READ_STORAGE"]`.
- Grant source:
  `fail-closed read-only default (#1539; no SYNAPSE_MCP_ALLOWED_PERMISSIONS / --allowed-permissions set)`.
- `CF_REFLEX_AUDIT=43`, active reflex list `[]`.

Post-restore write-denied edge with a valid reflex payload:

- Before `CF_REFLEX_AUDIT=43`.
- Error: `SAFETY_PERMISSION_DENIED`.
- Message: `tool reflex_register requires permission WRITE_REFLEX`.
- After `CF_REFLEX_AUDIT=43`; expected delta `0`, actual delta `0`.
- Active reflex list remained `[]`.

Direct authenticated `/health` retry after restore also returned
`ok=true`, PID `79364`.

## Verdict

Issue #1737 is fixed at the root: isolated non-degraded scheduler deadline
misses no longer create durable `REFLEX_TICK_LATE` audit rows during normal
verification workload. The durable path now carries explicit classification and
threshold evidence when it does fire, while health exposes the configured
thresholds and current streak for diagnosis.
