# Issues #1723 and #1725 FSV - browser exact-target readbacks and owner-ledger recovery - 2026-07-17

## Issues Covered

- #1723: Chrome bridge tools timed out on already-authenticated pages and leaked ambient tab state during mutation readback.
- #1725: stale or missing local durable owner state could leave the Chrome bridge mutation gate unrecoverable.

#1724 remains open. It is the separate audit oversized-row issue discovered while reading lifecycle evidence.

## Root Causes

1. Exact-target browser operations still enumerated every tab before honoring `cdp_target_id` / `targetIdHint`. `selectTabTarget()` called broad `chrome.tabs.query({})`, so a target-specific navigation or DOM read could become proportional to all open tabs and return ambient `target_candidate_count` evidence.
2. Mutating `browser_tabs` operations relisted the whole browser after open/activate/close. The public response for a single tab mutation could include unrelated tab titles/URLs, and any slow unrelated page could influence the mutation verdict.
3. The daemon-side Chrome bridge readback summary copied raw URL/title fields from extension responses into lifecycle rows. That included restricted and high-entropy URLs which are not needed for diagnosis.
4. The durable owner gate had no safe local recovery path for a stale browser-session ledger. Rows from a prior Chrome session had to fail closed to avoid tab-id reuse, but the bridge also could not prune rows for tabs that no longer existed or rebase after all stale owners drained.
5. Restricted-scheme DOM errors wrapped Chrome's raw scripting error. The public typed error was correct, but the nested original error could still include the raw restricted URL.

First-principles invariant: when a caller names an owned target, only that target may be inspected or mutated; the after-read must be the exact target's physical state or exact absence. Ambient tabs are discovery state, not mutation evidence. Owner recovery may prune only facts proven at the Chrome tab Source of Truth.

## Research Used

Research was done after isolating the root cause with both Exa MCP and native web research. Primary sources:

- Chrome Tabs API: https://developer.chrome.com/docs/extensions/reference/api/tabs
- Chrome Scripting API: https://developer.chrome.com/docs/extensions/reference/api/scripting
- Chrome extension service worker lifecycle: https://developer.chrome.com/docs/extensions/develop/concepts/service-workers/lifecycle

Applied conclusions:

- `chrome.tabs.get(tabId)` is the exact-tab readback primitive; `tabs.query({})` returns all tabs and exposes sensitive `url`, `pendingUrl`, `title`, and `favIconUrl` when permissions allow it.
- `chrome.scripting.executeScript` targets a specific tab through `target.tabId`; exact target validation should happen before injection.
- MV3 extension service workers are ephemeral, so owner/session continuity must be persisted in `chrome.storage` and reconciled after worker or extension restart. Globals are not a durable Source of Truth.

## Fix

- Exact tab hints now use `chrome.tabs.get(tabId)` before any broad tab enumeration.
- `browser_tabs` mutation responses use exact target readbacks:
  - open: `chrome.tabs.create` followed by `chrome.tabs.get`
  - activate: exact target state before/after, with Chrome window/bounds/title consistency checks
  - close: exact target state before, then exact `chrome.tabs.get` absence after removal
- Mutation responses return only the affected target state, never all ambient tabs.
- Extension and Rust readback summaries redact URL/title fields by scheme/origin and omit high-entropy title text from public diagnostics.
- The durable owner ledger now:
  - initializes an empty local ledger when no durable local ledger exists for the current browser session,
  - prunes stale rows only when `chrome.tabs.get(tabId)` proves the tab is absent,
  - stores URL fingerprints for newly opened tabs so future stale opened-tab rows can be closed only when the live tab still matches the recorded fingerprint and window,
  - rebases continuity only after stale owners drain and no mutation is in flight,
  - stays fail-closed for live/unreadable stale owners without fingerprints.
- Restricted-scheme DOM errors now expose the typed unsupported-scheme verdict without the raw Chrome cause payload.

No fallback path was added. If exact readback or owner reconciliation cannot prove safety, the bridge fails closed with structured diagnostics.

## Sources Of Truth

- Process/socket: Windows process table and `Get-NetTCPConnection` for `127.0.0.1:7700`.
- MCP client parity: real `mcp__synapse.health`, `mcp__synapse.browser_tabs`, `mcp__synapse.browser_nav`, `mcp__synapse.browser_dom`, and `mcp__synapse.audit`.
- Chrome tab state: exact `chrome.tabs.get(tabId)` present/absent readback exposed through browser tool responses.
- Owner ledger: Chrome extension `chrome.storage.local` / `chrome.storage.session` read and repaired by the installed bridge.
- Audit ledger: `C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-tool-events.jsonl`.
- Deployed extension bytes: configured setup health reported the deployed worker hash and expected build id.

