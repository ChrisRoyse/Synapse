# Foreground-capability computer control - acceptance & operator runbook (epic #717)

This is the **manual acceptance runbook** required by #1009 (explicitly *not* an
automated FSV harness) and the human-active acceptance procedure required by
#1220, plus the operator-safe bootstrap for #1011. It records the
Source-of-Truth (SoT) readbacks to inspect for each gate.

Status legend: **VERIFIED** = proven against an independent SoT in this repo;
**OPERATOR** = requires the human at the machine (by design); **CODE-DONE** =
implemented and unit/integration-gated, awaiting the human-active acceptance run.

---

## 1. What the daemon enforces today (verified)

| Invariant | Tool/code | SoT proof |
|---|---|---|
| Default surface preserves foreground-equivalent capability through routes (#1219) | `normal_agent` profile | `profile operation=status` -> `foreground_capability.profile_preserves_capability=true`, `hidden_tool_routes` names the preferred public `act`/browser/session-lane route for each raw implementation primitive; CF_SESSIONS `mcp/tool-profile/v1/<sid>` row |
| Hidden raw tool fails closed with route proof (#1002/#1004/#1219) | tool-profile policy gate | calling `act_type` from `normal_agent` -> error `TOOL_PROFILE_POLICY_DENIED` carrying the CF_SESSIONS `policy_row` plus `capability_route` |
| Break-glass needs lease + reason + confirm (#999) | `validate_profile_set_policy` | `profile operation=set profile=break_glass` is rejected unless this session owns the lease and passes confirmation plus a non-empty reason |
| Break-glass has a stable Codex callable route (#1261/#1379/#1621/#1622) | `act operation=foreground` | after `target operation=set/claim`, the always-visible facade runs as one daemon-tracked, session-keyed authority transaction through lease acquisition, temporary profile escalation, internal focus delegation, independently verified cleanup, and final audit; caller cancellation detaches rather than aborts that transaction, while a newer physical `operator_panic_epoch` supersedes every agent lease snapshot |
| Profile changes preserve a stable schema (#1020/#1379) | `profile operation=set` | every production profile retains the same <=40 facade names/hash; the CF_SESSIONS row changes operation-level authority without exposing raw implementation tools |
| Agent target distinct from human foreground (#994) | `target operation=list/set` | `target operation=list` reports the human OS foreground separately; per-entry `is_foreground`; `GetForegroundWindow()` cross-check matches |
| Passive target discovery without shelling out (#1021) | `target operation=list` | HWND+PID rows match Win32 `Get-Process MainWindowHandle`; round-trips through `target operation=set` with no activation |

### Manual readback checklist

Use the real wired Synapse MCP client for the trigger, then read the SoT with a
separate operation. Do not use any script, helper client, or harness as
acceptance.
Supporting tests may exercise notifications/tool-list mechanics, but issue
closure still requires manual `profile operation=status`, `session operation=list`,
`target operation=status`, `storage operation=inspect`/read-only CF row, and physical
window/DOM/UIA readbacks.

Cross-check `target operation=list` against Win32 when the manual scenario needs the
human OS foreground SoT:

```powershell
Add-Type 'using System;using System.Runtime.InteropServices;
  public class Fg{ [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); }'
[int64][Fg]::GetForegroundWindow()        # == target operation=list human_os_foreground_hwnd
Get-Process | ? {$_.MainWindowHandle -ne 0} | % { "{0} {1} {2}" -f [int64]$_.MainWindowHandle,$_.Id,$_.ProcessName }
```

---

## 2. Operator-safe Chrome-bridge bootstrap (#1011, unblocks #996/#997/#1000)

The daemon ships the new bridge extension on disk
(`extensions/synapse-chrome-debugger`, build
`synapse-chrome-bridge-2026-06-15-1011-reload-self-997-type-active-v1`) which
implements `targetInfo`, `typeActiveElement`, `navigateTab`, `openTab`,
`closeTab`, and `reloadSelf`. **The currently *loaded* worker in the running
Chrome predates `reloadSelf`**, so the background self-reload path cannot
activate the new worker — this is the documented chicken-and-egg in #1011.

The daemon already behaves correctly while stale (verified in `health`):
- `chrome_bridge.status = "stale"`, `extension_stale = true`
- `extension_stale_reasons` names the exact missing capabilities and expected
  build id/sha256 (fail-closed, no silent fallback)
- any bridge command requiring a missing capability fails with
  `CHROME_BRIDGE_EXTENSION_STALE` rather than foregrounding.

**One-time operator activation (the only hands-on step; do it once):**
1. Confirm the on-disk build matches the daemon's expected hash:
   `health` → `chrome_bridge.extension_build_sha256 expected=…`.
2. In the already-open Chrome, open `chrome://extensions`, enable Developer
   mode, and click **Reload** on "Synapse Chrome bridge" — OR fully quit and
   reopen Chrome. (This is unavoidable for the *first* activation because the
   loaded worker has no `reloadSelf`; every subsequent update uses
   `cdp_bridge_reload` with no foreground.)
3. Re-read `health`: `chrome_bridge.status` must flip to `ok`,
   `extension_capabilities` must list all six commands, `extension_build_id`
   must equal the expected build.

After activation, `cdp_bridge_reload` performs all future reloads in the
background (`chrome.runtime.reload()`), so this manual step never recurs.

Then verify #996/#997/#1000 (no new Chrome process, human foreground on a
different window throughout):
- **#996**: `cdp_target_info` on an owned background tab returns url/title/
  ready_state/active-element with `backend_tier_used` non-foreground and no
  debugger attach. SoT: Chrome tab table; `GetForegroundWindow` unchanged.
- **#997**: `act_type` into a known `<input>`/`<textarea>`/contenteditable in an
  **inactive** owned tab; read the DOM value back via `cdp_target_info`. SoT:
  target DOM value; OS foreground/cursor unchanged; action-audit tier=background.
- **#1000**: open/bind a dashboard tab, observe an approval inactive, decide it
  from that tab; read CF_KV approval row + mailbox. SoT: CF_KV decision row.

---

## 3. Human-active foreground-equivalent acceptance run (#1220 - the final gate)

Run this with **you actively using the computer** (switching foreground
windows, typing, moving the mouse) the entire time.

Setup:
- Start the target 50+ Synapse MCP sessions, or the maximum locally supported
  count plus a filed capacity issue if a hard local limit remains after all
  reversible setup. Each session opens or discovers a different target through
  `browser_tabs`/`target`, then uses `target operation=set` and `target operation=claim`.
- Each session must have an `agent_logical_foreground` / `foreground_lane`
  readback. None claims the window you (the human) are using.
- Keep a third shell open to sample SoT.

During the run, while the human keeps working:
1. Mix browser navigation/eval, click, type/set_field, read, screenshot, and
   shell tasks across separate agent lanes/targets.
2. Routine work uses `act operation=invoke`, browser facades, or the session's
   foreground lane. No routine task may fall back to the human OS foreground.
3. Exactly one edge may test explicit real OS foreground break-glass with an
   owned lease and audit proof.

SoT samples to capture **before / during / after** (paste into the #1220 close
comment):
- `session operation=list` - all live sessions, distinct `agent_logical_foreground` /
  `foreground_lane` readbacks, and no implicit human OS foreground fallback.
- `target operation=status` - each window/tab target owned by exactly one session;
  no overlap.
- `GetForegroundWindow()` / `GetCursorPos()` - the human's foreground/cursor are
  whatever the human set; **no agent action changed them** (sample repeatedly).
- Per-target DOM/UIA/window readbacks — each agent sees only its own target.
- `CF_ACTION_LOG` rows - routine actions identify target/lane/session routing;
  any real foreground tier row is present **only** for the explicit break-glass
  test and carries a held lease id.

**Pass =** many agents act/read in their own lanes/targets with zero cross-talk
while the human freely uses the real foreground; the only real foreground-tier
action in the audit is the deliberate break-glass test. Close #1220 with this
evidence.

---

## 4. Behavioral tool-affordance acceptance (#1009)

Goal: prove the task-scoped surface steers agents to capability-preserving
routers and lane-aware tools without stranding foreground-equivalent work.
Compare these `tools/list` profiles via real spawned agents / the wired client:

| Profile | How to get it | Expected surface |
|---|---|---|
| trusted unscoped admin/diagnostic | unscoped stdio, or `SYNAPSE_DEBUG_TOOLS=1` | full implementation surface; never the scoped HTTP profile contract |
| normal capability-preserving | default `normal_agent` | the <=40 public facade surface; raw implementation primitives stay hidden and ordinary target/browser operations are admitted |
| browser-control task | `profile operation=set profile=browser_control` | the same stable facade names; browser-control authority is enforced inside facade operations |
| browser-debugger task | `profile operation=set profile=browser_debugger` with confirm + reason | the same stable facade names; debugger-backed `browser_debugger` operations are admitted while raw implementation tools remain hidden |
| break-glass/admin | `act operation=foreground` for one action, or lease + `profile operation=set profile=break_glass` | the same stable facade names; foreground-capable facade operations are admitted while raw `act_*` implementation primitives remain hidden |

Codex/client compatibility note (#1261/#1445): some clients keep a static callable
tool namespace even after the server sends `notifications/tools/list_changed`.
For deliberate real-foreground activation, bind and claim the exact window with
the public `target` facade, then call
`act {"operation":"foreground","reason":"<why>","action":{"verb":"focus_window"}}`.
The facade acquires the foreground lease, temporarily transitions the profile,
delegates internally, byte-restores the prior CF_SESSIONS profile row, releases
a newly acquired lease, and verifies both cleanup Sources of Truth before
returning. The entire operation is daemon-tracked, so request cancellation
detaches while cleanup and final audit continue under the same session gate;
panic is caught while the rollback guard remains live. A newer physical
`operator_panic_epoch` deletes the guarded session lease row, preserves the
operator owner, and returns a structured safety interruption.
For browser debugger work, use the already-visible `browser_debugger` facade
after setting `profile=browser_debugger`; raw debugger implementation tools stay
hidden, while debugger-backed facade operations fail closed with
`TOOL_PROFILE_POLICY_DENIED` until the CF_SESSIONS profile row is updated.

For each, give the same synthetic task with a known target/lane solution
("read the title of the LinkedIn tab", "type into the dashboard search box",
"run `git status` in the repo") and record, per the #1009 schema:
- first tool attempted, any rejected tool attempts, selected target semantics,
  whether any foreground-tier tool was attempted.

Expected: normal/browser-control agents pick `target` -> `act`/`read_text`/browser
facades and can still complete valid
foreground-equivalent work through the session lane. Any attempt at a hidden raw
tool returns `TOOL_PROFILE_POLICY_DENIED` with the CF_SESSIONS policy row,
`capability_route`, and a physical audit row. Break-glass authority is exercised
through the same public facade names, never by rediscovering raw implementation
tools.
Feed the table into the #1007 matrix (`docs/multi-agent-capability-matrix.md`,
already kept in sync by `multi_agent_capability_matrix.rs`).

---

## 5. #998 verify_delta preflight — verified by code audit (already satisfied)

All four `act_type` verify paths read the exact postcondition Source-of-Truth
*before* mutating and fail closed before sending input when it is unreadable, and
emit a distinct error with before/after evidence when the readback is lost
*after* mutation:

| Verify mode | Preflight (fail closed before mutation) | Post-mutation-loss error |
|---|---|---|
| foreground focused-text | `capture_act_type_text_signature(require_focused_text_value=true)` errors `ACTION_VERIFY_SURFACE_UNAVAILABLE` before `act_type_with_handle` (m2_tools.rs:384,2384) | `act_type_verify_surface_unavailable_error` + before/after signatures (m2_tools.rs:5067) |
| `expected_browser_url_regex` | `require_browser_url=true` → `cdp_selected_target_url`/bridge readback fail closed before input (m2_tools.rs:388,2298-2304) | `postcondition_failed_error` w/ before/after (m2_tools.rs:4966) |
| chromium foreground fallback | `capture_act_type_text_signature(require_focused_text_value=true)` (m2_tools.rs:356) | same as foreground focused-text |
| `into_element` CDP/bridge | `before` node-value read fails closed before `Input.insertText` (type_text.rs:153-163) | `verify_cdp_type_delta` distinct error + evidence (type_text.rs:183-192) |

The #925 mutate-then-fail symptom was fixed by this preflight wiring +
debugger-free Chrome bridge readback. Re-verify after the operator extension
reload (§2): an `act_type` with `verify_delta` into a tab whose readback surface
is blocked must refuse *before* input (SoT: target value unchanged).

## 6. Remaining net-new work

- **#1005** — a high-level capability-preserving computer-use router
  (`navigate/click/set_field/read/screenshot/run_shell` by target/lane
  capability) so models pick one intent-level verb instead of raw primitives.
  Net-new tool surface.
