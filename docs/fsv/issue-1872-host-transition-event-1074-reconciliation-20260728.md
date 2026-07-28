# FSV — issue #1872: planned host transition bricks the daemon

Host: `CABTOP`, Windows 11 Pro 10.0.26200. Date: 2026-07-28.
Related: #1856 (gates 3/4/5), #1869 (boot instance identity).

## 1. The physical failure, as found

A real planned restart had been executed through
`setup operation=host_transition action=execute` for #1856 gate 3. The host
rebooted correctly. The daemon then never came back.

Supervisor Source of Truth
`%LOCALAPPDATA%\synapse\logs\daemon-supervisor-current.json`:

```json
{ "updated_utc": "2026-07-28T13:08:40.4878039Z",
  "state": "fatal", "generation": 5, "child_pid": null, "exit_code": 1,
  "message": "SYNAPSE_DAEMON_CRASH_LOOP rapid_failures=5 window_seconds=60" }
```

Each of the 5 generations died the same way
(`daemon-stderr-gen5-20260728080839.log`):

```
ERROR refusing to start: pending planned host transition did not reconcile
  code="MCP_DAEMON_STARTUP_HOST_TRANSITION_RECONCILIATION_FAILED"
  detail="matching planned-transition event is not EventID 1074"
  detail_code="HOST_TRANSITION_EVENT_ID_MISMATCH"
```

Independent confirmation that the binary — not the environment — was the cause:
`synapse-setup.ps1 -SkipBuild` ran the *installed* daemon as a candidate against
an isolated DB and port and it failed health identically
(`SYNAPSE_CANDIDATE_HEALTH_FAILED … 127.0.0.1:59205`), because the
host-transition state root is machine-global.

## 2. Root cause 1 — the EventID guard can never match

`find_event_1074` queried `wevtutil qe System /q:*[System[(EventID=1074)]]`
and then asserted `event.contains("<EventID>1074</EventID>")`.

Event 1074 is written by **`User32`, a classic (`.mc`-defined) provider**.
Windows renders classic events with a `Qualifiers` attribute. Read directly off
this host, outside Synapse:

```
$x = wevtutil qe System "/q:*[System[(EventID=1074)]]" /rd:true /c:2 /f:xml
literal '<EventID>1074</EventID>' present? -> False
EventID as actually emitted -> <EventID Qualifiers='32768'>1074</EventID>
intent marker present?      -> True
```

So the guard rejected the exact record the query had filtered *for*, 100% of the
time, for every planned restart or poweroff.

Research lane: Exa re-probed and still `402 EXA_API_CREDITS_EXHAUSTED`
(unchanged known state). Built-in web lane: Microsoft "Publishing Your Event
Schema for a Classic Provider" and the libyal EVTX documentation both state that
classic providers carry the message-id split as `EventID` + `Qualifiers`
attribute, i.e. an `EventID` element with attributes is the normal rendering and
must never be matched as a bare literal.

## 3. Root cause 2 — an audit gap wired to daemon startup

`reconcile_pending_intent_on_startup()` returning `Err` made both transports
`return Ok(ExitCode::from(4))`. The failure is deterministic, so the supervisor
crash-looped and latched `fatal`. There was no automatic recovery, and the
error's own remediation text pointed at `setup` — the tool the crash loop
removes.

The gate was also in the wrong place. Reconciliation is an observation about a
transition that already happened; the kernel boot identity independently proves
the host rebooted. The genuinely unsafe act under an unattributed transition is
performing **another** transition.

## 4. Fix

1. `find_event_1074` now walks each `<Event>…</Event>` block, requires the intent
   marker to appear inside that event's own `EventData`, and reads `EventID`
   attribute-aware. Zero matches and multiple matches are distinct, loud errors.
   Provider and `TimeCreated` are captured as corroborating witnesses.
2. The fail-closed boundary moved. An attribution failure persists
   `status="reconciliation_failed"` plus the exact `reconciliation_error` into
   the intent record, logs `SETUP_HOST_TRANSITION_RECONCILIATION_FAILED` at
   ERROR, is surfaced by `action=status`, and makes `preflight`/`execute` refuse
   with `HOST_TRANSITION_PRIOR_INTENT_UNRECONCILED`. It is retried on every
   attempt, so a transient event-log condition heals instead of latching.

## 5. Verification

Deployed through `scripts\synapse-setup.ps1 -SourceDir C:\code\synapse` (exit 0).
Installed daemon `sha256=76C5EAD8A8A319E01B23E3F5D72BABE6E16B1C5CB393C9694391DFF60558D864`.
All triggers below went through the real wired MCP HTTP transport
(`initialize` → `notifications/initialized` → `tools/call` against
`http://127.0.0.1:7700/mcp`), and every result was read back independently from
the file on disk, never from the tool's return value.

