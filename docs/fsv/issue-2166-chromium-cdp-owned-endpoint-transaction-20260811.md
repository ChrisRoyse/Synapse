# Issue #2166 FSV — Chromium CDP launch is an owned endpoint transaction

Date: 2026-08-11 (America/Chicago)

## Verdict

PASS. A Chromium/CDP launch is now published only after its configuration,
process generation, loopback listener owner, browser WebSocket identity, and
page target agree. Failure terminates the exact spawned tree and preserves or
deletes the profile according to explicit ownership. Natural exit evicts the
exact registry row and deletes a Synapse-owned profile. A later launch also
reclaims an ownership-marked profile whose launcher process generation is dead.

Implementation commits:

- `fa4f0f3ba0b138520259684e20d792ff0a24cdda` — owned endpoint transaction
- `9b076713748c8290709977cb489a2f9b641b67b4` — strict HTTP response framing

No automated test or CI surface was added or used. Acceptance below is manual
Full State Verification against the installed release daemon and real Microsoft
Edge processes.

## Diagnosis from first principles

Before the fix, `act_launch` treated either a caller port flag or a
`--user-data-dir` flag as sufficient reason to skip CDP ownership. Its
ephemeral path read only the first line of `DevToolsActivePort`, immediately
published a bare `(launch_pid, port)`, returned a nullable endpoint on timeout,
and skipped endpoint verification entirely when no URL was supplied. Later
perception accepted any TCP listener on the registered/default port. The
session process resource owned the Windows Job Object, but did not own the CDP
registry row or ephemeral profile.

That violated the actual identity equation:

```text
owned CDP endpoint =
  exact launch process generation
  + exact loopback socket owner process generation in the owned tree
  + exact DevToolsActivePort browser path
  + exact /json/version browser WebSocket identity
  + exact /json/list page target
  + exact profile ownership and cleanup policy
```

A reachable port or successful return value proves none of those associations.

## Independent research after diagnosis

The Exa lane was checked with:

```powershell
pwsh -File scripts/check-research-lane.ps1 -Probe \
  'Chromium remote debugging endpoint ownership DevToolsActivePort json version websocket process port identity crash safe temporary profile cleanup best practices'
```

`exa-search-server` 3.4.0 initialized, advertised its tools, and completed a
real query (`verdict=live`). The built-in web lane independently read these
primary sources:

- Chrome 136 remote-debugging security and the required non-default profile:
  <https://developer.chrome.com/blog/remote-debugging-port>
- Chrome DevTools Protocol discovery endpoints and browser WebSocket URL:
  <https://chromedevtools.github.io/devtools-protocol/>
- Windows owner-PID TCP listener table:
  <https://learn.microsoft.com/windows/win32/api/tcpmib/ns-tcpmib-mib_tcptable_owner_pid>
- Windows `GetExtendedTcpTable` API:
  <https://learn.microsoft.com/windows/win32/api/iphlpapi/nf-iphlpapi-getextendedtcptable>

The resulting design requires a dedicated profile, treats `/json/version` as
the browser endpoint authority, treats the two-line `DevToolsActivePort` file as
the ephemeral-port/browser-path authority, and uses the Windows owner-PID TCP
table plus process creation FILETIMEs as the physical process authority.

## Implemented contract

- Caller switches are parsed structurally before spawn. Partial pairs,
  duplicates, empty/missing values, pipe transport, disabled-CDP contradictions,
  and ports outside `0..=65535` fail with
  `ACTION_LAUNCH_CDP_CONFIG_INVALID`.
- Synapse ephemeral profiles are exclusively created beneath
  `%TEMP%\synapse-cdp-profiles` with a durable v1 ownership marker containing an
  exact token plus daemon PID/creation FILETIME.
- Ephemeral `DevToolsActivePort` must be UTF-8, at most 4096 bytes, and exactly
  two lines: a non-zero port and `/devtools/browser/<id>`.
- `/json/version` is read with strict HTTP framing: one `Content-Length`, a
  64-KiB header cap, a 1-MiB total cap, and an exact framed-body read. Duplicate,
  missing, chunked, malformed, truncated, or oversized framing fails loudly.
- `GetExtendedTcpTable(TCP_TABLE_OWNER_PID_LISTENER)` must report exactly one
  loopback-only owner. That PID must be in the spawned process tree, and both
  the launch and listener creation FILETIMEs must match independent reads.
- `/json/version.webSocketDebuggerUrl`, `DevToolsActivePort`, and `/json/list`
  must agree on port/browser/page identities. A `/json/list` page with a matching
  page WebSocket is required even when the caller supplied no URL.
- Only the fully attested row is published. It contains registration id, launch
  and listener process generations, port, browser id/WebSocket, and a canonical
  profile-path hash. A contradictory PID/listener/port/WebSocket row is refused.
- Registered probes revalidate the exact identity and never fall through to a
  conventional/default port. Poisoned registry state also fails closed.
- Session teardown and an exact-generation natural-exit monitor both perform
  idempotent conditional registry eviction. Synapse deletes only an exact
  marker-proven ephemeral profile; caller-managed stable profiles are retained.
