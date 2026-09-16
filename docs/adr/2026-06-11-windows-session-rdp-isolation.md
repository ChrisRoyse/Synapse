# ADR: Separate Windows Session / RDP Isolation Alternative

Status: Accepted
Date: 2026-06-11
Issue: #746
Parent: #721

## Context

#741 established the normal Synapse hidden-desktop path:

- A hidden Win32 desktop gives an agent a separate desktop object inside the
  operator's current Windows session.
- It is useful for background CDP, UIA, same-desktop window messages, and
  hidden-desktop PrintWindow capture.
- It is not the active input desktop, so raw `SendInput`, DirectInput, and
  hardware-level input paths must fail closed unless the desktop is switched
  visible/input. Switching would steal the operator's session and is not an
  acceptable background-agent behavior.

Some applications still require a real interactive Windows session: raw input
games, DirectInput/Raw Input surfaces, legacy apps tied to the active
interactive desktop, and vendor software that refuses background UIA or window
messages. For those applications, the heavier isolation boundary is not a
hidden desktop. It is a separate Windows user session, usually created through
Remote Desktop Services (RDS) / RDP and run under a separate Windows account.

Sources:

- Microsoft Learn, Remote Desktop Sessions:
  https://learn.microsoft.com/en-us/windows/win32/termserv/terminal-services-sessions
- Microsoft Learn, Window Stations:
  https://learn.microsoft.com/en-us/windows/win32/winstation/window-stations
- Microsoft Learn, `WTSEnumerateSessions`:
  https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsenumeratesessionsa
- Microsoft Learn, `WTSGetActiveConsoleSessionId`:
  https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-wtsgetactiveconsolesessionid
- Microsoft Learn, `WTSQueryUserToken`:
  https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsqueryusertoken
- Microsoft Learn, Remote Desktop Services overview:
  https://learn.microsoft.com/en-us/windows-server/remote/remote-desktop-services/
- UiPath, High-Density Robots:
  https://docs.uipath.com/robot/standalone/2025.10/admin-guide/high-density-robots
- UiPath, Windows sessions:
  https://docs.uipath.com/robot/standalone/2025.10/admin-guide/windows-sessions

## Decision

Synapse keeps hidden Win32 desktops as the default local concurrency path.
Separate Windows sessions / RDP are the documented heavier alternative for
raw-input-required workloads and fleet-scale unattended GUI automation.

Do not implement a simple `--session-id` daemon flag in #746. A flag on the
current user-mode daemon would be misleading because a process cannot safely
see, enumerate, or inject into arbitrary interactive sessions merely by naming
a session id. Cross-session launch and control need either:

1. a per-session Synapse daemon or worker already running inside the target
   interactive session, or
2. a privileged broker service that enumerates sessions, obtains the exact
   target user's primary token, launches a per-session daemon/worker with that
   token, and then limits all MCP actions to that session.

Until that broker/per-session architecture exists, Synapse must report this
mode as not implemented and fail closed for cross-session action attempts with
an explicit reason. Hidden desktops remain the accepted implementation for
#741-#745.

## When To Use Each Isolation Mode

| Need | Hidden Win32 desktop | Separate Windows session / RDP |
|---|---|---|
| Keep operator visible foreground usable | Yes | Yes, if the agent runs in another session |
| Background browser/DOM automation | Yes, prefer CDP | Yes, but usually unnecessary |
| UIA/value/message-based desktop app automation | Yes | Yes |
| PrintWindow-based hidden capture | Yes, explicit #744 path | Yes, inside the target session |
| Raw `SendInput` into an app | No, fail closed | Yes, only from inside that session and behind that session's foreground lease |
| DirectInput/Raw Input app requiring active input desktop | No | Candidate use case |
| Multiple concurrent unattended users on one host | Limited | Yes on supported RDS / multi-session hosts |
| Same Windows user profile shared across agents | Risky | Avoid; prefer one account/profile per agent |
| Cheap local development on a workstation | Yes | Usually no; workstation RDP concurrency is limited and product/licensing dependent |

## Windows Model

Remote Desktop Services creates a separate session for each user logon. Each
session has a unique session id and its own interactive window station named
`WinSta0`. A window station contains desktops, clipboard state, atom table, and
the input/display surface for that session.

Important consequences:

- `WinSta0` is not global. It is per interactive session.
- A process is assigned to a Windows session. Use `ProcessIdToSessionId` or
  WTS APIs as the Source of Truth for that process/session binding.
- Window handles, foreground state, clipboard, and input delivery must be
  interpreted in the owning session, not as machine-global facts.
- A service normally runs in Session 0. It does not become an interactive
  agent in another session by naming that session.
- To launch a process into a logged-on user's session from a service,
  Microsoft's supported path is session enumeration, a user token for the
  exact target session, environment creation, and `CreateProcessAsUser`.
- `WTSQueryUserToken` is a highly privileged API. It requires LocalSystem plus
  `SE_TCB_NAME`; token handles must be protected and closed.

## Required Architecture For Future Support

### Broker

A future RDP/session mode needs a privileged local broker service, separate
from the ordinary per-user MCP daemon.

Broker responsibilities:

- Enumerate sessions with WTS APIs and persist session id, connection state,
  username/SID hash, protocol, and timestamp.