### 5.1 Happy path — real intent, real event, predicted before running

The expected values were derived from `wevtutil` **before** the fixed build ran,
so this is a prediction, not a description:

| field | predicted | on disk after |
| --- | --- | --- |
| `status` | `reconciled_after_boot` | `reconciled_after_boot` |
| `event_1074_record_id` | `123110` | `123110` |
| `event_1074_provider` | `User32` | `User32` |
| `event_1074_time_created_utc` | `2026-07-28T11:04:34.4947131Z` | `2026-07-28T11:04:34.4947131Z` |
| `reconciled_host_boot_id` | `winboot:…:63` | `winboot:…:63` |

The first daemon to do this was setup's own **candidate health probe** — i.e. the
same gate that had failed with `SYNAPSE_CANDIDATE_HEALTH_FAILED` on the old
binary now passes, on an isolated DB and port.

Cryptographic grounding: the recorded
`event_1074_sha256 = sha256:35efc890224c1233250f7d5149dbb9a0150a5cc5d3b19c8c8f20f80c0bc64ef1`
was recomputed independently, outside Synapse, by re-querying `wevtutil`, slicing
that one `<Event>` block and hashing it. Byte-identical, and its
`<EventRecordID>` reads `123110`.

### 5.2 Edge case — a transition that cannot be attributed

Synthetic intent id `intent-1785000000000-fsv1872edge1neverissuedaaaaaaaa`,
proven absent from the last 64 Event 1074 records before the trigger.

```
BEFORE  status = request_accepted
AFTER   status = reconciliation_failed
        reconciliation_error = "no recent Windows System Event 1074 contains
                                planned host-transition intent=…neverissued…"
        event_1074_record_id = <null>   reconciled_host_boot_id = <null>
```

Fail-closed gate, over wired MCP:

```
action=preflight ->
  message     : a prior planned host transition is unreconciled; refusing to authorize another
  detail_code : HOST_TRANSITION_PRIOR_INTENT_UNRECONCILED
  current     : winboot:…:63     prior : winboot:…:62
```

Zero side effects: the preflight directory still held 7 files, i.e. no
authorization record was minted.

### 5.3 Edge case — the regression that caused the outage

With `status=reconciliation_failed` on disk, the live daemon (pid 11116) was
killed to force a cold startup through the same code path that previously
returned `ExitCode::from(4)`:

```
BEFORE  daemon pid 11116, supervisor state=running, intent=reconciliation_failed
TRIGGER Stop-Process -Id 11116 -Force
AFTER   daemon pid 18008, supervisor state=running generation=2
        /health -> 200 {"ok":true,"pid":18008,"tool_count":241}
```

On the old build this exact condition produced 5 failures in 60 s and a latched
`state=fatal`. The startup ERROR is still emitted —
`SETUP_HOST_TRANSITION_RECONCILIATION_FAILED`, "the host boot identity changed
but the transition could not be attributed to this intent; further planned host
transitions are refused" — so the condition is louder than before, not quieter.

`reconciliation_failed_unix_ms` advanced `1785250982233` → `1785251012887` across
the restart, proving the attempt is retried rather than latched.

### 5.4 Edge case — self-heal

The real (attributable) intent was forced into `reconciliation_failed` with a
stale error, as if the event log had been briefly unreadable:

```
BEFORE  status=reconciliation_failed  error="stale prior failure: Windows Event Log was unavailable"  record_id=<null>
AFTER   status=reconciled_after_boot  error=<null>  record_id=123110  provider=User32  reconciled=winboot:…:63
        action=preflight -> authorized=True (pf-1785251083909-b64958d669e34399a2914c6ce8c9a672)
```

The gate lifts by itself once attribution succeeds.

### 5.5 Edge case — same boot, no transition occurred

Intent whose `prior_host_boot_id` equals the current identity (`…:63`):

```
BEFORE  status = request_accepted
AFTER   status = request_accepted   (unchanged; no event lookup attempted)
        event_1074_record_id = <null>
        action=preflight -> authorized = True   (correctly NOT blocked)
```

### 5.6 Not proven

The `HOST_TRANSITION_EVENT_ID_MISMATCH` arm is now unreachable through the
`wevtutil` query, which filters `EventID=1074` before results are returned. It is
retained as defence in depth against a future change of query, and is honestly
recorded here as *not* exercised against reality. Likewise
`HOST_TRANSITION_EVENT_1074_AMBIGUOUS` was not reproduced, because minting two
genuine Event 1074 records carrying one intent id is not something this host can
be made to do.

### 5.7 State restored

`pending-intent.json` was returned to the real reconciled record and re-read
through the daemon after restore: `status=reconciled_after_boot record_id=123110
provider=User32`.