- Before creating another ephemeral profile, reconciliation fails on unsafe or
  unreadable entries and reclaims only marker-proven entries whose exact
  launcher generation is dead.
- The response and durable `CF_PROCESS_HISTORY` row expose the complete
  attestation so operators can independently inspect it.

## Build, lint, and installed release

```text
cargo check --workspace
  PASS

pwsh -File scripts/lint.ps1
  PASS: all seven gates in root and Calyx workspaces
  root/calyx fmt, cargo-deny, clippy --workspace --all-targets -D warnings PASS
  unreached Calyx public API ratchet: 357 == baseline
```

Pre-deploy Source of Truth:

```json
{
  "daemon_pid": 14120,
  "installed_sha256": "8CF698DAE1559712BA6D97568F9D2E26AAE98329BD04562AE284F073E676A30A",
  "profile_root_exists": false
}
```

The supported installer built with 12 Cargo/CMake jobs, detected no NVIDIA/CUDA
device, selected the CPU/AVX2 path, candidate-verified the daemon, installed it,
and restarted the service. It then deliberately returned
`SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` because this already-running Codex
PID began with the previous tool-surface fingerprint. That fail-closed client
guard did not obscure deployment state; separate reads proved:

```json
{
  "daemon_pid": 21916,
  "build": "9b076713748c",
  "build_commit": "9b076713748c8290709977cb489a2f9b641b67b4",
  "build_checkout_commit": "9b076713748c8290709977cb489a2f9b641b67b4",
  "build_tree_state": "clean",
  "build_matches_checkout": true,
  "installed_sha256": "4D678DB3132EEA17547A8D2B6A3D3A1BC4C5EFD83B9BBD5559F33DDC351D238A",
  "bind": "127.0.0.1:7700",
  "health_ok": true,
  "chrome_bridge_status": "ok",
  "chrome_bridge_host_count": 1
}
```

## Source of Truth definitions

1. Process identity: live Windows process table and `GetProcessTimes` FILETIME.
2. Listener identity: live `Get-NetTCPConnection`/Windows owner-PID table.
3. Browser identity: physical `DevToolsActivePort` bytes plus independent HTTP
   reads of `/json/version` and `/json/list`.
4. Profile ownership: directory contents and durable ownership-marker bytes.
5. Durable launch result: decoded `CF_PROCESS_HISTORY` row read after launch.
6. Registry cleanup: daemon cleanup readback after independently observing
   process, listener, and profile absence.

## Manual execution and evidence

### Initial execution found and fixed a real framing defect

The first installed launch reached a valid listener/port file but failed with:

```text
ACTION_LAUNCH_CDP_ATTESTATION_FAILED
last_error=read CDP version response ... os error 10060
```

Root cause: the raw HTTP reader waited for TCP EOF. Real Edge kept the
connection open after sending the valid response, so the reader timed out.
This was fixed with strict `Content-Length` framing (commit `9b076713`). The
failed launch independently proved cleanup:

```json
{
  "failed_root_pid": 18964,
  "owned_process_count": 9,
  "all_owned_processes_exist_after": false,
  "profile": "...\\synapse-cdp-profiles\\816-0-18cac6c522be9f4c",
  "profile_exists_after": false,
  "profile_root_entries_after": 0
}
```

### Happy path: real headless Edge, isolated ephemeral profile

Trigger:

```text
process operation=launch
target=C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe
args=--headless=new --disable-gpu
     https://example.com/?synapse_fsv_2166=2plus2equals4
cdp_debug=true
```

Before:

```json
{
  "matching_CF_PROCESS_HISTORY_rows": 0,
  "ephemeral_profile_entry_count": 0
}
```

Independent after-state:

```json
{
  "pid": 17504,
  "process_creation_time_100ns": 134309336560485209,
  "listener": "127.0.0.1:52815",
  "listener_owner_pid": 17504,
  "registration_id": "607828ee5a5035684b6da11c3ff153dfa83728054b74a939278730f9cffffece",
  "browser_id": "d87d2ced-afbd-4c07-9f75-ee2db416dbb0",
  "browser_websocket": "ws://127.0.0.1:52815/devtools/browser/d87d2ced-afbd-4c07-9f75-ee2db416dbb0",
  "target_id": "22F3829B69BD9A048210CF136917D21B",
  "target_url": "https://example.com/?synapse_fsv_2166=2plus2equals4",
  "target_title": "Example Domain",
  "profile": "C:\\Users\\hotra\\AppData\\Local\\Temp\\synapse-cdp-profiles\\21916-0-18cac7f4b5e2c3e0",
  "profile_file_count": 712,
  "profile_ownership": "synapse_ephemeral"
}
```

The exact command line contained the injected port/profile/silent/disable flags
plus the synthetic URL. The ownership marker named launcher PID 21916 and exact
creation FILETIME `134309336017740159`. `DevToolsActivePort` contained:

```text
52815
/devtools/browser/d87d2ced-afbd-4c07-9f75-ee2db416dbb0
```