## Runtime Preconditions

Browser FSV installed daemon and bridge before the audit-only rebuild:

```text
process: PID 87208
binary: C:\Users\hotra\.cargo\bin\synapse-mcp.exe
socket: 127.0.0.1:7700 LISTEN owner=87208
bridge build_id: synapse-chrome-bridge-2026-07-17-exact-target-owner-ledger-v6
bridge stale: false
tool_count: 40
tool_surface_sha256: 7baef0742b0aacbb2a838a301a3ef25af3175468b15d831f6aa90b88dfd7b776
```

After the #1724 audit reader rebuild, the final installed daemon is PID `29796`,
`C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, listening on `127.0.0.1:7700`,
with tool surface SHA `d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb`.
The Chrome bridge build remains `synapse-chrome-bridge-2026-07-17-exact-target-owner-ledger-v6`,
`extension_stale=false`.

The browser FSV rows below were captured before that audit-only rebuild on the
same v6 Chrome bridge build. The daemon PIDs in the ledger rows are therefore
part of the evidence, not the final live process PID.

`scripts\synapse-setup.ps1 -SourceDir C:\code\Synapse` installed the repo-built daemon and bridge. The setup command then failed closed with the expected `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` handoff for the already-running Codex process, but the real wired `mcp__synapse` browser/audit tools in this process loaded and executed successfully through the strict client.

## Manual FSV

### Happy Path - Exact Open, Navigate, DOM Locate, Close

Before:

```text
mcp__synapse.health: ok=true pid=87208 chrome_bridge.status=ok extension_stale=false tool_count=40
process/socket: PID 87208 listening on 127.0.0.1:7700
audit ledger: daemon-tool-events.jsonl present
```

Trigger:

```text
mcp__synapse.browser_tabs operation=new url=https://example.com/?synthetic_exact_open
mcp__synapse.browser_nav operation=navigate cdp_target_id=<opened chrome-tab id> url=https://example.com/?synthetic_exact_nav
mcp__synapse.browser_dom operation=locate cdp_target_id=<opened chrome-tab id> query=h1
mcp__synapse.browser_tabs operation=close cdp_target_id=<opened chrome-tab id>
```

After readback:

```text
open response: target_count=1, chrome_window_candidate_count=1, chrome_window_selection_reason=exact_open_tab_readback
navigate response: target_candidate_count=1, target_selection_reason=chrome_tab_id_hint_direct, readback_backend=chrome.tabs.get
DOM locate response: match_count=1, returned_count=1, readback_backend=chrome.scripting.executeScript(debugger-free DOM locator)
close response: target_count=0, tabs=[], chrome_window_selection_reason=exact_close_absence_readback
```

Separate audit ledger read:

```text
browser_tabs line 697 pid=87208 seq=4 status=ok raw_sha256=sha256:921513c390620714ace9b67766c331be2542ed58c653c2080d19938ce17a5cee
browser_tabs line 701 pid=87208 seq=6 status=ok raw_sha256=sha256:c112c571c78c5d7ec47d69a84083534dee7ad0a40833a9b9c1fe6712c7929ed6
browser_nav  line 671 pid=50268 seq=4 status=ok raw_sha256=sha256:2361bfa7b17553a06cc70e40000a86cad6eb962388ce7f8ee4468f3c167cf986
browser_dom  line 673 pid=50268 seq=5 status=ok raw_sha256=sha256:c15ec83e1b2649888a56e6bcd990d6a74fe98a7caee399210d01f73200253d38
```

The earlier happy-path navigation/DOM rows were captured under the prior v5 installed daemon PID `50268`; the final v6 runtime repeated the redaction-sensitive open/DOM/close edge below and is the installed bridge state left running.

### Owner-Ledger Recovery

Before:

```text
browser_tabs operation=new failed closed with operator panic:
durable owners belong to a prior browser session;
stale_browser_session_owner_count=1
```

The stale row pointed at an old synthetic Example Domain tab from this investigation. It had no URL fingerprint because it predated the new ledger format. The bridge correctly refused to mutate it automatically.

Manual recovery trigger:

```text
Verified the live rightmost synthetic Example Domain tab in the existing Chrome window, then used foreground-equivalent UI input to close that exact tab.
```

After readback:

```text
onRemoved pruning drained the stale owner row.
browser_tabs operation=new succeeded afterward.
new opened owner rows now persist urlSha256, chromeWindowId, and tab id so future stale live opened-tab rows can be fingerprint-verified before closure.
```

Verdict: legacy unsafe stale state remains fail-closed until a human-equivalent exact closure removes it; new states have a durable fingerprinted recovery path.

### Edge 1 - Empty URL Opens About Blank

Before:

```text
health ok; Chrome bridge mutation gate enabled; no target for the edge open.
```

Trigger:

```text
mcp__synapse.browser_tabs operation=new url=""
mcp__synapse.browser_tabs operation=close cdp_target_id=<opened about:blank tab>
```

After readback:

```text
new response: one tab only, url=about:blank, exact_open_tab_readback
close response: target_count=0, tabs=[], exact_close_absence_readback
audit: browser_tabs line 685 pid=50268 seq=11 status=ok; line 689 pid=50268 seq=13 status=ok
```

Verdict: empty input follows the documented blank-tab behavior and still uses exact present/absent readback.

### Edge 2 - Structurally Invalid Target

Before:

```text
no tab id chrome-tab:notanumber can exist as a numeric Chrome tab id.
```

Trigger:

```text
mcp__synapse.browser_nav operation=navigate cdp_target_id=chrome-tab:notanumber url=https://example.com/?invalid_target
```

After readback:

```text
result: ACTION_TARGET_INVALID, refused target is not active/owned
audit: browser_nav line 681 pid=50268 seq=9 status=error raw_sha256=sha256:d7ff647204e60adf690b788bd6ba7fc1d771552b13e960c81912a94d2beea4d4
```

Verdict: malformed target ids fail closed before mutation.

### Edge 3 - Closed Target Cannot Be Resurrected

Before:

```text
about:blank synthetic tab had already been closed and exact absence was read back.
```

Trigger:

```text
mcp__synapse.browser_nav operation=navigate cdp_target_id=<closed chrome-tab id> url=https://example.com/?closed_target
```

After readback:

```text
result: ACTION_TARGET_INVALID, refused target is not active/owned
audit: browser_nav line 683 pid=50268 seq=10 status=error raw_sha256=sha256:81d4b529c8979f65f64ca8d6e212061c99d1bbb7f409f28d475035286942186e
```

Verdict: an exact absent target remains absent; navigation does not reopen or retarget.

### Edge 4 - Restricted Scheme Redaction

Before:

```text
v5 restricted-scheme DOM read failed closed with BROWSER_URL_SCHEME_UNSUPPORTED but leaked raw nested Chrome cause text.
```

Fix trigger:

```text
Redacted extension public errors and Rust restricted-scheme wrapper cause details.
Installed v6 bridge build.
Opened a synthetic data: URL tab, attempted browser_dom operation=content, then closed the exact tab.
```

After readback:

```text
new response: one tab only, url=data:redacted, title=redacted, exact_open_tab_readback
DOM content response: BROWSER_URL_SCHEME_UNSUPPORTED; message names restricted scheme "data" only
close response: target_count=0, tabs=[], exact_close_absence_readback
audit: browser_tabs line 697 pid=87208 seq=4 status=ok raw_sha256=sha256:921513c390620714ace9b67766c331be2542ed58c653c2080d19938ce17a5cee
audit: browser_dom  line 699 pid=87208 seq=5 status=error raw_sha256=sha256:642f0f46f17d105439cc267448f558a4948f57b9b028b93b7571c114a0e3040a
audit: browser_tabs line 701 pid=87208 seq=6 status=ok raw_sha256=sha256:c112c571c78c5d7ec47d69a84083534dee7ad0a40833a9b9c1fe6712c7929ed6
```

Byte-level ledger inspection:

```text
daemon-tool-events.jsonl lines 696..701:
line 699 ContainsRawSyntheticMarker=false
line 699 ContainsRawDataPayload=false
line 699 ContainsUnsupported=true
lines 696..701 ContainsRawSyntheticMarker=false
lines 696..701 ContainsRawDataPayload=false
```

Verdict: restricted schemes fail closed and the lifecycle ledger keeps only bounded typed diagnostics.

## Structural Checks

These are structural gates only, not FSV:

```text
node --check extensions\synapse-chrome-debugger\service_worker.js
cargo fmt --all --check
cargo check
cargo clippy --workspace --all-targets
git diff --check
```

`cargo clippy --workspace --all-targets` exited 0 with pre-existing warning-only findings in `synapse-storage` and `dump_cf`; no clippy failure remained in this browser diff.

## Verdict

#1723 and #1725 are fixed and manually FSV-verified through the real wired Synapse MCP client, the installed repo-built daemon, the deployed Chrome bridge, exact Chrome tab present/absent readbacks, and the physical daemon lifecycle JSONL ledger. No automated tests, FSV scripts, FSV harnesses, or GitHub Actions were added or used.