- Select a session by exact `windows_session_id`, not just username.
- Obtain the logged-on user's primary token only for the selected session.
- Launch a per-session Synapse daemon/worker inside that session with
  `CreateProcessAsUser` and `lpDesktop = "WinSta0\\Default"`.
- Close token, process, thread, and environment handles on every path.
- Refuse sessions that are disconnected, locked, missing a user token, or not
  authorized for the requesting agent.
- Record every launch/action with the broker PID, session id, user SID hash,
  target process id, and failure code.

### Per-Session Daemon/Worker

The worker that actually performs perception/action must run in the target
session. It owns that session's:

- foreground lease
- target claim registry
- UIA client state
- capture backend state
- process launch history
- cleanup/job objects

Cross-session calls must not reuse the operator-session daemon's foreground
lease or target registry. The lease is only meaningful within the session that
contains the input queue and interactive `WinSta0`.

### MCP Contract

Future tools should expose session identity explicitly:

- `session_mode`: `current_user_session`, `hidden_desktop`, or
  `windows_session`
- `windows_session_id`: integer WTS session id
- `windows_user_sid_sha256`: redacted user identity
- `session_connection_state`: active, connected, disconnected, locked, logged
  off, or unknown
- `broker_pid` and `worker_pid`
- `input_surface`: `win32_session_foreground`

Every action response must include the session id used for the trigger. A
request naming a window or process from another session must fail closed before
input/capture with a code such as `SESSION_TARGET_MISMATCH`.

## Security And Operations

Separate Windows sessions are a security boundary as much as an automation
surface. Minimum rules:

- Use one Windows account/profile per concurrent agent when possible.
- Do not share mapped drive assumptions across users. Paths and credentials
  are per user/profile.
- Do not cache plaintext credentials in Synapse config. Use Windows credential
  manager, a service account, or an operator-approved secret store.
- Do not grant local administrator rights to robot accounts unless a specific
  application requires it and the risk is accepted.
- Do not enable unsupported workstation RDP patching to bypass Windows product
  limits.
- Treat Remote Desktop Session Host, RDS CALs, Azure multi-session licensing,
  and account creation as operator-owned external policy/billing decisions.
  Synapse can document and verify local state, but it must not silently change
  licensing, billing, or external account state.
- Store logs with user/session identifiers redacted to hashes unless the
  operator explicitly approves raw identity capture.
- Provide cleanup for disconnected sessions, orphan per-session daemons, and
  stale target claims.

## Current Synapse Status

Implemented today:

- Session-owned hidden Win32 desktop launch/lifecycle (#742).
- Hidden-desktop raw foreground-tier refusal (#743).
- Hidden-desktop PrintWindow/UIA perception path (#744).
- Read-only hidden desktop PiP frame surface is implemented locally for #745
  but awaits a fresh Codex MCP tool namespace before manual FSV can accept it.

Not implemented today:

- RDS/RDP session broker.
- Per-session daemon launch through `CreateProcessAsUser`.
- `windows_session_id` MCP target binding.
- Cross-session target mismatch errors.
- Per-session foreground lease registry.
- RDS host setup or licensing automation.

Therefore #746 is a documentation decision only. It does not claim runtime
support for separate Windows session control.

## Future Manual FSV Runbook

When a future issue implements this mode, acceptance must use real
`mcp__synapse` tool triggers plus separate physical Source-of-Truth readbacks.

Precondition SoTs:

- `WTSEnumerateSessions` / `quser` / `qwinsta` readback shows target session
  id, user, and connection state.
- `ProcessIdToSessionId` proves broker and worker PIDs are in expected
  sessions.
- Process table proves worker executable path and command line.
- Real `mcp__synapse.health` from the per-session worker reports the same
  `windows_session_id`.
- `session_list` reports no cross-session target or lease leakage.

Happy path:

1. Create or select an RDP session for a synthetic robot user.
2. Launch a known-text app in that session through the real MCP tool.
3. Read the process session id and enumerate windows from inside that session.
4. Trigger raw input through the real MCP action path from the per-session
   worker.
5. Read the app text/window state inside the target session; expected text is
   present.
6. Separately read the operator console foreground/cursor/input desktop; they
   are unchanged.

Required edge cases:

- Cross-session HWND or PID: request names a target from another session and
  fails closed with explicit session mismatch; no input and no storage success
  row.
- Disconnected/logged-off target session: request fails closed with session
  unavailable; orphan worker is cleaned or marked stale.
- Same username with multiple sessions: request binds by exact session id, not
  username; the non-target session remains unchanged.
- Missing privilege/token: broker fails closed before launch and records the
  missing permission/token reason; no partial worker remains.
- RDS licensing/host role absent: setup reports a prerequisite/operator
  decision instead of falling back to the operator console.

## Consequences

Positive:

- The raw-input path is documented without weakening the hidden-desktop
  fail-closed invariant.
- Future implementation has a clear broker/per-session worker boundary.
- Security-sensitive token/session handling is explicit before code exists.

Negative:

- This does not make raw-input concurrency available on the current daemon.
- A real implementation is larger than a config flag because Windows sessions
  are security-scoped OS objects.
- RDS setup can involve licensing/account decisions that require operator
  approval after local readback.

## Follow-Up

No new issue is filed by this ADR. #746 documents the alternative and the
future implementation boundary. If/when the operator wants runtime support,
file a scoped implementation issue for a broker/per-session worker prototype
and FSV it with the runbook above.