Its SHA-256 was
`D48FE9D6A3A617E4A8B6B02AD4E1EC2A59D8170462651AC892F11D9E984CAF8F`.
Independent `/json/version` and `/json/list` reads matched the browser URL,
page target id, synthetic URL, and title above.

The separate durable read returned one row:

```text
CF_PROCESS_HISTORY
key=process_history/v1/act_launch/2026-08-11T14_54_17.191969300+00_00/17504
pid=17504
process_creation_time_100ns=134309336560485209
cdp_listener_pid=17504
cdp_debug_port=52815
cdp_registration_id=607828ee5a5035684b6da11c3ff153dfa83728054b74a939278730f9cffffece
cdp_verified_target_id=22F3829B69BD9A048210CF136917D21B
status=started
```

### Boundary/edge audit

| Case | Trigger | Expected | Physical after-state |
|---|---|---|---|
| Partial pair | caller `--user-data-dir` without a port | reject before spawn | `ACTION_LAUNCH_CDP_CONFIG_INVALID`, reason `user_data_dir_without_remote_debugging_port`; profile absent; CF history rows 0 |
| Duplicate | two `--remote-debugging-port=0` flags | reject ambiguity before spawn | `ACTION_LAUNCH_CDP_CONFIG_INVALID`, reason `duplicate_cdp_switch`; profile absent; CF history rows 0 |
| Out of range | port `65536` | reject parse before spawn | `ACTION_LAUNCH_CDP_CONFIG_INVALID`, reason `remote_debugging_port_invalid`; profile absent; CF history rows 0 |
| Foreign/reused port | second real Edge tree requested live port 52815 | reject listener outside owned tree and terminate only second tree | last error named listener PID 17504 vs root 23468 and its 16-PID tree; all 16 rejected-tree PIDs absent; good PID/listener unchanged; caller stable profile retained with 290 files; CF history rows 0 |

The collision cleanup reported `profile_ownership=caller_managed_stable`,
`profile_deletion_attempted=false`, and `profile_exists_after=true`; its exact
temporary FSV profile was removed manually only after a separate process-table
read proved zero live Edge owners.

### Natural-exit cleanup

Trigger: sent real CDP `{"id":1,"method":"Browser.close"}` to the attested
browser WebSocket. In under three seconds:

```json
{
  "pid_17504_exists": false,
  "listener_52815_exists": false,
  "profile_exists": false,
  "profile_root_entries": 0
}
```

Daemon registry/profile readback for the same registration:

```json
{
  "registration_present_before": true,
  "registry_eviction_attempted": true,
  "registry_evicted": true,
  "registration_present_after": false,
  "profile_existed_before": true,
  "profile_deletion_attempted": true,
  "profile_exists_after": false,
  "failed": false,
  "errors": []
}
```

### Dead-generation profile reconciliation

A real directory `stale-fsv-2166` was created under the owned profile root with
a valid marker/token, a sentinel file, launcher PID `4294967294`, and launcher
creation `1`. A separate CIM process-table read returned zero matching
processes. The next real Edge launch independently produced:

```json
{
  "stale_path_exists": false,
  "stale_sentinel_exists": false,
  "new_pid": 5552,
  "new_process_creation_time_100ns": 134309339408717909,
  "new_listener": "127.0.0.1:62488",
  "new_listener_owner_pid": 5552,
  "new_browser_id": "5c3eb39e-ac40-447f-8dc9-a7d192f9a1ea",
  "new_registration_id": "c7ae529ac275ca015ac45329421321868fd79200153a21e9c1e0b63744d5ba24"
}
```

The daemon logged `M4_ACT_LAUNCH_CDP_STALE_PROFILE_RECLAIMED` with the exact
stale path/PID/creation identity. Its durable `CF_PROCESS_HISTORY` row exists at
`process_history/v1/act_launch/2026-08-11T14_59_02.431337900+00_00/5552`.
After a real `Browser.close`, PID 5552, port 62488, the new profile, the stale
profile, and all profile-root entries were absent. The empty FSV root was then
removed; `%TEMP%\synapse-cdp-profiles` is absent at final readback.

## Final state

- Installed release daemon is healthy at PID 21916 and serves exact build
  `9b076713748c` from the clean `main` checkout.
- Both successful launch rows physically remain in `CF_PROCESS_HISTORY`.
- Rejected launch rows are absent from `CF_PROCESS_HISTORY`.
- Every FSV Edge process and loopback debug listener is absent.
- Every temporary FSV profile and the now-empty ephemeral profile root is absent.
- The user’s already-open Chrome/Gmail state was not used or modified by these
  headless Edge launch tests.

## GitHub acceptance and closure

The acceptance evidence was posted through the already-open authenticated
Chrome tab and independently read back from GitHub's rendered DOM:

- acceptance permalink:
  <https://github.com/ChrisRoyse/Synapse/issues/2166#issuecomment-5254974773>
- marker `SYNAPSE_FSV_ACCEPTANCE_2166_20260811` matched exactly one rendered
  element after submission;
- after the close trigger, `Close issue` was absent and `Reopen issue` matched
  exactly one enabled button, independently proving issue #2166 is closed.
